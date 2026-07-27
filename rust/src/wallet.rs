//! On-chain wallet: a BIP84 `bdk_wallet` over the mnemonic-derived
//! descriptors (U1, KTD-4), persisted into the shared KVStore as a merged
//! `ChangeSet` blob and synced via the shared esplora client. Also serves as
//! the sweeper's change-destination source, the signer's address source
//! (deterministic destination scripts, next-unused shutdown scripts), and the
//! U8 send engine's tx factory (focused build/sign methods over the mutexed
//! bdk wallet — the raw wallet is never exposed).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use bdk_esplora::EsploraAsyncExt;
use bdk_wallet::chain::spk_client::SyncRequest;
use bdk_wallet::chain::{spk_txout, tx_graph, ConfirmationBlockTime, Merge};
use bdk_wallet::{
    ChangeSet, KeychainKind, PersistedWallet, SignOptions, Wallet as BdkWallet, WalletPersister,
};
use bitcoin::{Amount, FeeRate, Network, OutPoint, Psbt, ScriptBuf, Transaction};
use esplora_client::AsyncClient as EsploraAsyncClient;
use lightning::log_error;
use lightning::sign::ChangeDestinationSourceSync;
use lightning::util::logger::Logger as _;
use lightning::util::persist::KVStoreSync;
use lightning_persister::fs_store::FilesystemStore;

