//! Restart-safety and startup-semantics integration tests.
//!
//! Offline-runnable by design: the Esplora URL points at a closed local port,
//! so a fresh (zero-monitor) node exercises the degraded-start path and the
//! restore path is proven by dropping and rebuilding the node over the same
//! storage directory.

use std::path::Path;

use lightning::util::persist::{
    KVStoreSync, CHANNEL_MANAGER_PERSISTENCE_KEY, CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
    CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE, CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
    CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE, NETWORK_GRAPH_PERSISTENCE_KEY,
    NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE, NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
    SCORER_PERSISTENCE_KEY, SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
    SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
};
use lightning_persister::fs_store::FilesystemStore;
use wallet_core::builder::KV_STORE_SUBDIR;
use wallet_core::keys::{MNEMONIC_FILE_NAME, RESTORE_IN_PROGRESS_FILE_NAME};
use wallet_core::{BuildError, Config, Node};

/// A local port nothing listens on: connection refused, instantly, offline.
const UNREACHABLE_ESPLORA: &str = "http://127.0.0.1:1";
const UNREACHABLE_RGS: &str = "http://127.0.0.1:1/snapshot";

fn test_config(storage_dir: &Path) -> Config {
    let mut config = Config::new(storage_dir.to_str().unwrap().to_string());
    config.esplora_url = UNREACHABLE_ESPLORA.to_string();
    config.rgs_url = UNREACHABLE_RGS.to_string();
    // Offline suite: local-only persistence (the U3 VSS paths are covered by
    // in-crate tests with an injected mock transport).
    config.vss_disabled = true;
    config
}

fn kv_store(storage_dir: &Path) -> FilesystemStore {
    FilesystemStore::new(storage_dir.join(KV_STORE_SUBDIR))
}

#[test]
fn fresh_node_starts_degraded_offline_and_restarts_from_disk() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(dir.path());

    // First start: fresh seed, fresh manager, unreachable Esplora tolerated.
    let node = Node::new(config.clone());
    assert!(!node.is_running());
    node.start()
        .expect("fresh offline start must succeed degraded");
    assert!(node.is_running());
    assert!(
        !node.is_chain_synced(),
        "unreachable esplora must leave the node in a degraded-sync state"
    );
    assert_eq!(node.onchain_balance_sats(), Some(0));
    let first_node_id = node.node_id().expect("running node has a node id");

    // The mnemonic landed as a 12-word file in the node data dir (U1, R1)...
    let mnemonic_path = dir.path().join(MNEMONIC_FILE_NAME);
    assert!(mnemonic_path.exists());
    let words = std::fs::read_to_string(&mnemonic_path).unwrap();
    assert_eq!(words.split_whitespace().count(), 12);

    // ...and the manager was persisted under LDK's persist key constants.
    let store = kv_store(dir.path());
    let manager_bytes = store
        .read(
            CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
            CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
            CHANNEL_MANAGER_PERSISTENCE_KEY,
        )
        .expect("channel manager must be persisted after first start");
    assert!(!manager_bytes.is_empty());

    node.stop().expect("stop must succeed");
    assert!(!node.is_running());
    assert!(node.start().is_ok(), "second start on same handle");
    node.stop().unwrap();
    drop(node);

    // Rebuild over the same directory: the restore path must reload the
    // manager (and all — currently zero — monitors), re-run the sync path,
    // and watch_channel each reloaded monitor.
    let rebuilt = Node::new(config);
    rebuilt
        .start()
        .expect("restart from persisted state must succeed");
    assert_eq!(
        rebuilt.node_id().unwrap(),
        first_node_id,
        "restart must reload the same node identity from the mnemonic file"
    );
    rebuilt.stop().unwrap();

    // Write-once (R1): restarts reuse the words, never regenerate them.
    assert_eq!(
        std::fs::read_to_string(&mnemonic_path).unwrap(),
        words,
        "the mnemonic file must be byte-stable across restarts"
    );
}

#[test]
fn each_data_dir_gets_its_own_fresh_mnemonic_and_identity() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();

    let node_a = Node::new(test_config(dir_a.path()));
    let node_b = Node::new(test_config(dir_b.path()));
    node_a.start().unwrap();
    node_b.start().unwrap();
    assert_ne!(
        node_a.node_id().unwrap(),
        node_b.node_id().unwrap(),
        "fresh installs must auto-generate distinct mnemonics (R1)"
    );
    assert_ne!(
        std::fs::read_to_string(dir_a.path().join(MNEMONIC_FILE_NAME)).unwrap(),
        std::fs::read_to_string(dir_b.path().join(MNEMONIC_FILE_NAME)).unwrap(),
        "each data dir must hold its own words"
    );
    node_a.stop().unwrap();
    node_b.stop().unwrap();
}

/// U1: a corrupt/invalid mnemonic file fails start with a typed error rather
/// than being silently replaced (replacing it would strand the old funds).
#[test]
fn corrupt_mnemonic_file_fails_start_with_typed_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(MNEMONIC_FILE_NAME),
        "not twelve valid words",
    )
    .unwrap();

    let node = Node::new(test_config(dir.path()));
    assert_eq!(node.start().unwrap_err(), BuildError::InvalidMnemonic);
    assert!(!node.is_running());
    assert_eq!(
        std::fs::read_to_string(dir.path().join(MNEMONIC_FILE_NAME)).unwrap(),
        "not twelve valid words",
        "a failed start must not touch the mnemonic file"
    );
}

