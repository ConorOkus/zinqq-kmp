//! On-chain wallet: a BIP84 `bdk_wallet` over the mnemonic-derived
//! descriptors (U1, KTD-4), persisted into the shared KVStore as a merged
//! `ChangeSet` blob and synced via the shared esplora client. Also serves as
//! the sweeper's change-destination source and the signer's address source
//! (deterministic destination scripts, next-unused shutdown scripts).

use std::sync::{Arc, Mutex};

use bdk_esplora::EsploraAsyncExt;
use bdk_wallet::chain::Merge;
use bdk_wallet::{ChangeSet, KeychainKind, PersistedWallet, Wallet as BdkWallet, WalletPersister};
use bitcoin::{Network, ScriptBuf};
use esplora_client::AsyncClient as EsploraAsyncClient;
use lightning::log_error;
use lightning::sign::ChangeDestinationSourceSync;
use lightning::util::logger::Logger as _;
use lightning::util::persist::KVStoreSync;
use lightning_persister::fs_store::FilesystemStore;

use crate::builder::BuildError;
use crate::chain::ChainError;
use crate::types::Logger;

pub(crate) const BDK_WALLET_PRIMARY_NAMESPACE: &str = "bdk_wallet";
pub(crate) const BDK_WALLET_SECONDARY_NAMESPACE: &str = "";
pub(crate) const BDK_WALLET_CHANGESET_KEY: &str = "changeset";

/// Persists the bdk wallet as a single merged JSON `ChangeSet` under the
/// shared KVStore. Simpler than per-component keys and plenty for a spike; the
/// changeset is small until the wallet actually holds on-chain history.
pub(crate) struct KVStoreWalletPersister {
    /// Merged aggregate of everything persisted so far.
    aggregate: Option<ChangeSet>,
    kv_store: Arc<FilesystemStore>,
}

impl KVStoreWalletPersister {
    pub(crate) fn new(kv_store: Arc<FilesystemStore>) -> Self {
        Self {
            aggregate: None,
            kv_store,
        }
    }
}

impl WalletPersister for KVStoreWalletPersister {
    type Error = std::io::Error;

    fn initialize(persister: &mut Self) -> Result<ChangeSet, Self::Error> {
        if let Some(aggregate) = persister.aggregate.as_ref() {
            return Ok(aggregate.clone());
        }
        let change_set = match persister.kv_store.read(
            BDK_WALLET_PRIMARY_NAMESPACE,
            BDK_WALLET_SECONDARY_NAMESPACE,
            BDK_WALLET_CHANGESET_KEY,
        ) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("corrupt bdk wallet changeset: {e}"),
                )
            })?,
            Err(e) if e.kind() == lightning::io::ErrorKind::NotFound => ChangeSet::default(),
            Err(e) => return Err(e.into()),
        };
        persister.aggregate = Some(change_set.clone());
        Ok(change_set)
    }

    fn persist(persister: &mut Self, change_set: &ChangeSet) -> Result<(), Self::Error> {
        if change_set.is_empty() {
            return Ok(());
        }
        let aggregate = persister
            .aggregate
            .as_mut()
            .ok_or_else(|| std::io::Error::other("wallet persister used before initialization"))?;
        aggregate.merge(change_set.clone());
        let bytes = serde_json::to_vec(aggregate).map_err(std::io::Error::other)?;
        persister
            .kv_store
            .write(
                BDK_WALLET_PRIMARY_NAMESPACE,
                BDK_WALLET_SECONDARY_NAMESPACE,
                BDK_WALLET_CHANGESET_KEY,
                bytes,
            )
            .map_err(Into::into)
    }
}

struct WalletInner {
    wallet: PersistedWallet<KVStoreWalletPersister>,
    persister: KVStoreWalletPersister,
}

/// The node's on-chain wallet.
pub(crate) struct OnchainWallet {
    inner: Mutex<WalletInner>,
    logger: Arc<Logger>,
}

