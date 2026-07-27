//! PWA-compatible VSS wire client (U2; R3 wire format, R15 endpoints; KTD-1,
//! KTD-2).
//!
//! Bytes on the wire are indistinguishable from the PWA's
//! (`zinq/src/ldk/storage/vss-client.ts` / `vss-crypto.ts`): the same
//! endpoints against `https://zinqq.app/api/vss-proxy`
//! ([`crate::config::DEFAULT_VSS_URL`]), HMAC-SHA256 key obfuscation and
//! ChaCha20-Poly1305 `[nonce(12)][ciphertext+tag]` blobs without AAD
//! ([`crypto`]), the signature `authorization` header ([`auth`]), and the
//! VSS versioning discipline (first write 0, client increments; [`client`]).
//! Keys come from U1 (`crate::keys`): `vss_encryption_key` (`m/535'/1'`),
//! `vss_signing_key` (`m/535'/2'`), store id `hex(SHA-256(ldk_seed))`.
//!
//! Per KTD-2, `vss-client-ng` contributes ONLY its prost message types for
//! the standard LDK VSS protocol; its `StorableBuilder`/`KeyObfuscator`
//! envelope is incompatible with PWA blobs and its transport hides the HTTP
//! status the PWA's 404-to-None mapping needs, so both are unused.
//!
//! U3 (KTD-3, R3) builds the dual-write persistence on top: [`store`] (the
//! custom monitor `Persist`, the CM dual-write `KVStoreSync`, version cache,
//! manifest, and fence), [`startup`] (silent recovery / migration / mandatory
//! version seeding), and [`known_peers`] (`_known_peers` whole-map LWW).

pub mod auth;
pub mod client;
pub mod crypto;
pub(crate) mod known_peers;
pub(crate) mod startup;
pub mod store;

pub use client::{VssRetryPolicy, VssWireClient};
pub use crypto::CryptoError;
pub use store::{DualWriteKvStore, VssBackedStore};

