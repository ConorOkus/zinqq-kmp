//! Node assembly with first-class fresh and restore paths, mirroring
//! ldk-node's wiring order: mnemonic-derived keys (U1, KTD-4) → bdk wallet →
//! KeysManager + custom SignerProvider → ChainMonitor → ChannelManager →
//! OnionMessenger → PeerManager.
//!
//! The restore sequence is load-bearing (see the plan's lifecycle diagram):
//! initialize the bdk wallet from the descriptors (eager, no network — the
//! signer resolves destination scripts during deserialization) → read all
//! monitors with the custom SignerProvider → `ChannelManagerReadArgs` →
//! deserialize the manager → Esplora `Confirm` sync on BOTH manager and
//! monitors → `watch_channel` each monitor. Only then may peers connect and
//! the background processor run.

use std::fmt;
use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use bitcoin::BlockHash;
use lightning::chain::channelmonitor::ChannelMonitor;
use lightning::chain::{BestBlock, Confirm, Watch};
use lightning::ln::channelmanager::{ChainParameters, ChannelManagerReadArgs};
use lightning::ln::peer_handler::{IgnoringMessageHandler, MessageHandler};
use lightning::log_error;
use lightning::log_info;
use lightning::routing::router::DefaultRouter;
use lightning::routing::scoring::{
    ProbabilisticScorer, ProbabilisticScoringDecayParameters, ProbabilisticScoringFeeParameters,
};
use lightning::sign::{EntropySource, InMemorySigner, KeysManager, NodeSigner};
use lightning::util::logger::Logger as _;
use lightning::util::persist::KVStoreSyncWrapper;
use lightning::util::persist::{
    read_channel_monitors, KVStoreSync, CHANNEL_MANAGER_PERSISTENCE_KEY,
    CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE, CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
    NETWORK_GRAPH_PERSISTENCE_KEY, NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
    NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE, OUTPUT_SWEEPER_PERSISTENCE_KEY,
    OUTPUT_SWEEPER_PERSISTENCE_PRIMARY_NAMESPACE, OUTPUT_SWEEPER_PERSISTENCE_SECONDARY_NAMESPACE,
    SCORER_PERSISTENCE_KEY, SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
    SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
};
use lightning::util::ser::{ReadableArgs, Writeable};
use lightning::util::sweep::OutputSweeperSync;
use lightning_liquidity::lsps2::client::LSPS2ClientConfig;
use lightning_liquidity::LiquidityClientConfig;
use lightning_persister::fs_store::FilesystemStore;

use crate::chain::{Broadcaster, ChainSource, GossipSource};
use crate::config::{default_user_config, Config};
use crate::fees::CachedFeeEstimator;
use crate::keys::{derive_wallet_keys, read_or_generate_mnemonic, KeysError};
use crate::signer::WalletSignerProvider;
use crate::types::{
    ChainMonitor, ChannelManager, Graph, LiquidityManager, Logger, MessageRouter, OnionMessenger,
    PeerManager, Scorer, Sweeper,
};
use crate::wallet::OnchainWallet;

/// Subdirectory of the storage dir backing the `FilesystemStore`. Public as
/// test-support API surface: integration tests open the store at this path to
/// inspect persisted state.
pub const KV_STORE_SUBDIR: &str = "store";

