//! Node configuration: network, storage location, and the external service
//! endpoints (Esplora, VSS, Rapid Gossip Sync, peers). Everything that would
//! otherwise be a hardcoded URL at a call site lives here (KTD-5, KTD-6), and
//! U12 grows it to full PWA infrastructure parity (KTD-12): VSS/explorer
//! defaults, LSP override + trusted-LSP set, the network-keyed constants
//! module, shared JIT channel constants (KTD-10), and the unified invoice
//! description strings.

use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;

use bitcoin::secp256k1::PublicKey;
use bitcoin::Network;
use lightning::util::config::UserConfig;

/// Every network-dependent constant, keyed by the one network this wallet
/// runs on (KTD-12, mainnet-audit learning): adding a second network means
/// adding a sibling module, not hunting hardcoded values across call sites.
/// Currency/bech32 prefixes are derived from [`mainnet::NETWORK`] by
/// `lightning-invoice`/`bdk` and need no separate constants.
pub mod mainnet {
    use bitcoin::Network;

    /// The network itself. [`super::Config`] is fixed to this.
    pub const NETWORK: Network = Network::Bitcoin;

    /// Bitcoin mainnet's genesis block hash, compared against the Esplora
    /// backend's `/block-height/0` answer at startup (U12/KTD-12): a backend
    /// that answers with anything else is serving the wrong chain and the
    /// start fails hard with a typed error.
    pub const GENESIS_BLOCK_HASH: &str =
        "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";

    /// [`GENESIS_BLOCK_HASH`] parsed.
    pub fn genesis_block_hash() -> bitcoin::BlockHash {
        GENESIS_BLOCK_HASH
            .parse()
            .expect("the mainnet genesis hash constant is valid")
    }
}

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

/// Default VSS endpoint (U12/KTD-12): the Zinqq PWA's pass-through proxy
/// (adds no trust; the direct origin is configurable via [`Config::vss_url`]).
pub const DEFAULT_VSS_URL: &str = "https://zinqq.app/api/vss-proxy";

/// Default block-explorer base URL for outbound transaction links
/// (U12/KTD-12), matching the PWA's `https://mempool.space/tx/<txid>` links.
pub const DEFAULT_EXPLORER_URL: &str = "https://mempool.space";

/// Description on standard (non-JIT) receive invoices, matching the PWA's
/// `createInvoice` default (`src/ldk/context.tsx`: 'Zinqq Wallet').
pub const RECEIVE_INVOICE_DESCRIPTION: &str = "Zinqq Wallet";

/// Description on JIT (LSPS2-wrapped) invoices, matching what the PWA's
/// Receive flow passes to `executeJitBuy` (`src/pages/Receive.tsx`:
/// 'zinqq wallet' — deliberately distinct from the standard-receive string).
pub const JIT_INVOICE_DESCRIPTION: &str = "zinqq wallet";

/// Description on the persistent BOLT12 offer, matching the PWA's
/// `builder.description('zinqq wallet')` (`src/ldk/context.tsx:1657`) — the
/// same lowercase string the JIT invoices carry (U7, R6).
pub const BOLT12_OFFER_DESCRIPTION: &str = "zinqq wallet";

/// Shared JIT channel constants (U12/KTD-10), the single source of truth for
/// the two settings a JIT (LSP-opened, 0-conf) receive depends on — mirroring
/// the PWA's `jit-channel-config.ts`. They are applied in two places that
/// MUST stay in agreement:
///   - wallet-globally in [`default_user_config`] (the safety net);
///   - per-channel via `ChannelConfigOverrides` on the 0-conf accept
///     (`LiquiditySource::on_open_channel_request`).
///
/// Accept HTLCs that pay less than the invoice amount: the LSP deducts its
/// opening fee before forwarding, so the arriving JIT HTLC is below the
/// invoice amount; the fee is validated at invoice-creation time.
pub(crate) const JIT_ACCEPT_UNDERPAYING_HTLCS: bool = true;

/// Allow the full channel capacity for a single inbound HTLC: LDK's default
/// (10%) is too restrictive for JIT channels, where the entire payment
/// arrives in one HTLC that may be close to channel capacity (KTD-10).
pub(crate) const JIT_MAX_INBOUND_INFLIGHT_PCT: u8 = 100;

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

/// How often the background task POLLS whether the fee-rate cache needs a
/// refresh (U12/KTD-9). The actual refresh cadence is governed by the cache's
/// 60 s TTL and 15 s failure backoff (see [`crate::fees`]); this tick equals
/// the backoff so a failed refresh is retried as soon as the backoff allows.
pub(crate) const FEE_UPDATE_INTERVAL: Duration = Duration::from_secs(15);

