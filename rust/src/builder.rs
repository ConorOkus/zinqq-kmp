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

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::SystemTime;

use bitcoin::BlockHash;
use lightning::chain::channelmonitor::ChannelMonitor;
use lightning::chain::{BestBlock, Confirm, Watch};
use lightning::ln::channelmanager::{ChainParameters, ChannelManagerReadArgs};
use lightning::ln::peer_handler::{IgnoringMessageHandler, MessageHandler};
use lightning::ln::types::ChannelId;
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
    NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE, SCORER_PERSISTENCE_KEY,
    SCORER_PERSISTENCE_PRIMARY_NAMESPACE, SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
};
use lightning::util::ser::{ReadableArgs, Writeable};
use lightning_liquidity::lsps2::client::LSPS2ClientConfig;
use lightning_liquidity::LiquidityClientConfig;
use lightning_persister::fs_store::FilesystemStore;

use crate::chain::{Broadcaster, ChainError, ChainSource, GossipSource, PendingBroadcasts};
use crate::config::{default_user_config, Config};
use crate::fees::CachedFeeEstimator;
use crate::keys::{derive_wallet_keys, read_or_generate_mnemonic, KeysError, WalletKeys};
use crate::node::EventSink;
use crate::signer::WalletSignerProvider;
use crate::types::{
    ChainMonitor, ChannelManager, Graph, LiquidityManager, Logger, MessageRouter, OnionMessenger,
    PeerManager, Scorer,
};
use crate::vss::known_peers::KnownPeersStore;
use crate::vss::startup::{establish_vss_state, VssStartupState};
use crate::vss::store::{
    CompletionSink, DualWriteKvStore, RetryTuning, VssBackedStore, VssTransport,
    FENCED_FLAG_FILE_NAME,
};
use crate::vss::VssWireClient;
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
    /// The restore-in-progress marker exists and the restore cannot proceed
    /// here (U4): either no mnemonic exists (auto-generating fresh words
    /// would silently abandon the wallet being restored) or VSS is disabled
    /// (the voided local state can never be recovered), so the start is
    /// refused.
    RestoreInProgress,
    /// Channel monitors exist locally but no channel manager does: channels
    /// exist but their state is lost. Starting fresh would silently discard
    /// channel funds, so the start halts (U4 — the PWA's orphaned-monitors
    /// halt).
    OrphanedMonitors,
    /// The restored channel manager references a funded channel with no
    /// local monitor — a partial restore. Booting would run channels without
    /// their fund-safety state, so the start halts (U4 — the missing mirror
    /// of the orphaned-monitors check).
    MissingChannelMonitor,
    /// The on-chain wallet could not be set up.
    WalletSetupFailed,
    /// A persisted channel monitor is unreadable or corrupt.
    InvalidMonitorData,
    /// A restored channel monitor was rejected by the chain monitor.
    WatchChannelFailed,
    /// Chain sync failed while channel monitors exist; starting without a
    /// synced view of monitored channels is not fund-safe.
    ChainSyncFailed,
    /// The Esplora backend answered the startup genesis-hash probe with a
    /// non-mainnet hash: it serves the wrong chain (U12/KTD-12). Always a
    /// hard error — an unreachable backend degraded-starts instead, but a
    /// wrong-chain view is never fund-safe.
    WrongNetworkBackend,
    /// The Esplora client could not be constructed from the configured URL.
    InvalidEsploraConfig,
    /// The system clock is set before the UNIX epoch.
    InvalidSystemTime,
    /// The tokio runtime could not be created.
    RuntimeSetupFailed,
    /// The durable `fenced` flag exists: another client wrote this seed's VSS
    /// store and the node fenced itself (KTD-3). Start is refused until the
    /// user wipes and restores (readable queries stay available while
    /// stopped, per KTD-5).
    Fenced,
    /// The VSS wire client could not be constructed (invalid signing key or
    /// HTTP client setup).
    VssSetupFailed,
    /// Silent recovery against a non-empty VSS namespace failed (or the
    /// probe could not prove the namespace empty): starting fresh would
    /// write over an existing backup, so the start is refused (KTD-3:
    /// never fresh-over-backup).
    VssRecoveryFailed,
    /// Local state exists but the VSS version cache could not be seeded;
    /// writing at guessed versions would false-trip the fence, so the start
    /// is refused (KTD-3: seeding is mandatory).
    VssVersionSeedFailed,
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
                "a restore is in progress; it must complete before the node can start"
            }
            BuildError::OrphanedMonitors => {
                "channel monitors exist but the channel manager is missing; refusing to start \
                 with channel state lost"
            }
            BuildError::MissingChannelMonitor => {
                "the channel manager references a channel with no local monitor; refusing to \
                 start against a partial restore"
            }
            BuildError::WalletSetupFailed => "failed to set up the on-chain wallet",
            BuildError::InvalidMonitorData => "persisted channel monitor data is unreadable",
            BuildError::WatchChannelFailed => "failed to watch a restored channel monitor",
            BuildError::ChainSyncFailed => "chain sync failed with channel monitors present",
            BuildError::WrongNetworkBackend => {
                "the esplora backend serves a different network than mainnet"
            }
            BuildError::InvalidEsploraConfig => "invalid esplora configuration",
            BuildError::InvalidSystemTime => "system time is before the UNIX epoch",
            BuildError::RuntimeSetupFailed => "failed to create the tokio runtime",
            BuildError::Fenced => {
                "this wallet is active on another device (fenced); wipe and restore from backup \
                 to take over here"
            }
            BuildError::VssSetupFailed => "failed to set up the VSS backup client",
            BuildError::VssRecoveryFailed => {
                "recovering wallet state from the cloud backup failed; refusing to start fresh \
                 over an existing backup"
            }
            BuildError::VssVersionSeedFailed => {
                "seeding cloud-backup versions failed; refusing to write at guessed versions"
            }
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
    /// The local store. U3 routes all node persistence through
    /// `dual_kv_store`/`vss_store`; kept here for the units that need raw
    /// local access (U9 funding store, U4 restore, U10 close records).
    pub(crate) kv_store: Arc<FilesystemStore>,
    /// U3: the composite store — the ChainMonitor's monitor `Persist`, plus
    /// the fence/version/manifest state and the CM/LWW remote write paths.
    pub(crate) vss_store: Arc<VssBackedStore>,
    /// U3: the `KVStoreSync` the background processor persists through
    /// (channel manager → bounded VSS-then-local; everything else local).
    pub(crate) dual_kv_store: Arc<DualWriteKvStore>,
    /// U3: `_known_peers` — local mirror + LWW VSS sync; feeds the node's
    /// reconnect loop.
    pub(crate) known_peers: Arc<KnownPeersStore>,
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
    /// Whether the initial chain sync reached the tip. `false` is a degraded
    /// start (only possible with zero monitors); the background loop retries.
    pub(crate) chain_synced_at_start: bool,
}

