//! Node lifecycle (KTD-3, KTD-10): the `Node` owns a 2-worker tokio runtime
//! created at `start()` and dropped at `stop()`. The background processor runs
//! via `process_events_async_with_kv_store_sync` with
//! `mobile_interruptable_platform = true`; periodic chain sync, fee refresh,
//! RGS refresh, broadcast draining, and peer reconnects run as runtime tasks
//! stopped through watch channels.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use bitcoin::secp256k1::PublicKey;
use lightning::chain::Confirm;
use lightning::events::{Event, ReplayEvent};
use lightning::log_error;
use lightning::log_info;
use lightning::util::logger::Logger as _;
use lightning_background_processor::{
    process_events_async_with_kv_store_sync, GossipSync, NO_LIQUIDITY_MANAGER,
};
use tokio::runtime::Runtime;
use tokio::sync::watch;

use crate::builder::{build, persist_channel_manager, BuildError, NodeComponents};
use crate::config::{
    Config, FEE_UPDATE_INTERVAL, LIGHTNING_SYNC_INTERVAL, ONCHAIN_SYNC_INTERVAL,
    PEER_RECONNECT_INTERVAL, RGS_SYNC_INTERVAL,
};
use crate::types::{Logger, Sweeper};

/// Internal core events. This is the seam U3's persisted event queue plugs
/// into: the queue will implement [`EventSink`] and map/extend these into the
/// public FFI `Event` enum. Until then a logging sink consumes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CoreEvent {
    /// A background chain sync pass reached the tip.
    ChainSyncCompleted,
    /// A background chain sync pass failed; it will be retried.
    ChainSyncFailed,
}

/// Consumer of [`CoreEvent`]s (U3 seam).
pub(crate) trait EventSink: Send + Sync {
    fn emit(&self, event: CoreEvent);
}

struct LoggingEventSink {
    logger: Arc<Logger>,
}

impl EventSink for LoggingEventSink {
    fn emit(&self, event: CoreEvent) {
        log_info!(self.logger, "Core event: {event:?}");
    }
}

struct RunningState {
    runtime: Runtime,
    components: NodeComponents,
    /// Stops the sync/broadcast/reconnect tasks.
    stop_sender: watch::Sender<()>,
    /// Stops the background processor (which persists on the way out).
    bp_stop_sender: watch::Sender<()>,
    bp_handle: tokio::task::JoinHandle<()>,
    chain_synced: Arc<AtomicBool>,
}

/// A foreground-only mainnet LDK node over the wallet-core stack.
///
/// There is deliberately no way to construct a `Node` from an existing seed or
/// mnemonic (AE2): the only input is a [`Config`], and entropy is generated
/// into the storage dir on first start.
pub struct Node {
    config: Config,
    state: Mutex<Option<RunningState>>,
    event_sink: Arc<dyn EventSink>,
}

impl Node {
    /// Creates a stopped node handle for the given config.
    pub fn new(config: Config) -> Self {
        let event_sink = Arc::new(LoggingEventSink {
            logger: Arc::new(Logger),
        });
        Self {
            config,
            state: Mutex::new(None),
            event_sink,
        }
    }

    /// Starts the node: creates the runtime, assembles (fresh or restore),
    /// and spawns the background processor and periodic tasks.
    ///
    /// Fails hard with a typed [`BuildError`] on restore/persistence problems;
    /// tolerates an unreachable Esplora when no channel monitors exist (a
    /// degraded start — see [`Node::is_chain_synced`]).
    pub fn start(&self) -> Result<(), BuildError> {
        let mut state_lock = self.state.lock().unwrap();
        if state_lock.is_some() {
            return Err(BuildError::AlreadyRunning);
        }

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("wallet-core-node")
            .enable_all()
            .build()
            .map_err(|_| BuildError::RuntimeSetupFailed)?;

        let components = build(&self.config, &runtime)?;
        let chain_synced = Arc::new(AtomicBool::new(components.chain_synced_at_start));

        let (stop_sender, _) = watch::channel(());
        let (bp_stop_sender, _) = watch::channel(());

        self.spawn_broadcast_task(&runtime, &components, stop_sender.subscribe());
        self.spawn_sync_task(
            &runtime,
            &components,
            stop_sender.subscribe(),
            Arc::clone(&chain_synced),
        );
        self.spawn_peer_reconnect_task(&runtime, &components, stop_sender.subscribe());
        let bp_handle =
            spawn_background_processor(&runtime, &components, bp_stop_sender.subscribe());

        *state_lock = Some(RunningState {
            runtime,
            components,
            stop_sender,
            bp_stop_sender,
            bp_handle,
            chain_synced,
        });
        Ok(())
    }

