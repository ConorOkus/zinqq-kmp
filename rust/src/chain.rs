//! Chain access: the Esplora-backed transaction sync client (which doubles as
//! the `ChainMonitor`'s `Filter`), the queued transaction broadcaster with
//! persisted pending broadcasts (U12/KTD-9: startup drain + 48 h TTL), the
//! startup genesis-hash network check (U12/KTD-12), the fee-rate cache
//! refresh, and the Rapid Gossip Sync source.

use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bitcoin::consensus::{deserialize, serialize};
use bitcoin::{BlockHash, Script, Transaction, Txid};
use esplora_client::AsyncClient as EsploraAsyncClient;
use lightning::chain::chaininterface::BroadcasterInterface;
use lightning::chain::{Confirm, Filter, WatchedOutput};
use lightning::log_error;
use lightning::log_info;
use lightning::util::logger::Logger as _;
use lightning::util::persist::KVStoreSync;
use lightning_persister::fs_store::FilesystemStore;
use lightning_transaction_sync::EsploraSyncClient;
use tokio::sync::{mpsc, Mutex, MutexGuard};

use crate::config::{
    BDK_CLIENT_CONCURRENCY, BDK_CLIENT_STOP_GAP, BDK_COLD_RESTORE_STOP_GAP, CHAIN_SYNC_TIMEOUT,
    ESPLORA_CLIENT_TIMEOUT_SECS, FEE_UPDATE_TIMEOUT, RGS_SYNC_TIMEOUT, TX_BROADCAST_TIMEOUT,
};
use crate::fees::{cache_from_esplora_estimates, CachedFeeEstimator};
use crate::types::{Graph, Logger, RapidGossipSync};
use crate::util::unix_now;
use crate::wallet::OnchainWallet;

/// Runtime chain-access failures. These are logged and retried by the
/// background sync loop; only start-up turns them into hard errors (and only
/// when channel monitors exist), except [`ChainError::WrongNetworkBackend`],
/// which always fails the start (U12/KTD-12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainError {
    /// The Esplora endpoint could not be reached or returned an error.
    EsploraUnreachable(String),
    /// The Esplora endpoint ANSWERED the genesis-hash probe with a hash that
    /// is not mainnet's: it serves the wrong chain (U12/KTD-12). Never
    /// degraded-start over this — a wrong-chain view is not fund-safe.
    WrongNetworkBackend { got: String },
    /// The endpoint answered, but with unusable fee estimates on mainnet.
    EmptyFeeEstimates,
    /// The RGS server could not be reached or the snapshot failed to apply.
    GossipUpdateFailed(String),
    /// The bdk wallet sync failed to apply or persist.
    WalletSyncFailed(String),
}

impl fmt::Display for ChainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChainError::EsploraUnreachable(e) => write!(f, "esplora unreachable: {e}"),
            ChainError::WrongNetworkBackend { got } => write!(
                f,
                "the esplora backend serves a different network (genesis hash {got})"
            ),
            ChainError::EmptyFeeEstimates => write!(f, "empty fee estimates on mainnet"),
            ChainError::GossipUpdateFailed(e) => write!(f, "gossip update failed: {e}"),
            ChainError::WalletSyncFailed(e) => write!(f, "wallet sync failed: {e}"),
        }
    }
}

impl std::error::Error for ChainError {}

/// KVStore namespace for pending broadcasts (U12/KTD-9), keyed by txid hex.
/// Mirrors the PWA's `ldk_pending_broadcasts` IDB store: persisted before
/// the broadcast attempt, removed on success/already-known, drained at
/// startup, expired after [`PENDING_BROADCAST_TTL`].
pub(crate) const PENDING_BROADCASTS_PRIMARY_NAMESPACE: &str = "pending_broadcasts";
pub(crate) const PENDING_BROADCASTS_SECONDARY_NAMESPACE: &str = "";

/// How long a pending broadcast is retried across restarts before being
/// discarded (inputs likely spent by then) — PWA `PENDING_BROADCAST_TTL_MS`.
pub(crate) const PENDING_BROADCAST_TTL: Duration = Duration::from_secs(48 * 60 * 60);

/// Encodes a pending-broadcast entry: `created_at` UNIX seconds (LE) followed
/// by the consensus-serialized transaction. Pure, for offline tests.
pub(crate) fn encode_pending_broadcast(created_at_secs: u64, tx: &Transaction) -> Vec<u8> {
    let mut bytes = created_at_secs.to_le_bytes().to_vec();
    bytes.extend(serialize(tx));
    bytes
}

/// Decodes [`encode_pending_broadcast`]'s format; `None` on corrupt entries
/// (which are dropped, never retried).
pub(crate) fn decode_pending_broadcast(bytes: &[u8]) -> Option<(u64, Transaction)> {
    let created_at_secs = u64::from_le_bytes(bytes.get(..8)?.try_into().ok()?);
    let tx = deserialize(bytes.get(8..)?).ok()?;
    Some((created_at_secs, tx))
}

/// Persisted pending-broadcast store over the node's KVStore (U12/KTD-9).
/// The [`Broadcaster`] writes entries before queueing; the [`ChainSource`]
/// removes them once the network knows the transaction; the startup drain
/// rebroadcasts survivors and expires stale ones.
pub(crate) struct PendingBroadcasts {
    kv_store: Arc<FilesystemStore>,
    logger: Arc<Logger>,
}

impl PendingBroadcasts {
    pub(crate) fn new(kv_store: Arc<FilesystemStore>, logger: Arc<Logger>) -> Self {
        Self { kv_store, logger }
    }

