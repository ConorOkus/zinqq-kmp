//! `_known_peers` store (U3; R3): a whole-map JSON blob
//! `{pubkeyHex: {host, port}}` mirrored locally and written to VSS with
//! last-writer-wins conflict handling (adopt the server version, retry once
//! with our bytes) — the PWA's `known-peers.ts` semantics exactly. Peers are
//! convenience state, not fund-critical: writes are best-effort and never
//! fence.
//!
//! The node's reconnect loop reads [`KnownPeersStore::reconnect_targets`]
//! each tick (the seam U12 left in `Node::reconnect_targets`), so peers
//! added during a session are dialed without restarting the loop.

use std::collections::BTreeMap;
use std::str::FromStr as _;
use std::sync::{Arc, Mutex};

use lightning::log_error;
use lightning::util::logger::Logger as _;
use lightning::util::persist::KVStoreSync;
use lightning_persister::fs_store::FilesystemStore;
use serde::{Deserialize, Serialize};

use super::store::{VssBackedStore, KNOWN_PEERS_VSS_KEY};
use crate::config::PeerInfo;
use crate::types::Logger;

/// Local KVStore location of the JSON map (the IDB mirror's equivalent).
pub(crate) const KNOWN_PEERS_PRIMARY_NAMESPACE: &str = "known_peers";
pub(crate) const KNOWN_PEERS_SECONDARY_NAMESPACE: &str = "";
pub(crate) const KNOWN_PEERS_LOCAL_KEY: &str = "peers";

/// One saved peer, exactly the PWA's `{host, port}` JSON shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KnownPeer {
    pub host: String,
    pub port: u16,
}

/// Parses the whole-map JSON leniently like the PWA's `parseKnownPeers`:
/// the top level must be an object (error otherwise); entries that are not
/// `{host: string, port: number}` are skipped, not fatal.
pub(crate) fn parse_known_peers(bytes: &[u8]) -> Result<BTreeMap<String, KnownPeer>, String> {
    let parsed: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("known_peers is not JSON: {e}"))?;
    let object = parsed
        .as_object()
        .ok_or_else(|| "known_peers must be a JSON object".to_string())?;
    let mut result = BTreeMap::new();
    for (pubkey, value) in object {
        if let Ok(peer) = serde_json::from_value::<KnownPeer>(value.clone()) {
            result.insert(pubkey.clone(), peer);
        }
    }
    Ok(result)
}

/// Serializes the map to the wire/local JSON shape.
pub(crate) fn serialize_known_peers(map: &BTreeMap<String, KnownPeer>) -> Vec<u8> {
    serde_json::to_vec(map).expect("a string-keyed map always serializes to JSON")
}

/// Reads the local mirror, degrading to empty on absence or corruption
/// (peers are convenience state).
pub(crate) fn read_local_known_peers(local: &FilesystemStore) -> BTreeMap<String, KnownPeer> {
    match local.read(
        KNOWN_PEERS_PRIMARY_NAMESPACE,
        KNOWN_PEERS_SECONDARY_NAMESPACE,
        KNOWN_PEERS_LOCAL_KEY,
    ) {
        Ok(bytes) => parse_known_peers(&bytes).unwrap_or_else(|e| {
            log_error!(Logger, "Corrupt local known-peers map, starting empty: {e}");
            BTreeMap::new()
        }),
        Err(_) => BTreeMap::new(),
    }
}

/// Writes the local mirror.
pub(crate) fn write_local_known_peers(
    local: &FilesystemStore,
    map: &BTreeMap<String, KnownPeer>,
) -> Result<(), lightning::io::Error> {
    local.write(
        KNOWN_PEERS_PRIMARY_NAMESPACE,
        KNOWN_PEERS_SECONDARY_NAMESPACE,
        KNOWN_PEERS_LOCAL_KEY,
        serialize_known_peers(map),
    )
}