/// Typed startup failures. Restore/persistence problems fail `start()` hard;
/// an unreachable Esplora on a fresh (zero-monitor) node is *not* an error —
/// that start degrades and sync retries in the background.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildError {
    /// The node is already running.
    AlreadyRunning,
    /// Another node already holds the storage directory's lock. Two live nodes
    /// on one seed diverge on channel state, so the second start is refused.
    InstanceAlreadyRunning,
    /// The node is not running.
    NotRunning,
    /// The storage directory or mnemonic file could not be written.
    WriteFailed,
    /// Persisted state exists but could not be read or deserialized.
    ReadFailed,
    /// The mnemonic file exists but does not hold a valid BIP39 English
    /// 12-word mnemonic (U1, R1).
    InvalidMnemonic,
    /// A mnemonic already exists where one was about to be written;
    /// overwriting would destroy access to the existing wallet's funds (R1:
    /// the mnemonic file is write-once).
    MnemonicExists,
    /// No mnemonic exists but the restore-in-progress marker does (U4):
    /// auto-generating fresh words now would silently abandon the wallet
    /// being restored, so the start is refused.
    RestoreInProgress,
    /// The on-chain wallet could not be set up.
    WalletSetupFailed,
    /// A persisted channel monitor is unreadable or corrupt.
    InvalidMonitorData,
    /// A restored channel monitor was rejected by the chain monitor.
    WatchChannelFailed,
    /// Chain sync failed while channel monitors exist; starting without a
    /// synced view of monitored channels is not fund-safe.
    ChainSyncFailed,
    /// The Esplora client could not be constructed from the configured URL.
    InvalidEsploraConfig,
    /// The system clock is set before the UNIX epoch.
    InvalidSystemTime,
    /// The tokio runtime could not be created.
    RuntimeSetupFailed,
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            BuildError::AlreadyRunning => "the node is already running",
            BuildError::InstanceAlreadyRunning => {
                "another node is already running against this storage directory"
            }
            BuildError::NotRunning => "the node is not running",
            BuildError::WriteFailed => "failed to write node data",
            BuildError::ReadFailed => "failed to read persisted node data",
            BuildError::InvalidMnemonic => {
                "the persisted mnemonic file is not a valid BIP39 English 12-word mnemonic"
            }
            BuildError::MnemonicExists => "a mnemonic already exists; refusing to overwrite it",
            BuildError::RestoreInProgress => {
                "a restore is in progress; refusing to generate a fresh mnemonic"
            }
            BuildError::WalletSetupFailed => "failed to set up the on-chain wallet",
            BuildError::InvalidMonitorData => "persisted channel monitor data is unreadable",
            BuildError::WatchChannelFailed => "failed to watch a restored channel monitor",
            BuildError::ChainSyncFailed => "chain sync failed with channel monitors present",
            BuildError::InvalidEsploraConfig => "invalid esplora configuration",
            BuildError::InvalidSystemTime => "system time is before the UNIX epoch",
            BuildError::RuntimeSetupFailed => "failed to create the tokio runtime",
        };
        write!(f, "{msg}")
    }
}

impl std::error::Error for BuildError {}

impl From<KeysError> for BuildError {
    fn from(error: KeysError) -> Self {
        match error {
            KeysError::WriteFailed => BuildError::WriteFailed,
            KeysError::ReadFailed => BuildError::ReadFailed,
            KeysError::InvalidMnemonic => BuildError::InvalidMnemonic,
            KeysError::MnemonicExists => BuildError::MnemonicExists,
            KeysError::RestoreInProgress => BuildError::RestoreInProgress,
        }
    }
}

/// Everything the running node owns, fully wired.
pub(crate) struct NodeComponents {
    pub(crate) kv_store: Arc<FilesystemStore>,
    pub(crate) logger: Arc<Logger>,
    pub(crate) chain_source: Arc<ChainSource>,
    pub(crate) broadcaster: Arc<Broadcaster>,
    pub(crate) onchain_wallet: Arc<OnchainWallet>,
    pub(crate) keys_manager: Arc<KeysManager>,
    pub(crate) chain_monitor: Arc<ChainMonitor>,
    pub(crate) channel_manager: Arc<ChannelManager>,
    pub(crate) onion_messenger: Arc<OnionMessenger>,
    pub(crate) liquidity_manager: Arc<LiquidityManager>,
    pub(crate) peer_manager: Arc<PeerManager>,
    pub(crate) scorer: Arc<Mutex<Scorer>>,
    pub(crate) gossip_source: Arc<GossipSource>,
    pub(crate) sweeper: Arc<Sweeper>,
    /// Whether the initial chain sync reached the tip. `false` is a degraded
    /// start (only possible with zero monitors); the background loop retries.
    pub(crate) chain_synced_at_start: bool,
}

/// Persists the channel manager under LDK's persist key constants. The
/// background processor does this continuously while running; this explicit
/// write covers the fresh-build and shutdown moments. The manager may lag the
/// monitors, never the other way around (KTD-4).
pub(crate) fn persist_channel_manager(
    channel_manager: &ChannelManager,
    kv_store: &FilesystemStore,
) -> Result<(), BuildError> {
    kv_store
        .write(
            CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
            CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
            CHANNEL_MANAGER_PERSISTENCE_KEY,
            channel_manager.encode(),
        )
        .map_err(|_| BuildError::WriteFailed)
}