    /// Persists a transaction pending broadcast. Failures are logged, not
    /// fatal: the live broadcast attempt still proceeds.
    pub(crate) fn persist(&self, tx: &Transaction, created_at_secs: u64) {
        let key = tx.compute_txid().to_string();
        if let Err(e) = self.kv_store.write(
            PENDING_BROADCASTS_PRIMARY_NAMESPACE,
            PENDING_BROADCASTS_SECONDARY_NAMESPACE,
            &key,
            encode_pending_broadcast(created_at_secs, tx),
        ) {
            log_error!(
                self.logger,
                "Failed to persist pending broadcast {key}: {e}"
            );
        }
    }

    /// Removes a pending entry once the network knows the transaction.
    pub(crate) fn remove(&self, txid: &Txid) {
        let key = txid.to_string();
        if let Err(e) = self.kv_store.remove(
            PENDING_BROADCASTS_PRIMARY_NAMESPACE,
            PENDING_BROADCASTS_SECONDARY_NAMESPACE,
            &key,
            false,
        ) {
            log_error!(self.logger, "Failed to remove pending broadcast {key}: {e}");
        }
    }

    /// Loads every entry younger than [`PENDING_BROADCAST_TTL`], removing
    /// expired and corrupt ones (the startup-drain read half).
    pub(crate) fn load_fresh(&self, now_secs: u64) -> Vec<Transaction> {
        let keys = match self.kv_store.list(
            PENDING_BROADCASTS_PRIMARY_NAMESPACE,
            PENDING_BROADCASTS_SECONDARY_NAMESPACE,
        ) {
            Ok(keys) => keys,
            Err(e) => {
                log_error!(self.logger, "Failed to list pending broadcasts: {e}");
                return Vec::new();
            }
        };

        let mut fresh = Vec::new();
        for key in keys {
            let bytes = match self.kv_store.read(
                PENDING_BROADCASTS_PRIMARY_NAMESPACE,
                PENDING_BROADCASTS_SECONDARY_NAMESPACE,
                &key,
            ) {
                Ok(bytes) => bytes,
                Err(e) => {
                    log_error!(self.logger, "Failed to read pending broadcast {key}: {e}");
                    continue;
                }
            };
            match decode_pending_broadcast(&bytes) {
                Some((created_at_secs, tx))
                    if now_secs.saturating_sub(created_at_secs)
                        <= PENDING_BROADCAST_TTL.as_secs() =>
                {
                    fresh.push(tx);
                }
                Some(_) | None => {
                    // Expired (inputs likely spent) or corrupt: discard.
                    if let Err(e) = self.kv_store.remove(
                        PENDING_BROADCASTS_PRIMARY_NAMESPACE,
                        PENDING_BROADCASTS_SECONDARY_NAMESPACE,
                        &key,
                        false,
                    ) {
                        log_error!(
                            self.logger,
                            "Failed to discard stale pending broadcast {key}: {e}"
                        );
                    }
                }
            }
        }
        fresh
    }

    #[cfg(test)]
    pub(crate) fn pending_txids(&self) -> Vec<String> {
        self.kv_store
            .list(
                PENDING_BROADCASTS_PRIMARY_NAMESPACE,
                PENDING_BROADCASTS_SECONDARY_NAMESPACE,
            )
            .unwrap_or_default()
    }
}

/// Per-transaction broadcast outcome (U12/KTD-9): "already known" is a
/// distinguishable success sentinel, not a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BroadcastOutcome {
    /// The endpoint accepted the transaction.
    Accepted,
    /// The mempool/chain already knows the transaction — success for our
    /// purposes (the pending entry is cleared either way).
    AlreadyKnown,
    /// The broadcast failed; the pending entry is kept for retry.
    Failed(String),
}

impl BroadcastOutcome {
    pub(crate) fn is_success(&self) -> bool {
        matches!(
            self,
            BroadcastOutcome::Accepted | BroadcastOutcome::AlreadyKnown
        )
    }
}

const BCAST_PACKAGE_QUEUE_SIZE: usize = 50;

/// `BroadcasterInterface` that queues packages for async broadcast via
/// Esplora. LDK's broadcast call sites are sync; the queue decouples them
/// from HTTP. Every transaction is persisted to the pending-broadcast store
/// BEFORE it is queued (U12/KTD-9 crash safety: a crash mid-broadcast is
/// redelivered by the startup drain).
pub(crate) struct Broadcaster {
    queue_sender: mpsc::Sender<Vec<Transaction>>,
    queue_receiver: Mutex<mpsc::Receiver<Vec<Transaction>>>,
    pending: Arc<PendingBroadcasts>,
    logger: Arc<Logger>,
}

impl Broadcaster {
    pub(crate) fn new(pending: Arc<PendingBroadcasts>, logger: Arc<Logger>) -> Self {
        let (queue_sender, queue_receiver) = mpsc::channel(BCAST_PACKAGE_QUEUE_SIZE);
        Self {
            queue_sender,
            queue_receiver: Mutex::new(queue_receiver),
            pending,
            logger,
        }
    }

    pub(crate) async fn queue(&self) -> MutexGuard<'_, mpsc::Receiver<Vec<Transaction>>> {
        self.queue_receiver.lock().await
    }
}

impl BroadcasterInterface for Broadcaster {
    fn broadcast_transactions(&self, txs: &[&Transaction]) {
        let package = txs.iter().map(|&tx| tx.clone()).collect::<Vec<_>>();
        let now_secs = unix_now().as_secs();
        for tx in &package {
            self.pending.persist(tx, now_secs);
        }
        if let Err(e) = self.queue_sender.try_send(package) {
            log_error!(
                self.logger,
                "Failed to queue transactions for broadcast: {e}"
            );
        }
    }
}