/// How often the background task refreshes the RGS snapshot.
pub(crate) const RGS_SYNC_INTERVAL: Duration = Duration::from_secs(3600);

/// How often the reconnect loop checks configured peers.
pub(crate) const PEER_RECONNECT_INTERVAL: Duration = Duration::from_secs(10);

/// BDK full-scan stop gap / request concurrency. The steady-state value, used
/// for the full scan of a wallet THIS device created: its revealed indices and
/// its history were produced here, so BIP44's conventional 20-address gap is
/// ample.
pub(crate) const BDK_CLIENT_STOP_GAP: usize = 20;
pub(crate) const BDK_CLIENT_CONCURRENCY: usize = 4;

/// Stop gap for the FIRST full scan of a wallet that came from a restore or a
/// silent recovery (U4/KTD-3) — a cold start over someone else's address
/// history.
///
/// A cross-client restore inherits an EMPTY bdk changeset: nothing local
/// records how many addresses the PWA revealed before the seed moved here, and
/// [`BDK_CLIENT_STOP_GAP`] (20) gives no room for a wallet that burned
/// addresses without receiving to them (every Receive-screen tap, every
/// shutdown script, every sweep destination advances the PWA's external index
/// whether or not the address is ever paid). 20 consecutive unused addresses is
/// a realistic gap on a wallet that has been used for a while; 200 is not.
///
/// 200 is one order of magnitude of headroom for ~400 extra Esplora script
/// queries (external + internal) on a ONE-TIME scan, and it is deliberately far
/// short of the 10 000-index deterministic destination space: those indices are
/// reached by DERIVATION (see
/// [`crate::signer::WalletSignerProvider::reveal_derived_destinations`]), never
/// by brute-force scanning, so this number does not have to cover them.
pub(crate) const BDK_COLD_RESTORE_STOP_GAP: usize = 200;

/// Bound on ONE on-chain wallet sync pass.
///
/// This pass is awaited inline in the background loop's `select!`, alongside the
/// lightning sync tick and the stop signal, so an unbounded pass starves both.
/// It is deliberately much larger than [`CHAIN_SYNC_TIMEOUT`]: the widest
/// legitimate pass is a cold-restore full scan, roughly
/// `2 * BDK_COLD_RESTORE_STOP_GAP` script queries at
/// [`BDK_CLIENT_CONCURRENCY`], which at realistic per-request latency lands in
/// tens of seconds — this leaves several times that headroom before cutting a
/// dragging backend loose. A timed-out pass is not fund-relevant: nothing is
/// persisted, `initial_scan_complete` stays false, and the next tick retries
/// from scratch.
pub(crate) const ONCHAIN_SYNC_TIMEOUT: Duration = Duration::from_secs(180);

/// How many revealed SPKs per keychain each END of the revealed range
/// contributes to the INCREMENTAL (steady-state) on-chain sync — see
/// [`crate::wallet::OnchainWallet::bounded_sync_request`]. Governs nothing
/// about the full scan (that still uses the stop gaps above).
///
/// WHY A WINDOW AT ALL: bdk's `start_sync_with_revealed_spks` queries EVERY
/// revealed SPK, and the KTD-4 close-destination scheme reveals at
/// `BE(channel_keys_id[0..4]) mod 10_000`, so ONE closed channel drags
/// `last_revealed` to ~5 000 on average (the indices are uniform over
/// 0..9 999). Measured on a real mainnet wallet with `last_revealed` at 5 030:
/// 804 s (13.4 min) between sync writes against an
/// [`ONCHAIN_SYNC_INTERVAL`] of 120 s, through the Vercel→Blockstream proxy,
/// with one pass tripping [`CHAIN_SYNC_TIMEOUT`]. Correct but useless: an
/// incoming payment took 13 minutes to appear. The ~5 000 indices in between
/// were never handed to anyone — `reveal_addresses_to` is INCLUSIVE, so they
/// are collateral of revealing the one destination that matters.
///
/// WHY 20: the same anchor as [`BDK_CLIENT_STOP_GAP`] — BIP44's conventional
/// 20-address gap limit. A wallet is only ever vended addresses from the two
/// ends of its revealed range (`next_unused_address` returns the LOWEST unused
/// index; `reveal_next_address` returns `last_revealed + 1`), and 20 is the
/// industry-standard bound on how far a gap of unpaid-but-vended addresses
/// realistically runs. Deliberately the SAME number as the full scan's stop
/// gap: a steady-state sync that watched a narrower window than the scan that
/// discovered the wallet would be able to miss what the scan found.
pub(crate) const ONCHAIN_SYNC_KEYCHAIN_WINDOW: usize = 20;

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

