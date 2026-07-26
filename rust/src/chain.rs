//! Chain access: the Esplora-backed transaction sync client (which doubles as
//! the `ChainMonitor`'s `Filter`), the queued transaction broadcaster, the
//! fee-rate cache refresh, and the Rapid Gossip Sync source.

use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use bitcoin::{Script, Transaction, Txid};
use esplora_client::AsyncClient as EsploraAsyncClient;
use lightning::chain::chaininterface::BroadcasterInterface;
use lightning::chain::{Confirm, Filter, WatchedOutput};
use lightning::log_error;
use lightning::util::logger::Logger as _;
use lightning_transaction_sync::EsploraSyncClient;
use tokio::sync::{mpsc, Mutex, MutexGuard};

use crate::config::{
    BDK_CLIENT_CONCURRENCY, BDK_CLIENT_STOP_GAP, CHAIN_SYNC_TIMEOUT, ESPLORA_CLIENT_TIMEOUT_SECS,
    FEE_UPDATE_TIMEOUT, RGS_SYNC_TIMEOUT, TX_BROADCAST_TIMEOUT,
};
use crate::fees::{cache_from_esplora_estimates, CachedFeeEstimator};
use crate::types::{Graph, Logger, RapidGossipSync};
use crate::wallet::OnchainWallet;

/// Runtime chain-access failures. These are logged and retried by the
/// background sync loop; only start-up turns them into hard errors (and only
/// when channel monitors exist).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainError {
    /// The Esplora endpoint could not be reached or returned an error.
    EsploraUnreachable(String),
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
            ChainError::EmptyFeeEstimates => write!(f, "empty fee estimates on mainnet"),
            ChainError::GossipUpdateFailed(e) => write!(f, "gossip update failed: {e}"),
            ChainError::WalletSyncFailed(e) => write!(f, "wallet sync failed: {e}"),
        }
    }
}

impl std::error::Error for ChainError {}

const BCAST_PACKAGE_QUEUE_SIZE: usize = 50;

/// `BroadcasterInterface` that queues packages for async broadcast via
/// Esplora. LDK's broadcast call sites are sync; the queue decouples them
/// from HTTP.
pub(crate) struct Broadcaster {
    queue_sender: mpsc::Sender<Vec<Transaction>>,
    queue_receiver: Mutex<mpsc::Receiver<Vec<Transaction>>>,
    logger: Arc<Logger>,
}

impl Broadcaster {
    pub(crate) fn new(logger: Arc<Logger>) -> Self {
        let (queue_sender, queue_receiver) = mpsc::channel(BCAST_PACKAGE_QUEUE_SIZE);
        Self {
            queue_sender,
            queue_receiver: Mutex::new(queue_receiver),
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
        if let Err(e) = self.queue_sender.try_send(package) {
            log_error!(
                self.logger,
                "Failed to queue transactions for broadcast: {e}"
            );
        }
    }
}

/// Whether a failed broadcast is benign because the network already knows the
/// transaction (already in mempool / already confirmed). Pure so it is
/// unit-testable; matches bitcoind's `sendrawtransaction` error strings that
/// Esplora relays in HTTP 400 bodies.
pub(crate) fn broadcast_error_is_benign(message: &str) -> bool {
    // Normalize case and bitcoind's hyphenated reject codes
    // ("txn-already-in-mempool") to plain words before matching.
    let message = message.to_lowercase().replace('-', " ");
    [
        "already in mempool",
        "already in the mempool",
        "already in block chain",
        "already known",
    ]
    .iter()
    .any(|benign| message.contains(benign))
}

/// Esplora-backed chain source. `tx_sync` implements LDK's `Filter` and
/// `Confirm`-driven sync; the shared `esplora_client` also serves the bdk
/// wallet, fee estimates, and broadcasts (one esplora-client 0.12 stack).
pub(crate) struct ChainSource {
    esplora_client: EsploraAsyncClient,
    tx_sync: Arc<EsploraSyncClient<Arc<Logger>>>,
    fee_estimator: Arc<CachedFeeEstimator>,
    network: bitcoin::Network,
    logger: Arc<Logger>,
}

impl ChainSource {
    pub(crate) fn new(
        esplora_url: &str,
        network: bitcoin::Network,
        fee_estimator: Arc<CachedFeeEstimator>,
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
            fee_estimator,
            network,
            logger,
        })
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
        wallet
            .sync(
                &self.esplora_client,
                BDK_CLIENT_STOP_GAP,
                BDK_CLIENT_CONCURRENCY,
            )
            .await
    }

    /// Refresh the fee-rate cache from the Esplora fee-estimates endpoint.
    pub(crate) async fn update_fee_rate_estimates(&self) -> Result<(), ChainError> {
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
        Ok(())
    }

    /// Broadcast one queued package, tolerating already-known transactions.
    pub(crate) async fn process_broadcast_package(&self, package: Vec<Transaction>) {
        for tx in &package {
            let txid = tx.compute_txid();
            let res =
                tokio::time::timeout(TX_BROADCAST_TIMEOUT, self.esplora_client.broadcast(tx)).await;
            match res {
                Ok(Ok(())) => {}
                Ok(Err(esplora_client::Error::HttpResponse { status, message }))
                    if broadcast_error_is_benign(&message) =>
                {
                    // The mempool/chain already knows this transaction; that
                    // is success for our purposes, not a failure (status is
                    // typically 400 here).
                    let _ = status;
                }
                Ok(Err(e)) => {
                    log_error!(self.logger, "Failed to broadcast transaction {txid}: {e}");
                }
                Err(e) => {
                    log_error!(
                        self.logger,
                        "Broadcast of transaction {txid} timed out: {e}"
                    );
                }
            }
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

    #[test]
    fn already_known_broadcast_errors_are_benign() {
        for message in [
            "sendrawtransaction RPC error: {\"code\":-27,\"message\":\"Transaction already in block chain\"}",
            "txn-already-in-mempool",
            "Transaction already in the mempool",
            "TXN-ALREADY-KNOWN",
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
            "bad-txns-inputs-missingorspent",
            "dust",
            "",
        ] {
            assert!(
                !broadcast_error_is_benign(message),
                "expected fatal: {message}"
            );
        }
    }
}
