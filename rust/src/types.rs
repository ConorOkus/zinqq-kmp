//! Concrete instantiations of LDK's generic types (à la ldk-node `src/types.rs`)
//! plus the crate's `lightning::util::logger::Logger` implementation.

use std::sync::{Arc, Mutex};

use lightning::chain::chainmonitor;
use lightning::ln::peer_handler::IgnoringMessageHandler;
use lightning::onion_message::messenger::{
    DefaultMessageRouter, OnionMessenger as LdkOnionMessenger,
};
use lightning::routing::gossip::NetworkGraph;
use lightning::routing::router::DefaultRouter;
use lightning::routing::scoring::{ProbabilisticScorer, ProbabilisticScoringFeeParameters};
use lightning::sign::{InMemorySigner, KeysManager};
use lightning::util::logger::{Logger as LdkLogger, Record};
use lightning::util::persist::KVStoreSyncWrapper;
use lightning::util::sweep::OutputSweeperSync;
use lightning_liquidity::utils::time::DefaultTimeProvider;
use lightning_net_tokio::SocketDescriptor;
use lightning_persister::fs_store::FilesystemStore;

use crate::chain::{Broadcaster, ChainSource};
use crate::fees::CachedFeeEstimator;
use crate::wallet::OnchainWallet;

pub(crate) type Graph = NetworkGraph<Arc<Logger>>;

pub(crate) type Scorer = ProbabilisticScorer<Arc<Graph>, Arc<Logger>>;

pub(crate) type Router = DefaultRouter<
    Arc<Graph>,
    Arc<Logger>,
    Arc<KeysManager>,
    Arc<Mutex<Scorer>>,
    ProbabilisticScoringFeeParameters,
    Scorer,
>;

pub(crate) type MessageRouter = DefaultMessageRouter<Arc<Graph>, Arc<Logger>, Arc<KeysManager>>;

/// The `Persist` implementation is the blanket `impl Persist for K: KVStoreSync`
/// on [`FilesystemStore`]: full-monitor writes under LDK's persist key
/// constants, durable before `Completed` (KTD-4).
pub(crate) type ChainMonitor = chainmonitor::ChainMonitor<
    InMemorySigner,
    Arc<ChainSource>,
    Arc<Broadcaster>,
    Arc<CachedFeeEstimator>,
    Arc<Logger>,
    Arc<FilesystemStore>,
    Arc<KeysManager>,
>;

pub(crate) type ChannelManager = lightning::ln::channelmanager::ChannelManager<
    Arc<ChainMonitor>,
    Arc<Broadcaster>,
    Arc<KeysManager>,
    Arc<KeysManager>,
    Arc<KeysManager>,
    Arc<CachedFeeEstimator>,
    Arc<Router>,
    Arc<MessageRouter>,
    Arc<Logger>,
>;

pub(crate) type OnionMessenger = LdkOnionMessenger<
    Arc<KeysManager>,
    Arc<KeysManager>,
    Arc<Logger>,
    Arc<ChannelManager>,
    Arc<MessageRouter>,
    Arc<ChannelManager>,
    Arc<ChannelManager>,
    IgnoringMessageHandler,
    IgnoringMessageHandler,
>;

/// The LSPS2 client (U4). The async-`KVStore` variant is required because the
/// background processor's `_with_kv_store_sync` entry point still takes the
/// async `ALiquidityManager` bound; `KVStoreSyncWrapper` adapts our
/// `FilesystemStore`. Client-only (no service config), system clock.
pub(crate) type LiquidityManager = lightning_liquidity::LiquidityManager<
    Arc<KeysManager>,
    Arc<KeysManager>,
    Arc<ChannelManager>,
    Arc<ChainSource>,
    KVStoreSyncWrapper<Arc<FilesystemStore>>,
    DefaultTimeProvider,
    Arc<Broadcaster>,
>;

/// Gossip arrives via RGS (KTD-6), so the routing message handler is ignoring;
/// the custom message handler slot carries the `LiquidityManager` — without
/// this, LSPS2 silently does nothing (KTD-9).
pub(crate) type PeerManager = lightning::ln::peer_handler::PeerManager<
    SocketDescriptor,
    Arc<ChannelManager>,
    Arc<IgnoringMessageHandler>,
    Arc<OnionMessenger>,
    Arc<Logger>,
    Arc<LiquidityManager>,
    Arc<KeysManager>,
    Arc<ChainMonitor>,
>;

pub(crate) type RapidGossipSync =
    lightning_rapid_gossip_sync::RapidGossipSync<Arc<Graph>, Arc<Logger>>;

pub(crate) type Sweeper = OutputSweeperSync<
    Arc<Broadcaster>,
    Arc<OnchainWallet>,
    Arc<CachedFeeEstimator>,
    Arc<ChainSource>,
    Arc<FilesystemStore>,
    Arc<Logger>,
    Arc<KeysManager>,
>;

/// Minimal stderr logger for the spike; platform log routing can replace this
/// later without touching the type graph.
pub(crate) struct Logger;

impl LdkLogger for Logger {
    fn log(&self, record: Record) {
        eprintln!(
            "{} [{}:{}] {}",
            record.level, record.module_path, record.line, record.args
        );
    }
}
