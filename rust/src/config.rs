//! Node configuration: network, storage location, and the external service
//! endpoints (Esplora, Rapid Gossip Sync, peers). Everything that would
//! otherwise be a hardcoded URL at a call site lives here (KTD-5, KTD-6).

use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;

use bitcoin::secp256k1::PublicKey;
use bitcoin::Network;
use lightning::util::config::UserConfig;

/// Default Esplora endpoint (KTD-5): the Zinqq PWA's own proxy, which fronts
/// Blockstream Enterprise staging and holds the credentials server-side, so the
/// spike shares the production client's chain infrastructure without embedding
/// a key. Measured at ~0.2-0.7s per request where the public mempool.space
/// endpoint throttled a single request to 75s under this repo's own test
/// volume, which stalled every sync pass. Swap in a fallback below if needed.
pub const DEFAULT_ESPLORA_URL: &str = "https://zinqq.app/api/esplora";

/// Public fallbacks per KTD-5, in preference order. Blockstream's open endpoint
/// is the faster of the two in practice; mempool.space throttles aggressively.
pub const ESPLORA_FALLBACK_URL: &str = "https://blockstream.info/api";

/// Second fallback: keyless, esplora-compatible, but rate-limits hard.
pub const ESPLORA_PUBLIC_FALLBACK_URL: &str = "https://mempool.space/api";

/// LDK's public Rapid Gossip Sync server (KTD-6).
pub const DEFAULT_RGS_URL: &str = "https://rapidsync.lightningdevkit.org/snapshot";

/// Timeout for individual Esplora HTTP requests.
pub(crate) const ESPLORA_CLIENT_TIMEOUT_SECS: u64 = 10;

/// Upper bound on a full lightning-wallet `Confirm` sync pass.
pub(crate) const CHAIN_SYNC_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound on a fee-rate cache refresh.
pub(crate) const FEE_UPDATE_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on an RGS snapshot download.
pub(crate) const RGS_SYNC_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound on a single transaction broadcast.
pub(crate) const TX_BROADCAST_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the background task re-syncs the lightning wallet. Kept
/// conservative so a two-device spike stays far below public rate limits
/// (KTD-5: retry frequency is capped).
pub(crate) const LIGHTNING_SYNC_INTERVAL: Duration = Duration::from_secs(30);

/// How often the background task re-syncs the on-chain (bdk) wallet.
pub(crate) const ONCHAIN_SYNC_INTERVAL: Duration = Duration::from_secs(120);

/// How often the background task refreshes the fee-rate cache.
pub(crate) const FEE_UPDATE_INTERVAL: Duration = Duration::from_secs(300);

/// How often the background task refreshes the RGS snapshot.
pub(crate) const RGS_SYNC_INTERVAL: Duration = Duration::from_secs(3600);

/// How often the reconnect loop checks configured peers.
pub(crate) const PEER_RECONNECT_INTERVAL: Duration = Duration::from_secs(10);

/// BDK full-scan stop gap / request concurrency.
pub(crate) const BDK_CLIENT_STOP_GAP: usize = 20;
pub(crate) const BDK_CLIENT_CONCURRENCY: usize = 4;

/// Megalith's LSPS2 node id (U4), taken from the Zinqq PWA's own working
/// configuration (`VITE_LSP_NODE_ID`) rather than a public explorer listing.
/// The explorer-listed `038a9e56...e889bf` at 64.23.162.51 completes a BOLT8
/// handshake but never answers `lsps2.get_info`; it is not the LSPS2 service.
pub const MEGALITH_LSP_NODE_ID: &str =
    "034066e29e402d9cf55af1ae1026cc5adf92eed1e0e421785442f53717ad1453b0";

/// Megalith's LSPS2 listening address, matching the PWA's `VITE_LSP_HOST`.
pub const MEGALITH_LSP_ADDRESS: &str = "64.23.159.177:9735";