/// The known-peers store: in-memory map + local mirror + LWW VSS sync.
pub(crate) struct KnownPeersStore {
    local: Arc<FilesystemStore>,
    vss: Arc<VssBackedStore>,
    map: Mutex<BTreeMap<String, KnownPeer>>,
    logger: Arc<Logger>,
}

impl KnownPeersStore {
    /// Loads the map from the local mirror (empty when absent/corrupt).
    pub(crate) fn load(
        local: Arc<FilesystemStore>,
        vss: Arc<VssBackedStore>,
        logger: Arc<Logger>,
    ) -> Self {
        let map = read_local_known_peers(&local);
        Self {
            local,
            vss,
            map: Mutex::new(map),
            logger,
        }
    }

    /// Adds or replaces a peer, persists locally, and schedules the LWW VSS
    /// write. Local persistence failure is surfaced; the VSS half is
    /// best-effort by design.
    pub(crate) fn upsert(
        &self,
        pubkey: &str,
        host: &str,
        port: u16,
    ) -> Result<(), lightning::io::Error> {
        let bytes = {
            let mut map = self.map.lock().unwrap();
            map.insert(
                pubkey.to_string(),
                KnownPeer {
                    host: host.to_string(),
                    port,
                },
            );
            serialize_known_peers(&map)
        };
        self.persist(bytes)
    }

    /// Removes a peer, persists locally, and schedules the LWW VSS write.
    pub(crate) fn remove(&self, pubkey: &str) -> Result<(), lightning::io::Error> {
        let bytes = {
            let mut map = self.map.lock().unwrap();
            map.remove(pubkey);
            serialize_known_peers(&map)
        };
        self.persist(bytes)
    }

    /// The full saved-peer map.
    pub(crate) fn all(&self) -> BTreeMap<String, KnownPeer> {
        self.map.lock().unwrap().clone()
    }

    /// Saved peers as reconnect targets. Entries with an unparsable pubkey
    /// or a non-IP host are skipped with a log (U9 decision: the core dials
    /// `SocketAddr`s only — `parse_peer_address` rejects hostnames with a
    /// typed error, and the PWA stores IPs today).
    pub(crate) fn reconnect_targets(&self) -> Vec<PeerInfo> {
        let map = self.map.lock().unwrap();
        let mut targets = Vec::with_capacity(map.len());
        for (pubkey, peer) in map.iter() {
            let Ok(node_id) = bitcoin::secp256k1::PublicKey::from_str(pubkey) else {
                log_error!(
                    self.logger,
                    "Skipping known peer with invalid pubkey {pubkey}"
                );
                continue;
            };
            let Ok(address) =
                std::net::SocketAddr::from_str(&format!("{}:{}", peer.host, peer.port))
            else {
                log_error!(
                    self.logger,
                    "Skipping known peer {pubkey}: {}:{} is not an ip:port address",
                    peer.host,
                    peer.port
                );
                continue;
            };
            targets.push(PeerInfo { node_id, address });
        }
        targets
    }