impl OnchainWallet {
    /// Loads the persisted wallet, or creates a fresh one from the
    /// mnemonic-derived BIP84 descriptors (KTD-4). No network access: eager
    /// construction must precede any LDK monitor/manager deserialization so
    /// the custom signer can resolve destination scripts during restore.
    pub(crate) fn new(
        descriptor: &str,
        change_descriptor: &str,
        network: Network,
        kv_store: Arc<FilesystemStore>,
        logger: Arc<Logger>,
    ) -> Result<Self, BuildError> {
        let descriptor = descriptor.to_string();
        let change_descriptor = change_descriptor.to_string();
        let mut persister = KVStoreWalletPersister::new(kv_store);

        let wallet_opt = BdkWallet::load()
            .descriptor(KeychainKind::External, Some(descriptor.clone()))
            .descriptor(KeychainKind::Internal, Some(change_descriptor.clone()))
            .extract_keys()
            .check_network(network)
            .load_wallet(&mut persister)
            .map_err(|e| {
                log_error!(logger, "Failed to load on-chain wallet: {e}");
                BuildError::WalletSetupFailed
            })?;
        let wallet = match wallet_opt {
            Some(wallet) => wallet,
            None => BdkWallet::create(descriptor, change_descriptor)
                .network(network)
                .create_wallet(&mut persister)
                .map_err(|e| {
                    log_error!(logger, "Failed to create on-chain wallet: {e}");
                    BuildError::WalletSetupFailed
                })?,
        };

        Ok(Self {
            inner: Mutex::new(WalletInner { wallet, persister }),
            logger,
        })
    }

    /// Syncs against Esplora: full scan on first use, incremental afterwards.
    pub(crate) async fn sync(
        &self,
        client: &EsploraAsyncClient,
        stop_gap: usize,
        concurrency: usize,
    ) -> Result<(), ChainError> {
        // A fresh wallet's local chain only knows genesis; do a full scan
        // once, then cheaper revealed-script syncs.
        let needs_full_scan = self
            .inner
            .lock()
            .unwrap()
            .wallet
            .latest_checkpoint()
            .height()
            == 0;
        if needs_full_scan {
            let request = self.inner.lock().unwrap().wallet.start_full_scan().build();
            let update = client
                .full_scan(request, stop_gap, concurrency)
                .await
                .map_err(|e| ChainError::EsploraUnreachable(e.to_string()))?;
            self.apply_update(update)
        } else {
            let request = self
                .inner
                .lock()
                .unwrap()
                .wallet
                .start_sync_with_revealed_spks()
                .build();
            let update = client
                .sync(request, concurrency)
                .await
                .map_err(|e| ChainError::EsploraUnreachable(e.to_string()))?;
            self.apply_update(update)
        }
    }

    fn apply_update(&self, update: impl Into<bdk_wallet::Update>) -> Result<(), ChainError> {
        let mut inner = self.inner.lock().unwrap();
        let WalletInner { wallet, persister } = &mut *inner;
        wallet
            .apply_update(update)
            .map_err(|e| ChainError::WalletSyncFailed(e.to_string()))?;
        wallet
            .persist(persister)
            .map_err(|e| ChainError::WalletSyncFailed(e.to_string()))?;
        Ok(())
    }

    /// Current confirmed + unconfirmed balance.
    pub(crate) fn balance(&self) -> bdk_wallet::Balance {
        self.inner.lock().unwrap().wallet.balance()
    }

    /// Deterministic external script at `index` for the signer's
    /// `get_destination_script` (KTD-4): peek the address, then
    /// `reveal_addresses_to` so bdk tracks it for syncing, and persist the
    /// reveal — a restored wallet must watch the same close scripts.
    pub(crate) fn destination_script_for_index(&self, index: u32) -> Result<ScriptBuf, ()> {
        let mut inner = self.inner.lock().unwrap();
        let WalletInner { wallet, persister } = &mut *inner;
        let script = wallet
            .peek_address(KeychainKind::External, index)
            .address
            .script_pubkey();
        // reveal_addresses_to stages the index update eagerly; the returned
        // iterator of newly revealed addresses is not needed.
        drop(wallet.reveal_addresses_to(KeychainKind::External, index));
        wallet.persist(persister).map_err(|e| {
            log_error!(
                self.logger,
                "Failed to persist destination address reveal: {e}"
            );
        })?;
        Ok(script)
    }