use crate::builder::BuildError;
use crate::chain::ChainError;
use crate::config::ONCHAIN_SYNC_KEYCHAIN_WINDOW;
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
    /// U10 Initial Scan flag (the PWA's `onchain/scan-state.ts`): set once
    /// the FIRST successful chain sync of this process completes. Recovery
    /// entry is gated on it — on a restore the wallet is empty BY
    /// CONSTRUCTION until the scan lands, so "no UTXOs" is meaningless and
    /// once fired a false Recover-Funds banner on every restore. Never set
    /// on a failed scan. Per-process by design (a fresh wallet is built at
    /// every `start()`), mirroring the PWA's per-session module flag.
    initial_scan_complete: std::sync::atomic::AtomicBool,
    /// Set by `builder::build` when this boot loaded pre-existing LDK state —
    /// a U4 restore, a `vss::startup` silent recovery, or an existing install's
    /// restart. Only the FIRST full scan of such a wallet consults it (see
    /// [`ChainSource::sync_onchain_wallet`]): a cross-client cold start has an
    /// empty bdk changeset, so it needs more stop-gap headroom than a wallet
    /// this device created itself.
    ///
    /// [`ChainSource::sync_onchain_wallet`]: crate::chain::ChainSource::sync_onchain_wallet
    cold_restore: std::sync::atomic::AtomicBool,
    /// EXTERNAL indices that a KTD-4 deterministic close destination lands on
    /// (`BE(channel_keys_id[0..4]) mod 10_000`), pinned into every incremental
    /// sync by [`OnchainWallet::bounded_sync_request`].
    ///
    /// WHY A SEPARATE SET: destination indices are uniform over 0..9 999, so
    /// they sit in the MIDDLE of the revealed range — neither the lowest-unused
    /// window (where `next_unused_address` vends) nor the highest-revealed
    /// window (where `reveal_next_address` vends) can cover them, and dropping
    /// one would permanently hide a channel's close funds. Every path that can
    /// hand a destination script to LDK registers here:
    /// [`OnchainWallet::destination_script_for_index`] (channel open, and the
    /// signer's lazy `get_destination_script`) and
    /// [`crate::signer::WalletSignerProvider::reveal_derived_destinations`]
    /// (every boot, over every loaded monitor's `channel_keys_id`).
    ///
    /// Per-process by design, and correct without persistence because it is
    /// rebuilt from the SAME source of truth on every boot — the monitors —
    /// before the first scan runs. RESIDUAL (inherited verbatim from
    /// `reveal_derived_destinations`): a channel whose monitor was fully
    /// archived AND deleted leaves no `channel_keys_id` to derive from, so its
    /// index is not pinned. Such a channel was resolved on chain, which means
    /// its destination either already received (→ it is a USED spk, and used
    /// spks are always in the sync set) or never will.
    destination_indexes: Mutex<BTreeSet<u32>>,
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
            initial_scan_complete: std::sync::atomic::AtomicBool::new(false),
            cold_restore: std::sync::atomic::AtomicBool::new(false),
            destination_indexes: Mutex::new(BTreeSet::new()),
            logger,
        })
    }

    /// Marks this boot as a cold restore / recovery (see
    /// [`OnchainWallet::cold_restore`]).
    pub(crate) fn mark_cold_restore(&self) {
        self.cold_restore
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Whether this boot loaded pre-existing LDK state, which widens the stop
    /// gap of the first full scan.
    pub(crate) fn is_cold_restore(&self) -> bool {
        self.cold_restore.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Registers `index` as a KTD-4 deterministic close destination that every
    /// incremental sync must keep querying (see
    /// [`OnchainWallet::destination_indexes`]). Monotone and idempotent.
    pub(crate) fn watch_destination_indexes(&self, indexes: impl IntoIterator<Item = u32>) {
        self.destination_indexes.lock().unwrap().extend(indexes);
    }

    /// Whether the wallet has never seen a block, i.e. the next sync must be a
    /// FULL scan. A fresh (or freshly restored) wallet's local chain knows only
    /// genesis.
    fn needs_full_scan(&self) -> bool {
        self.inner
            .lock()
            .unwrap()
            .wallet
            .latest_checkpoint()
            .height()
            == 0
    }

    /// Syncs against Esplora: full scan on first use, incremental afterwards.
    /// The first SUCCESSFUL pass marks the Initial Scan complete (U10 gate);
    /// a failed scan never does. Returns whether the pass CHANGED any
    /// wallet-visible data (see [`OnchainWallet::apply_update`]).
    pub(crate) async fn sync(
        &self,
        client: &EsploraAsyncClient,
        stop_gap: usize,
        concurrency: usize,
    ) -> Result<bool, ChainError> {
        let result = if self.needs_full_scan() {
            let request = self.inner.lock().unwrap().wallet.start_full_scan().build();
            let update = client
                .full_scan(request, stop_gap, concurrency)
                .await
                .map_err(|e| ChainError::EsploraUnreachable(e.to_string()))?;
            self.apply_update(update)
        } else {
            let request = self.bounded_sync_request();
            let update = client
                .sync(request, concurrency)
                .await
                .map_err(|e| ChainError::EsploraUnreachable(e.to_string()))?;
            self.apply_update(update)
        };
        if result.is_ok() {
            self.initial_scan_complete
                .store(true, std::sync::atomic::Ordering::Release);
        }
        result
    }

    /// The INCREMENTAL sync request: a SPARSE UNION of the SPKs that can
    /// plausibly move, instead of bdk's `start_sync_with_revealed_spks()` —
    /// which queries every revealed SPK and therefore pays the full price of
    /// the KTD-4 destination scheme's inclusive reveal on every single tick
    /// (see [`ONCHAIN_SYNC_KEYCHAIN_WINDOW`] for the measurement that forced
    /// this).
    ///
    /// Additive over the same primitives bdk uses: `revealed_spks_from_indexer`
    /// is itself a one-line wrapper over `spks_with_indexes(...)`
    /// (`bdk_chain::keychain_txout`), so narrowing the SPK set changes nothing
    /// else about the request — the chain tip and the `expected_spk_txids`
    /// eviction detection are constructed exactly as
    /// `Wallet::start_sync_with_revealed_spks` does.
    ///
    /// THE SAFETY PROPERTY: an SPK that has ever been handed out, or that has
    /// any known history, is NEVER dropped — missing a payment permanently is
    /// strictly worse than a slow sync. Each member of the union carries its
    /// own proof:
    ///
    /// 1. Every USED SPK the indexer TRACKS, on both keychains (revealed range
    ///    plus lookahead). Covers all history we know about, hence every UTXO's
    ///    own script and every previously-paid address, and (with 5.) keeps
    ///    spends of our coins detectable. It is also a superset of the SPKs
    ///    `expected_spk_txids` names — those come from canonical txs, which by
    ///    definition made their SPKs used — so eviction /
    ///    malicious-replacement detection is fully preserved.
    /// 2. The LOWEST [`ONCHAIN_SYNC_KEYCHAIN_WINDOW`] UNUSED revealed SPKs per
    ///    keychain. This is where the wallet vends next: `next_receive_address`
    ///    and the signer's shutdown script both use `next_unused_address`, and
    ///    bdk returns the LOWEST unused revealed index — never a high junk one.
    ///    Change outputs land here too (bdk's `TxBuilder` takes the next unused
    ///    internal spk).
    /// 3. The HIGHEST [`ONCHAIN_SYNC_KEYCHAIN_WINDOW`] revealed SPKs per
    ///    keychain. `reveal_next_external_script` (U11 sweep destinations) and
    ///    the reserve-output change script use `reveal_next_address`, which
    ///    vends `last_revealed + 1` — the HIGH end. (That is exactly why the
    ///    observed sweep paid to index 5030 rather than 3.)
    /// 4. The pinned KTD-4 destination indices
    ///    ([`OnchainWallet::destination_indexes`]) — mid-range by construction,
    ///    so provably outside 2. and 3.
    /// 5. The outpoints of the current UTXO set, queried BY OUTPOINT rather
    ///    than by script. Exact and index-independent: a spend of one of our
    ///    coins is seen even if some pathological wallet shape ever excluded
    ///    its script.
    ///
    /// What this deliberately DOES drop is the interior of the revealed range:
    /// `reveal_addresses_to(5030)` reveals 0..=5030 inclusive, and the ~5 000
    /// indices in the middle were never vended to anybody and have no history.
    /// The FULL SCAN path is untouched — it still walks the descriptors
    /// exhaustively under the stop gaps (`BDK_CLIENT_STOP_GAP` /
    /// `BDK_COLD_RESTORE_STOP_GAP`), which is what discovers history in the
    /// first place.
    fn bounded_sync_request(&self) -> SyncRequest<(KeychainKind, u32)> {
        let inner = self.inner.lock().unwrap();
        let wallet = &inner.wallet;
        let index = wallet.spk_index();
        let tip = wallet.latest_checkpoint();
        let tip_block_id = tip.block_id();

        // Deduped by (keychain, derivation index): `spks_with_indexes` only
        // extends a queue, so a duplicate would be a duplicate Esplora query.
        let mut spks: BTreeMap<(KeychainKind, u32), ScriptBuf> = BTreeMap::new();
        // (1) everything with known history, taken from the indexer's FULL spk
        // map rather than the revealed range: bdk also tracks a lookahead
        // window, and an incremental sync (unlike a full scan) does not bump
        // `last_revealed` for what it finds there. Reading "used" off the
        // tracked set makes this an unconditional superset of every SPK
        // `expected_spk_txids` can name.
        let tracked: &spk_txout::SpkTxOutIndex<(KeychainKind, u32)> = index.as_ref();
        spks.extend(
            tracked
                .all_spks()
                .iter()
                .filter(|(keychain_index, _)| tracked.is_used(keychain_index))
                .map(|(keychain_index, spk)| (*keychain_index, spk.clone())),
        );
        for keychain in [KeychainKind::External, KeychainKind::Internal] {
            // (2) the low end: where `next_unused_address` vends.
            spks.extend(
                index
                    .unused_keychain_spks(keychain)
                    .take(ONCHAIN_SYNC_KEYCHAIN_WINDOW)
                    .map(|(i, spk)| ((keychain, i), spk)),
            );
            // (3) the high end: where `reveal_next_address` vends.
            spks.extend(
                index
                    .revealed_keychain_spks(keychain)
                    .rev()
                    .take(ONCHAIN_SYNC_KEYCHAIN_WINDOW)
                    .map(|(i, spk)| ((keychain, i), spk)),
            );
        }
        // (4) the pinned close destinations. `peek_address` is a pure
        // derivation, so this needs no wallet mutation; every pinned index was
        // revealed by the path that pinned it, so the indexer recognises the
        // script when a tx for it comes back.
        //
        // Lock order is inner -> destination_indexes throughout (this is the
        // only place that holds both).
        for destination in self.destination_indexes.lock().unwrap().iter().copied() {
            spks.entry((KeychainKind::External, destination))
                .or_insert_with(|| {
                    wallet
                        .peek_address(KeychainKind::External, destination)
                        .address
                        .script_pubkey()
                });
        }
        // (5) spends of our current coins, by outpoint.
        let outpoints: Vec<OutPoint> = wallet.list_unspent().map(|utxo| utxo.outpoint).collect();

        SyncRequest::builder()
            .chain_tip(tip)
            .spks_with_indexes(spks)
            .outpoints(outpoints)
            .expected_spk_txids(wallet.tx_graph().list_expected_spk_txids(
                wallet.local_chain(),
                tip_block_id,
                index,
                ..,
            ))
            .build()
    }

    /// Whether this process completed a successful chain scan yet (U10:
    /// recovery entry is forbidden before this — no wallet-emptiness
    /// decision before the Initial Scan, the plan's named invariant).
    pub(crate) fn is_initial_scan_complete(&self) -> bool {
        self.initial_scan_complete
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Whether any CONFIRMED UTXO exists — the CPFP pre-check
    /// (PWA `event-handler.ts:711-721`).
    pub(crate) fn has_confirmed_utxo(&self) -> bool {
        self.inner
            .lock()
            .unwrap()
            .wallet
            .list_unspent()
            .any(|utxo| utxo.chain_position.is_confirmed())
    }

    /// Whether `txid` (display hex) is CONFIRMED in this wallet — the
    /// reconcile pass's receipt evidence (PWA `reconcile.ts:69-76`).
    /// Unknown/unparsable txids are `false`: absence is never evidence.
    pub(crate) fn tx_is_confirmed(&self, txid_hex: &str) -> bool {
        let Ok(txid) = txid_hex.parse::<bitcoin::Txid>() else {
            return false;
        };
        self.inner
            .lock()
            .unwrap()
            .wallet
            .get_tx(txid)
            .is_some_and(|tx| tx.chain_position.is_confirmed())
    }

    /// Applies a sync/scan update and persists it, reporting whether the update
    /// CHANGED wallet-visible data — the trigger for the shells' on-chain
    /// refresh (see [`crate::node::CoreEvent::OnchainStateChanged`]).
    ///
    /// The signal is read off the STAGED bdk changeset between apply and
    /// persist, which is exact: every other mutation path on this wallet
    /// persists immediately after staging (the address-reveal learning) and the
    /// estimate path discards with `take_staged`, so the stage is empty when a
    /// sync starts and holds precisely this update's effect here.
    fn apply_update(&self, update: impl Into<bdk_wallet::Update>) -> Result<bool, ChainError> {
        let mut inner = self.inner.lock().unwrap();
        let WalletInner { wallet, persister } = &mut *inner;
        wallet
            .apply_update(update)
            .map_err(|e| ChainError::WalletSyncFailed(e.to_string()))?;
        let changed = wallet
            .staged()
            .is_some_and(|staged| changes_wallet_data(&staged.tx_graph));
        wallet
            .persist(persister)
            .map_err(|e| ChainError::WalletSyncFailed(e.to_string()))?;
        Ok(changed)
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
        // Pin it BEFORE any early return: a destination handed to LDK that the
        // incremental sync stopped querying would hide close funds forever.
        self.watch_destination_indexes([index]);
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

    /// Reveals EXTERNAL addresses up to and INCLUDING `index` and persists the
    /// changeset — the startup destination reveal
    /// ([`crate::signer::WalletSignerProvider::reveal_derived_destinations`]),
    /// which walks the deterministic close destinations of every channel loaded
    /// this boot so the next chain scan watches them.
    ///
    /// `reveal_addresses_to` is monotone: an `index` at or below what the wallet
    /// already tracks is a no-op, so this can run on every boot. The persist is
    /// mandatory — an unpersisted reveal is silently lost on the next start (the
    /// PWA's `bdk-address-reveal-not-persisted` learning).
    pub(crate) fn reveal_external_addresses_to(&self, index: u32) -> Result<(), ()> {
        let mut inner = self.inner.lock().unwrap();
        let WalletInner { wallet, persister } = &mut *inner;
        // The iterator of newly revealed addresses is not needed; the index
        // update is staged eagerly.
        drop(wallet.reveal_addresses_to(KeychainKind::External, index));
        wallet.persist(persister).map_err(|e| {
            log_error!(
                self.logger,
                "Failed to persist the startup destination reveal: {e}"
            );
        })?;
        Ok(())
    }

    /// The SPKs the next INCREMENTAL sync would query (tests): proof that a
    /// revealed destination index is actually watched by the next sync, and the
    /// bound on how many scripts a tick costs.
    #[cfg(test)]
    pub(crate) fn sync_request_spks(&self) -> Vec<ScriptBuf> {
        let mut request = self.bounded_sync_request();
        request
            .iter_spks_with_expected_txids()
            .map(|spk| spk.spk)
            .collect()
    }

    /// The outpoints the next incremental sync would query directly (tests).
    #[cfg(test)]
    pub(crate) fn sync_request_outpoints(&self) -> Vec<OutPoint> {
        let mut request = self.bounded_sync_request();
        request.iter_outpoints().collect()
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

    /// Whether `script` belongs to this wallet (either keychain) — the sweep
    /// pipeline's wallet-owned `StaticOutput` exclusion, first half (U11/
    /// KTD-8, PWA `sweep.ts:117-149` `isWalletOwnedStaticOutput`).
    pub(crate) fn is_mine_script(&self, script: &bitcoin::Script) -> bool {
        self.inner.lock().unwrap().wallet.is_mine(script.into())
    }

    /// Whether `outpoint` is an unspent output the wallet currently tracks —
    /// the subsidized sweep's shared-input verification helper (tests).
    #[cfg(test)]
    pub(crate) fn owns_unspent_outpoint(&self, outpoint: &bitcoin::OutPoint) -> bool {
        self.inner
            .lock()
            .unwrap()
            .wallet
            .list_unspent()
            .any(|utxo| utxo.outpoint == *outpoint)
    }

    /// The EXTERNAL script at `index` WITHOUT revealing it — the exclusion's
    /// second half (post-recovery re-derivation by `channel_keys_id`, PWA
    /// `sweep.ts:131-144`): compare against a pure derivation first; reveal
    /// (a wallet mutation) only on a confirmed match via
    /// [`OnchainWallet::destination_script_for_index`].
    pub(crate) fn peek_external_script(&self, index: u32) -> ScriptBuf {
        self.inner
            .lock()
            .unwrap()
            .wallet
            .peek_address(KeychainKind::External, index)
            .address
            .script_pubkey()
    }

    /// Reveals the next EXTERNAL script for a sweep destination (U11, PWA
    /// `revealNextAddress` — always advances, unlike next-unused) and
    /// persists the reveal so a restart keeps watching it.
    pub(crate) fn reveal_next_external_script(&self) -> Result<ScriptBuf, ()> {
        let mut inner = self.inner.lock().unwrap();
        let WalletInner { wallet, persister } = &mut *inner;
        let address = wallet.reveal_next_address(KeychainKind::External);
        wallet.persist(persister).map_err(|e| {
            log_error!(
                self.logger,
                "Failed to persist sweep-destination reveal: {e}"
            );
        })?;
        Ok(address.address.script_pubkey())
    }

    /// Every CONFIRMED UTXO as `(outpoint, txout)` — the CPFP wallet source
    /// and the subsidized sweep's candidate list (U11). Confirmed only: an
    /// unconfirmed parent could drop from the mempool and invalidate the
    /// child, leaving the force-close stuck (PWA `bdk-wallet-source.ts:36-47`,
    /// `subsidized-sweep.ts:185-187`).
    pub(crate) fn confirmed_utxos(&self) -> Vec<(bitcoin::OutPoint, bitcoin::TxOut)> {
        self.inner
            .lock()
            .unwrap()
            .wallet
            .list_unspent()
            .filter(|utxo| utxo.chain_position.is_confirmed())
            .map(|utxo| (utxo.outpoint, utxo.txout))
            .collect()
    }

    /// Signs `psbt`'s wallet-owned inputs with `trust_witness_utxo: true`
    /// (U11 — the historic CPFP-cannot-sign bug): LDK-produced PSBTs carry
    /// only `witness_utxo` for our inputs, and bdk's default `SignOptions`
    /// reject that (CVE-2020-14199 fee-siphon mitigation, aimed at UNTRUSTED
    /// PSBT producers). Here LDK builds the PSBT on our behalf from state we
    /// already trust, so the trust flag is safe — and the only way CPFP or a
    /// subsidized sweep can sign (PWA `bdk-wallet-source.ts:102-112`,
    /// `subsidized-sweep.ts:403-407`). Returns whether the PSBT finalized
    /// (inputs already carrying a final witness — LDK's — count as done).
    pub(crate) fn sign_psbt_trusted(&self, psbt: &mut Psbt) -> Result<bool, String> {
        let sign_options = SignOptions {
            trust_witness_utxo: true,
            ..Default::default()
        };
        self.inner
            .lock()
            .unwrap()
            .wallet
            .sign(psbt, sign_options)
            .map_err(|e| e.to_string())
    }

    /// Registers a self-broadcast unconfirmed tx with the wallet graph so
    /// coin selection excludes its inputs BEFORE the next chain sync (~180 s
    /// window), persisting the changeset (U11, PWA
    /// `subsidized-sweep.ts:248-277` `markSubsidyInputsSpent`). Failure is
    /// non-fatal for the caller — the session reservation set still guards
    /// the sweep path.
    pub(crate) fn apply_unconfirmed_tx(
        &self,
        tx: Transaction,
        last_seen_secs: u64,
    ) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        let WalletInner { wallet, persister } = &mut *inner;
        wallet.apply_unconfirmed_txs([(tx, last_seen_secs)]);
        wallet.persist(persister).map_err(|e| e.to_string())?;
        Ok(())
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

/// Whether a staged bdk `tx_graph` changeset carries something the WALLET
/// SCREENS can see — the "did anything actually change" test behind the
/// on-chain refresh event.
///
/// Deliberately NOT `ChangeSet::is_empty()`:
/// - `local_chain` is non-empty on every new block (~10 min), which is a tip
///   advance, not a balance or activity change. `SyncCompleted`/`SyncFailed`
///   already carry sync liveness.
/// - `last_seen` / `first_seen` are non-empty on EVERY tick while any
///   unconfirmed tx sits in the mempool, because bdk re-stamps a mempool tx's
///   last-seen with the request's start time each pass. Firing on those would
///   reintroduce exactly the "event every 120 s for nothing" the shells must
///   not get.
///
/// What remains is genuinely new information: a new tx (`txs`), a new
/// floating txout (`txouts`), a CONFIRMATION (`anchors`), or a mempool
/// eviction (`last_evicted`) — each of which moves a balance or an activity
/// row.
fn changes_wallet_data(staged: &tx_graph::ChangeSet<ConfirmationBlockTime>) -> bool {
    !staged.txs.is_empty()
        || !staged.txouts.is_empty()
        || !staged.anchors.is_empty()
        || !staged.last_evicted.is_empty()
}

/// U10: the reconcile pass's wallet-receipt evidence rides on the bdk
/// wallet. This invariant relies on BDK's graph containing only SPK-tracked
/// txs — nothing `insert_tx`es broadcast transactions, so a force-close
/// commitment (spending the untracked 2-of-2 funding output) can never
/// false-positive as a receipt.
impl crate::close_records::WalletReceipts for OnchainWallet {
    fn tx_confirmed_in_wallet(&self, txid: &str) -> bool {
        self.tx_is_confirmed(txid)
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

    /// Signs `psbt` with explicit `SignOptions` — the U11 trust-flag guard
    /// compares default-vs-trusted signing behavior.
    pub(crate) fn sign_with_options(
        onchain: &OnchainWallet,
        psbt: &mut Psbt,
        sign_options: SignOptions,
    ) -> Result<bool, String> {
        onchain
            .inner
            .lock()
            .unwrap()
            .wallet
            .sign(psbt, sign_options)
            .map_err(|e| e.to_string())
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

    /// U10: the CPFP pre-check counts only CONFIRMED UTXOs — unconfirmed
    /// external receives must not suppress the recovery signal (they cannot
    /// fund a CPFP), and a fresh wallet has not completed its Initial Scan.
    #[test]
    fn has_confirmed_utxo_ignores_unconfirmed_receives() {
        let (_dir, store) = fresh_store();
        let wallet = test_wallet(store, Network::Bitcoin).unwrap();
        assert!(
            !wallet.is_initial_scan_complete(),
            "fresh wallet: no scan yet"
        );
        assert!(!wallet.has_confirmed_utxo(), "empty wallet");

        crate::wallet::test_support::fund_untrusted_pending(&wallet, 50_000);
        assert!(
            !wallet.has_confirmed_utxo(),
            "unconfirmed funds cannot pay for a CPFP"
        );

        crate::wallet::test_support::fund_confirmed(&wallet, 25_000);
        assert!(wallet.has_confirmed_utxo());
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

    // ---------- bounded incremental sync (the 804 s regression) ----------

    /// The index the real mainnet wallet's closed channel derived to, which
    /// dragged `last_revealed` to 5 030 and every 120 s sync tick to ~5 031
    /// Esplora script queries / 804 s wall clock.
    const OBSERVED_DESTINATION_INDEX: u32 = 5_030;
    /// A second destination in the MIDDLE of the revealed range: neither the
    /// low-unused nor the high-revealed window can reach it, so only the pinned
    /// destination set keeps it watched.
    const INTERIOR_DESTINATION_INDEX: u32 = 2_000;

    fn spk_of(address: &str) -> ScriptBuf {
        address
            .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()
            .unwrap()
            .assume_checked()
            .script_pubkey()
    }

    /// The whole point of the fix: a wallet whose `last_revealed` sits at 5 030
    /// must still cost a SMALL, concretely bounded number of script queries per
    /// tick, while keeping every SPK that can matter.
    #[test]
    fn a_high_index_destination_keeps_the_incremental_sync_bounded() {
        let (_dir, store) = fresh_store();
        let wallet = test_wallet(store, Network::Bitcoin).unwrap();

        // Real history at the low indices (external 0 and 1) ...
        test_support::fund_confirmed(&wallet, 25_000);
        test_support::fund_confirmed(&wallet, 10_000);
        // ... plus two closed channels' deterministic KTD-4 destinations. The
        // reveal is INCLUSIVE, so this alone makes 5 031 external addresses
        // "revealed" — that is what `start_sync_with_revealed_spks` queried.
        let interior = wallet
            .destination_script_for_index(INTERIOR_DESTINATION_INDEX)
            .unwrap();
        let observed = wallet
            .destination_script_for_index(OBSERVED_DESTINATION_INDEX)
            .unwrap();
        assert_eq!(
            test_support::derivation_index(&wallet, KeychainKind::External),
            Some(OBSERVED_DESTINATION_INDEX),
        );

        let spks = wallet.sync_request_spks();

        // The concrete bound, itemised so a future change to the union shows up
        // here as an arithmetic mismatch rather than a silent cost regression:
        //   2 used external (0, 1)
        //   + 20 lowest UNUSED external (2..=21)      — `next_unused_address`
        //   + 20 highest revealed external (5011..=5030) — `reveal_next_address`
        //   + 1 pinned interior destination (2 000)
        //   + 0 internal (nothing revealed on the change keychain yet)
        const EXPECTED_SPKS: usize = 2 + 20 + 20 + 1;
        assert_eq!(
            spks.len(),
            EXPECTED_SPKS,
            "the incremental sync must stay bounded, not scale with last_revealed"
        );
        assert!(
            spks.len() < 100,
            "5 031 scripts per tick is the regression being fixed"
        );

        // ... and nothing that can matter was dropped.
        assert!(
            spks.contains(&observed),
            "the high close destination must stay watched"
        );
        assert!(
            spks.contains(&interior),
            "an INTERIOR close destination is only reachable via the pinned set"
        );
        for used in [0, 1] {
            assert!(
                spks.contains(&wallet.peek_external_script(used)),
                "external index {used} has history and must never be dropped"
            );
        }
        // The address the Receive screen would hand out next (lowest unused).
        let next_receive = spk_of(&wallet.next_receive_address().unwrap());
        assert!(
            spks.contains(&next_receive),
            "the next vended receive address must be watched before it is paid"
        );
        // Spends of our own coins are followed by outpoint, independent of any
        // script window.
        let outpoints = wallet.sync_request_outpoints();
        assert_eq!(outpoints.len(), 2, "both confirmed UTXOs, queried directly");
    }

    /// `reveal_next_external_script` (U11 sweep destinations, the PWA's
    /// `revealNextAddress`) vends `last_revealed + 1` — the HIGH end, which is
    /// exactly why the observed sweep paid to 5 030 rather than 3. Only the
    /// high-end window covers it: it is not a KTD-4 destination, so nothing
    /// pins it.
    #[test]
    fn a_freshly_revealed_sweep_destination_is_in_the_high_end_window() {
        let (_dir, store) = fresh_store();
        let wallet = test_wallet(store, Network::Bitcoin).unwrap();
        test_support::fund_confirmed(&wallet, 25_000);
        wallet
            .destination_script_for_index(OBSERVED_DESTINATION_INDEX)
            .unwrap();

        let sweep_destination = wallet.reveal_next_external_script().unwrap();
        assert_eq!(
            test_support::derivation_index(&wallet, KeychainKind::External),
            Some(OBSERVED_DESTINATION_INDEX + 1),
            "reveal_next_address always advances past last_revealed"
        );
        assert_ne!(
            sweep_destination,
            wallet.peek_external_script(OBSERVED_DESTINATION_INDEX),
            "the sweep destination is not the pinned KTD-4 destination"
        );
        assert!(
            wallet.sync_request_spks().contains(&sweep_destination),
            "a sweep destination must be watched the moment it is vended"
        );
    }

    /// The non-negotiable safety property, at the point where a window alone is
    /// not enough: an SPK with KNOWN HISTORY at a low index that has already
    /// fallen out of the lowest-unused window is still queried. Missing a
    /// payment permanently is strictly worse than a slow sync.
    #[test]
    fn a_used_low_index_spk_survives_a_high_index_destination() {
        let (_dir, store) = fresh_store();
        let wallet = test_wallet(store, Network::Bitcoin).unwrap();

        // Burn the whole low window: external 0..=24 all have history, so the
        // lowest-UNUSED window is 25..=44 and index 0 is outside every window.
        for i in 0..25u64 {
            test_support::fund_confirmed(&wallet, 10_000 + i);
        }
        wallet
            .destination_script_for_index(OBSERVED_DESTINATION_INDEX)
            .unwrap();

        let spks = wallet.sync_request_spks();
        for used in 0..25u32 {
            assert!(
                spks.contains(&wallet.peek_external_script(used)),
                "used external index {used} must be queried, window or not"
            );
        }
        assert_eq!(
            spks.len(),
            25 + 20 + 20,
            "used SPKs widen the set exactly by their own count"
        );
    }

    /// The FULL SCAN is deliberately untouched: it still walks the descriptors
    /// exhaustively (the client applies `BDK_CLIENT_STOP_GAP` /
    /// `BDK_COLD_RESTORE_STOP_GAP`), because that scan is what DISCOVERS the
    /// history the bounded incremental sync then maintains.
    #[test]
    fn the_full_scan_path_is_unchanged() {
        let (_dir, store) = fresh_store();
        let wallet = test_wallet(store, Network::Bitcoin).unwrap();
        assert!(
            wallet.needs_full_scan(),
            "a wallet that has never seen a block must full-scan"
        );

        let unbounded = {
            let inner = wallet.inner.lock().unwrap();
            let mut request = inner.wallet.start_full_scan().build();
            request.iter_spks(KeychainKind::External).take(500).count()
        };
        assert_eq!(
            unbounded, 500,
            "the full scan's spk iterator is still unbounded by any revealed set"
        );

        // Once a block is known, the bounded incremental path takes over.
        test_support::fund_confirmed(&wallet, 25_000);
        assert!(!wallet.needs_full_scan());
    }

    // ---------- on-chain sync change detection (Fix B) ----------

    /// The refresh event must fire on real news and stay silent otherwise. The
    /// dangerous case is the mempool re-stamp: bdk re-records a pending tx's
    /// `last_seen` with the request's start time on EVERY pass, so a
    /// changeset-is-non-empty test would fire every 120 s for a wallet with one
    /// unconfirmed transaction.
    #[test]
    fn only_new_chain_facts_count_as_an_onchain_change() {
        let (_dir, store) = fresh_store();
        let wallet = test_wallet(store, Network::Bitcoin).unwrap();
        test_support::fund_untrusted_pending(&wallet, 50_000);
        // Every real mutation path persists immediately after staging (the
        // address-reveal learning), so the stage is empty when a sync starts.
        {
            let mut inner = wallet.inner.lock().unwrap();
            let WalletInner { wallet, persister } = &mut *inner;
            wallet.persist(persister).unwrap();
        }
        let txid: bitcoin::Txid = wallet.list_transactions()[0].txid.parse().unwrap();
        let tip = wallet.inner.lock().unwrap().wallet.latest_checkpoint();

        // A quiet tick over a still-pending tx: same tip, a later last-seen.
        let quiet = bdk_wallet::Update {
            last_active_indices: BTreeMap::new(),
            tx_update: {
                let mut tx_update = bdk_wallet::chain::TxUpdate::default();
                tx_update.seen_ats = [(txid, 1_700_001_000)].into_iter().collect();
                tx_update
            },
            chain: Some(tip.clone()),
        };
        assert_eq!(
            wallet.apply_update(quiet),
            Ok(false),
            "a mempool re-stamp is not news; firing here is the 120 s heartbeat bug"
        );

        // The same tx confirming IS news (balance moves from untrusted pending
        // to confirmed).
        let confirmed = bdk_wallet::Update {
            last_active_indices: BTreeMap::new(),
            tx_update: {
                let mut tx_update = bdk_wallet::chain::TxUpdate::default();
                tx_update.anchors = [(
                    ConfirmationBlockTime {
                        block_id: tip.block_id(),
                        confirmation_time: 1_700_002_000,
                    },
                    txid,
                )]
                .into_iter()
                .collect();
                tx_update
            },
            chain: Some(tip),
        };
        assert_eq!(
            wallet.apply_update(confirmed),
            Ok(true),
            "a confirmation must tell the shells to re-query"
        );
    }

    /// The individual discriminations behind [`changes_wallet_data`], stated as
    /// assertions so the exclusions are deliberate rather than accidental.
    #[test]
    fn changes_wallet_data_ignores_tip_and_last_seen_noise() {
        let txid = bitcoin::Txid::from_raw_hash(bitcoin::hashes::Hash::all_zeros());
        let mut staged = tx_graph::ChangeSet::<ConfirmationBlockTime>::default();
        assert!(
            !changes_wallet_data(&staged),
            "an empty pass is not a change"
        );

        staged.last_seen.insert(txid, 1_700_000_000);
        staged.first_seen.insert(txid, 1_700_000_000);
        assert!(
            !changes_wallet_data(&staged),
            "mempool seen-at bookkeeping moves no balance and no activity row"
        );

        let mut evicted = staged.clone();
        evicted.last_evicted.insert(txid, 1_700_000_000);
        assert!(
            changes_wallet_data(&evicted),
            "an eviction removes an activity row"
        );

        staged.txs.insert(std::sync::Arc::new(Transaction {
            version: bitcoin::transaction::Version::ONE,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![],
        }));
        assert!(changes_wallet_data(&staged), "a new transaction is news");
    }
}