/// Assembles the node from the storage dir: fresh on first start, restore
/// otherwise. Blocks on the runtime for the initial `Confirm` sync.
pub(crate) fn build(
    config: &Config,
    runtime: &tokio::runtime::Runtime,
) -> Result<NodeComponents, BuildError> {
    let logger = Arc::new(Logger);
    let storage_dir = PathBuf::from(&config.storage_dir);
    fs::create_dir_all(&storage_dir).map_err(|_| BuildError::WriteFailed)?;

    // U1 (KTD-4, R1/R2): one BIP39 mnemonic — auto-generated on first start,
    // write-once — roots every key. Derivations are byte-identical to the PWA.
    let mnemonic = read_or_generate_mnemonic(&storage_dir)?;
    let keys = derive_wallet_keys(&mnemonic, config.network);

    let kv_store = Arc::new(FilesystemStore::new(storage_dir.join(KV_STORE_SUBDIR)));

    let broadcaster = Arc::new(Broadcaster::new(Arc::clone(&logger)));
    let fee_estimator = Arc::new(CachedFeeEstimator::new());
    let chain_source = Arc::new(
        ChainSource::new(
            &config.esplora_url,
            config.network,
            Arc::clone(&fee_estimator),
            Arc::clone(&logger),
        )
        .map_err(|e| {
            log_error!(logger, "Failed to build esplora client: {e}");
            BuildError::InvalidEsploraConfig
        })?,
    );

    // BDK wallet FIRST (KTD-4 ordering): eager, no network — the custom
    // signer below resolves destination scripts during LDK deserialization.
    let onchain_wallet = Arc::new(OnchainWallet::new(
        &keys.descriptor_external,
        &keys.descriptor_internal,
        config.network,
        Arc::clone(&kv_store),
        Arc::clone(&logger),
    )?);

    // KeysManager over the m/535'/0' LDK seed. `v2_remote_key_derivation =
    // false` is not merely parity (KTD-4): `true` forbids downgrade below LDK
    // 0.2 and changes counterparty-close script pubkeys, breaking byte-compat
    // with the PWA's WASM signer.
    let cur_time = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(|_| BuildError::InvalidSystemTime)?;
    let keys_manager = Arc::new(KeysManager::new(
        &keys.ldk_seed,
        cur_time.as_secs(),
        cur_time.subsec_nanos(),
        false,
    ));
    let signer_provider = Arc::new(WalletSignerProvider::new(
        Arc::clone(&keys_manager),
        Arc::clone(&onchain_wallet),
        keys.channel_keys_id_hmac_key,
        Arc::clone(&logger),
    ));
    drop(keys); // Scrubs the derived key material (WalletKeys::drop).

    // Read ALL channel monitors — with the custom SignerProvider — before
    // anything touches the manager.
    let channel_monitors: Vec<(BlockHash, ChannelMonitor<InMemorySigner>)> = read_channel_monitors(
        Arc::clone(&kv_store),
        Arc::clone(&keys_manager),
        Arc::clone(&signer_provider),
    )
    .map_err(|e| {
        log_error!(logger, "Failed to read channel monitors: {e}");
        BuildError::InvalidMonitorData
    })?;

    // ChainMonitor persists full monitors straight into the KVStoreSync
    // (durable-before-Completed, KTD-4).
    let chain_monitor: Arc<ChainMonitor> = Arc::new(ChainMonitor::new(
        Some(Arc::clone(&chain_source)),
        Arc::clone(&broadcaster),
        Arc::clone(&logger),
        Arc::clone(&fee_estimator),
        Arc::clone(&kv_store),
        Arc::clone(&keys_manager),
        keys_manager.get_peer_storage_key(),
    ));

    // Network graph and scorer: reload if present, else start empty.
    let network_graph = match kv_store.read(
        NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
        NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
        NETWORK_GRAPH_PERSISTENCE_KEY,
    ) {
        Ok(bytes) => Arc::new(
            Graph::read(&mut Cursor::new(bytes), Arc::clone(&logger))
                .map_err(|_| BuildError::ReadFailed)?,
        ),
        Err(e) if e.kind() == lightning::io::ErrorKind::NotFound => {
            Arc::new(Graph::new(config.network, Arc::clone(&logger)))
        }
        Err(_) => return Err(BuildError::ReadFailed),
    };

    let scorer = match kv_store.read(
        SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
        SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
        SCORER_PERSISTENCE_KEY,
    ) {
        Ok(bytes) => {
            let args = (
                ProbabilisticScoringDecayParameters::default(),
                Arc::clone(&network_graph),
                Arc::clone(&logger),
            );
            Arc::new(Mutex::new(
                Scorer::read(&mut Cursor::new(bytes), args).map_err(|_| BuildError::ReadFailed)?,
            ))
        }
        Err(e) if e.kind() == lightning::io::ErrorKind::NotFound => {
            Arc::new(Mutex::new(ProbabilisticScorer::new(
                ProbabilisticScoringDecayParameters::default(),
                Arc::clone(&network_graph),
                Arc::clone(&logger),
            )))
        }
        Err(_) => return Err(BuildError::ReadFailed),
    };

    let router = Arc::new(DefaultRouter::new(
        Arc::clone(&network_graph),
        Arc::clone(&logger),
        Arc::clone(&keys_manager),
        Arc::clone(&scorer),
        ProbabilisticScoringFeeParameters::default(),
    ));
    let message_router = Arc::new(MessageRouter::new(
        Arc::clone(&network_graph),
        Arc::clone(&keys_manager),
    ));

    let user_config = default_user_config();

    // ChannelManager: restore if persisted, else fresh from genesis (the
    // initial sync below brings it to tip).
    let channel_manager: Arc<ChannelManager> = match kv_store.read(
        CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
        CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
        CHANNEL_MANAGER_PERSISTENCE_KEY,
    ) {
        Ok(bytes) => {
            let monitor_refs = channel_monitors
                .iter()
                .map(|(_, monitor)| monitor)
                .collect();
            let read_args = ChannelManagerReadArgs::new(
                Arc::clone(&keys_manager),
                Arc::clone(&keys_manager),
                Arc::clone(&signer_provider),
                Arc::clone(&fee_estimator),
                Arc::clone(&chain_monitor),
                Arc::clone(&broadcaster),
                Arc::clone(&router),
                Arc::clone(&message_router),
                Arc::clone(&logger),
                user_config,
                monitor_refs,
            );
            let (_block_hash, channel_manager) =
                <(BlockHash, ChannelManager)>::read(&mut Cursor::new(bytes), read_args).map_err(
                    |e| {
                        log_error!(logger, "Failed to deserialize channel manager: {e}");
                        BuildError::ReadFailed
                    },
                )?;
            log_info!(logger, "Restored channel manager from disk.");
            Arc::new(channel_manager)
        }
        Err(e) if e.kind() == lightning::io::ErrorKind::NotFound => {
            let chain_params = ChainParameters {
                network: config.network,
                best_block: BestBlock::from_network(config.network),
            };
            let channel_manager = Arc::new(ChannelManager::new(
                Arc::clone(&fee_estimator),
                Arc::clone(&chain_monitor),
                Arc::clone(&broadcaster),
                Arc::clone(&router),
                Arc::clone(&message_router),
                Arc::clone(&logger),
                Arc::clone(&keys_manager),
                Arc::clone(&keys_manager),
                Arc::clone(&signer_provider),
                user_config,
                chain_params,
                cur_time.as_secs() as u32,
            ));
            // Persist immediately so the next start always takes the restore
            // path, even if the background processor never got to run.
            persist_channel_manager(&channel_manager, &kv_store)?;
            log_info!(logger, "Created fresh channel manager.");
            channel_manager
        }
        Err(_) => return Err(BuildError::ReadFailed),
    };

    // Initial chain sync of manager AND monitors — BEFORE watch_channel.
    let chain_synced_at_start = {
        let monitor_sync_wrappers: Vec<_> = channel_monitors
            .iter()
            .map(|(_, monitor)| {
                (
                    monitor,
                    Arc::clone(&broadcaster),
                    Arc::clone(&fee_estimator),
                    Arc::clone(&logger),
                )
            })
            .collect();
        let mut confirmables: Vec<&(dyn Confirm + Sync + Send)> = vec![&*channel_manager];
        for wrapper in &monitor_sync_wrappers {
            confirmables.push(wrapper);
        }
        match runtime.block_on(chain_source.sync_confirmables(confirmables)) {
            Ok(()) => true,
            Err(e) if channel_monitors.is_empty() => {
                // Degraded start: nothing on-chain to watch yet, so an
                // unreachable Esplora only delays sync, it doesn't risk funds.
                log_error!(logger, "Initial chain sync failed, starting degraded: {e}");
                false
            }
            Err(e) => {
                log_error!(
                    logger,
                    "Initial chain sync failed with monitors present: {e}"
                );
                return Err(BuildError::ChainSyncFailed);
            }
        }
    };

    // Hand the synced monitors to the chain monitor.
    for (_block_hash, monitor) in channel_monitors {
        let channel_id = monitor.channel_id();
        chain_monitor
            .watch_channel(channel_id, monitor)
            .map_err(|e| {
                log_error!(logger, "Failed to watch channel monitor: {e:?}");
                BuildError::WatchChannelFailed
            })?;
    }

    let onion_messenger: Arc<OnionMessenger> = Arc::new(OnionMessenger::new(
        Arc::clone(&keys_manager),
        Arc::clone(&keys_manager),
        Arc::clone(&logger),
        Arc::clone(&channel_manager),
        Arc::clone(&message_router),
        Arc::clone(&channel_manager),
        Arc::clone(&channel_manager),
        IgnoringMessageHandler {},
        IgnoringMessageHandler {},
    ));

    let gossip_source = Arc::new(GossipSource::new(
        config.rgs_url.clone(),
        Arc::clone(&network_graph),
        Arc::clone(&logger),
    ));

    // LSPS2 client (U4): the LiquidityManager is BOTH the PeerManager's
    // custom message handler (below) and the background processor's liquidity
    // slot — omitting either makes LSPS2 silently do nothing (KTD-9). The
    // constructor is async only because of the async-KVStore bound; over the
    // sync FilesystemStore it resolves on the first poll.
    let liquidity_manager: Arc<LiquidityManager> = Arc::new(
        runtime
            .block_on(LiquidityManager::new(
                Arc::clone(&keys_manager),
                Arc::clone(&keys_manager),
                Arc::clone(&channel_manager),
                Some(Arc::clone(&chain_source)),
                Some(ChainParameters {
                    network: config.network,
                    best_block: channel_manager.current_best_block(),
                }),
                KVStoreSyncWrapper(Arc::clone(&kv_store)),
                Arc::clone(&broadcaster),
                None,
                Some(LiquidityClientConfig {
                    lsps1_client_config: None,
                    lsps2_client_config: Some(LSPS2ClientConfig::default()),
                    lsps5_client_config: None,
                }),
            ))
            .map_err(|e| {
                log_error!(logger, "Failed to build liquidity manager: {e}");
                BuildError::ReadFailed
            })?,
    );

    let msg_handler = MessageHandler {
        chan_handler: Arc::clone(&channel_manager),
        route_handler: Arc::new(IgnoringMessageHandler {}),
        onion_message_handler: Arc::clone(&onion_messenger),
        custom_message_handler: Arc::clone(&liquidity_manager),
        send_only_message_handler: Arc::clone(&chain_monitor),
    };
    let ephemeral_bytes: [u8; 32] = keys_manager.get_secure_random_bytes();
    let peer_manager = Arc::new(PeerManager::new(
        msg_handler,
        cur_time.as_secs() as u32,
        &ephemeral_bytes,
        Arc::clone(&logger),
        Arc::clone(&keys_manager),
    ));

    // Output sweeper: durable Event::SpendableOutputs handling.
    let sweeper: Arc<Sweeper> = match kv_store.read(
        OUTPUT_SWEEPER_PERSISTENCE_PRIMARY_NAMESPACE,
        OUTPUT_SWEEPER_PERSISTENCE_SECONDARY_NAMESPACE,
        OUTPUT_SWEEPER_PERSISTENCE_KEY,
    ) {
        Ok(bytes) => {
            let args = (
                Arc::clone(&broadcaster),
                Arc::clone(&fee_estimator),
                Some(Arc::clone(&chain_source)),
                Arc::clone(&keys_manager),
                Arc::clone(&onchain_wallet),
                Arc::clone(&kv_store),
                Arc::clone(&logger),
            );
            let (_best_block, sweeper) =
                <(BestBlock, Sweeper)>::read(&mut Cursor::new(bytes), args)
                    .map_err(|_| BuildError::ReadFailed)?;
            Arc::new(sweeper)
        }
        Err(e) if e.kind() == lightning::io::ErrorKind::NotFound => {
            Arc::new(OutputSweeperSync::new(
                channel_manager.current_best_block(),
                Arc::clone(&broadcaster),
                Arc::clone(&fee_estimator),
                Some(Arc::clone(&chain_source)),
                Arc::clone(&keys_manager),
                Arc::clone(&onchain_wallet),
                Arc::clone(&kv_store),
                Arc::clone(&logger),
            ))
        }
        Err(_) => return Err(BuildError::ReadFailed),
    };

    Ok(NodeComponents {
        kv_store,
        logger,
        chain_source,
        broadcaster,
        onchain_wallet,
        keys_manager,
        chain_monitor,
        channel_manager,
        onion_messenger,
        liquidity_manager,
        peer_manager,
        scorer,
        gossip_source,
        sweeper,
        chain_synced_at_start,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_errors_map_to_distinct_typed_build_errors() {
        // Mnemonic loading/generation failures must stay distinguishable at
        // the start() surface (U1: typed start errors).
        let cases = [
            (KeysError::WriteFailed, BuildError::WriteFailed),
            (KeysError::ReadFailed, BuildError::ReadFailed),
            (KeysError::InvalidMnemonic, BuildError::InvalidMnemonic),
            (KeysError::MnemonicExists, BuildError::MnemonicExists),
            (KeysError::RestoreInProgress, BuildError::RestoreInProgress),
        ];
        for (keys_error, build_error) in cases {
            assert_eq!(BuildError::from(keys_error), build_error);
        }
    }
}
