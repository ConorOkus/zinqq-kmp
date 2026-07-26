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
fn restart_purges_a_stale_node_stopped_so_the_new_event_loop_does_not_exit() {
    let dir = tempfile::tempdir().unwrap();

    // Start + stop with NOTHING consumed: [NodeStarted, SyncFailed,
    // NodeStopped] persists unacked — the same on-disk state as a process
    // dying between stop()'s push and the consumer's ack.
    let wallet = test_wallet(dir.path());
    wallet.start().unwrap();
    wallet.stop().unwrap();
    drop(wallet);

    // Next launch starts the node again. The shells' event loops exit on ANY
    // NodeStopped, so the stale one (only meaningful to the process that
    // pushed it) must be purged by start(): the head of the queue is the
    // stale-but-harmless startup pair, then THIS run's startup pair, with no
    // NodeStopped anywhere before the new NodeStarted.
    let rebuilt = test_wallet(dir.path());
    rebuilt.start().unwrap();
    let rt = foreign_executor();
    for expected in [
        Event::NodeStarted,
        Event::SyncFailed,
        Event::NodeStarted,
        Event::SyncFailed,
    ] {
        let event = next_with_timeout(&rt, &rebuilt, 5);
        assert_ne!(
            event,
            Event::NodeStopped,
            "a stale NodeStopped from a previous run must not be redelivered \
             to a running node's event loop"
        );
        assert_eq!(event, expected);
        rebuilt.event_handled().unwrap();
    }

    // stop() still delivers a fresh NodeStopped for THIS run.
    rebuilt.stop().unwrap();
    assert_eq!(next_with_timeout(&rt, &rebuilt, 5), Event::NodeStopped);
    rebuilt.event_handled().unwrap();
    assert_eq!(rebuilt.event_handled(), Err(WalletError::NoPendingEvent));
}

#[test]
fn wired_operations_return_typed_errors_while_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let wallet = test_wallet(dir.path());

    // receive_jit is wired (U4): on a stopped wallet it fails typed, without
    // ever touching the network.
    assert!(matches!(
        wallet.receive_jit(100_000),
        Err(WalletError::NotRunning)
    ));
    // send is wired (U5): on a stopped wallet it fails typed BEFORE any
    // parsing (the garbage string is never looked at).
    assert!(matches!(
        wallet.send("definitely not an invoice".to_string()),
        Err(WalletError::NotRunning)
    ));
}

/// Builds and signs a fresh fixed-amount mainnet invoice for the send tests.
fn signed_mainnet_invoice() -> String {
    use bitcoin::hashes::{sha256, Hash};
    use bitcoin::secp256k1::{Secp256k1, SecretKey};
    use lightning::types::payment::PaymentSecret;
    use lightning_invoice::{Currency, InvoiceBuilder};

    let secret = SecretKey::from_slice(&[0x4d; 32]).unwrap();
    InvoiceBuilder::new(Currency::Bitcoin)
        .description("u5 ffi send test".to_string())
        .payment_hash(sha256::Hash::from_byte_array([0x55; 32]))
        .payment_secret(PaymentSecret([0x66; 32]))
        .duration_since_epoch(
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap(),
        )
        .min_final_cltv_expiry_delta(144)
        .expiry_time(Duration::from_secs(3_600))
        .amount_milli_satoshis(50_000_000)
        .build_signed(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &secret))
        .unwrap()
        .to_string()
}

#[test]
fn send_failures_surface_as_typed_errors_and_payment_failed_events() {
    let dir = tempfile::tempdir().unwrap();
    let wallet = test_wallet(dir.path());
    let rt = foreign_executor();

    wallet.start().unwrap();
    assert_eq!(next_with_timeout(&rt, &wallet, 5), Event::NodeStarted);
    wallet.event_handled().unwrap();
    assert_eq!(next_with_timeout(&rt, &wallet, 5), Event::SyncFailed);
    wallet.event_handled().unwrap();

    // Validation failure: a distinct typed error, and NO event (nothing was
    // attempted, so there is no payment outcome to report).
    assert!(matches!(
        wallet.send("junk".to_string()),
        Err(WalletError::InvalidInvoice { .. })
    ));

    // Attempt failure: a valid mainnet invoice on a channel-less node has no
    // route. The typed error and the queued PaymentFailed carry the SAME
    // distinct reason (KTD-8: the queue is the durable source of truth).
    let reason = match wallet.send(signed_mainnet_invoice()) {
        Err(WalletError::SendFailed { reason }) => reason,
        other => panic!("expected SendFailed, got {other:?}"),
    };
    assert_eq!(
        next_with_timeout(&rt, &wallet, 5),
        Event::PaymentFailed { reason }
    );
    wallet.event_handled().unwrap();

    // The malformed send queued nothing: the queue is fully drained.
    assert_eq!(wallet.event_handled(), Err(WalletError::NoPendingEvent));
    wallet.stop().unwrap();
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

/// The Android back-press scenario: an activity is destroyed without stopping
/// the node, so its `Wallet` and the still-running LDK node leak into the
/// cached process; relaunching builds a *second* `Wallet` over the same
/// `filesDir/wallet`. Two live nodes on one seed write the same monitors and
/// manager with last-writer-wins, which is the channel-state divergence the
/// plan's fresh-wallet decision exists to avoid. The second start must be
/// refused rather than silently succeed.
#[test]
fn a_second_wallet_over_the_same_storage_dir_cannot_start() {
    let dir = tempfile::tempdir().unwrap();

    let first = test_wallet(dir.path());
    first.start().expect("first wallet must start");

    // The leaked-activity case: a brand-new Wallet over the same directory.
    let second = test_wallet(dir.path());
    let result = second.start();
    assert!(
        matches!(result, Err(WalletError::InstanceAlreadyRunning)),
        "a second wallet over the same storage dir must be refused, got {result:?}"
    );

    // Once the first releases, a normal relaunch works — the guard must not
    // strand the wallet after a legitimate stop.
    first.stop().expect("first wallet must stop");
    second
        .start()
        .expect("after the first stops, a relaunch must start");
    second.stop().unwrap();
}