/// Persists the channel manager under LDK's persist key constants, through
/// the U3 dual-write store (bounded VSS attempt, then the local write — the
/// local half always happens). The background processor does this
/// continuously while running; this explicit write covers the fresh-build and
/// shutdown moments. The manager may lag the monitors, never the other way
/// around (KTD-4).
pub(crate) fn persist_channel_manager(
    channel_manager: &ChannelManager,
    kv_store: &DualWriteKvStore,
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

/// Reads an optional KVStore value: `Some(bytes)` if present, `None` on
/// NotFound, and `BuildError::ReadFailed` on any other error.
fn read_optional(
    kv_store: &FilesystemStore,
    primary_namespace: &str,
    secondary_namespace: &str,
    key: &str,
) -> Result<Option<Vec<u8>>, BuildError> {
    match kv_store.read(primary_namespace, secondary_namespace, key) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == lightning::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(BuildError::ReadFailed),
    }
}

/// U4 startup integrity check (the missing mirror of the PWA's
/// monitors-without-CM halt): every FUNDED channel the deserialized manager
/// references must have a local monitor, or the state is a partial restore
/// and booting it is not fund-safe. Channels still awaiting funding
/// legitimately have no monitor yet and must be filtered out by the caller.
pub(crate) fn check_channel_monitor_coverage(
    funded_channels: impl IntoIterator<Item = ChannelId>,
    monitored: &HashSet<ChannelId>,
) -> Result<(), BuildError> {
    for channel_id in funded_channels {
        if !monitored.contains(&channel_id) {
            return Err(BuildError::MissingChannelMonitor);
        }
    }
    Ok(())
}

/// Builds the VSS wire transport for `config`, or `None` when VSS is
/// disabled. Tests inject an in-process transport via the config override.
/// Shared with U4's restore flow, which derives keys from the ENTERED
/// mnemonic rather than the stored one.
pub(crate) fn make_vss_transport(
    config: &Config,
    keys: &WalletKeys,
) -> Result<Option<Arc<dyn VssTransport>>, BuildError> {
    if config.vss_disabled {
        return Ok(None);
    }
    #[cfg(test)]
    if let Some(overridden) = &config.vss_transport_override {
        return Ok(Some(Arc::clone(&overridden.0)));
    }
    let client = VssWireClient::new(
        config.vss_url.clone(),
        keys.vss_store_id.clone(),
        keys.vss_encryption_key,
        &keys.vss_signing_key,
    )
    .map_err(|_| BuildError::VssSetupFailed)?;
    Ok(Some(Arc::new(client) as Arc<dyn VssTransport>))
}