/// Deterministic in-process transport for tests: an in-memory versioned map
/// with injectable failures, so U3's failure/conflict/crash-seam scenarios
/// run without a network. Obfuscation is the identity function, keeping
/// listing entries matchable against plaintext keys in assertions.
#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    use super::store::{BoxFuture, VersionedValue, VssTransport};
    use super::VssError;

    /// One `(plaintext key, value, version)` batch item.
    pub(crate) type BatchItems = Vec<(String, Vec<u8>, i64)>;

    #[derive(Default)]
    pub(crate) struct MockTransport {
        state: Mutex<HashMap<String, (Vec<u8>, i64)>>,
        pub fail_puts: AtomicBool,
        pub fail_gets: AtomicBool,
        pub fail_list: AtomicBool,
        pub fail_put_many: AtomicBool,
        fail_puts_keys: Mutex<HashSet<String>>,
        /// Keys whose GET fails (scripted recovery failures).
        fail_gets_keys: Mutex<HashSet<String>>,
        /// Keys whose PUT is applied server-side but still returns an error to
        /// the client: the lost-acknowledgement seam (request timeout, dropped
        /// mobile connection after the server committed).
        commit_then_fail_keys: Mutex<HashSet<String>>,
        put_attempts: Mutex<BatchItems>,
        put_many_calls: Mutex<Vec<BatchItems>>,
        get_calls: Mutex<Vec<String>>,
    }

    impl MockTransport {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// Plants `(bytes, version)` under `key`, as if another client wrote.
        pub(crate) fn seed(&self, key: &str, bytes: &[u8], version: i64) {
            self.state
                .lock()
                .unwrap()
                .insert(key.to_string(), (bytes.to_vec(), version));
        }

        pub(crate) fn value(&self, key: &str) -> Option<(Vec<u8>, i64)> {
            self.state.lock().unwrap().get(key).cloned()
        }

        pub(crate) fn snapshot(&self) -> HashMap<String, (Vec<u8>, i64)> {
            self.state.lock().unwrap().clone()
        }

        /// Total put ATTEMPTS (including failed ones) across single puts.
        pub(crate) fn put_attempt_count(&self) -> usize {
            self.put_attempts.lock().unwrap().len()
        }

        pub(crate) fn put_attempts_for(&self, key: &str) -> usize {
            self.put_attempts
                .lock()
                .unwrap()
                .iter()
                .filter(|(k, _, _)| k == key)
                .count()
        }

        pub(crate) fn put_many_calls(&self) -> Vec<BatchItems> {
            self.put_many_calls.lock().unwrap().clone()
        }

        pub(crate) fn get_calls_for(&self, key: &str) -> usize {
            self.get_calls
                .lock()
                .unwrap()
                .iter()
                .filter(|k| *k == key)
                .count()
        }

        pub(crate) fn fail_puts_for(&self, key: &str, fail: bool) {
            let mut keys = self.fail_puts_keys.lock().unwrap();
            if fail {
                keys.insert(key.to_string());
            } else {
                keys.remove(key);
            }
        }

        /// Makes `key`'s put COMMIT and then report failure, modelling a lost
        /// acknowledgement (the server applied the write; the client never
        /// learned it).
        pub(crate) fn commit_then_fail_for(&self, key: &str, fail: bool) {
            let mut keys = self.commit_then_fail_keys.lock().unwrap();
            if fail {
                keys.insert(key.to_string());
            } else {
                keys.remove(key);
            }
        }

        pub(crate) fn fail_gets_for(&self, key: &str, fail: bool) {
            let mut keys = self.fail_gets_keys.lock().unwrap();
            if fail {
                keys.insert(key.to_string());
            } else {
                keys.remove(key);
            }
        }

        fn put_sync(&self, key: &str, value: &[u8], version: i64) -> Result<i64, VssError> {
            self.put_attempts
                .lock()
                .unwrap()
                .push((key.to_string(), value.to_vec(), version));
            if self.fail_puts.load(Ordering::SeqCst)
                || self.fail_puts_keys.lock().unwrap().contains(key)
            {
                return Err(VssError::InternalServer {
                    message: "mock: put failure injected".to_string(),
                });
            }
            let mut state = self.state.lock().unwrap();
            let current = state.get(key).map(|(_, v)| *v).unwrap_or(0);
            if version != current {
                return Err(VssError::Conflict {
                    message: format!("mock: version {version} != current {current}"),
                });
            }
            state.insert(key.to_string(), (value.to_vec(), version + 1));
            if self.commit_then_fail_keys.lock().unwrap().contains(key) {
                return Err(VssError::Network {
                    message: "mock: put committed but the acknowledgement was lost".to_string(),
                });
            }
            Ok(version + 1)
        }
    }

    impl VssTransport for MockTransport {
        fn get<'a>(
            &'a self,
            plaintext_key: &'a str,
        ) -> BoxFuture<'a, Result<VersionedValue, VssError>> {
            self.get_calls
                .lock()
                .unwrap()
                .push(plaintext_key.to_string());
            let result = if self.fail_gets.load(Ordering::SeqCst)
                || self.fail_gets_keys.lock().unwrap().contains(plaintext_key)
            {
                Err(VssError::Network {
                    message: "mock: get failure injected".to_string(),
                })
            } else {
                Ok(self.state.lock().unwrap().get(plaintext_key).cloned())
            };
            Box::pin(std::future::ready(result))
        }

        fn put<'a>(
            &'a self,
            plaintext_key: &'a str,
            value: &'a [u8],
            version: i64,
        ) -> BoxFuture<'a, Result<i64, VssError>> {
            let result = self.put_sync(plaintext_key, value, version);
            Box::pin(std::future::ready(result))
        }

        fn put_many<'a>(
            &'a self,
            items: Vec<(String, Vec<u8>, i64)>,
        ) -> BoxFuture<'a, Result<(), VssError>> {
            self.put_many_calls.lock().unwrap().push(items.clone());
            let result = if self.fail_put_many.load(Ordering::SeqCst) {
                Err(VssError::InternalServer {
                    message: "mock: put_many failure injected".to_string(),
                })
            } else {
                // Transactional: all versions must match before any applies.
                let mut state = self.state.lock().unwrap();
                let conflict = items.iter().find(|(key, _, version)| {
                    let current = state.get(key).map(|(_, v)| *v).unwrap_or(0);
                    *version != current
                });
                if let Some((key, _, _)) = conflict {
                    Err(VssError::Conflict {
                        message: format!("mock: transactional conflict on {key}"),
                    })
                } else {
                    for (key, value, version) in items {
                        state.insert(key, (value, version + 1));
                    }
                    Ok(())
                }
            };
            Box::pin(std::future::ready(result))
        }

        fn delete<'a>(
            &'a self,
            plaintext_key: &'a str,
            version: i64,
        ) -> BoxFuture<'a, Result<(), VssError>> {
            let mut state = self.state.lock().unwrap();
            let result = match state.get(plaintext_key) {
                Some((_, current)) if *current != version => Err(VssError::Conflict {
                    message: format!("mock: delete at {version} != current {current}"),
                }),
                _ => {
                    state.remove(plaintext_key);
                    Ok(())
                }
            };
            Box::pin(std::future::ready(result))
        }

        fn list_key_versions<'a>(&'a self) -> BoxFuture<'a, Result<Vec<(String, i64)>, VssError>> {
            let result = if self.fail_list.load(Ordering::SeqCst) {
                Err(VssError::Network {
                    message: "mock: list failure injected".to_string(),
                })
            } else {
                Ok(self
                    .state
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(k, (_, v))| (k.clone(), *v))
                    .collect())
            };
            Box::pin(std::future::ready(result))
        }

        fn obfuscate(&self, plaintext_key: &str) -> String {
            plaintext_key.to_string()
        }
    }
}