/// Timeout for one LSPS2 request round-trip (`lsps2.get_info` or `lsps2.buy`),
/// copied from ldk-node's `LIQUIDITY_REQUEST_TIMEOUT_SECS`.
pub(crate) const LSPS2_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on the connect-on-demand handshake with the LSP peer.
pub(crate) const LSP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The LSPS2 liquidity provider the node buys JIT channels from (U4).
#[derive(Clone, Debug)]
pub struct LspConfig {
    /// The LSP's node id.
    pub node_id: PublicKey,
    /// The LSP's TCP socket address.
    pub address: SocketAddr,
    /// Optional LSPS2 token (API key / coupon code) sent with `get_info`.
    pub token: Option<String>,
}

impl LspConfig {
    /// Megalith, the spike's LSP (no token required).
    pub fn megalith() -> Self {
        Self {
            node_id: PublicKey::from_str(MEGALITH_LSP_NODE_ID)
                .expect("Megalith node id constant is a valid public key"),
            address: MEGALITH_LSP_ADDRESS
                .parse()
                .expect("Megalith address constant is a valid socket address"),
            token: None,
        }
    }
}

/// A peer the node keeps connected while running (direct TCP per R7).
/// U4 configures Megalith here.
#[derive(Clone, Debug)]
pub struct PeerInfo {
    /// The peer's node id.
    pub node_id: PublicKey,
    /// The peer's TCP socket address.
    pub address: SocketAddr,
}

/// Top-level node configuration.
///
/// Deliberately has no seed/mnemonic input (AE2): the seed is always generated
/// fresh on first start and persisted inside `storage_dir` (KTD-11).
#[derive(Clone, Debug)]
pub struct Config {
    /// Bitcoin network the node runs on.
    pub network: Network,
    /// App-private data directory holding the seed, channel monitors, and all
    /// other persisted state.
    pub storage_dir: String,
    /// Esplora REST endpoint.
    pub esplora_url: String,
    /// Rapid Gossip Sync snapshot server.
    pub rgs_url: String,
    /// Peers to keep connected while the node runs.
    pub peers: Vec<PeerInfo>,
    /// The LSPS2 liquidity provider (defaults to Megalith).
    pub lsp: LspConfig,
}

impl Config {
    /// Mainnet defaults with the given app-private storage directory.
    pub fn new(storage_dir: String) -> Self {
        Self {
            network: Network::Bitcoin,
            storage_dir,
            esplora_url: DEFAULT_ESPLORA_URL.to_string(),
            rgs_url: DEFAULT_RGS_URL.to_string(),
            peers: Vec::new(),
            lsp: LspConfig::megalith(),
        }
    }
}

/// The LDK `UserConfig` used for both fresh and restored channel managers.
///
/// `manually_accept_inbound_channels` is the config half of the KTD-9 0-conf
/// JIT cluster; the per-channel half (trusted-peer 0conf acceptance and the
/// underpaying-HTLC override) is applied when the LSP's `OpenChannelRequest`
/// is accepted (see `LiquiditySource::on_open_channel_request`), copying
/// ldk-node's `Event::OpenChannelRequest` handling.
pub(crate) fn default_user_config() -> UserConfig {
    UserConfig {
        manually_accept_inbound_channels: true,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_point_at_mainnet_and_public_services() {
        let config = Config::new("/tmp/data".to_string());
        assert_eq!(config.network, Network::Bitcoin);
        assert_eq!(config.esplora_url, DEFAULT_ESPLORA_URL);
        assert_eq!(config.rgs_url, DEFAULT_RGS_URL);
        assert!(config.peers.is_empty());
        assert_eq!(
            config.lsp.node_id,
            PublicKey::from_str(MEGALITH_LSP_NODE_ID).unwrap()
        );
        assert_eq!(
            config.lsp.address,
            MEGALITH_LSP_ADDRESS.parse::<SocketAddr>().unwrap()
        );
        assert!(config.lsp.token.is_none());
    }

    #[test]
    fn user_config_manually_accepts_inbound_channels() {
        assert!(default_user_config().manually_accept_inbound_channels);
    }
}