/// Whether a failed broadcast is benign because the network already knows the
/// transaction (or one resolving the same outputs/inputs). Pure so it is
/// unit-testable; matches bitcoind's `sendrawtransaction` error strings that
/// Esplora relays in HTTP 400 bodies — the PWA's full sentinel list
/// (`broadcaster.ts:33-49`, U11/KTD-9):
///
/// - the already-known family (mempool / block chain / known / confirmed);
/// - "insufficient fee, rejecting replacement" — an equivalent tx already
///   rides the mempool and ours cannot RBF it;
/// - RPC `-27` / "outputs already in utxo set" — the tx (or one with the same
///   outputs) is on chain; after a successful CPFP'd force close LDK keeps
///   re-issuing the confirmed commitment + anchor child for a while;
/// - RPC `-25` / "bad-txns-inputs-missingorspent" — for a persisted pending
///   broadcast this nearly always means the tx (or a conflicting one over the
///   same inputs) already confirmed; retrying can't do better. The sweep
///   pipeline treats this as a SENTINEL, not proof: shared-input (subsidized)
///   txs verify against chain truth before any descriptor is deleted
///   (sweep.rs — a concurrently spent wallet input produces the same error).
pub(crate) fn broadcast_error_is_benign(message: &str) -> bool {
    // Normalize case and bitcoind's hyphenated reject codes
    // ("txn-already-in-mempool") to plain words before matching. The raw
    // (lowercased) message is kept for the numeric RPC codes, whose minus
    // sign must not be eaten by the hyphen normalization.
    let raw = message.to_lowercase();
    let message = raw.replace('-', " ");
    [
        "already in mempool",
        "already in the mempool",
        "already in block chain",
        "already known",
        "already confirmed",
        "insufficient fee, rejecting replacement",
        "outputs already in utxo set",
        "bad txns inputs missingorspent",
    ]
    .iter()
    .any(|benign| message.contains(benign))
        || raw.contains("-25")
        || raw.contains("-27")
}

// ---------------------------------------------------------------------------
// Fee-sanity middleware (U11, adopted from the incident review)
// ---------------------------------------------------------------------------

/// The fee-sanity ceiling: no self-built broadcast may exceed this multiple
/// of a fresh 3-block estimate (U11 — a real incident overpaid ~30x when the
/// urgent sweep target answered a 1-block panic rate).
pub(crate) const FEE_SANITY_MULTIPLIER: u32 = 5;

/// Typed fee-sanity refusal (distinct `Display` per convention). The caller
/// must NOT broadcast; sweep paths surface it as a failed-attempt state
/// change, the CPFP path skips the bump (LDK re-yields next block).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FeeSanityError {
    /// The transaction's effective rate exceeds 5x a fresh 3-block estimate.
    Overpay {
        effective_sat_per_kw: u64,
        max_sat_per_kw: u64,
    },
}

impl fmt::Display for FeeSanityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FeeSanityError::Overpay {
                effective_sat_per_kw,
                max_sat_per_kw,
            } => write!(
                f,
                "fee-sanity refusal: effective rate {effective_sat_per_kw} sat/kW exceeds the \
                 5x-of-3-block ceiling {max_sat_per_kw} sat/kW"
            ),
        }
    }
}

impl std::error::Error for FeeSanityError {}

/// The current fee-sanity ceiling in sat/kW: 5x the estimator's fresh
/// 3-block answer ([`ConfirmationTarget::UrgentOnChainSweep`] — KTD-9 pins
/// that variant to a 3-block target; `fees.rs` has the table test).
pub(crate) fn fee_sanity_max_sat_per_kw(fee_estimator: &CachedFeeEstimator) -> u64 {
    use lightning::chain::chaininterface::{ConfirmationTarget, FeeEstimator as _};
    u64::from(fee_estimator.get_est_sat_per_1000_weight(ConfirmationTarget::UrgentOnChainSweep))
        * u64::from(FEE_SANITY_MULTIPLIER)
}

/// Refuses any transaction whose effective rate (`fee / weight`) exceeds
/// `max_sat_per_kw` (see [`fee_sanity_max_sat_per_kw`]). Pure, applied where
/// the fee IS computable — our own built txs: the sweep, subsidized-sweep,
/// and CPFP paths (LDK-relayed txs like commitments carry no input values
/// here and pre-commit their fees channel-side).
pub(crate) fn check_fee_sanity(
    fee_sats: u64,
    tx_weight_wu: u64,
    max_sat_per_kw: u64,
) -> Result<(), FeeSanityError> {
    if tx_weight_wu == 0 {
        return Ok(());
    }
    let effective_sat_per_kw = fee_sats.saturating_mul(1000) / tx_weight_wu;
    if effective_sat_per_kw > max_sat_per_kw {
        return Err(FeeSanityError::Overpay {
            effective_sat_per_kw,
            max_sat_per_kw,
        });
    }
    Ok(())
}

/// U10: the reconcile pass's chain queries go through the ONE configured
/// (first-party) Esplora client — never a fallback: recurring outspend
/// polling of channel outpoints through a third party would leak the user's
/// IP + entire channel set (PWA `reconcile.ts:56-59`).
impl crate::close_records::ChainTruth for ChainSource {
    fn tip_height(&self) -> crate::vss::store::BoxFuture<'_, Result<u32, String>> {
        Box::pin(async move {
            ChainSource::tip_height(self)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn outspend<'a>(
        &'a self,
        txid: &'a str,
        vout: u32,
    ) -> crate::vss::store::BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(async move {
            let txid: Txid = txid.parse().map_err(|e| format!("bad txid: {e}"))?;
            self.output_spender(&txid, vout)
                .await
                .map(|spender| spender.map(|txid| txid.to_string()))
                .map_err(|e| e.to_string())
        })
    }

