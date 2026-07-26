//! Event-queue lifecycle integration tests over the public `Wallet` FFI
//! object (KTD-8). Offline-runnable like `restart.rs`: the Esplora URL points
//! at a closed local port.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use wallet_core::{Event, Wallet, WalletConfig, WalletError};

const UNREACHABLE_ESPLORA: &str = "http://127.0.0.1:1";
const UNREACHABLE_RGS: &str = "http://127.0.0.1:1/snapshot";

fn test_wallet(storage_dir: &Path) -> Wallet {
    Wallet::new(WalletConfig {
        storage_dir: storage_dir.to_str().unwrap().to_string(),
        esplora_url: Some(UNREACHABLE_ESPLORA.to_string()),
        rgs_url: Some(UNREACHABLE_RGS.to_string()),
    })
}

/// A runtime standing in for the foreign executor that drives UniFFI async
/// fns — deliberately NOT the node's runtime.
fn foreign_executor() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

fn next_with_timeout(rt: &tokio::runtime::Runtime, wallet: &Wallet, secs: u64) -> Event {
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(secs), wallet.next_event()).await
    })
    .expect("next_event must complete within the timeout")
}

#[test]
fn offline_fresh_start_queues_node_started_before_sync_completes() {
    let dir = tempfile::tempdir().unwrap();
    let wallet = test_wallet(dir.path());
    let rt = foreign_executor();

    // Fresh + offline: start succeeds degraded, and the queue is observable
    // with no network — NodeStarted first (emitted without waiting for chain
    // sync, which here never completes), then SyncFailed.
    wallet.start().unwrap();
    assert_eq!(next_with_timeout(&rt, &wallet, 5), Event::NodeStarted);
    wallet.event_handled().unwrap();
    assert_eq!(next_with_timeout(&rt, &wallet, 5), Event::SyncFailed);
    wallet.event_handled().unwrap();

    wallet.stop().unwrap();
    assert_eq!(next_with_timeout(&rt, &wallet, 5), Event::NodeStopped);
    wallet.event_handled().unwrap();

    // Fully drained: an extra ack is a typed misuse error.
    assert_eq!(wallet.event_handled(), Err(WalletError::NoPendingEvent));
}

#[test]
fn stop_completes_a_pending_next_event_with_node_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let wallet = Arc::new(test_wallet(dir.path()));
    let rt = foreign_executor();

    wallet.start().unwrap();
    // Drain the startup events so the queue is empty and next_event blocks.
    assert_eq!(next_with_timeout(&rt, &wallet, 5), Event::NodeStarted);
    wallet.event_handled().unwrap();
    assert_eq!(next_with_timeout(&rt, &wallet, 5), Event::SyncFailed);
    wallet.event_handled().unwrap();

    // stop() from a plain thread while next_event is awaiting on the test
    // (foreign) runtime. The node's own runtime is dropped inside stop();
    // the pending await must still complete promptly (KTD-8 lifecycle
    // contract: runtime-independent notification).
    let stopper = {
        let wallet = Arc::clone(&wallet);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            wallet.stop().unwrap();
        })
    };

    let terminal = next_with_timeout(&rt, &wallet, 10);
    assert_eq!(
        terminal,
        Event::NodeStopped,
        "a pending next_event must complete with the terminal NodeStopped"
    );
    wallet.event_handled().unwrap();
    stopper.join().unwrap();
}

#[test]
fn unacked_events_are_redelivered_by_a_rebuilt_wallet() {
    let dir = tempfile::tempdir().unwrap();

    let wallet = test_wallet(dir.path());
    wallet.start().unwrap();
    wallet.stop().unwrap();
    // Three events queued (NodeStarted, SyncFailed, NodeStopped) — none
    // acked. Drop the whole wallet, simulating process death mid-handling.
    drop(wallet);

    // The rebuilt wallet redelivers the same events in order, WITHOUT the
    // node ever starting — the queue is readable while stopped.
    let rebuilt = test_wallet(dir.path());
    let rt = foreign_executor();
    for expected in [Event::NodeStarted, Event::SyncFailed, Event::NodeStopped] {
        // Redelivery is idempotent: the front event repeats until acked.
        assert_eq!(next_with_timeout(&rt, &rebuilt, 5), expected);
        assert_eq!(next_with_timeout(&rt, &rebuilt, 5), expected);
        rebuilt.event_handled().unwrap();
    }
    assert_eq!(rebuilt.event_handled(), Err(WalletError::NoPendingEvent));
}

#[test]
fn stubbed_operations_return_typed_not_implemented_errors() {
    let dir = tempfile::tempdir().unwrap();
    let wallet = test_wallet(dir.path());

    assert!(matches!(
        wallet.receive_jit(100_000),
        Err(WalletError::NotImplemented { .. })
    ));
    assert!(matches!(
        wallet.send("lnbc1exampleinvoice".to_string()),
        Err(WalletError::NotImplemented { .. })
    ));
}

#[test]
fn balances_require_a_running_node_and_read_zero_on_fresh_start() {
    let dir = tempfile::tempdir().unwrap();
    let wallet = test_wallet(dir.path());

    assert_eq!(wallet.balances(), Err(WalletError::NotRunning));

    wallet.start().unwrap();
    let balances = wallet.balances().unwrap();
    assert_eq!(balances.lightning_msat, 0);
    assert_eq!(balances.onchain_sats, 0);
    wallet.stop().unwrap();

    assert_eq!(wallet.balances(), Err(WalletError::NotRunning));
}