/// U1: while a restore-in-progress marker (written by the U4 restore flow)
/// exists and no mnemonic does, start refuses to auto-generate fresh words —
/// the interrupted restore owns the directory.
#[test]
fn restore_marker_blocks_mnemonic_generation_at_start() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(RESTORE_IN_PROGRESS_FILE_NAME), b"").unwrap();

    let node = Node::new(test_config(dir.path()));
    assert_eq!(node.start().unwrap_err(), BuildError::RestoreInProgress);
    assert!(!node.is_running());
    assert!(
        !dir.path().join(MNEMONIC_FILE_NAME).exists(),
        "no mnemonic may be generated while a restore is incomplete"
    );
}

#[test]
fn kv_store_round_trips_under_ldk_persist_key_constants() {
    let dir = tempfile::tempdir().unwrap();
    let store = kv_store(dir.path());

    // Monitor namespace, keyed by MonitorName ("<funding txid>_<index>").
    let monitor_name = format!("{}_0", "ab".repeat(32));
    let cases: [(&str, &str, &str, Vec<u8>); 4] = [
        (
            CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
            CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
            monitor_name.as_str(),
            vec![1, 2, 3, 4],
        ),
        (
            CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
            CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
            CHANNEL_MANAGER_PERSISTENCE_KEY,
            vec![5, 6],
        ),
        (
            NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
            NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
            NETWORK_GRAPH_PERSISTENCE_KEY,
            vec![7],
        ),
        (
            SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
            SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
            SCORER_PERSISTENCE_KEY,
            vec![8, 9],
        ),
    ];

    for (primary, secondary, key, value) in &cases {
        store.write(primary, secondary, key, value.clone()).unwrap();
    }
    for (primary, secondary, key, value) in &cases {
        assert_eq!(&store.read(primary, secondary, key).unwrap(), value);
        assert!(store
            .list(primary, secondary)
            .unwrap()
            .contains(&key.to_string()));
    }
}

/// U4 stale-manager defense (PWA `init.ts` parity): a channel manager that
/// fails deserialization while ZERO monitors exist (e.g. a stale CM that
/// survived a clear race) is DISCARDED and replaced with a fresh manager —
/// no channels means no funds at risk, and crashing would brick the wallet.
#[test]
fn corrupt_channel_manager_with_zero_monitors_is_discarded_for_a_fresh_one() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(dir.path());

    // Create valid persisted state first (zero channels/monitors).
    let node = Node::new(config.clone());
    node.start().unwrap();
    let node_id = node.node_id().unwrap();
    node.stop().unwrap();
    drop(node);

    // Corrupt the persisted manager blob.
    let store = kv_store(dir.path());
    store
        .write(
            CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
            CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
            CHANNEL_MANAGER_PERSISTENCE_KEY,
            b"not a channel manager".to_vec(),
        )
        .unwrap();

    let node = Node::new(config);
    node.start()
        .expect("a stale CM with zero monitors must be discarded, not a crash");
    assert!(node.is_running());
    assert_eq!(
        node.node_id().unwrap(),
        node_id,
        "the identity comes from the untouched mnemonic"
    );
    node.stop().unwrap();

    // The garbage blob was replaced by a freshly persisted manager.
    let replaced = store
        .read(
            CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
            CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
            CHANNEL_MANAGER_PERSISTENCE_KEY,
        )
        .unwrap();
    assert_ne!(replaced, b"not a channel manager".to_vec());
}

/// U4: a restore marker with VSS disabled can never resume (the backup is
/// unreachable), and local LDK state is void while the marker exists — so
/// the start is refused even though a mnemonic is present. The node must
/// never boot against a possibly-partial set.
#[test]
fn restore_marker_with_vss_disabled_refuses_start_even_with_a_mnemonic() {
    let dir = tempfile::tempdir().unwrap();
    // A valid mnemonic exists (BIP39 test vector #0)...
    std::fs::write(
        dir.path().join(MNEMONIC_FILE_NAME),
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon \
         abandon about",
    )
    .unwrap();
    // ...but so does the restore marker: local state is void.
    std::fs::write(dir.path().join(RESTORE_IN_PROGRESS_FILE_NAME), b"").unwrap();

    let node = Node::new(test_config(dir.path()));
    assert_eq!(node.start().unwrap_err(), BuildError::RestoreInProgress);
    assert!(!node.is_running());
}

#[test]
fn corrupt_channel_monitor_data_fails_start_with_typed_error() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(dir.path());

    // Plant garbage under a validly-named monitor key; restore must refuse to
    // proceed rather than start with unreadable fund-safety state.
    let store = kv_store(dir.path());
    let monitor_name = format!("{}_1", "cd".repeat(32));
    store
        .write(
            CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
            CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
            &monitor_name,
            b"garbage monitor bytes".to_vec(),
        )
        .unwrap();

    let node = Node::new(config);
    assert_eq!(node.start().unwrap_err(), BuildError::InvalidMonitorData);
    assert!(!node.is_running());
}

#[test]
fn lifecycle_misuse_returns_typed_errors() {
    let dir = tempfile::tempdir().unwrap();
    let node = Node::new(test_config(dir.path()));

    assert_eq!(node.stop().unwrap_err(), BuildError::NotRunning);
    node.start().unwrap();
    assert_eq!(node.start().unwrap_err(), BuildError::AlreadyRunning);
    node.stop().unwrap();
}