    /// Stops the node: signals every task, waits for the background processor
    /// (which persists manager/graph/scorer on exit), persists the manager
    /// once more, disconnects peers, and drops the runtime.
    pub fn stop(&self) -> Result<(), BuildError> {
        let state = self
            .state
            .lock()
            .unwrap()
            .take()
            .ok_or(BuildError::NotRunning)?;
        let RunningState {
            runtime,
            components,
            stop_sender,
            bp_stop_sender,
            bp_handle,
            ..
        } = state;

        let _ = stop_sender.send(());
        let _ = bp_stop_sender.send(());
        let _ = runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(15), bp_handle).await });

        // The background processor already persisted on exit; this is the
        // belt-and-braces write for the paths where it never got to run.
        let persist_res =
            persist_channel_manager(&components.channel_manager, &components.kv_store);
        if let Err(e) = persist_res {
            log_error!(
                components.logger,
                "Failed to persist channel manager on stop: {e}"
            );
        }

        components.peer_manager.disconnect_all_peers();
        runtime.shutdown_timeout(Duration::from_secs(5));
        persist_res
    }

    /// Whether the node is currently running.
    pub fn is_running(&self) -> bool {
        self.state.lock().unwrap().is_some()
    }

    /// The node id, once running.
    pub fn node_id(&self) -> Option<PublicKey> {
        self.state
            .lock()
            .unwrap()
            .as_ref()
            .map(|state| state.components.channel_manager.get_our_node_id())
    }

    /// Whether the last chain sync pass (including the one at start) reached
    /// the tip. `false` while stopped or after a degraded start.
    pub fn is_chain_synced(&self) -> bool {
        self.state
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|state| state.chain_synced.load(Ordering::Acquire))
    }

    /// Total on-chain balance (confirmed + pending) in sats, once running.
    pub fn onchain_balance_sats(&self) -> Option<u64> {
        self.state
            .lock()
            .unwrap()
            .as_ref()
            .map(|state| state.components.onchain_wallet.balance().total().to_sat())
    }

    fn spawn_broadcast_task(
        &self,
        runtime: &Runtime,
        components: &NodeComponents,
        mut stop_receiver: watch::Receiver<()>,
    ) {
        let broadcaster = Arc::clone(&components.broadcaster);
        let chain_source = Arc::clone(&components.chain_source);
        runtime.spawn(async move {
            let mut queue = broadcaster.queue().await;
            loop {
                tokio::select! {
                    _ = stop_receiver.changed() => return,
                    package = queue.recv() => match package {
                        Some(package) => chain_source.process_broadcast_package(package).await,
                        None => return,
                    },
                }
            }
        });
    }

    fn spawn_sync_task(
        &self,
        runtime: &Runtime,
        components: &NodeComponents,
        mut stop_receiver: watch::Receiver<()>,
        chain_synced: Arc<AtomicBool>,
    ) {
        let chain_source = Arc::clone(&components.chain_source);
        let channel_manager = Arc::clone(&components.channel_manager);
        let chain_monitor = Arc::clone(&components.chain_monitor);
        let sweeper = Arc::clone(&components.sweeper);
        let onchain_wallet = Arc::clone(&components.onchain_wallet);
        let gossip_source = Arc::clone(&components.gossip_source);
        let logger = Arc::clone(&components.logger);
        let event_sink = Arc::clone(&self.event_sink);

        runtime.spawn(async move {
            let mut lightning_interval = tokio::time::interval(LIGHTNING_SYNC_INTERVAL);
            lightning_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut onchain_interval = tokio::time::interval(ONCHAIN_SYNC_INTERVAL);
            onchain_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut fee_interval = tokio::time::interval(FEE_UPDATE_INTERVAL);
            fee_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut rgs_interval = tokio::time::interval(RGS_SYNC_INTERVAL);
            rgs_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    _ = stop_receiver.changed() => return,
                    _ = lightning_interval.tick() => {
                        let confirmables: Vec<&(dyn Confirm + Sync + Send)> = vec![
                            &*channel_manager,
                            &*chain_monitor,
                            &*sweeper,
                        ];
                        let now_synced = match chain_source.sync_confirmables(confirmables).await {
                            Ok(()) => true,
                            Err(e) => {
                                log_error!(logger, "Background chain sync failed: {e}");
                                false
                            }
                        };
                        let was_synced = chain_synced.swap(now_synced, Ordering::AcqRel);
                        if was_synced != now_synced {
                            event_sink.emit(if now_synced {
                                CoreEvent::ChainSyncCompleted
                            } else {
                                CoreEvent::ChainSyncFailed
                            });
                        }
                    }
                    _ = onchain_interval.tick() => {
                        if let Err(e) = chain_source.sync_onchain_wallet(&onchain_wallet).await {
                            log_error!(logger, "On-chain wallet sync failed: {e}");
                        }
                    }
                    _ = fee_interval.tick() => {
                        if let Err(e) = chain_source.update_fee_rate_estimates().await {
                            log_error!(logger, "Fee rate update failed: {e}");
                        }
                    }
                    _ = rgs_interval.tick() => {
                        if let Err(e) = gossip_source.update_rgs_snapshot().await {
                            log_error!(logger, "RGS snapshot update failed: {e}");
                        }
                    }
                }
            }
        });
    }

    fn spawn_peer_reconnect_task(
        &self,
        runtime: &Runtime,
        components: &NodeComponents,
        mut stop_receiver: watch::Receiver<()>,
    ) {
        if self.config.peers.is_empty() {
            return;
        }
        let peers = self.config.peers.clone();
        let peer_manager = Arc::clone(&components.peer_manager);
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(PEER_RECONNECT_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = stop_receiver.changed() => return,
                    _ = interval.tick() => {
                        for peer in &peers {
                            let connected = peer_manager
                                .list_peers()
                                .iter()
                                .any(|details| details.counterparty_node_id == peer.node_id);
                            if !connected {
                                if let Some(connection) = lightning_net_tokio::connect_outbound(
                                    Arc::clone(&peer_manager),
                                    peer.node_id,
                                    peer.address,
                                )
                                .await
                                {
                                    tokio::spawn(connection);
                                }
                            }
                        }
                    }
                }
            }
        });
    }
}

