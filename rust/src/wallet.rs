//! On-chain wallet: a BIP84 `bdk_wallet` over the mnemonic-derived
//! descriptors (U1, KTD-4), persisted into the shared KVStore as a merged
//! `ChangeSet` blob and synced via the shared esplora client. Also serves as
//! the sweeper's change-destination source, the signer's address source
//! (deterministic destination scripts, next-unused shutdown scripts), and the
//! U8 send engine's tx factory (focused build/sign methods over the mutexed
//! bdk wallet — the raw wallet is never exposed).

use std::sync::{Arc, Mutex};

use bdk_esplora::EsploraAsyncExt;
use bdk_wallet::chain::Merge;
use bdk_wallet::{
    ChangeSet, KeychainKind, PersistedWallet, SignOptions, Wallet as BdkWallet, WalletPersister,
};
use bitcoin::{Amount, FeeRate, Network, Psbt, ScriptBuf, Transaction};
use esplora_client::AsyncClient as EsploraAsyncClient;
use lightning::log_error;
use lightning::sign::ChangeDestinationSourceSync;
use lightning::util::logger::Logger as _;
use lightning::util::persist::KVStoreSync;
use lightning_persister::fs_store::FilesystemStore;

use crate::builder::BuildError;
use crate::chain::ChainError;
use crate::onchain_send::{BuiltTxFacts, OnchainSendError, TxBuildFailure, TxSpec};
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

    /// All wallet transactions with net amounts and confirmation status, for
    /// the unified activity merge (U5, KTD-7). Timestamps follow the PWA:
    /// confirmation time, else first-seen-in-mempool, else none.
    pub(crate) fn list_transactions(&self) -> Vec<crate::history::OnchainTxSummary> {
        let inner = self.inner.lock().unwrap();
        inner
            .wallet
            .transactions()
            .map(|wallet_tx| {
                let (sent, received) = inner.wallet.sent_and_received(&wallet_tx.tx_node.tx);
                let (confirmed, confirmation_time_secs, first_seen_secs) =
                    match wallet_tx.chain_position {
                        bdk_wallet::chain::ChainPosition::Confirmed { anchor, .. } => {
                            (true, Some(anchor.confirmation_time), None)
                        }
                        bdk_wallet::chain::ChainPosition::Unconfirmed {
                            first_seen,
                            last_seen,
                        } => (false, None, first_seen.or(last_seen)),
                    };
                crate::history::OnchainTxSummary {
                    txid: wallet_tx.tx_node.txid.to_string(),
                    sent_sats: sent.to_sat(),
                    received_sats: received.to_sat(),
                    confirmed,
                    confirmation_time_secs,
                    first_seen_secs,
                }
            })
            .collect()
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

    /// Trusted-spendable balance in sats: confirmed + trusted pending — the
    /// PWA's `spendableSats` (`context.tsx:47-51`). Untrusted pending is
    /// NEVER counted (U8, R7).
    pub(crate) fn trusted_spendable_sats(&self) -> u64 {
        self.inner
            .lock()
            .unwrap()
            .wallet
            .balance()
            .trusted_spendable()
            .to_sat()
    }

    /// Builds `spec` at `fee_rate` to learn its fee and outputs WITHOUT
    /// broadcasting (U8): staged changes from the estimate build are always
    /// discarded, mirroring the PWA's `discardStagedChanges`
    /// (`context.tsx:147-166`).
    pub(crate) fn estimate_onchain_tx(
        &self,
        spec: &TxSpec,
        fee_rate: FeeRate,
    ) -> Result<BuiltTxFacts, TxBuildFailure> {
        let mut inner = self.inner.lock().unwrap();
        let WalletInner { wallet, .. } = &mut *inner;
        let result = build_tx_locked(wallet, spec, fee_rate).and_then(|psbt| facts_from(&psbt));
        let _ = wallet.take_staged();
        result
    }

    /// Builds `spec`, runs `verify` on the built facts at the broadcast
    /// boundary (U8/R7 drift + fee guards — BEFORE anything is signed), then
    /// signs, extracts, and persists the changeset (address reveals). On any
    /// rejection the abandoned build's staged changes are discarded, exactly
    /// like the PWA's `buildSignBroadcast` (`context.tsx:168-237`).
    pub(crate) fn create_onchain_tx(
        &self,
        spec: &TxSpec,
        fee_rate: FeeRate,
        verify: impl FnOnce(&BuiltTxFacts) -> Result<(), OnchainSendError>,
        map_build_failure: impl FnOnce(TxBuildFailure) -> OnchainSendError,
    ) -> Result<Transaction, OnchainSendError> {
        let mut inner = self.inner.lock().unwrap();
        let WalletInner { wallet, persister } = &mut *inner;

        let discard = |wallet: &mut PersistedWallet<KVStoreWalletPersister>| {
            let _ = wallet.take_staged();
        };

        let mut psbt = match build_tx_locked(wallet, spec, fee_rate).and_then(|psbt| {
            let facts = facts_from(&psbt)?;
            Ok((psbt, facts))
        }) {
            Ok((psbt, facts)) => {
                if let Err(rejection) = verify(&facts) {
                    discard(wallet);
                    return Err(rejection);
                }
                psbt
            }
            Err(failure) => {
                discard(wallet);
                return Err(map_build_failure(failure));
            }
        };

        let finalized = wallet
            .sign(&mut psbt, SignOptions::default())
            .map_err(|e| {
                discard(wallet);
                OnchainSendError::SigningFailed {
                    detail: e.to_string(),
                }
            })?;
        if !finalized {
            discard(wallet);
            return Err(OnchainSendError::SigningFailed {
                detail: "the signed transaction did not finalize".to_string(),
            });
        }
        let tx = psbt.extract_tx().map_err(|e| {
            discard(wallet);
            OnchainSendError::SigningFailed {
                detail: e.to_string(),
            }
        })?;

        // Persist BEFORE handing the tx to the broadcaster: the reveal of the
        // change/reserve address must survive a crash between sign and
        // broadcast (the address-reveal learning).
        wallet.persist(persister).map_err(|e| {
            log_error!(self.logger, "Failed to persist the send changeset: {e}");
            OnchainSendError::BuildFailed {
                detail: format!("failed to persist the wallet changeset: {e}"),
            }
        })?;
        Ok(tx)
    }

    /// Next unused EXTERNAL address for the Receive screen (U8, PWA
    /// `generateAddress`, `context.tsx:134-139`): the changeset is persisted
    /// after every reveal — the address-reveal learning — so a restart keeps
    /// the index.
    pub(crate) fn next_receive_address(&self) -> Result<String, ()> {
        let mut inner = self.inner.lock().unwrap();
        let WalletInner { wallet, persister } = &mut *inner;
        let address = wallet.next_unused_address(KeychainKind::External);
        wallet.persist(persister).map_err(|e| {
            log_error!(self.logger, "Failed to persist receive-address reveal: {e}");
        })?;
        Ok(address.address.to_string())
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

/// Builds a [`TxSpec`] into a PSBT over the locked bdk wallet (U8).
///
/// Untrusted-pending UTXOs (unconfirmed EXTERNAL receives) are always marked
/// unspendable so they are never counted, matching the trusted-spendable
/// arithmetic (R7). The reserve branch adds an EXPLICIT reserve output to the
/// next unused internal (change) address, so a send-max with channels leaves
/// exactly the reserve behind (AE6).
fn build_tx_locked(
    wallet: &mut PersistedWallet<KVStoreWalletPersister>,
    spec: &TxSpec,
    fee_rate: FeeRate,
) -> Result<Psbt, TxBuildFailure> {
    let untrusted: Vec<bitcoin::OutPoint> = wallet
        .list_unspent()
        .filter(|utxo| {
            !utxo.chain_position.is_confirmed() && utxo.keychain == KeychainKind::External
        })
        .map(|utxo| utxo.outpoint)
        .collect();
    // The reserve output's internal address is revealed FRESH per build
    // (`reveal_next_address`, like the sweeper's change source) rather than
    // `next_unused_address`: next-unused stages the reveal only once, and an
    // earlier estimate build's staged-then-discarded reveal would leave the
    // send's persisted changeset without it — the reserve script would be
    // unwatched after a restart (the address-reveal learning). Reveals are
    // monotone, so persisting the send's higher index covers the estimates'.
    let reserve_script = match spec {
        TxSpec::DrainWithReserve { .. } => Some(
            wallet
                .reveal_next_address(KeychainKind::Internal)
                .address
                .script_pubkey(),
        ),
        TxSpec::Recipient { .. } | TxSpec::DrainAll { .. } | TxSpec::FundingOutput { .. } => None,
    };

    let mut builder = wallet.build_tx();
    builder.unspendable(untrusted).fee_rate(fee_rate);
    match spec {
        TxSpec::Recipient {
            script,
            amount_sats,
        } => {
            builder.add_recipient(script.clone(), Amount::from_sat(*amount_sats));
        }
        TxSpec::FundingOutput {
            script,
            amount_sats,
        } => {
            // U9: LDK requires a final locktime on the funding tx; bdk's
            // anti-fee-sniping default (current height) is overridden to 0,
            // exactly like the PWA's funding build (`event-handler.ts`).
            builder
                .add_recipient(script.clone(), Amount::from_sat(*amount_sats))
                .nlocktime(bitcoin::absolute::LockTime::ZERO);
        }
        TxSpec::DrainAll { script } => {
            builder.drain_wallet().drain_to(script.clone());
        }
        TxSpec::DrainWithReserve {
            recipient,
            reserve_sats,
        } => {
            builder
                .add_recipient(
                    reserve_script.expect("reserve script is Some for DrainWithReserve"),
                    Amount::from_sat(*reserve_sats),
                )
                .drain_wallet()
                .drain_to(recipient.clone());
        }
    }
    builder.finish().map_err(|e| match e {
        bdk_wallet::error::CreateTxError::OutputBelowDustLimit(_) => {
            TxBuildFailure::OutputBelowDust
        }
        bdk_wallet::error::CreateTxError::CoinSelection(e) => {
            TxBuildFailure::InsufficientFunds(e.to_string())
        }
        other => TxBuildFailure::Other(other.to_string()),
    })
}

/// The facts the U8 guards inspect: absolute fee and the built outputs.
fn facts_from(psbt: &Psbt) -> Result<BuiltTxFacts, TxBuildFailure> {
    let fee_sats = psbt
        .fee()
        .map_err(|e| TxBuildFailure::Other(e.to_string()))?
        .to_sat();
    Ok(BuiltTxFacts {
        fee_sats,
        outputs: psbt
            .unsigned_tx
            .output
            .iter()
            .map(|out| (out.script_pubkey.clone(), out.value.to_sat()))
            .collect(),
    })
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

/// Offline funding helpers for the U8 send tests: bdk's own `test_utils`
/// (`insert_tx` + checkpoint anchors) inject confirmed / trusted-pending /
/// untrusted-pending outputs without any network.
#[cfg(test)]
pub(crate) mod test_support {
    use bdk_wallet::chain::{BlockId, ConfirmationBlockTime};
    use bdk_wallet::test_utils::{receive_output, receive_output_to_address, ReceiveTo};
    use bitcoin::hashes::Hash as _;

    use super::*;

    fn ensure_checkpoint(wallet: &mut PersistedWallet<KVStoreWalletPersister>) {
        if wallet.latest_checkpoint().height() == 0 {
            bdk_wallet::test_utils::insert_checkpoint(
                wallet,
                BlockId {
                    height: 1_000,
                    hash: bitcoin::BlockHash::all_zeros(),
                },
            );
        }
    }

    /// A confirmed external receive: counted in every balance.
    pub(crate) fn fund_confirmed(onchain: &OnchainWallet, sats: u64) {
        let mut inner = onchain.inner.lock().unwrap();
        let wallet = &mut inner.wallet;
        ensure_checkpoint(wallet);
        let anchor = ConfirmationBlockTime {
            block_id: wallet.latest_checkpoint().block_id(),
            confirmation_time: 100,
        };
        receive_output(wallet, Amount::from_sat(sats), ReceiveTo::Block(anchor));
    }

    /// An UNCONFIRMED external receive: bdk's untrusted pending — never
    /// spendable, never counted (U8, R7).
    pub(crate) fn fund_untrusted_pending(onchain: &OnchainWallet, sats: u64) {
        let mut inner = onchain.inner.lock().unwrap();
        let wallet = &mut inner.wallet;
        ensure_checkpoint(wallet);
        receive_output(
            wallet,
            Amount::from_sat(sats),
            ReceiveTo::Mempool(1_700_000_000),
        );
    }

    /// An UNCONFIRMED internal (change) receive: bdk's trusted pending —
    /// counted in the trusted-spendable balance.
    pub(crate) fn fund_trusted_pending(onchain: &OnchainWallet, sats: u64) {
        let mut inner = onchain.inner.lock().unwrap();
        let wallet = &mut inner.wallet;
        ensure_checkpoint(wallet);
        let address = wallet.reveal_next_address(KeychainKind::Internal).address;
        receive_output_to_address(
            wallet,
            address,
            Amount::from_sat(sats),
            ReceiveTo::Mempool(1_700_000_000),
        );
    }

    /// Whether `script` belongs to the wallet's INTERNAL (change) keychain —
    /// the AE6 reserve-output assertion.
    pub(crate) fn is_internal_script(onchain: &OnchainWallet, script: &ScriptBuf) -> bool {
        matches!(
            onchain
                .inner
                .lock()
                .unwrap()
                .wallet
                .derivation_of_spk(script.clone()),
            Some((KeychainKind::Internal, _))
        )
    }

    /// The wallet's revealed derivation index for a keychain (restart tests).
    pub(crate) fn derivation_index(onchain: &OnchainWallet, keychain: KeychainKind) -> Option<u32> {
        onchain
            .inner
            .lock()
            .unwrap()
            .wallet
            .derivation_index(keychain)
    }

    /// How many transactions the wallet's local graph knows (estimates must
    /// never add one).
    pub(crate) fn tx_count(onchain: &OnchainWallet) -> usize {
        onchain.inner.lock().unwrap().wallet.transactions().count()
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
