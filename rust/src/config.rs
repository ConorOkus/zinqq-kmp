//! Node configuration: network, storage location, and the external service
//! endpoints (Esplora, Rapid Gossip Sync, peers). Everything that would
//! otherwise be a hardcoded URL at a call site lives here (KTD-5, KTD-6).

use std::net::SocketAddr;
use std::time::Duration;

use bitcoin::secp256k1::PublicKey;
use bitcoin::Network;
use lightning::util::config::UserConfig;

/// Default Esplora endpoint (KTD-5). Keyless, esplora-compatible, actively
/// maintained. To fall back, swap in [`ESPLORA_FALLBACK_URL`].
pub const DEFAULT_ESPLORA_URL: &str = "https://mempool.space/api";

/// One-line fallback Esplora endpoint per KTD-5.
pub const ESPLORA_FALLBACK_URL: &str = "https://blockstream.info/api";

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
        }
    }
}

/// The LDK `UserConfig` used for both fresh and restored channel managers.
///
/// `manually_accept_inbound_channels` is set from day one; the rest of the
/// KTD-9 0-conf JIT cluster (trusted-peer 0conf acceptance, underpaying-HTLC
/// acceptance, JIT CLTV floor) lands with the LSPS2 client in U4.
pub(crate) fn default_user_config() -> UserConfig {
    let mut user_config = UserConfig::default();
    user_config.manually_accept_inbound_channels = true;
    user_config
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
    }

    #[test]
    fn user_config_manually_accepts_inbound_channels() {
        assert!(default_user_config().manually_accept_inbound_channels);
    }
}