/// Assembles the node from the storage dir: fresh on first start, restore
/// otherwise. Blocks on the runtime for the initial `Confirm` sync.
///
/// `event_sink` carries the U3 backup events (`BackupDegraded`, `Fenced`)
/// from the persistence layer into the public queue.
pub(crate) fn build(
    config: &Config,
    runtime: &tokio::runtime::Runtime,
    event_sink: Arc<dyn EventSink>,
) -> Result<NodeComponents, BuildError> {
    let logger = Arc::new(Logger);
    let storage_dir = PathBuf::from(&config.storage_dir);
    fs::create_dir_all(&storage_dir).map_err(|_| BuildError::WriteFailed)?;

    // U4 crash-prefix resume, BEFORE the fence check and the mnemonic load:
    // when the restore marker carries a restore context, adopt its mnemonic
    // (redoing the interrupted clear — which also lifts a stale fence — if
    // it never completed). The marker itself stays until recovery is
    // durable, so `establish_vss_state` below resumes silent recovery.
    crate::restore::prepare_marker_resume(&storage_dir, &logger)?;

    // U3 (KTD-3): the durable fenced flag survives restarts and blocks the
    // start until the user wipes and restores — no automatic un-fence.
    if storage_dir.join(FENCED_FLAG_FILE_NAME).exists() {
        log_error!(
            logger,
            "The fenced flag is present; refusing to start (wipe + restore to take over)"
        );
        return Err(BuildError::Fenced);
    }

    // U1 (KTD-4, R1/R2): one BIP39 mnemonic — auto-generated on first start,
    // write-once — roots every key. Derivations are byte-identical to the PWA.
    let mnemonic = read_or_generate_mnemonic(&storage_dir)?;
    let keys = derive_wallet_keys(&mnemonic, config.network);

    // Built before `keys` is scrubbed; consumed by the VSS phase below.
    let vss_transport = make_vss_transport(config, &keys)?;

    // U4: the marker voids local LDK state; without a VSS transport nothing
    // can recover it, so booting (against a possibly-partial set) is refused.
    if vss_transport.is_none()
        && storage_dir
            .join(crate::keys::RESTORE_IN_PROGRESS_FILE_NAME)
            .exists()
    {
        log_error!(
            logger,
            "Restore marker present but VSS is disabled: the restore cannot resume; refusing \
             to start"
        );
        return Err(BuildError::RestoreInProgress);
    }

    let kv_store = Arc::new(FilesystemStore::new(storage_dir.join(KV_STORE_SUBDIR)));

    let pending_broadcasts = Arc::new(PendingBroadcasts::new(
        Arc::clone(&kv_store),
        Arc::clone(&logger),
    ));
    let broadcaster = Arc::new(Broadcaster::new(
        Arc::clone(&pending_broadcasts),
        Arc::clone(&logger),
    ));
    let fee_estimator = Arc::new(CachedFeeEstimator::new());
    let chain_source = Arc::new(
        ChainSource::new(
            &config.esplora_url,
            config.network,
            Arc::clone(&fee_estimator),
            Arc::clone(&pending_broadcasts),
            Arc::clone(&logger),
        )
        .map_err(|e| {
            log_error!(logger, "Failed to build esplora client: {e}");
            BuildError::InvalidEsploraConfig
        })?,
    );

    // U12/KTD-12: genesis-hash network check, keyed to the configured network
    // (U3/R3). A backend that ANSWERS with a genesis other than this build's
    // fails the start hard; an unreachable one only logs — the fresh-node
    // degraded start (and the monitors-present hard failure below) keep their
    // existing semantics.
    //
    // This is what makes a custom signet safe to add: point a Mutinynet build
    // at mainnet infrastructure (or the reverse) and it refuses to start
    // rather than syncing the wrong chain into the wrong wallet's monitors.
    match runtime
        .block_on(chain_source.check_genesis_hash(config.wallet_network.genesis_block_hash()))
    {
        Ok(()) => {}
        Err(ChainError::WrongNetworkBackend { got }) => {
            log_error!(
                logger,
                "Esplora backend serves the wrong network (genesis {got}); refusing to start"
            );
            return Err(BuildError::WrongNetworkBackend);
        }
        Err(e) => {
            log_error!(
                logger,
                "Genesis-hash probe failed (not a network mismatch), continuing: {e}"
            );
        }
    }

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

    // U3 (KTD-3): resolve the VSS startup branch — silent recovery (which
    // writes local state read below), migration, or mandatory version
    // seeding — BEFORE any component that could write is built.
    let vss_state = match &vss_transport {
        Some(transport) => establish_vss_state(
            Arc::clone(transport),
            &kv_store,
            &keys_manager,
            &signer_provider,
            &storage_dir,
            &event_sink,
            &logger,
            runtime,
        )?,
        None => VssStartupState::local_only(),
    };
    let vss_recovered = vss_state.recovered;
    let vss_store = Arc::new(VssBackedStore::new(
        vss_state.remote,
        Arc::clone(&kv_store),
        runtime.handle().clone(),
        &storage_dir,
        Arc::clone(&event_sink),
        Arc::clone(&logger),
        RetryTuning::default(),
        vss_state.versions,
        vss_state.monitor_keys,
        vss_state.probe_empty,
    ));
    let dual_kv_store = Arc::new(DualWriteKvStore::new(
        Arc::clone(&vss_store),
        Arc::clone(&kv_store),
    ));
    log_info!(
        logger,
        "VSS persistence ready: probe_empty={}, recovered={}, fenced={}",
        vss_store.probe_empty_this_session(),
        vss_recovered,
        vss_store.is_fenced()
    );

    // Read ALL channel monitors — with the custom SignerProvider — before
    // anything touches the manager. (Silent recovery above has already
    // written any remotely-recovered monitors locally: local storage is the
    // source of truth for node start, KTD-3.)
    let channel_monitors: Vec<(BlockHash, ChannelMonitor<InMemorySigner>)> = read_channel_monitors(
        Arc::clone(&kv_store),
        Arc::clone(&keys_manager),
        Arc::clone(&signer_provider),
    )
    .map_err(|e| {
        log_error!(logger, "Failed to read channel monitors: {e}");
        BuildError::InvalidMonitorData
    })?;

    // ChainMonitor persists full monitors through U3's custom `Persist`
    // (VSS-first dual-write, `InProgress`, per-channel serialized chains —
    // KTD-3); with VSS disabled it degrades to the spike's synchronous
    // local durable-before-Completed behavior.
    let chain_monitor: Arc<ChainMonitor> = Arc::new(ChainMonitor::new(
        Some(Arc::clone(&chain_source)),
        Arc::clone(&broadcaster),
        Arc::clone(&logger),
        Arc::clone(&fee_estimator),
        Arc::clone(&vss_store),
        Arc::clone(&keys_manager),
        keys_manager.get_peer_storage_key(),
    ));
    // Completed monitor writes report back via `channel_monitor_updated`
    // (Weak: the ChainMonitor holds the store, not the other way around).
    vss_store.set_completion_sink(Arc::downgrade(&chain_monitor) as Weak<dyn CompletionSink>);
    // Pre-register restored monitors (MonitorName → VSS key for archives,
    // manifest membership) and backfill the manifest for pre-manifest
    // stores (PWA `backfillManifest`).
    for (_block_hash, monitor) in &channel_monitors {
        vss_store.register_loaded_monitor(monitor);
    }
    vss_store.backfill_manifest_if_needed();

    // Network graph and scorer: reload if present, else start empty.
    let network_graph = match read_optional(
        &kv_store,
        NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
        NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
        NETWORK_GRAPH_PERSISTENCE_KEY,
    )? {
        Some(bytes) => Arc::new(
            Graph::read(&mut Cursor::new(bytes), Arc::clone(&logger))
                .map_err(|_| BuildError::ReadFailed)?,
        ),
        None => Arc::new(Graph::new(config.network, Arc::clone(&logger))),
    };

    let scorer = match read_optional(
        &kv_store,
        SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
        SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
        SCORER_PERSISTENCE_KEY,
    )? {
        Some(bytes) => {
            let args = (
                ProbabilisticScoringDecayParameters::default(),
                Arc::clone(&network_graph),
                Arc::clone(&logger),
            );
            Arc::new(Mutex::new(
                Scorer::read(&mut Cursor::new(bytes), args).map_err(|_| BuildError::ReadFailed)?,
            ))
        }
        None => Arc::new(Mutex::new(ProbabilisticScorer::new(
            ProbabilisticScoringDecayParameters::default(),
            Arc::clone(&network_graph),
            Arc::clone(&logger),
        ))),
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
    let cm_bytes = read_optional(
        &kv_store,
        CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
        CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
        CHANNEL_MANAGER_PERSISTENCE_KEY,
    )?;

    // Whether local LDK state pre-dated this boot: with a persisted manager the
    // bdk wallet's address history was NOT necessarily produced on this device
    // (a restored install writes the manager before its first scan), so the
    // first full scan takes the cold-restore stop gap.
    let had_local_channel_manager = cm_bytes.is_some();

    // U4 integrity check, first half (the PWA's orphaned-monitors halt):
    // monitors with no manager mean channels whose state is lost — starting
    // fresh would silently discard channel funds.
    if cm_bytes.is_none() && !channel_monitors.is_empty() {
        log_error!(
            logger,
            "Found {} channel monitor(s) but no channel manager; refusing to start",
            channel_monitors.len()
        );
        return Err(BuildError::OrphanedMonitors);
    }

    let make_fresh_manager = || -> Result<Arc<ChannelManager>, BuildError> {
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
            user_config.clone(),
            chain_params,
            cur_time.as_secs() as u32,
        ));
        // Persist immediately so the next start always takes the restore
        // path, even if the background processor never got to run. Goes
        // through the dual store: on a fresh VSS-enabled wallet this is
        // the first remote CM write (version 0, authorized by the empty
        // probe this session — KTD-3).
        persist_channel_manager(&channel_manager, &dual_kv_store)?;
        log_info!(logger, "Created fresh channel manager.");
        Ok(channel_manager)
    };

    let channel_manager: Arc<ChannelManager> = match cm_bytes {
        Some(bytes) => {
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
                user_config.clone(),
                monitor_refs,
            );
            match <(BlockHash, ChannelManager)>::read(&mut Cursor::new(bytes), read_args) {
                Ok((_block_hash, channel_manager)) => {
                    log_info!(logger, "Restored channel manager from disk.");
                    let channel_manager = Arc::new(channel_manager);
                    // U4 integrity check, second half (the missing mirror):
                    // a manager referencing a funded channel with no local
                    // monitor is a partial restore — hard halt, never a
                    // silent start.
                    let monitored: HashSet<ChannelId> = channel_monitors
                        .iter()
                        .map(|(_, monitor)| monitor.channel_id())
                        .collect();
                    let funded = channel_manager
                        .list_channels()
                        .into_iter()
                        .filter(|details| details.funding_txo.is_some())
                        .map(|details| details.channel_id);
                    check_channel_monitor_coverage(funded, &monitored).inspect_err(|_| {
                        log_error!(
                            logger,
                            "The channel manager references a funded channel with no local \
                             monitor; refusing to start against a partial set"
                        );
                    })?;
                    channel_manager
                }
                Err(e) if channel_monitors.is_empty() => {
                    // U4 stale-manager defense (PWA init.ts parity): a CM
                    // that fails deserialization with ZERO monitors (e.g. a
                    // stale blob that survived a clear race) is discarded
                    // for a fresh one — no channels means no funds at risk,
                    // and crashing would brick the wallet.
                    log_error!(
                        logger,
                        "Channel manager deserialization failed with no monitors ({e}); \
                         discarding the stale manager and creating fresh"
                    );
                    let _ = kv_store.remove(
                        CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
                        CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
                        CHANNEL_MANAGER_PERSISTENCE_KEY,
                        false,
                    );
                    make_fresh_manager()?
                }
                Err(e) => {
                    log_error!(logger, "Failed to deserialize channel manager: {e}");
                    return Err(BuildError::ReadFailed);
                }
            }
        }
        None => make_fresh_manager()?,
    };

    // KTD-4 cross-client restore fix, BEFORE any chain scan: reveal the
    // deterministic close/sweep destination of every channel this boot loaded
    // to the bdk wallet, so the on-chain side of a restored wallet actually
    // watches where its close funds landed. Both reads above
    // (`read_channel_monitors` and the `ChannelManager` deserialization) ran
    // through `signer_provider`, which recorded each channel's
    // `channel_keys_id`; see `WalletSignerProvider::reveal_derived_destinations`
    // for the full rationale, including the archived-monitor residual. Runs on
    // EVERY boot — plain restarts and `vss::startup::silent_recovery` need the
    // same guarantee, and re-revealing is a monotone no-op.
    match signer_provider.reveal_derived_destinations() {
        Some(max_index) => log_info!(
            logger,
            "Revealed on-chain close destinations for {} loaded channel(s), up to external \
             index {max_index}",
            signer_provider.derived_channel_count()
        ),
        None => log_info!(logger, "No channel close destinations to reveal."),
    }

    // A restore / silent recovery inherits an EMPTY bdk changeset over another
    // client's address history, so its one full scan gets the wider cold-restore
    // stop gap (`BDK_COLD_RESTORE_STOP_GAP`).
    //
    // THIS TEST IS DELIBERATELY LOOSE and gates the STOP GAP ONLY.
    // `had_local_channel_manager` is true on every boot after the very first, so
    // this fires for plain restarts too — acceptable, because a wallet
    // full-scans exactly once and a needlessly wide one-time scan costs a few
    // hundred script queries. It must NOT be used for anything recurring: the
    // steady-state sync window reads the durable
    // `OnchainWallet::revealed_range_from_wide_scan` marker, recorded only when
    // a wide scan actually succeeds, precisely so a restart does not inherit a
    // 200-wide window it has no interior to justify.
    if vss_recovered || !channel_monitors.is_empty() || had_local_channel_manager {
        onchain_wallet.mark_wide_gap_first_scan();
    }

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

    // U11/KTD-8: NO OutputSweeperSync is built — spendable outputs are
    // tracked and swept by the core-owned descriptor store (`crate::sweep`),
    // wired by the node at start. The spike's persisted `output_sweeper`
    // blob (if any) is ignored: spike installs are disposable per plan.

    // U3: `_known_peers` — local mirror + LWW VSS sync (recovery has already
    // written any remotely-recovered peers to the local mirror).
    let known_peers = Arc::new(KnownPeersStore::load(
        Arc::clone(&kv_store),
        Arc::clone(&vss_store),
        Arc::clone(&logger),
    ));

    Ok(NodeComponents {
        kv_store,
        vss_store,
        dual_kv_store,
        known_peers,
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
        chain_synced_at_start,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VssTransportOverride;
    use crate::node::{CoreEvent, LoggingEventSink, Node};
    use crate::vss::known_peers::read_local_known_peers;
    use crate::vss::store::{CHANNEL_MANAGER_VSS_KEY, KNOWN_PEERS_VSS_KEY, MONITOR_MANIFEST_KEY};
    use crate::vss::test_support::MockTransport;

    fn test_sink() -> Arc<dyn EventSink> {
        Arc::new(LoggingEventSink::new())
    }

    /// Offline test config: unreachable RGS, given Esplora, VSS disabled
    /// (spike behavior — the VSS-enabled paths inject a mock transport).
    fn offline_config(dir: &std::path::Path, esplora_url: String) -> Config {
        let mut config = Config::new(dir.to_str().unwrap().to_string());
        config.esplora_url = esplora_url;
        config.rgs_url = "http://127.0.0.1:1/snapshot".to_string();
        config.vss_disabled = true;
        config
    }

    /// Offline config with a mock VSS transport injected (vss enabled).
    fn vss_config(dir: &std::path::Path, transport: &Arc<MockTransport>) -> Config {
        let mut config = offline_config(dir, "http://127.0.0.1:1".to_string());
        config.vss_disabled = false;
        config.vss_transport_override = Some(VssTransportOverride(
            Arc::clone(transport) as Arc<dyn VssTransport>
        ));
        config
    }

    /// Minimal HTTP stub answering every request with `body` (Connection:
    /// close so each request is a fresh socket). Returns the base URL; the
    /// accept loop lives on a detached thread for the life of the test
    /// process.
    fn spawn_http_stub(body: &'static str) -> String {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    /// U12/KTD-12: an Esplora endpoint that ANSWERS `/block-height/0` with a
    /// non-mainnet genesis hash must fail the start with the typed
    /// wrong-network error — never degraded-start against the wrong chain.
    #[test]
    fn wrong_genesis_hash_fails_start_with_typed_error() {
        // Testnet3's genesis hash: a reachable, answering, wrong-network backend.
        let url =
            spawn_http_stub("000000000933ea01ad0ee984209779baaec3ced90fa3f408719526f8d77f4943");
        let dir = tempfile::tempdir().unwrap();
        let config = offline_config(dir.path(), url);

        let rt = test_runtime();
        let result = build(&config, &rt, test_sink());
        assert!(
            matches!(result, Err(BuildError::WrongNetworkBackend)),
            "wrong genesis must fail the build with the typed error"
        );
    }

    /// U12/KTD-12 counterpart: a backend that answers with the RIGHT genesis
    /// hash passes the check (and a fresh zero-monitor node still tolerates
    /// the rest of the sync failing — degraded start preserved).
    #[test]
    fn correct_genesis_hash_passes_the_network_check() {
        let url =
            spawn_http_stub("000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f");
        let dir = tempfile::tempdir().unwrap();
        let config = offline_config(dir.path(), url);

        let rt = test_runtime();
        let components =
            build(&config, &rt, test_sink()).expect("matching genesis must not block the build");
        // The stub serves garbage for every other endpoint, so this is the
        // degraded-start path, not a synced one.
        assert!(!components.chain_synced_at_start);
    }

    // ---------- U3 startup branches (KTD-3) over the mock transport ----------

    #[derive(Default)]
    struct CapturingSink(Mutex<Vec<CoreEvent>>);

    impl EventSink for CapturingSink {
        fn emit(&self, event: CoreEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    /// Creates a local-only wallet in `dir` (offline degraded start) and
    /// returns its node id and the persisted channel-manager bytes.
    fn create_local_wallet(dir: &std::path::Path) -> (String, Vec<u8>) {
        let node = Node::new(offline_config(dir, "http://127.0.0.1:1".to_string()));
        node.start().expect("offline degraded start");
        let node_id = node.node_id().unwrap().to_string();
        node.stop().unwrap();
        let cm_bytes = FilesystemStore::new(dir.join(KV_STORE_SUBDIR))
            .read(
                CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
                CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
                CHANNEL_MANAGER_PERSISTENCE_KEY,
            )
            .expect("channel manager persisted");
        (node_id, cm_bytes)
    }

    /// KTD-3: the durable fenced flag refuses the start with the typed error
    /// before anything else runs — restart never clears a fence.
    #[test]
    fn fenced_flag_blocks_start_with_the_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FENCED_FLAG_FILE_NAME), b"divergent").unwrap();
        let node = Node::new(offline_config(dir.path(), "http://127.0.0.1:1".to_string()));
        assert_eq!(node.start().unwrap_err(), BuildError::Fenced);
        assert!(
            !dir.path().join(crate::keys::MNEMONIC_FILE_NAME).exists(),
            "a fenced start must touch nothing"
        );
    }

    /// Branch (1), empty namespace: a fresh wallet proceeds, records the
    /// empty probe (authorizing version-0 writes), and dual-writes the fresh
    /// channel manager — remote and local CM bytes converge.
    #[test]
    fn fresh_wallet_with_empty_namespace_starts_and_dual_writes_the_manager() {
        let dir = tempfile::tempdir().unwrap();
        let transport = Arc::new(MockTransport::new());
        let node = Node::new(vss_config(dir.path(), &transport));
        node.start().expect("fresh VSS-enabled start");
        node.stop().unwrap();

        let (remote_cm, version) = transport
            .value(CHANNEL_MANAGER_VSS_KEY)
            .expect("the fresh CM must be dual-written to VSS");
        assert!(version >= 1, "first write at 0 lands at server version 1");
        let local_cm = FilesystemStore::new(dir.path().join(KV_STORE_SUBDIR))
            .read(
                CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
                CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
                CHANNEL_MANAGER_PERSISTENCE_KEY,
            )
            .unwrap();
        assert_eq!(remote_cm, local_cm, "remote and local CM converge");
        assert!(!dir.path().join(FENCED_FLAG_FILE_NAME).exists());
    }

    /// Scenario 4 — the fresh-over-backup guard: empty local + non-empty
    /// VSS + a recovery download failure refuses the start with the typed
    /// error, issues NO VSS write, and leaves the remote state unchanged.
    #[test]
    fn fresh_over_backup_guard_refuses_start_and_never_writes_remotely() {
        let dir = tempfile::tempdir().unwrap();
        let transport = Arc::new(MockTransport::new());
        let monitor_key = format!("{}:0", "ab".repeat(32));
        transport.seed(
            MONITOR_MANIFEST_KEY,
            &serde_json::to_vec(&vec![monitor_key.clone()]).unwrap(),
            2,
        );
        transport.seed(CHANNEL_MANAGER_VSS_KEY, b"remote-cm-bytes", 5);
        // The manifest references a monitor the download cannot produce.
        transport.fail_gets_for(&monitor_key, true);
        let before = transport.snapshot();

        let node = Node::new(vss_config(dir.path(), &transport));
        assert_eq!(
            node.start().unwrap_err(),
            BuildError::VssRecoveryFailed,
            "recovery failure on a non-empty namespace is fatal, never fresh-over-backup"
        );
        assert_eq!(transport.put_attempt_count(), 0, "no VSS write issued");
        assert!(
            transport.put_many_calls().is_empty(),
            "no batch write issued"
        );
        assert_eq!(transport.snapshot(), before, "remote state unchanged");
        // Rollback: no partial local LDK state survives.
        let store = FilesystemStore::new(dir.path().join(KV_STORE_SUBDIR));
        assert!(store
            .read(
                CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
                CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
                CHANNEL_MANAGER_PERSISTENCE_KEY,
            )
            .is_err());
    }

    /// A manifest listing a monitor that fails validation (garbage bytes) is
    /// equally fatal — validate-by-deserialization happens BEFORE any local
    /// write.
    #[test]
    fn recovery_rejects_a_monitor_that_fails_deserialization() {
        let dir = tempfile::tempdir().unwrap();
        let transport = Arc::new(MockTransport::new());
        let monitor_key = format!("{}:0", "cd".repeat(32));
        transport.seed(
            MONITOR_MANIFEST_KEY,
            &serde_json::to_vec(&vec![monitor_key.clone()]).unwrap(),
            1,
        );
        transport.seed(
            CHANNEL_MANAGER_VSS_KEY,
            b"remote-cm-bytes-32-or-more.......",
            1,
        );
        transport.seed(&monitor_key, b"not a channel monitor", 1);

        let node = Node::new(vss_config(dir.path(), &transport));
        assert_eq!(node.start().unwrap_err(), BuildError::VssRecoveryFailed);
        assert_eq!(transport.put_attempt_count(), 0);
        let monitors = FilesystemStore::new(dir.path().join(KV_STORE_SUBDIR))
            .list("monitors", "")
            .unwrap_or_default();
        assert!(
            monitors.is_empty(),
            "no invalid monitor may be written locally"
        );
    }

    /// Scenario 5 — mandatory version seeding: local state + a failing probe
    /// is a typed startup error; nothing is written at guessed versions.
    #[test]
    fn version_seed_failure_with_local_state_is_a_typed_startup_error() {
        let dir = tempfile::tempdir().unwrap();
        create_local_wallet(dir.path());

        let transport = Arc::new(MockTransport::new());
        transport
            .fail_list
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let node = Node::new(vss_config(dir.path(), &transport));
        assert_eq!(
            node.start().unwrap_err(),
            BuildError::VssVersionSeedFailed,
            "never guess versions (a lost cache would false-trip the fence)"
        );
        assert_eq!(
            transport.put_attempt_count(),
            0,
            "no writes at guessed versions"
        );
        assert!(transport.put_many_calls().is_empty());
    }

    /// Scenario 7 — migration: local data + empty namespace uploads ONE
    /// transactional batch at version 0 and seeds the cache to 1 (the next
    /// CM write conflicts against neither).
    #[test]
    fn migration_uploads_one_transactional_batch_and_seeds_versions() {
        let dir = tempfile::tempdir().unwrap();
        let (node_id, cm_bytes) = create_local_wallet(dir.path());

        let transport = Arc::new(MockTransport::new());
        let node = Node::new(vss_config(dir.path(), &transport));
        node.start().expect("migration start");
        assert_eq!(node.node_id().unwrap().to_string(), node_id);
        node.stop().unwrap();

        let batches = transport.put_many_calls();
        assert_eq!(batches.len(), 1, "exactly one transactional batch");
        assert_eq!(
            batches[0]
                .iter()
                .map(|(key, bytes, version)| (key.as_str(), bytes.clone(), *version))
                .collect::<Vec<_>>(),
            vec![(CHANNEL_MANAGER_VSS_KEY, cm_bytes, 0)],
            "a zero-channel wallet migrates its CM at version 0 (no empty manifest)"
        );
        // Versions were seeded to 1: the post-migration CM writes succeeded
        // without tripping the fence.
        assert!(!dir.path().join(FENCED_FLAG_FILE_NAME).exists());
        let (_, version) = transport.value(CHANNEL_MANAGER_VSS_KEY).unwrap();
        assert!(
            version >= 2,
            "post-migration CM writes landed on top of the batch"
        );
    }

    /// Migration failure is NON-fatal: the node starts local-only for the
    /// session, `BackupDegraded` is emitted, and nothing else goes remote.
    #[test]
    fn migration_failure_is_non_fatal_and_degrades_to_local_only() {
        let dir = tempfile::tempdir().unwrap();
        create_local_wallet(dir.path());

        let transport = Arc::new(MockTransport::new());
        transport
            .fail_put_many
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let sink = Arc::new(CapturingSink::default());
        let node =
            Node::with_event_sink(vss_config(dir.path(), &transport), Arc::clone(&sink) as _);
        node.start()
            .expect("migration failure must not block the start");
        node.stop().unwrap();

        assert!(
            sink.0
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, CoreEvent::BackupDegraded { .. })),
            "the failed migration surfaces as BackupDegraded"
        );
        assert_eq!(
            transport.put_attempt_count(),
            0,
            "local-only session: no further remote writes after the failed batch"
        );
        assert!(transport.value(CHANNEL_MANAGER_VSS_KEY).is_none());
    }

    /// Silent recovery (branch 1, non-empty namespace): an empty local dir
    /// with the same mnemonic rebuilds the wallet from the remote CM and
    /// known peers, seeds versions, and starts with the same identity.
    #[test]
    fn silent_recovery_rebuilds_the_wallet_from_the_remote_state() {
        let dir_a = tempfile::tempdir().unwrap();
        let (node_id, cm_bytes) = create_local_wallet(dir_a.path());
        let mnemonic =
            std::fs::read_to_string(dir_a.path().join(crate::keys::MNEMONIC_FILE_NAME)).unwrap();

        let transport = Arc::new(MockTransport::new());
        transport.seed(CHANNEL_MANAGER_VSS_KEY, &cm_bytes, 3);
        let peers_json = r#"{"034066e29e402d9cf55af1ae1026cc5adf92eed1e0e421785442f53717ad1453b0": {"host": "64.23.159.177", "port": 9735}}"#;
        transport.seed(KNOWN_PEERS_VSS_KEY, peers_json.as_bytes(), 2);

        // Fresh install, same seed: silent recovery, no user input (R4).
        let dir_b = tempfile::tempdir().unwrap();
        std::fs::write(
            dir_b.path().join(crate::keys::MNEMONIC_FILE_NAME),
            &mnemonic,
        )
        .unwrap();
        let sink = Arc::new(CapturingSink::default());
        let node =
            Node::with_event_sink(vss_config(dir_b.path(), &transport), Arc::clone(&sink) as _);
        node.start().expect("silent recovery start");
        assert_eq!(node.node_id().unwrap().to_string(), node_id);
        node.stop().unwrap();

        let recovered_peers =
            read_local_known_peers(&FilesystemStore::new(dir_b.path().join(KV_STORE_SUBDIR)));
        assert_eq!(recovered_peers.len(), 1, "known peers recovered from VSS");
        assert!(
            !sink
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, CoreEvent::Fenced { .. })),
            "seeded versions: the post-recovery CM writes never conflict"
        );
        let (_, version) = transport.value(CHANNEL_MANAGER_VSS_KEY).unwrap();
        assert!(version >= 4, "CM writes continued at the recovered version");
    }

    /// The U4 restore-in-progress marker voids local LDK state: startup takes
    /// the silent-recovery branch (remote wins), and the marker clears once
    /// recovery is durable.
    #[test]
    fn restore_marker_voids_local_state_and_resumes_silent_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let (node_id, cm_bytes) = create_local_wallet(dir.path());

        let transport = Arc::new(MockTransport::new());
        transport.seed(CHANNEL_MANAGER_VSS_KEY, &cm_bytes, 7);
        std::fs::write(
            dir.path().join(crate::keys::RESTORE_IN_PROGRESS_FILE_NAME),
            b"",
        )
        .unwrap();

        let node = Node::new(vss_config(dir.path(), &transport));
        node.start().expect("marker resumes silent recovery");
        assert_eq!(node.node_id().unwrap().to_string(), node_id);
        node.stop().unwrap();

        assert!(
            transport.get_calls_for(CHANNEL_MANAGER_VSS_KEY) >= 1,
            "the recovery branch downloaded the remote CM"
        );
        assert!(
            !dir.path()
                .join(crate::keys::RESTORE_IN_PROGRESS_FILE_NAME)
                .exists(),
            "the marker clears after recovery is durable"
        );
    }

    /// The integration point of the on-chain sync-cost fix, exercised through
    /// `build`'s REAL decision rather than by calling the wallet's mark method:
    /// a plain restart of a locally-created wallet — no restore, no VSS
    /// recovery, no monitors, just a persisted channel manager — must keep the
    /// CHEAP steady-state sync window.
    ///
    /// `build` sets the per-boot `wide_gap_first_scan` flag here (its condition
    /// includes `cm_bytes.is_some()`, true on every boot after the very first),
    /// and that is fine for the one-time full scan's stop gap. What must NOT
    /// follow is a 200-wide low window on every 120 s tick forever: that is the
    /// ~10x cost regression, and it is what happened while both decisions read
    /// one flag. The durable marker is absent because no wide scan ever
    /// succeeded here (this suite is offline by construction), which is exactly
    /// the state of every established install on restart.
    #[test]
    fn a_restart_over_a_persisted_manager_keeps_the_cheap_sync_window() {
        let dir = tempfile::tempdir().unwrap();
        create_local_wallet(dir.path());

        let rt = test_runtime();
        let components = build(
            &offline_config(dir.path(), "http://127.0.0.1:1".to_string()),
            &rt,
            test_sink(),
        )
        .expect("a restart over a persisted manager starts degraded");
        let wallet = &components.onchain_wallet;

        assert!(
            wallet.wide_gap_first_scan(),
            "build's own condition fires on a persisted manager — the premise of this test"
        );
        assert!(
            !wallet.revealed_range_from_wide_scan(),
            "no wide scan ever succeeded, so the steady-state window must stay cheap"
        );

        // And observably so, on the wallet shape that made the cost visible: one
        // closed channel's KTD-4 destination drags `last_revealed` to 5 030.
        crate::wallet::test_support::fund_confirmed(wallet, 25_000);
        wallet.destination_script_for_index(5_030).unwrap();
        let spks = wallet.sync_request_spks();
        assert!(
            !spks.contains(&wallet.peek_external_script(100)),
            "a mere restart must not watch a 200-wide low window it has no interior to justify"
        );
        assert_eq!(
            spks.len(),
            1 + 20 + 20,
            "1 used + 20 lowest unused + 20 highest revealed: the intended steady-state bound"
        );
    }

    /// U4 integrity mirror: a funded channel without a monitor is the typed
    /// hard halt; full coverage passes. (The wiring in `build` filters to
    /// funded channels and feeds the loaded monitors' channel ids; the
    /// end-to-end partial-restore path needs a real funded channel, which
    /// only the U23 cross-client drill produces.)
    #[test]
    fn channel_monitor_coverage_check_halts_on_a_missing_monitor() {
        let id_a = ChannelId([0xaa; 32]);
        let id_b = ChannelId([0xbb; 32]);
        let monitored: HashSet<ChannelId> = [id_a].into_iter().collect();

        check_channel_monitor_coverage([id_a], &monitored).expect("full coverage passes");
        check_channel_monitor_coverage([], &monitored)
            .expect("monitors without funded channels are the orphan check's job, not this one");
        assert_eq!(
            check_channel_monitor_coverage([id_a, id_b], &monitored).unwrap_err(),
            BuildError::MissingChannelMonitor
        );
    }

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