    /// Next unused external script for the signer's
    /// `get_shutdown_scriptpubkey` — non-deterministic by design (PWA parity):
    /// shutdown scripts are recorded at channel open and replayed from
    /// serialized state, so they need no cross-device re-derivation.
    pub(crate) fn next_unused_address_script(&self) -> Result<ScriptBuf, ()> {
        let mut inner = self.inner.lock().unwrap();
        let WalletInner { wallet, persister } = &mut *inner;
        let address = wallet.next_unused_address(KeychainKind::External);
        wallet.persist(persister).map_err(|e| {
            log_error!(
                self.logger,
                "Failed to persist shutdown address reveal: {e}"
            );
        })?;
        Ok(address.address.script_pubkey())
    }
}

impl ChangeDestinationSourceSync for OnchainWallet {
    fn get_change_destination_script(&self) -> Result<ScriptBuf, ()> {
        let mut inner = self.inner.lock().unwrap();
        let WalletInner { wallet, persister } = &mut *inner;
        let address = wallet.reveal_next_address(KeychainKind::Internal);
        wallet.persist(persister).map_err(|e| {
            log_error!(
                self.logger,
                "Failed to persist revealed change address: {e}"
            );
        })?;
        Ok(address.address.script_pubkey())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn fresh_store() -> (tempfile::TempDir, Arc<FilesystemStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStore::new(PathBuf::from(dir.path())));
        (dir, store)
    }

    fn test_wallet(
        store: Arc<FilesystemStore>,
        network: Network,
    ) -> Result<OnchainWallet, BuildError> {
        let keys = crate::keys::derive_wallet_keys(
            &crate::keys::parse_mnemonic(crate::keys::tests::TEST_MNEMONIC).unwrap(),
            Network::Bitcoin,
        );
        OnchainWallet::new(
            &keys.descriptor_external,
            &keys.descriptor_internal,
            network,
            store,
            Arc::new(Logger),
        )
    }

    #[test]
    fn wallet_persists_and_reloads_from_the_kv_store() {
        let (_dir, store) = fresh_store();

        let wallet = test_wallet(Arc::clone(&store), Network::Bitcoin).unwrap();
        // Reveal an address so the persisted state carries indexer data.
        let script = wallet.get_change_destination_script().unwrap();
        assert!(!script.is_empty());
        drop(wallet);

        // Reload: same descriptors, existing changeset -> load path, and the
        // revealed index survives (the next change script differs).
        let reloaded = test_wallet(store, Network::Bitcoin).unwrap();
        let next_script = reloaded.get_change_destination_script().unwrap();
        assert_ne!(
            script, next_script,
            "revealed address index was not persisted"
        );
    }

    #[test]
    fn network_mismatch_fails_wallet_setup() {
        let (_dir, store) = fresh_store();
        test_wallet(Arc::clone(&store), Network::Bitcoin).unwrap();
        match test_wallet(store, Network::Testnet) {
            Err(err) => assert_eq!(err, BuildError::WalletSetupFailed),
            Ok(_) => panic!("network mismatch must fail wallet setup"),
        }
    }

    #[test]
    fn destination_reveal_survives_a_reload() {
        // The signer's deterministic destination indexes must stay revealed
        // (watched) across restarts: reveal to a high index, reload, and the
        // next change/shutdown reveals must not have collapsed the index.
        let (_dir, store) = fresh_store();
        let wallet = test_wallet(Arc::clone(&store), Network::Bitcoin).unwrap();
        let script = wallet.destination_script_for_index(735).unwrap();
        drop(wallet);

        let reloaded = test_wallet(store, Network::Bitcoin).unwrap();
        assert_eq!(
            reloaded.destination_script_for_index(735).unwrap(),
            script,
            "same index must resolve to the same script after reload"
        );
        assert_eq!(
            reloaded
                .inner
                .lock()
                .unwrap()
                .wallet
                .derivation_index(KeychainKind::External),
            Some(735),
            "the revealed external index must survive persistence"
        );
    }
}
