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
use wallet_core::{BuildError, Config, Node};

/// A local port nothing listens on: connection refused, instantly, offline.
const UNREACHABLE_ESPLORA: &str = "http://127.0.0.1:1";
const UNREACHABLE_RGS: &str = "http://127.0.0.1:1/snapshot";

fn test_config(storage_dir: &Path) -> Config {
    let mut config = Config::new(storage_dir.to_str().unwrap().to_string());
    config.esplora_url = UNREACHABLE_ESPLORA.to_string();
    config.rgs_url = UNREACHABLE_RGS.to_string();
    config
}

fn kv_store(storage_dir: &Path) -> FilesystemStore {
    FilesystemStore::new(storage_dir.join("store"))
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

    // The seed landed as a file in the node data dir (KTD-11)...
    let seed_path = dir.path().join("keys_seed");
    assert!(seed_path.exists());
    assert_eq!(std::fs::read(&seed_path).unwrap().len(), 64);

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
        "restart must reload the same node identity from the seed file"
    );
    rebuilt.stop().unwrap();
}

#[test]
fn each_data_dir_gets_its_own_fresh_seed_and_identity() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();

    let node_a = Node::new(test_config(dir_a.path()));
    let node_b = Node::new(test_config(dir_b.path()));
    node_a.start().unwrap();
    node_b.start().unwrap();
    assert_ne!(
        node_a.node_id().unwrap(),
        node_b.node_id().unwrap(),
        "fresh installs must generate distinct identities (AE2)"
    );
    node_a.stop().unwrap();
    node_b.stop().unwrap();
}

/// AE2, compile-level: `Config` is the node's entire constructor surface.
/// The exhaustive destructuring below stops compiling if anyone adds a
/// seed/mnemonic-import field to it.
#[test]
fn constructor_surface_has_no_seed_or_mnemonic_input() {
    let Config {
        network: _,
        storage_dir: _,
        esplora_url: _,
        rgs_url: _,
        peers: _,
    } = test_config(tempfile::tempdir().unwrap().path());
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

#[test]
fn corrupt_channel_manager_data_fails_start_with_typed_error() {
    let dir = tempfile::tempdir().unwrap();
    let config = test_config(dir.path());

    // Create valid persisted state first.
    let node = Node::new(config.clone());
    node.start().unwrap();
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
    assert_eq!(node.start().unwrap_err(), BuildError::ReadFailed);
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