    fn tx_confirmed_height<'a>(
        &'a self,
        txid: &'a str,
    ) -> crate::vss::store::BoxFuture<'a, Result<Option<u32>, String>> {
        Box::pin(async move {
            let txid: Txid = txid.parse().map_err(|e| format!("bad txid: {e}"))?;
            self.tx_confirmation_height(&txid)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn full_tx<'a>(
        &'a self,
        txid: &'a str,
    ) -> crate::vss::store::BoxFuture<'a, Result<Option<Transaction>, String>> {
        Box::pin(async move {
            let txid: Txid = txid.parse().map_err(|e| format!("bad txid: {e}"))?;
            self.transaction_by_txid(&txid)
                .await
                .map_err(|e| e.to_string())
        })
    }

    fn outpoint_spent<'a>(
        &'a self,
        txid: &'a str,
        vout: u32,
    ) -> crate::vss::store::BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let txid: Txid = txid.parse().map_err(|e| format!("bad txid: {e}"))?;
            self.outpoint_is_spent(&txid, vout)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

/// Esplora-backed chain source. `tx_sync` implements LDK's `Filter` and
/// `Confirm`-driven sync; the shared `esplora_client` also serves the bdk
/// wallet, fee estimates, and broadcasts (one esplora-client 0.12 stack).
pub(crate) struct ChainSource {
    esplora_client: EsploraAsyncClient,
    tx_sync: Arc<EsploraSyncClient<Arc<Logger>>>,
    /// Base URL, trailing slash trimmed. Kept so [`Self::transaction_by_txid`]
    /// can reach `/tx/:txid/hex` directly — see that method for why the
    /// esplora-client helper is unusable here.
    esplora_url: String,
    /// Shared with nothing else; `reqwest::Client` is internally an Arc and
    /// pools connections, so one per chain source is the intended shape.
    http: reqwest::Client,
    fee_estimator: Arc<CachedFeeEstimator>,
    pending_broadcasts: Arc<PendingBroadcasts>,
    network: bitcoin::Network,
    logger: Arc<Logger>,
}

impl ChainSource {
    pub(crate) fn new(
        esplora_url: &str,
        network: bitcoin::Network,
        fee_estimator: Arc<CachedFeeEstimator>,
        pending_broadcasts: Arc<PendingBroadcasts>,
        logger: Arc<Logger>,
    ) -> Result<Self, esplora_client::Error> {
        let esplora_client = esplora_client::Builder::new(esplora_url)
            .timeout(ESPLORA_CLIENT_TIMEOUT_SECS)
            .build_async()?;
        let tx_sync = Arc::new(EsploraSyncClient::from_client(
            esplora_client.clone(),
            Arc::clone(&logger),
        ));
        Ok(Self {
            esplora_client,
            tx_sync,
            esplora_url: esplora_url.trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
            fee_estimator,
            pending_broadcasts,
            network,
            logger,
        })
    }

    /// U12/KTD-12 startup network check: asks the backend for block 0's hash
    /// (`/block-height/0`) and compares it to `expected`.
    ///
    /// Only an ANSWERED probe with the wrong hash is
    /// [`ChainError::WrongNetworkBackend`] (a hard start error); an
    /// unreachable or garbled endpoint is [`ChainError::EsploraUnreachable`],
    /// which preserves the fresh-node degraded start.
    pub(crate) async fn check_genesis_hash(&self, expected: BlockHash) -> Result<(), ChainError> {
        let got = tokio::time::timeout(FEE_UPDATE_TIMEOUT, self.esplora_client.get_block_hash(0))
            .await
            .map_err(|e| ChainError::EsploraUnreachable(format!("genesis probe timed out: {e}")))?
            .map_err(|e| ChainError::EsploraUnreachable(e.to_string()))?;
        if got == expected {
            Ok(())
        } else {
            Err(ChainError::WrongNetworkBackend {
                got: got.to_string(),
            })
        }
    }

    /// Sync the given `Confirm` listeners (channel manager, chain monitor,
    /// sweeper, and pre-`watch_channel` monitors on restore) to chain tip.
    pub(crate) async fn sync_confirmables(
        &self,
        confirmables: Vec<&(dyn Confirm + Sync + Send)>,
    ) -> Result<(), ChainError> {
        tokio::time::timeout(CHAIN_SYNC_TIMEOUT, self.tx_sync.sync(confirmables))
            .await
            .map_err(|e| ChainError::EsploraUnreachable(format!("sync timed out: {e}")))?
            .map_err(|e| ChainError::EsploraUnreachable(e.to_string()))
    }

    /// Sync the on-chain bdk wallet (separate from the lightning sync path,
    /// sharing the same esplora client).
    pub(crate) async fn sync_onchain_wallet(
        &self,
        wallet: &OnchainWallet,
    ) -> Result<(), ChainError> {
        // The stop gap only ever governs a FULL scan (the revealed-SPK
        // incremental sync has none), and a wallet only full-scans once — so
        // this choice is exactly "how wide is the first scan". A restore /
        // silent recovery starts from an empty changeset over another client's
        // address history and needs the wider gap; a wallet this device created
        // keeps the cheap steady-state value.
        let stop_gap = if wallet.is_cold_restore() {
            BDK_COLD_RESTORE_STOP_GAP
        } else {
            BDK_CLIENT_STOP_GAP
        };
        wallet
            .sync(&self.esplora_client, stop_gap, BDK_CLIENT_CONCURRENCY)
            .await
    }