/// Test-only seam (U3): replaces the VSS wire transport with an in-process
/// fake so startup branches (recovery / migration / seeding / fence) run
/// deterministically without a network. Never constructed outside tests.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct VssTransportOverride(
    pub(crate) std::sync::Arc<dyn crate::vss::store::VssTransport>,
);

#[cfg(test)]
impl std::fmt::Debug for VssTransportOverride {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VssTransportOverride(..)")
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
/// Has no mnemonic input: the 12 words are auto-generated on first start and
/// persisted write-once inside `storage_dir` (U1, R1). Restore-from-words is
/// a separate destructive flow (U4), not a constructor parameter.
#[derive(Clone, Debug)]
pub struct Config {
    /// Bitcoin network the node runs on (fixed to [`mainnet::NETWORK`]).
    pub network: Network,
    /// App-private data directory holding the mnemonic, channel monitors, and
    /// all other persisted state.
    pub storage_dir: String,
    /// Esplora REST endpoint.
    pub esplora_url: String,
    /// Rapid Gossip Sync snapshot server.
    pub rgs_url: String,
    /// VSS endpoint for encrypted remote state backup (U12/KTD-12).
    pub vss_url: String,
    /// Disables VSS entirely (local-only persistence) when `true`.
    pub vss_disabled: bool,
    /// Block-explorer base URL for outbound transaction links.
    pub explorer_url: String,
    /// Peers to keep connected while the node runs.
    pub peers: Vec<PeerInfo>,
    /// The LSPS2 liquidity provider (defaults to Megalith).
    pub lsp: LspConfig,
    /// LSP node ids trusted for 0-conf inbound channels (KTD-10: kept as a
    /// set + [`Config::is_trusted_lsp`] predicate, never a single hardcoded
    /// pubkey). Seeded with Megalith; the configured [`Config::lsp`] is
    /// always trusted too (mirroring the PWA's `trustedLspIds`).
    pub trusted_lsps: Vec<PublicKey>,
    /// Test-only in-process VSS transport (see [`VssTransportOverride`]).
    #[cfg(test)]
    pub(crate) vss_transport_override: Option<VssTransportOverride>,
}

impl Config {
    /// Mainnet defaults with the given app-private storage directory.
    pub fn new(storage_dir: String) -> Self {
        Self {
            network: mainnet::NETWORK,
            storage_dir,
            esplora_url: DEFAULT_ESPLORA_URL.to_string(),
            rgs_url: DEFAULT_RGS_URL.to_string(),
            vss_url: DEFAULT_VSS_URL.to_string(),
            vss_disabled: false,
            explorer_url: DEFAULT_EXPLORER_URL.to_string(),
            peers: Vec::new(),
            lsp: LspConfig::megalith(),
            trusted_lsps: vec![PublicKey::from_str(MEGALITH_LSP_NODE_ID)
                .expect("Megalith node id constant is a valid public key")],
            #[cfg(test)]
            vss_transport_override: None,
        }
    }

    /// Whether `node_id` may open 0-conf channels to us (KTD-10). The
    /// configured LSP is implicitly trusted, so an LSP override does not need
    /// to be repeated in [`Config::trusted_lsps`].
    pub fn is_trusted_lsp(&self, node_id: &PublicKey) -> bool {
        *node_id == self.lsp.node_id || self.trusted_lsps.contains(node_id)
    }
}

/// The LDK `UserConfig` used for both fresh and restored channel managers —
/// the full KTD-10 parity cluster, mirroring the PWA's `createUserConfig`
/// (`src/ldk/user-config.ts`) field for field. The per-channel half (0-conf
/// acceptance from the trusted-LSP set with `ChannelConfigOverrides`) is
/// applied in `LiquiditySource::on_open_channel_request` from the same shared
/// JIT constants, so the override can never silently drift from the global
/// default. Pinned by the U12 snapshot test below.
pub(crate) fn default_user_config() -> UserConfig {
    let mut config = UserConfig {
        manually_accept_inbound_channels: true,
        ..Default::default()
    };
    // LSPS2 JIT channels require option_scid_alias (the invoice references
    // the channel before confirmation).
    config.channel_handshake_config.negotiate_scid_privacy = true;
    // Anchor channels (zero-fee HTLC anchors): the LSP opens anchor channels.
    config
        .channel_handshake_config
        .negotiate_anchors_zero_fee_htlc_tx = true;
    // JIT payments arrive as one HTLC that may be close to channel capacity.
    config
        .channel_handshake_config
        .max_inbound_htlc_value_in_flight_percent_of_channel = JIT_MAX_INBOUND_INFLIGHT_PCT;
    // 0-conf inbound channels from trusted peers (the LSP set).
    config.channel_handshake_limits.trust_own_funding_0conf = true;
    // LDK rejects opens whose announce flag differs from our default with
    // "announcement preference is different from ours"; some LSPs diverge, so
    // the check is off (retained defensively from the PWA).
    config
        .channel_handshake_limits
        .force_announced_channel_preference = false;
    // The LSP deducts its opening fee before forwarding; allow claiming the
    // underpaying HTLC (the fee is validated at invoice creation).
    config.channel_config.accept_underpaying_htlcs = JIT_ACCEPT_UNDERPAYING_HTLCS;
    config
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
        assert_eq!(config.vss_url, "https://zinqq.app/api/vss-proxy");
        assert!(!config.vss_disabled);
        assert_eq!(config.explorer_url, "https://mempool.space");
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
        assert_eq!(
            config.trusted_lsps,
            vec![PublicKey::from_str(MEGALITH_LSP_NODE_ID).unwrap()],
            "the trusted-LSP set is seeded with Megalith"
        );
    }