    /// Writes the pre-serialized map to the local mirror (failure surfaced),
    /// then schedules the LWW VSS write with the same bytes.
    fn persist(&self, bytes: Vec<u8>) -> Result<(), lightning::io::Error> {
        self.local.write(
            KNOWN_PEERS_PRIMARY_NAMESPACE,
            KNOWN_PEERS_SECONDARY_NAMESPACE,
            KNOWN_PEERS_LOCAL_KEY,
            bytes.clone(),
        )?;
        self.vss.put_lww(KNOWN_PEERS_VSS_KEY, bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::{CoreEvent, EventSink};
    use crate::vss::store::{MonitorKeySet, RetryTuning, VssTransport};
    use crate::vss::test_support::MockTransport;
    use std::collections::HashMap;
    use std::time::Duration;

    struct NullSink;
    impl EventSink for NullSink {
        fn emit(&self, _event: CoreEvent) {}
    }

    fn store_pair(
        dir: &std::path::Path,
        rt: &tokio::runtime::Runtime,
    ) -> (
        Arc<MockTransport>,
        Arc<FilesystemStore>,
        Arc<VssBackedStore>,
    ) {
        let transport = Arc::new(MockTransport::new());
        let local = Arc::new(FilesystemStore::new(dir.join("store")));
        let vss = Arc::new(VssBackedStore::new(
            Some(Arc::clone(&transport) as Arc<dyn VssTransport>),
            Arc::clone(&local),
            rt.handle().clone(),
            dir,
            Arc::new(NullSink),
            Arc::new(Logger),
            RetryTuning {
                initial_backoff: Duration::from_millis(2),
                max_backoff: Duration::from_millis(10),
                degraded_after: Duration::from_millis(6),
                cm_attempt_timeout: Duration::from_millis(200),
            },
            HashMap::new(),
            MonitorKeySet::default(),
            false,
        ));
        (transport, local, vss)
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    const PUBKEY: &str = "034066e29e402d9cf55af1ae1026cc5adf92eed1e0e421785442f53717ad1453b0";

    #[test]
    fn upsert_persists_locally_survives_reload_and_syncs_the_whole_map_to_vss() {
        let dir = tempfile::tempdir().unwrap();
        let rt = rt();
        let (transport, local, vss) = store_pair(dir.path(), &rt);
        let store = KnownPeersStore::load(Arc::clone(&local), Arc::clone(&vss), Arc::new(Logger));

        store.upsert(PUBKEY, "64.23.159.177", 9735).unwrap();

        // Whole-map JSON lands on VSS (LWW blob).
        rt.block_on(async {
            for _ in 0..500 {
                if transport.value(KNOWN_PEERS_VSS_KEY).is_some() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            panic!("VSS peers write never happened");
        });
        let (bytes, version) = transport.value(KNOWN_PEERS_VSS_KEY).unwrap();
        assert_eq!(version, 1);
        let parsed = parse_known_peers(&bytes).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed.get(PUBKEY).unwrap().port, 9735);

        // Reload from the local mirror alone.
        let reloaded = KnownPeersStore::load(Arc::clone(&local), vss, Arc::new(Logger));
        assert_eq!(reloaded.all().len(), 1);
        let targets = reloaded.reconnect_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].node_id.to_string(), PUBKEY);
        assert_eq!(targets[0].address.to_string(), "64.23.159.177:9735");

        // Remove empties both mirrors.
        reloaded.remove(PUBKEY).unwrap();
        assert!(reloaded.all().is_empty());
        assert!(reloaded.reconnect_targets().is_empty());
    }

    #[test]
    fn parse_known_peers_is_lenient_per_entry_but_strict_on_shape() {
        // Invalid entries are skipped, valid ones kept (PWA parseKnownPeers).
        let json = format!(
            r#"{{"{PUBKEY}": {{"host": "1.2.3.4", "port": 9735}},
                "bad": {{"host": 42}},
                "worse": "nope"}}"#
        );
        let parsed = parse_known_peers(json.as_bytes()).unwrap();
        assert_eq!(parsed.len(), 1);
        assert!(parsed.contains_key(PUBKEY));

        assert!(parse_known_peers(b"[1,2]").is_err(), "array is not a map");
        assert!(parse_known_peers(b"not json").is_err());
    }

    #[test]
    fn reconnect_targets_skip_unparsable_entries() {
        let dir = tempfile::tempdir().unwrap();
        let rt = rt();
        let (_transport, local, vss) = store_pair(dir.path(), &rt);
        let store = KnownPeersStore::load(Arc::clone(&local), vss, Arc::new(Logger));
        store.upsert("not-a-pubkey", "1.2.3.4", 9735).unwrap();
        store.upsert(PUBKEY, "not-an-ip-host", 9735).unwrap();
        assert!(
            store.reconnect_targets().is_empty(),
            "invalid entries are skipped, never panic the reconnect loop"
        );
    }
}