    /// Whether the fee cache is due a refresh (60 s TTL / 15 s failure
    /// backoff, U12/KTD-9); polled by the node's background tick.
    pub(crate) fn fee_refresh_due(&self) -> bool {
        self.fee_estimator.needs_refresh()
    }

    /// Refresh the fee-rate cache from the Esplora fee-estimates endpoint,
    /// recording success/failure for the TTL/backoff policy.
    pub(crate) async fn update_fee_rate_estimates(&self) -> Result<(), ChainError> {
        let result = self.fetch_fee_estimates().await;
        match &result {
            Ok(()) => {}
            Err(_) => self.fee_estimator.record_failure(),
        }
        result
    }

    async fn fetch_fee_estimates(&self) -> Result<(), ChainError> {
        let estimates =
            tokio::time::timeout(FEE_UPDATE_TIMEOUT, self.esplora_client.get_fee_estimates())
                .await
                .map_err(|e| {
                    ChainError::EsploraUnreachable(format!("fee estimates timed out: {e}"))
                })?
                .map_err(|e| ChainError::EsploraUnreachable(e.to_string()))?;

        if estimates.is_empty() && self.network == bitcoin::Network::Bitcoin {
            return Err(ChainError::EmptyFeeEstimates);
        }

        self.fee_estimator
            .set_cache(cache_from_esplora_estimates(&estimates));
        // U8: the raw 6-block rate rides along for the on-chain send path.
        self.fee_estimator.set_onchain_send_rate(
            crate::fees::onchain_send_sat_per_vb_from_estimates(&estimates),
        );
        Ok(())
    }

    /// The U8 on-chain send fee rate (6-block target, ceil'd, clamped >= 2
    /// sat/vB — KTD-9); answered from the cache, never the network.
    pub(crate) fn onchain_send_fee_rate_sat_per_vb(&self) -> u64 {
        self.fee_estimator.onchain_send_rate_sat_per_vb()
    }

    /// The shared cached fee estimator (U9's close estimates read LDK
    /// confirmation targets from it directly).
    pub(crate) fn fee_estimator(&self) -> Arc<CachedFeeEstimator> {
        Arc::clone(&self.fee_estimator)
    }

    /// The raw txid hex of the transaction spending `txid:vout`, if any
    /// (U10 reconcile step (a); Esplora reports unconfirmed spends too).
    pub(crate) async fn output_spender(
        &self,
        txid: &Txid,
        vout: u32,
    ) -> Result<Option<Txid>, ChainError> {
        let status = self
            .esplora_client
            .get_output_status(txid, u64::from(vout))
            .await
            .map_err(|e| ChainError::EsploraUnreachable(e.to_string()))?;
        Ok(status.and_then(|status| if status.spent { status.txid } else { None }))
    }

    /// The confirmation height of `txid`, `None` while unconfirmed (U10
    /// reconcile step (b)).
    pub(crate) async fn tx_confirmation_height(
        &self,
        txid: &Txid,
    ) -> Result<Option<u32>, ChainError> {
        let status = self
            .esplora_client
            .get_tx_status(txid)
            .await
            .map_err(|e| ChainError::EsploraUnreachable(e.to_string()))?;
        Ok(if status.confirmed {
            status.block_height
        } else {
            None
        })
    }

    /// Whether `txid:vout` is spent, straight off Esplora's `spent` flag
    /// (mempool spends included) — the missed-descriptor replay's already-spent
    /// guard ([`crate::replay`]). Distinct from [`ChainSource::output_spender`],
    /// which answers "which tx spent it" and cannot distinguish "spent, spender
    /// not reported" from "unspent"; here that difference decides whether an
    /// already-swept output gets re-tracked into a sweep that can never
    /// succeed. A missing outspend record is an ERROR, not a `false`: the
    /// caller must skip, never track.
    pub(crate) async fn outpoint_is_spent(
        &self,
        txid: &Txid,
        vout: u32,
    ) -> Result<bool, ChainError> {
        self.esplora_client
            .get_output_status(txid, u64::from(vout))
            .await
            .map_err(|e| ChainError::EsploraUnreachable(e.to_string()))?
            .map(|status| status.spent)
            .ok_or_else(|| {
                ChainError::EsploraUnreachable(format!("no outspend record for {txid}:{vout}"))
            })
    }