    /// U12/KTD-10: the 0-conf gate is a set + predicate, never a single
    /// pubkey compare; an overridden LSP is implicitly trusted.
    #[test]
    fn trusted_lsp_predicate_covers_the_seed_set_and_the_configured_lsp() {
        let mut config = Config::new("/tmp/data".to_string());
        let megalith = PublicKey::from_str(MEGALITH_LSP_NODE_ID).unwrap();
        let other = PublicKey::from_str(
            "02eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619",
        )
        .unwrap();
        assert!(config.is_trusted_lsp(&megalith));
        assert!(!config.is_trusted_lsp(&other));

        // Overriding the LSP trusts the override without dropping the seed.
        config.lsp.node_id = other;
        assert!(config.is_trusted_lsp(&other));
        assert!(config.is_trusted_lsp(&megalith));
    }

    /// U12/KTD-12: the network-keyed module carries mainnet's constants.
    #[test]
    fn mainnet_module_pins_the_genesis_hash_and_network() {
        assert_eq!(mainnet::NETWORK, Network::Bitcoin);
        assert_eq!(
            mainnet::genesis_block_hash().to_string(),
            "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"
        );
    }

    /// U12: invoice descriptions unified to the PWA's strings (standard
    /// receive vs JIT are deliberately different in the PWA).
    #[test]
    fn invoice_descriptions_match_the_pwa_strings() {
        assert_eq!(RECEIVE_INVOICE_DESCRIPTION, "Zinqq Wallet");
        assert_eq!(JIT_INVOICE_DESCRIPTION, "zinqq wallet");
    }

    #[test]
    fn user_config_manually_accepts_inbound_channels() {
        assert!(default_user_config().manually_accept_inbound_channels);
    }

    /// U12/KTD-10 snapshot: pins EVERY parity field of the `UserConfig` to
    /// the PWA's `createUserConfig` (`src/ldk/user-config.ts`). A changed
    /// default breaks the build here, on purpose.
    #[test]
    fn user_config_snapshot_pins_the_full_pwa_parity_cluster() {
        let config = default_user_config();
        assert!(
            config.manually_accept_inbound_channels,
            "manually_accept_inbound_channels must be true (0-conf JIT gate)"
        );
        assert!(
            config.channel_handshake_config.negotiate_scid_privacy,
            "negotiate_scid_privacy must be true (LSPS2 needs option_scid_alias)"
        );
        assert!(
            config
                .channel_handshake_config
                .negotiate_anchors_zero_fee_htlc_tx,
            "negotiate_anchors_zero_fee_htlc_tx must be true (LSP opens anchor channels)"
        );
        assert_eq!(
            config
                .channel_handshake_config
                .max_inbound_htlc_value_in_flight_percent_of_channel,
            100,
            "inbound in-flight must be 100% (JIT payments arrive as one big HTLC)"
        );
        assert!(
            config.channel_handshake_limits.trust_own_funding_0conf,
            "trust_own_funding_0conf must be true (0-conf from trusted LSPs)"
        );
        assert!(
            !config
                .channel_handshake_limits
                .force_announced_channel_preference,
            "force_announced_channel_preference must be false (LSP announce flags diverge)"
        );
        assert!(
            config.channel_config.accept_underpaying_htlcs,
            "accept_underpaying_htlcs must be true (the LSP skims its opening fee)"
        );
    }
}