/// Typed VSS failures. The taxonomy mirrors the PWA's `VssError.errorCode`
/// handling: each protocol `ErrorCode` stays distinguishable, and transport
/// failures (HTTP without a decodable `ErrorResponse`, network) are distinct
/// from protocol errors so U3's conflict fence and retry policies can match
/// on exactly the right variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VssError {
    /// Version conflict (`CONFLICT_EXCEPTION`, or a bare HTTP 409): the
    /// stored version diverged from ours. Never retried — U3 content-compares
    /// and fences on divergence (KTD-3). The VSS `ErrorResponse` carries no
    /// server-side version, so the caller refetches to learn it.
    Conflict {
        /// Server-provided description (log-only, per the proto contract).
        message: String,
    },
    /// `INVALID_REQUEST_EXCEPTION`: malformed request; never retried.
    InvalidRequest {
        /// Server-provided description.
        message: String,
    },
    /// `AUTH_EXCEPTION` (or a locally invalid signing key); never retried.
    Auth {
        /// Description of the auth failure.
        message: String,
    },
    /// `NO_SUCH_KEY_EXCEPTION` outside the `get_object` 404 path (which is
    /// `Ok(None)`, like the PWA's null).
    NoSuchKey {
        /// Server-provided description.
        message: String,
    },
    /// `INTERNAL_SERVER_EXCEPTION`: documented as safely retryable.
    InternalServer {
        /// Server-provided description.
        message: String,
    },
    /// Non-2xx response without a decodable `ErrorResponse` body (5xx
    /// variants are retried as transient).
    Http {
        /// The HTTP status code.
        status: u16,
        /// What made the response unusable.
        message: String,
    },
    /// The request never produced an HTTP response (DNS, TLS, refused
    /// connection, or the 15 s timeout). Retried as transient.
    Network {
        /// The underlying transport failure.
        message: String,
    },
    /// A fetched blob failed decryption — wrong key, truncation, or
    /// tampering ([`CryptoError`] keeps those distinct).
    Crypto(CryptoError),
    /// `listKeyVersions` exceeded the 100-page cap (PWA `MAX_LIST_PAGES`).
    TooManyListPages,
}

impl std::fmt::Display for VssError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VssError::Conflict { message } => {
                write!(f, "VSS version conflict: {message}")
            }
            VssError::InvalidRequest { message } => {
                write!(f, "VSS rejected the request as invalid: {message}")
            }
            VssError::Auth { message } => {
                write!(f, "VSS authentication failed: {message}")
            }
            VssError::NoSuchKey { message } => {
                write!(f, "VSS key does not exist: {message}")
            }
            VssError::InternalServer { message } => {
                write!(f, "VSS internal server error: {message}")
            }
            VssError::Http { status, message } => {
                write!(f, "VSS HTTP {status}: {message}")
            }
            VssError::Network { message } => {
                write!(f, "VSS network error: {message}")
            }
            VssError::Crypto(e) => write!(f, "VSS blob crypto error: {e}"),
            VssError::TooManyListPages => {
                write!(f, "VSS listKeyVersions exceeded the maximum page count")
            }
        }
    }
}