    /// The full transaction body for `txid`, `None` when the backend does not
    /// know it. Used by the missed-descriptor replay pass ([`crate::replay`]):
    /// `ChannelMonitor::get_spendable_outputs` scans a transaction's outputs,
    /// so the txid alone is not enough there.
    /// Fetches a full transaction over `/tx/:txid/hex`.
    ///
    /// NOT `esplora_client::get_tx`, which reads `/tx/:txid/raw`. That endpoint
    /// serves a raw BINARY body, and the first-party backend
    /// ([`crate::config::DEFAULT_ESPLORA_URL`]) mangles it: every byte >= 0x80
    /// comes back UTF-8 re-encoded as two bytes, so a 443-byte transaction
    /// arrives as 773 bytes and consensus decoding fails with
    /// `UnexpectedEof`. `/tx/:txid/hex` is the same standard Esplora API, is
    /// pure ASCII, and is therefore immune to that corruption — verified
    /// byte-identical to another backend's `/raw` for the same txid.
    ///
    /// Hex is the right primary for every backend, not a workaround for one:
    /// it costs 2x the bytes of a small body and removes a whole class of
    /// proxy-transparency assumptions.
    pub(crate) async fn transaction_by_txid(
        &self,
        txid: &Txid,
    ) -> Result<Option<Transaction>, ChainError> {
        let url = format!("{}/tx/{txid}/hex", self.esplora_url);
        let response = self
            .http
            .get(&url)
            .timeout(std::time::Duration::from_secs(ESPLORA_CLIENT_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|e| ChainError::EsploraUnreachable(e.to_string()))?;
        // A txid the backend does not know is absence, not failure — the
        // caller decides whether that is benign (a pruned or never-confirmed
        // tx) or fatal.
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(ChainError::EsploraUnreachable(format!(
                "GET /tx/{txid}/hex returned {}",
                response.status()
            )));
        }
        let hex = response
            .text()
            .await
            .map_err(|e| ChainError::EsploraUnreachable(e.to_string()))?;
        let bytes = <Vec<u8> as bitcoin::hex::FromHex>::from_hex(hex.trim())
            .map_err(|e| ChainError::EsploraUnreachable(format!("malformed tx hex: {e}")))?;
        let tx: Transaction = bitcoin::consensus::deserialize(&bytes)
            .map_err(|e| ChainError::EsploraUnreachable(format!("undecodable tx: {e}")))?;
        // Cheap integrity check: a backend that returned the wrong body would
        // otherwise feed a foreign transaction into monitor replay.
        if tx.compute_txid() != *txid {
            return Err(ChainError::EsploraUnreachable(format!(
                "backend returned {} for requested {txid}",
                tx.compute_txid()
            )));
        }
        Ok(Some(tx))
    }

    /// Current tip height (U10 reconcile).
    pub(crate) async fn tip_height(&self) -> Result<u32, ChainError> {
        self.esplora_client
            .get_height()
            .await
            .map_err(|e| ChainError::EsploraUnreachable(e.to_string()))
    }

    /// Broadcasts one transaction, mapping "already known" responses to the
    /// [`BroadcastOutcome::AlreadyKnown`] success sentinel (U12/KTD-9). On
    /// either success the pending-broadcast entry is cleared; on failure it
    /// stays for the next startup drain.
    pub(crate) async fn broadcast_transaction(&self, tx: &Transaction) -> BroadcastOutcome {
        let txid = tx.compute_txid();
        let res =
            tokio::time::timeout(TX_BROADCAST_TIMEOUT, self.esplora_client.broadcast(tx)).await;
        let outcome = match res {
            Ok(Ok(())) => BroadcastOutcome::Accepted,
            Ok(Err(esplora_client::Error::HttpResponse { status, message }))
                if broadcast_error_is_benign(&message) =>
            {
                // The mempool/chain already knows this transaction; that is
                // success for our purposes, not a failure (status is
                // typically 400 here).
                let _ = status;
                BroadcastOutcome::AlreadyKnown
            }
            Ok(Err(e)) => BroadcastOutcome::Failed(e.to_string()),
            Err(e) => BroadcastOutcome::Failed(format!("timed out: {e}")),
        };
        if outcome.is_success() {
            self.pending_broadcasts.remove(&txid);
        } else if let BroadcastOutcome::Failed(e) = &outcome {
            log_error!(self.logger, "Failed to broadcast transaction {txid}: {e}");
        }
        outcome
    }

    /// Persists a transaction to the pending-broadcast store ahead of an
    /// out-of-queue broadcast (the sweep engine's direct path — U11): a crash
    /// between build and broadcast is redelivered by the startup drain.
    pub(crate) fn persist_pending_broadcast(&self, tx: &Transaction) {
        self.pending_broadcasts.persist(tx, unix_now().as_secs());
    }

    /// Whether the chain view KNOWS `txid` (mempool or confirmed) — the
    /// sweep engine's sentinel verification (U11/KTD-8, PWA
    /// `subsidized-sweep.ts:213-230`): an unreachable Esplora reads as
    /// "unknown", never as proof.
    pub(crate) async fn tx_known_to_chain(&self, txid: &Txid) -> bool {
        matches!(self.esplora_client.get_tx(txid).await, Ok(Some(_)))
    }

    /// Broadcast one queued package, tolerating already-known transactions.
    pub(crate) async fn process_broadcast_package(&self, package: Vec<Transaction>) {
        for tx in &package {
            let _ = self.broadcast_transaction(tx).await;
        }
    }

    /// Startup drain (U12/KTD-9): rebroadcasts every persisted pending
    /// transaction younger than the 48 h TTL (a crash mid-broadcast must not
    /// lose a force-close tx), expiring the rest. Failures are tolerated —
    /// the entries stay persisted for the next start.
    pub(crate) async fn drain_pending_broadcasts(&self, now_secs: u64) {
        let pending = self.pending_broadcasts.load_fresh(now_secs);
        if pending.is_empty() {
            return;
        }
        log_info!(
            self.logger,
            "Draining {} pending broadcast(s) from a previous run",
            pending.len()
        );
        for tx in &pending {
            let _ = self.broadcast_transaction(tx).await;
        }
    }
}

impl Filter for ChainSource {
    fn register_tx(&self, txid: &Txid, script_pubkey: &Script) {
        self.tx_sync.register_tx(txid, script_pubkey);
    }

    fn register_output(&self, output: WatchedOutput) {
        self.tx_sync.register_output(output);
    }
}

/// Rapid Gossip Sync source (KTD-6): downloads snapshots from the configured
/// server and applies them to the network graph.
pub(crate) struct GossipSource {
    gossip_sync: Arc<RapidGossipSync>,
    server_url: String,
    latest_sync_timestamp: AtomicU32,
    logger: Arc<Logger>,
}