/// Handles LDK events. Only durable handling of `SpendableOutputs` matters for
/// U2 (fund safety on channel close); every other variant is tolerated by
/// logging and acking. U3/U4/U5 extend this with the payment/channel logic and
/// route events into the public queue.
fn handle_ldk_event(
    event: Event,
    sweeper: &Sweeper,
    logger: &Arc<Logger>,
) -> Result<(), ReplayEvent> {
    match event {
        Event::SpendableOutputs {
            outputs,
            channel_id,
        } => {
            // `track_spendable_outputs` persists before returning Ok; on
            // failure we replay the event rather than dropping funds.
            // Static outputs are NOT excluded: our SignerProvider is the bare
            // KeysManager, so the sweeper (not the bdk wallet) owns them.
            sweeper
                .track_spendable_outputs(outputs, channel_id, false, None)
                .map_err(|()| {
                    log_error!(
                        logger,
                        "Failed to persist spendable outputs; replaying event"
                    );
                    ReplayEvent()
                })
        }
        other => {
            log_info!(logger, "Acking unhandled LDK event: {other:?}");
            Ok(())
        }
    }
}

fn spawn_background_processor(
    runtime: &Runtime,
    components: &NodeComponents,
    bp_stop_receiver: watch::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    let kv_store = Arc::clone(&components.kv_store);
    let chain_monitor = Arc::clone(&components.chain_monitor);
    let channel_manager = Arc::clone(&components.channel_manager);
    let onion_messenger = Arc::clone(&components.onion_messenger);
    let gossip_sync = components.gossip_source.gossip_sync();
    let peer_manager = Arc::clone(&components.peer_manager);
    let sweeper = Arc::clone(&components.sweeper);
    let scorer = Arc::clone(&components.scorer);
    let logger = Arc::clone(&components.logger);
    let error_logger = Arc::clone(&components.logger);

    let event_sweeper = Arc::clone(&components.sweeper);
    let event_logger = Arc::clone(&components.logger);
    let event_handler = move |event: Event| {
        let sweeper = Arc::clone(&event_sweeper);
        let logger = Arc::clone(&event_logger);
        async move { handle_ldk_event(event, &sweeper, &logger) }
    };

    let sleeper = move |duration: Duration| {
        let mut stop = bp_stop_receiver.clone();
        Box::pin(async move {
            tokio::select! {
                _ = stop.changed() => true,
                _ = tokio::time::sleep(duration) => false,
            }
        })
    };

    runtime.spawn(async move {
        let res = process_events_async_with_kv_store_sync(
            kv_store,
            event_handler,
            chain_monitor,
            channel_manager,
            Some(onion_messenger),
            GossipSync::rapid(gossip_sync),
            peer_manager,
            // U4 wires the LiquidityManager into this slot (and the peer
            // manager's custom message handler).
            NO_LIQUIDITY_MANAGER,
            Some(sweeper),
            logger,
            Some(scorer),
            sleeper,
            // KTD-3: mobile platform, interruptible sleeps.
            true,
            || {
                Some(
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .expect("system time before UNIX epoch"),
                )
            },
        )
        .await;
        if let Err(e) = res {
            log_error!(error_logger, "Background processor exited with error: {e}");
        }
    })
}
