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
use crate::signer::WalletSignerProvider;
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

/// The `Persist` slot carries U3's [`VssBackedStore`] (KTD-3): full-monitor
/// VSS-first dual writes returning `InProgress` with per-channel serialized
/// completion, degrading to synchronous local durable-before-`Completed`
/// writes when VSS is disabled.
pub(crate) type ChainMonitor = chainmonitor::ChainMonitor<
    InMemorySigner,
    Arc<ChainSource>,
    Arc<Broadcaster>,
    Arc<CachedFeeEstimator>,
    Arc<Logger>,
    Arc<crate::vss::store::VssBackedStore>,
    Arc<KeysManager>,
>;

/// The signer-provider slot (5th param) carries U1's custom
/// [`WalletSignerProvider`] (KTD-4): PWA-parity `channel_keys_id` HMAC
/// derivation and bdk-backed destination/shutdown scripts; entropy and node
/// signing stay on the bare `KeysManager`.
pub(crate) type ChannelManager = lightning::ln::channelmanager::ChannelManager<
    Arc<ChainMonitor>,
    Arc<Broadcaster>,
    Arc<KeysManager>,
    Arc<KeysManager>,
    Arc<WalletSignerProvider>,
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

/// The sweeper persists through the same `KVStoreSync` the background
/// processor uses (the BP's generics require it); its `output_sweeper` key
/// routes local-only inside [`crate::vss::DualWriteKvStore`].
pub(crate) type Sweeper = OutputSweeperSync<
    Arc<Broadcaster>,
    Arc<OnchainWallet>,
    Arc<CachedFeeEstimator>,
    Arc<ChainSource>,
    Arc<crate::vss::DualWriteKvStore>,
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