impl GossipSource {
    pub(crate) fn new(server_url: String, network_graph: Arc<Graph>, logger: Arc<Logger>) -> Self {
        let gossip_sync = Arc::new(RapidGossipSync::new(network_graph, Arc::clone(&logger)));
        Self {
            gossip_sync,
            server_url,
            latest_sync_timestamp: AtomicU32::new(0),
            logger,
        }
    }

    pub(crate) fn gossip_sync(&self) -> Arc<RapidGossipSync> {
        Arc::clone(&self.gossip_sync)
    }

    pub(crate) async fn update_rgs_snapshot(&self) -> Result<u32, ChainError> {
        let query_timestamp = self.latest_sync_timestamp.load(Ordering::Acquire);
        let query_url = format!(
            "{}/{}",
            self.server_url.trim_end_matches('/'),
            query_timestamp
        );

        let response = tokio::time::timeout(RGS_SYNC_TIMEOUT, reqwest::get(query_url))
            .await
            .map_err(|e| ChainError::GossipUpdateFailed(format!("timed out: {e}")))?
            .map_err(|e| ChainError::GossipUpdateFailed(e.to_string()))?
            .error_for_status()
            .map_err(|e| ChainError::GossipUpdateFailed(e.to_string()))?;

        let update_data = response
            .bytes()
            .await
            .map_err(|e| ChainError::GossipUpdateFailed(e.to_string()))?;

        let new_timestamp = self
            .gossip_sync
            .update_network_graph(&update_data)
            .map_err(|e| {
                log_error!(self.logger, "Failed to apply RGS snapshot: {e:?}");
                ChainError::GossipUpdateFailed(format!("{e:?}"))
            })?;
        self.latest_sync_timestamp
            .store(new_timestamp, Ordering::Release);
        Ok(new_timestamp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// U11 guard (KTD-9, the historic fund-burn class): the broadcaster maps
    /// the FULL PWA sentinel list (`broadcaster.ts:33-49`) to success — the
    /// already-known family, RPC `-27` (outputs already in UTXO set), and RPC
    /// `-25` (inputs missing or spent: for a persisted pending broadcast this
    /// nearly always means the tx, or a conflict over the same inputs,
    /// already confirmed). The sweep path additionally verifies sentinel
    /// outcomes against chain truth before deleting descriptors (sweep.rs).
    #[test]
    fn already_known_broadcast_errors_are_benign() {
        for message in [
            "sendrawtransaction RPC error: {\"code\":-27,\"message\":\"Transaction already in block chain\"}",
            "txn-already-in-mempool",
            "Transaction already in the mempool",
            "TXN-ALREADY-KNOWN",
            "txn-already-confirmed",
            "insufficient fee, rejecting replacement",
            "sendrawtransaction RPC error: {\"code\":-27,\"message\":\"Outputs already in UTXO set\"}",
            "RPC error -27",
            "sendrawtransaction RPC error: {\"code\":-25,\"message\":\"bad-txns-inputs-missingorspent\"}",
            "bad-txns-inputs-missingorspent",
            "RPC error -25",
        ] {
            assert!(
                broadcast_error_is_benign(message),
                "expected benign: {message}"
            );
        }
    }

    #[test]
    fn real_broadcast_failures_are_not_benign() {
        for message in [
            "sendrawtransaction RPC error: {\"code\":-26,\"message\":\"min relay fee not met\"}",
            "dust",
            "mempool full",
            "",
        ] {
            assert!(
                !broadcast_error_is_benign(message),
                "expected fatal: {message}"
            );
        }
    }

    // ---------- fee-sanity middleware (U11) ----------

    /// U11 guard (the ~30x overpay incident): a transaction priced at 30x
    /// the fresh 3-block estimate is REFUSED with the typed error; a sanely
    /// priced one passes. The ceiling reads the estimator's
    /// `UrgentOnChainSweep` slot, which KTD-9 pins to a 3-block target
    /// (`fees.rs::fee_table_matches_pwa_floors_and_targets`).
    #[test]
    fn fee_sanity_blocks_a_30x_overpay_and_passes_sane_fees() {
        use std::collections::HashMap;
        let estimator = CachedFeeEstimator::new();
        // 3-block estimate: 100 sat/vB -> 25_000 sat/kW.
        let estimates: HashMap<u16, f64> = [(1u16, 400.0), (3u16, 100.0), (6u16, 50.0)]
            .into_iter()
            .collect();
        estimator.set_cache(cache_from_esplora_estimates(&estimates));
        let max = fee_sanity_max_sat_per_kw(&estimator);
        assert_eq!(max, 125_000, "5x the fresh 3-block estimate");

        // 30x overpay fixture: a 1000-wu tx paying 750_000 sats/kW-worth of
        // fee (30 x 25_000) must be blocked...
        let weight_wu = 1_000u64;
        let overpay_fee = 750_000u64 * weight_wu / 1000;
        assert_eq!(
            check_fee_sanity(overpay_fee, weight_wu, max),
            Err(FeeSanityError::Overpay {
                effective_sat_per_kw: 750_000,
                max_sat_per_kw: 125_000,
            })
        );
        // ...the typed error stays distinguishable in rendered form.
        let err = check_fee_sanity(overpay_fee, weight_wu, max).unwrap_err();
        assert!(err.to_string().contains("fee-sanity refusal"));

        // A tx at exactly the 3-block rate passes (25_000 sats over 1000
        // wu), as does one at the 5x boundary; one sat over is refused.
        assert_eq!(check_fee_sanity(25_000, weight_wu, max), Ok(()));
        assert_eq!(check_fee_sanity(125_000, weight_wu, max), Ok(()));
        assert!(check_fee_sanity(125_001, weight_wu, max).is_err());
    }

    #[test]
    fn fee_sanity_tolerates_a_zero_weight_probe() {
        // Degenerate input must not divide by zero or block: no weight means
        // no computable rate, and "not computable" never refuses (the
        // middleware only applies where the rate IS computable).
        assert_eq!(check_fee_sanity(1_000, 0, 1), Ok(()));
    }

    // ---------- pending broadcasts (U12/KTD-9) ----------

    fn dummy_tx(lock_time: u32) -> Transaction {
        Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::from_consensus(lock_time),
            input: Vec::new(),
            output: Vec::new(),
        }
    }

    fn pending_store(dir: &std::path::Path) -> Arc<PendingBroadcasts> {
        let kv_store = Arc::new(FilesystemStore::new(dir.join("store")));
        Arc::new(PendingBroadcasts::new(kv_store, Arc::new(Logger)))
    }

    #[test]
    fn pending_broadcast_encoding_round_trips() {
        let tx = dummy_tx(7);
        let bytes = encode_pending_broadcast(1_700_000_000, &tx);
        let (created_at, decoded) = decode_pending_broadcast(&bytes).unwrap();
        assert_eq!(created_at, 1_700_000_000);
        assert_eq!(decoded, tx);
        // Corrupt entries decode to None (dropped, never retried).
        assert!(decode_pending_broadcast(&bytes[..7]).is_none());
        assert!(decode_pending_broadcast(&bytes[..12]).is_none());
    }

    #[test]
    fn pending_broadcasts_persist_and_expire_after_48_hours() {
        let dir = tempfile::tempdir().unwrap();
        let pending = pending_store(dir.path());
        let now = 1_700_000_000u64;

        let fresh = dummy_tx(1);
        let stale = dummy_tx(2);
        pending.persist(&fresh, now - PENDING_BROADCAST_TTL.as_secs() + 60);
        pending.persist(&stale, now - PENDING_BROADCAST_TTL.as_secs() - 60);
        assert_eq!(pending.pending_txids().len(), 2);

        let survivors = pending.load_fresh(now);
        assert_eq!(survivors, vec![fresh.clone()], "only the fresh tx survives");
        assert_eq!(
            pending.pending_txids(),
            vec![fresh.compute_txid().to_string()],
            "the expired entry is discarded from the store"
        );

        // Explicit removal (the success path) clears the entry.
        pending.remove(&fresh.compute_txid());
        assert!(pending.pending_txids().is_empty());
    }

    /// The startup drain over an unreachable Esplora keeps fresh entries (a
    /// failed rebroadcast must survive to the next start) while still
    /// expiring stale ones.
    #[test]
    fn startup_drain_keeps_failed_rebroadcasts_and_expires_stale_ones() {
        let dir = tempfile::tempdir().unwrap();
        let kv_store = Arc::new(FilesystemStore::new(dir.path().join("store")));
        let logger = Arc::new(Logger);
        let pending = Arc::new(PendingBroadcasts::new(
            Arc::clone(&kv_store),
            Arc::clone(&logger),
        ));
        let chain_source = ChainSource::new(
            "http://127.0.0.1:1",
            bitcoin::Network::Bitcoin,
            Arc::new(CachedFeeEstimator::new()),
            Arc::clone(&pending),
            logger,
        )
        .unwrap();

        let now = 1_700_000_000u64;
        let fresh = dummy_tx(1);
        let stale = dummy_tx(2);
        pending.persist(&fresh, now - 60);
        pending.persist(&stale, now - PENDING_BROADCAST_TTL.as_secs() - 60);

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(chain_source.drain_pending_broadcasts(now));

        assert_eq!(
            pending.pending_txids(),
            vec![fresh.compute_txid().to_string()],
            "the fresh entry survives a failed rebroadcast; the stale one is expired"
        );
    }

    /// The LDK-facing `Broadcaster` persists every transaction BEFORE it is
    /// queued for HTTP, so a crash mid-broadcast is redelivered by the
    /// startup drain.
    #[test]
    fn broadcaster_persists_transactions_before_queueing() {
        let dir = tempfile::tempdir().unwrap();
        let pending = pending_store(dir.path());
        let broadcaster = Broadcaster::new(Arc::clone(&pending), Arc::new(Logger));

        let tx = dummy_tx(9);
        broadcaster.broadcast_transactions(&[&tx]);
        assert_eq!(
            pending.pending_txids(),
            vec![tx.compute_txid().to_string()],
            "the pending entry must exist as soon as LDK hands us the tx"
        );
    }

    /// A failed broadcast returns the Failed outcome and keeps the persisted
    /// entry; the sentinel mapping (Accepted/AlreadyKnown) is exercised by
    /// `broadcast_error_is_benign` above and the success-path removal in
    /// `broadcast_transaction`.
    #[test]
    fn failed_broadcast_returns_failed_outcome_and_keeps_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let kv_store = Arc::new(FilesystemStore::new(dir.path().join("store")));
        let logger = Arc::new(Logger);
        let pending = Arc::new(PendingBroadcasts::new(
            Arc::clone(&kv_store),
            Arc::clone(&logger),
        ));
        let chain_source = ChainSource::new(
            "http://127.0.0.1:1",
            bitcoin::Network::Bitcoin,
            Arc::new(CachedFeeEstimator::new()),
            Arc::clone(&pending),
            logger,
        )
        .unwrap();

        let tx = dummy_tx(3);
        pending.persist(&tx, 1_700_000_000);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = rt.block_on(chain_source.broadcast_transaction(&tx));
        assert!(matches!(outcome, BroadcastOutcome::Failed(_)));
        assert!(!outcome.is_success());
        assert_eq!(pending.pending_txids().len(), 1);
    }
}