impl std::error::Error for VssError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_taxonomy_has_distinct_display_per_variant() {
        let variants = [
            VssError::Conflict {
                message: "m".into(),
            },
            VssError::InvalidRequest {
                message: "m".into(),
            },
            VssError::Auth {
                message: "m".into(),
            },
            VssError::NoSuchKey {
                message: "m".into(),
            },
            VssError::InternalServer {
                message: "m".into(),
            },
            VssError::Http {
                status: 502,
                message: "m".into(),
            },
            VssError::Network {
                message: "m".into(),
            },
            VssError::Crypto(CryptoError::DecryptFailed),
            VssError::TooManyListPages,
        ];
        for (i, a) in variants.iter().enumerate() {
            for b in variants.iter().skip(i + 1) {
                assert_ne!(
                    a.to_string(),
                    b.to_string(),
                    "error variants must stay distinguishable in logs"
                );
            }
        }
    }

    // ---------- serialization interop (gates U3) ----------

    /// Deserialize-what-we-serialize through the U2 crypto layer: a native
    /// `ChannelManager` blob survives an encrypt/decrypt round-trip keyed by
    /// the wallet's real U1 `vss_encryption_key` and still deserializes with
    /// U1's custom `SignerProvider` on a rebuild. Real PWA-exported blobs are
    /// covered by the `live_pwa_blob_interop` fixture test below — the
    /// bindings wrap the same compiled crate, so formats match in principle;
    /// this proves the crypto layer is transparent to LDK serialization.
    #[test]
    fn channel_manager_blob_survives_the_vss_crypto_layer_and_redeserializes() {
        use bitcoin::Network;
        use lightning::util::persist::{
            KVStoreSync, CHANNEL_MANAGER_PERSISTENCE_KEY,
            CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
            CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
        };
        use lightning_persister::fs_store::FilesystemStore;

        use crate::builder::KV_STORE_SUBDIR;
        use crate::keys::{derive_wallet_keys, parse_mnemonic, MNEMONIC_FILE_NAME};

        // Offline node A (unreachable esplora → degraded start) persists a
        // fresh ChannelManager under LDK's persist key constants.
        let dir_a = tempfile::tempdir().unwrap();
        let mut config = crate::Config::new(dir_a.path().to_str().unwrap().to_string());
        config.esplora_url = "http://127.0.0.1:1".to_string();
        config.rgs_url = "http://127.0.0.1:1/snapshot".to_string();
        config.vss_disabled = true;
        let node_a = crate::Node::new(config);
        node_a.start().expect("offline degraded start");
        let node_id = node_a.node_id().unwrap();
        node_a.stop().unwrap();
        drop(node_a);

        let manager_bytes = FilesystemStore::new(dir_a.path().join(KV_STORE_SUBDIR))
            .read(
                CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
                CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
                CHANNEL_MANAGER_PERSISTENCE_KEY,
            )
            .expect("channel manager persisted after first start");

        // Round-trip the blob through the U2 crypto layer keyed by the
        // wallet's REAL vss_encryption_key (as U3's dual-write will).
        let mnemonic_words =
            std::fs::read_to_string(dir_a.path().join(MNEMONIC_FILE_NAME)).unwrap();
        let keys = derive_wallet_keys(&parse_mnemonic(&mnemonic_words).unwrap(), Network::Bitcoin);
        let blob = crypto::encrypt(&keys.vss_encryption_key, &manager_bytes);
        assert_ne!(blob, manager_bytes);
        let recovered = crypto::decrypt(&keys.vss_encryption_key, &blob).unwrap();
        assert_eq!(recovered, manager_bytes, "crypto layer must be transparent");

        // Node B: same mnemonic, recovered blob → the CM deserializes through
        // U1's custom SignerProvider and yields the same identity.
        let dir_b = tempfile::tempdir().unwrap();
        std::fs::write(dir_b.path().join(MNEMONIC_FILE_NAME), &mnemonic_words).unwrap();
        FilesystemStore::new(dir_b.path().join(KV_STORE_SUBDIR))
            .write(
                CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
                CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
                CHANNEL_MANAGER_PERSISTENCE_KEY,
                recovered,
            )
            .unwrap();
        let mut config_b = crate::Config::new(dir_b.path().to_str().unwrap().to_string());
        config_b.esplora_url = "http://127.0.0.1:1".to_string();
        config_b.rgs_url = "http://127.0.0.1:1/snapshot".to_string();
        config_b.vss_disabled = true;
        let node_b = crate::Node::new(config_b);
        node_b
            .start()
            .expect("restore from a crypto-round-tripped manager blob");
        assert_eq!(node_b.node_id().unwrap(), node_id);
        node_b.stop().unwrap();
    }

    // ---------- live tests (plan-required, network) ----------

    /// The Verification Contract's wire-compatibility gate (stop condition
    /// (a) lives here): a full put/get/list/conflict/delete round-trip
    /// against the real endpoint with a throwaway store id (fresh random
    /// mnemonic → U1 key derivation, so the namespace is empty and orphaned
    /// afterwards).
    /// Run manually: `cargo test --lib -- --ignored live_vss_roundtrip`
    #[tokio::test]
    #[ignore]
    async fn live_vss_roundtrip() {
        use bitcoin::secp256k1::rand::rngs::OsRng;
        use bitcoin::secp256k1::rand::RngCore;
        use bitcoin::Network;

        use crate::keys::derive_wallet_keys;

        let mut entropy = [0u8; 16];
        OsRng.fill_bytes(&mut entropy);
        let mnemonic = bip39::Mnemonic::from_entropy_in(bip39::Language::English, &entropy)
            .expect("16 bytes is valid BIP39 entropy");
        let keys = derive_wallet_keys(&mnemonic, Network::Bitcoin);
        eprintln!("throwaway store id: {}", keys.vss_store_id);

        let client = VssWireClient::new(
            crate::config::DEFAULT_VSS_URL.to_string(),
            keys.vss_store_id.clone(),
            keys.vss_encryption_key,
            &keys.vss_signing_key,
        )
        .unwrap();

        let started = std::time::Instant::now();

        // Fresh namespace: empty list, no object.
        let listed = client.list_key_versions().await.expect("live list");
        assert!(listed.is_empty(), "throwaway store must start empty");
        assert_eq!(
            client
                .get_object("channel_manager")
                .await
                .expect("live get"),
            None,
            "missing key must be None"
        );

        // First write at version 0 → 1; read back the exact bytes.
        let payload = b"zinqq-kmp U2 live round-trip".to_vec();
        let version = client
            .put_object("channel_manager", &payload, 0)
            .await
            .expect("live put at version 0");
        assert_eq!(version, 1);
        let (fetched, fetched_version) = client
            .get_object("channel_manager")
            .await
            .expect("live get after put")
            .expect("object must exist after put");
        assert_eq!(fetched, payload);
        assert_eq!(fetched_version, 1);

        // Stale write at version 0 again → typed conflict (R3's fence
        // primitive), never a silent overwrite.
        let stale = client
            .put_object("channel_manager", b"stale", 0)
            .await
            .expect_err("stale-version put must fail");
        assert!(
            matches!(stale, VssError::Conflict { .. }),
            "expected a typed conflict, got {stale:?}"
        );

        // The obfuscated key shows up in the listing at version 1.
        let obfuscated = crypto::obfuscate_key(&keys.vss_encryption_key, "channel_manager");
        let listed = client.list_key_versions().await.expect("live list");
        assert_eq!(listed, vec![(obfuscated, 1)]);

        // Delete at the current version, then it is gone.
        client
            .delete_object("channel_manager", 1)
            .await
            .expect("live delete");
        assert_eq!(
            client
                .get_object("channel_manager")
                .await
                .expect("live get after delete"),
            None
        );

        eprintln!(
            "live VSS round-trip OK against {} in {:?}",
            crate::config::DEFAULT_VSS_URL,
            started.elapsed()
        );
    }

    /// Belt-and-braces PWA blob interop (plan U2 execution note): decrypt
    /// REAL PWA-exported `channel_manager` and monitor blobs with this
    /// unit's crypto layer and deserialize them with U1's SignerProvider.
    /// Requires fixtures exported from a PWA dev wallet (no offline source
    /// exists — the PWA's tests fabricate monitor bytes):
    ///
    /// - `ZINQQ_PWA_MNEMONIC`: the dev wallet's 12 words
    /// - `ZINQQ_PWA_CM_FIXTURE`: path to the encrypted `channel_manager`
    ///   value exactly as fetched from VSS
    /// - `ZINQQ_PWA_MON_FIXTURE`: path to one encrypted monitor value
    ///
    /// Run manually (single-client discipline: stop the PWA first):
    /// `ZINQQ_PWA_MNEMONIC=... ZINQQ_PWA_CM_FIXTURE=... ZINQQ_PWA_MON_FIXTURE=... \
    ///  cargo test --lib -- --ignored live_pwa_blob_interop`
    #[test]
    #[ignore]
    fn live_pwa_blob_interop() {
        use std::io::Cursor;
        use std::sync::Arc;

        use bitcoin::hashes::Hash as _;
        use bitcoin::BlockHash;
        use bitcoin::Network;
        use lightning::chain::channelmonitor::ChannelMonitor;
        use lightning::sign::{InMemorySigner, KeysManager};
        use lightning::util::persist::{
            KVStoreSync, CHANNEL_MANAGER_PERSISTENCE_KEY,
            CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
            CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
            CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
            CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
        };
        use lightning::util::ser::ReadableArgs;
        use lightning_persister::fs_store::FilesystemStore;

        use crate::builder::KV_STORE_SUBDIR;
        use crate::keys::{derive_wallet_keys, parse_mnemonic, MNEMONIC_FILE_NAME};
        use crate::signer::WalletSignerProvider;
        use crate::types::Logger;
        use crate::wallet::OnchainWallet;

        let var = |name: &str| {
            std::env::var(name).unwrap_or_else(|_| {
                panic!("{name} must be set — see the test doc for the fixture protocol")
            })
        };
        let mnemonic_words = var("ZINQQ_PWA_MNEMONIC");
        let cm_blob = std::fs::read(var("ZINQQ_PWA_CM_FIXTURE")).expect("readable CM fixture");
        let mon_blob = std::fs::read(var("ZINQQ_PWA_MON_FIXTURE")).expect("readable mon fixture");

        // 1. U2 crypto layer decrypts the real PWA-encrypted values.
        let keys = derive_wallet_keys(&parse_mnemonic(&mnemonic_words).unwrap(), Network::Bitcoin);
        let cm_bytes = crypto::decrypt(&keys.vss_encryption_key, &cm_blob)
            .expect("PWA channel_manager blob must decrypt with the U2 crypto layer");
        let mon_bytes = crypto::decrypt(&keys.vss_encryption_key, &mon_blob)
            .expect("PWA monitor blob must decrypt with the U2 crypto layer");

        // 2. The monitor deserializes with U1's custom SignerProvider.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStore::new(dir.path().join(KV_STORE_SUBDIR)));
        let logger = Arc::new(Logger);
        let wallet = Arc::new(
            OnchainWallet::new(
                &keys.descriptor_external,
                &keys.descriptor_internal,
                Network::Bitcoin,
                Arc::clone(&store),
                Arc::clone(&logger),
            )
            .unwrap(),
        );
        let keys_manager = Arc::new(KeysManager::new(&keys.ldk_seed, 0, 0, false));
        let signer_provider = WalletSignerProvider::new(
            Arc::clone(&keys_manager),
            wallet,
            keys.channel_keys_id_hmac_key,
            logger,
        );
        let (block_hash, monitor) = <(BlockHash, ChannelMonitor<InMemorySigner>)>::read(
            &mut Cursor::new(&mon_bytes),
            (&*keys_manager, &signer_provider),
        )
        .expect("PWA monitor must deserialize with U1's SignerProvider");
        assert_ne!(block_hash, BlockHash::all_zeros());
        let monitor_key = monitor.persistence_key().to_string();
        eprintln!("PWA monitor deserialized: key {monitor_key}");

        // 3. The full production restore path (read_channel_monitors +
        // ChannelManagerReadArgs) accepts both blobs: seed a data dir and
        // build (needs the live esplora backend for the initial sync).
        std::fs::write(dir.path().join(MNEMONIC_FILE_NAME), &mnemonic_words).unwrap();
        store
            .write(
                CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
                CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
                &monitor_key,
                mon_bytes,
            )
            .unwrap();
        store
            .write(
                CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
                CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
                CHANNEL_MANAGER_PERSISTENCE_KEY,
                cm_bytes,
            )
            .unwrap();
        drop(signer_provider);
        drop(store);

        let mut config = crate::Config::new(dir.path().to_str().unwrap().to_string());
        // The fixture proves deserialization; the live namespace is left
        // alone (single-client discipline).
        config.vss_disabled = true;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let components = crate::builder::build(
            &config,
            &rt,
            std::sync::Arc::new(crate::node::LoggingEventSink::new()),
        )
        .expect("PWA channel_manager must deserialize through the full restore path");
        eprintln!(
            "PWA blob interop OK: node id {}",
            components.channel_manager.get_our_node_id()
        );
    }
}
