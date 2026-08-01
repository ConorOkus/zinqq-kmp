//! `Node`'s peer and channel API: connect/disconnect/forget, the list queries,
//! open/close, and the fee estimates around them — plus the handle bundle these
//! calls clone out of the state lock.
//!
//! Named `channels_api` rather than `channels` so it does not read as the
//! crate-root `crate::channels` module it delegates to. Split out of `node.rs`
//! (see that module's header). Pure move: no behavior, signature, or public
//! visibility change.

use std::collections::{HashMap, HashSet};
use std::str::FromStr as _;
use std::sync::Arc;

use bitcoin::secp256k1::PublicKey;

use crate::channels::{self, ChannelView, ChannelsError, CloseEstimate, OpenFeeEstimate, PeerView};
use crate::liquidity::LiquiditySource;
use crate::node::{spawn_and_wait, Node};
use crate::util::hex_str;

/// The handles the U9 peer/channel calls need, cloned out of the state lock
/// so no dial or list ever holds it.
pub(super) struct ChannelHandles {
    channel_manager: Arc<crate::types::ChannelManager>,
    chain_monitor: Arc<crate::types::ChainMonitor>,
    peer_manager: Arc<crate::types::PeerManager>,
    pub(super) known_peers: Arc<crate::vss::known_peers::KnownPeersStore>,
    onchain_wallet: Arc<crate::wallet::OnchainWallet>,
    chain_source: Arc<crate::chain::ChainSource>,
    liquidity_source: Arc<LiquiditySource>,
    runtime_handle: tokio::runtime::Handle,
}

impl Node {
    /// Clones the U9 peer/channel handles out of the state lock, so no call
    /// below ever blocks while holding it.
    pub(super) fn channel_handles(&self) -> Result<ChannelHandles, ChannelsError> {
        let state_lock = self.state.lock().unwrap();
        let state = state_lock.as_ref().ok_or(ChannelsError::NotRunning)?;
        Ok(ChannelHandles {
            channel_manager: Arc::clone(&state.components.channel_manager),
            chain_monitor: Arc::clone(&state.components.chain_monitor),
            peer_manager: Arc::clone(&state.components.peer_manager),
            known_peers: Arc::clone(&state.components.known_peers),
            onchain_wallet: Arc::clone(&state.components.onchain_wallet),
            chain_source: Arc::clone(&state.components.chain_source),
            liquidity_source: Arc::clone(&state.liquidity_source),
            runtime_handle: state.runtime.handle().clone(),
        })
    }

    /// Dials `node_id` at `socket_addr` (waiting for the BOLT8 handshake) and
    /// persists it to the known-peers store on success — the PWA's
    /// `connectToPeer` semantics (`context.tsx:746-755`). The configured LSP
    /// is dialed through the liquidity source's connect lock instead, so a
    /// racing `receive_jit` is never stranded on a dropped duplicate socket.
    fn dial_and_persist(
        &self,
        handles: &ChannelHandles,
        node_id: PublicKey,
        socket_addr: std::net::SocketAddr,
    ) -> Result<(), ChannelsError> {
        // Run on the node runtime, wait outside the state lock (the
        // receive_jit pattern): a dropped runtime surfaces as a closed
        // channel, not a hang.
        let result = if node_id == self.config.lsp.node_id {
            let liquidity_source = Arc::clone(&handles.liquidity_source);
            spawn_and_wait(&handles.runtime_handle, async move {
                liquidity_source.ensure_lsp_connected().await.map_err(|e| {
                    ChannelsError::ConnectFailed {
                        detail: e.to_string(),
                    }
                })
            })
        } else {
            let peer_manager = Arc::clone(&handles.peer_manager);
            spawn_and_wait(&handles.runtime_handle, async move {
                channels::dial_peer(peer_manager, node_id, socket_addr).await
            })
        };
        result.unwrap_or(Err(ChannelsError::ConnectFailed {
            detail: "the node is shutting down".to_string(),
        }))?;

        // Persist AFTER a successful connect, best-effort surfaced as typed.
        handles
            .known_peers
            .upsert(
                &node_id.to_string(),
                &socket_addr.ip().to_string(),
                socket_addr.port(),
            )
            .map_err(|e| ChannelsError::PersistFailed {
                detail: e.to_string(),
            })
    }

    /// Connects to a `pubkey@host:port` peer and saves it as a known peer
    /// (U9, R10). Blocking (dial + handshake): call from a background
    /// dispatcher. Returns the peer's pubkey hex.
    pub fn connect_peer(&self, address: &str) -> Result<String, ChannelsError> {
        let (node_id, socket_addr) = channels::parse_peer_address(address)?;
        let handles = self.channel_handles()?;
        self.dial_and_persist(&handles, node_id, socket_addr)?;
        Ok(node_id.to_string())
    }

    /// Disconnects a peer's socket (U9). Does NOT forget it: the reconnect
    /// loop will keep dialing saved peers (PWA `disconnectPeer`).
    pub fn disconnect_peer(&self, pubkey: &str) -> Result<(), ChannelsError> {
        let node_id = PublicKey::from_str(pubkey).map_err(|_| ChannelsError::InvalidPubkey)?;
        let handles = self.channel_handles()?;
        handles.peer_manager.disconnect_by_node_id(node_id);
        Ok(())
    }

    /// Removes a saved peer (U9, R10). Refused with
    /// [`ChannelsError::PeerHasOpenChannels`] while any channel with the
    /// peer is open (PWA `forgetPeer`, `context.tsx:852-868`).
    pub fn forget_peer(&self, pubkey: &str) -> Result<(), ChannelsError> {
        let handles = self.channel_handles()?;
        channels::ensure_no_open_channels_with(
            handles
                .channel_manager
                .list_channels()
                .iter()
                .map(|details| details.counterparty.node_id.to_string()),
            pubkey,
        )?;
        handles
            .known_peers
            .remove(pubkey)
            .map_err(|e| ChannelsError::PersistFailed {
                detail: e.to_string(),
            })
    }

    /// The Peers screen's rows (U9, R10): the union of saved and connected
    /// peers, connected first (PWA `Peers.tsx:79-99`).
    pub fn list_peers(&self) -> Result<Vec<PeerView>, ChannelsError> {
        let handles = self.channel_handles()?;
        let connected: HashSet<String> = handles
            .peer_manager
            .list_peers()
            .iter()
            .map(|details| details.counterparty_node_id.to_string())
            .collect();
        let mut channel_counts: HashMap<String, u32> = HashMap::new();
        for details in handles.channel_manager.list_channels() {
            *channel_counts
                .entry(details.counterparty.node_id.to_string())
                .or_insert(0) += 1;
        }
        Ok(channels::build_peer_views(
            &handles.known_peers.all(),
            &connected,
            &channel_counts,
        ))
    }

    /// Every channel as a Peers-screen row (U9, R10), including the in-flight
    /// HTLC count the close screen's warning uses.
    pub fn list_channels(&self) -> Result<Vec<ChannelView>, ChannelsError> {
        let handles = self.channel_handles()?;
        Ok(handles
            .channel_manager
            .list_channels()
            .iter()
            .map(channels::channel_view)
            .collect())
    }

    /// The open-channel review numbers (U9): the 6-block rate × 140 vB (PWA
    /// `OpenChannel.tsx:68-72,97-98`).
    pub fn estimate_open_fee(&self) -> Result<OpenFeeEstimate, ChannelsError> {
        let handles = self.channel_handles()?;
        Ok(channels::open_fee_estimate(
            handles.chain_source.onchain_send_fee_rate_sat_per_vb(),
        ))
    }

    /// Opens a channel to `pubkey@host:port` (U9, R10): bounds
    /// 20,000–16,777,215 sats, balance gate at amount + estimated fee,
    /// connect-if-needed (persisting the known peer), then `create_channel`
    /// with an 8-byte random `user_channel_id` (PWA `OpenChannel.tsx` +
    /// `context.tsx:757-780`). Blocking: call from a background dispatcher.
    /// Returns the TEMPORARY channel id hex; the funding flow proceeds via
    /// the event switchboard (FundingGenerationReady → persist-then-notify →
    /// FundingTxBroadcastSafe → broadcast).
    pub fn open_channel(&self, address: &str, amount_sats: u64) -> Result<String, ChannelsError> {
        channels::check_open_amount(amount_sats)?;
        let (node_id, socket_addr) = channels::parse_peer_address(address)?;
        let handles = self.channel_handles()?;

        // The PWA's balance gate (`OpenChannel.tsx:97-101`): amount plus the
        // 6-block × 140 vB estimate must fit the spendable balance.
        let estimate =
            channels::open_fee_estimate(handles.chain_source.onchain_send_fee_rate_sat_per_vb());
        if amount_sats + estimate.estimated_fee_sats
            > handles.onchain_wallet.trusted_spendable_sats()
        {
            return Err(ChannelsError::AmountExceedsBalance);
        }

        self.dial_and_persist(&handles, node_id, socket_addr)?;

        let temporary_channel_id = handles
            .channel_manager
            .create_channel(
                node_id,
                amount_sats,
                0,
                channels::random_user_channel_id(),
                None,
                None,
            )
            .map_err(|e| ChannelsError::OpenFailed {
                detail: format!("{e:?}"),
            })?;
        Ok(hex_str(&temporary_channel_id.0))
    }

    /// Closes a channel (U9, R10): cooperative `close_channel` or
    /// `force_close_broadcasting_latest_txn` with the PWA's reason string
    /// (`context.tsx:783-813`).
    pub fn close_channel(&self, channel_id_hex: &str, force: bool) -> Result<(), ChannelsError> {
        let handles = self.channel_handles()?;
        let details = handles
            .channel_manager
            .list_channels()
            .into_iter()
            .find(|details| hex_str(&details.channel_id.0) == channel_id_hex)
            .ok_or(ChannelsError::ChannelNotFound)?;
        let result = if force {
            handles.channel_manager.force_close_broadcasting_latest_txn(
                &details.channel_id,
                &details.counterparty.node_id,
                channels::FORCE_CLOSE_REASON.to_string(),
            )
        } else {
            handles
                .channel_manager
                .close_channel(&details.channel_id, &details.counterparty.node_id)
        };
        result.map_err(|e| ChannelsError::CloseFailed {
            detail: format!("{e:?}"),
        })
    }

    /// The informational pre-close estimate (U9, R10): nullable per field
    /// and NEVER an error — a stopped node or unknown channel returns the
    /// all-`None` estimate, so the close screen always renders (PWA
    /// `estimate.ts` contract).
    pub fn estimate_close(&self, channel_id_hex: &str) -> CloseEstimate {
        let Ok(handles) = self.channel_handles() else {
            return CloseEstimate::unavailable();
        };
        channels::estimate_close(
            &handles.channel_manager,
            &handles.chain_monitor,
            &handles.chain_source.fee_estimator(),
            channel_id_hex,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::node::tests::offline_config;

    /// U9 at the Node seam: every peer/channel endpoint is NotRunning while
    /// stopped (except estimate_close, which NEVER errors); once started
    /// (offline, degraded) the lists are empty, the open-fee estimate answers
    /// from the offline default rate, bounds and the balance gate fire before
    /// any dial, an unreachable peer fails typed, and closes of unknown
    /// channels are ChannelNotFound.
    #[test]
    fn channel_endpoints_follow_the_node_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::new(offline_config(dir.path()));
        // Deliberately NOT the configured LSP's pubkey: the LSP is dialed
        // through the liquidity connect lock at its CONFIGURED address, so an
        // LSP-pubkey test would leave the offline sandbox.
        const PEER: &str =
            "02eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619@127.0.0.1:1";
        const PEER_PUBKEY: &str =
            "02eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619";

        // Stopped: typed NotRunning everywhere; estimate_close still answers.
        assert_eq!(
            node.connect_peer(PEER).unwrap_err(),
            ChannelsError::NotRunning
        );
        assert_eq!(node.list_peers().unwrap_err(), ChannelsError::NotRunning);
        assert_eq!(node.list_channels().unwrap_err(), ChannelsError::NotRunning);
        assert_eq!(
            node.open_channel(PEER, 50_000).unwrap_err(),
            ChannelsError::NotRunning
        );
        assert_eq!(
            node.estimate_close(&"11".repeat(32)),
            CloseEstimate::unavailable(),
            "estimate_close never errors, even stopped"
        );

        // Validation fires before the running check reaches a dial: bounds
        // and address parsing are typed regardless.
        assert_eq!(
            node.open_channel(PEER, 19_999).unwrap_err(),
            ChannelsError::AmountBelowMinimum
        );
        assert_eq!(
            node.open_channel(PEER, 16_777_216).unwrap_err(),
            ChannelsError::AmountAboveMaximum
        );
        assert!(matches!(
            node.connect_peer("junk").unwrap_err(),
            ChannelsError::InvalidAddress(_)
        ));

        node.start().expect("offline degraded start");

        // Fresh wallet: no peers, no channels; the open-fee estimate answers
        // from the PWA's offline 6-block default (5 sat/vB × 140 vB).
        assert_eq!(node.list_peers().unwrap(), Vec::new());
        assert_eq!(node.list_channels().unwrap(), Vec::new());
        assert_eq!(
            node.estimate_open_fee().unwrap(),
            crate::channels::OpenFeeEstimate {
                fee_rate_sat_per_vb: 5,
                estimated_fee_sats: 700,
            }
        );
        // The balance gate fires BEFORE any dial (empty wallet).
        assert_eq!(
            node.open_channel(PEER, 50_000).unwrap_err(),
            ChannelsError::AmountExceedsBalance
        );
        // An unreachable peer fails typed, and nothing was persisted.
        assert!(matches!(
            node.connect_peer(PEER).unwrap_err(),
            ChannelsError::ConnectFailed { .. }
        ));
        assert_eq!(node.list_peers().unwrap(), Vec::new());
        // Forgetting with zero channels is allowed (idempotent no-op here).
        assert_eq!(node.forget_peer(PEER_PUBKEY), Ok(()));
        assert_eq!(
            node.disconnect_peer("junk").unwrap_err(),
            ChannelsError::InvalidPubkey
        );
        assert_eq!(node.disconnect_peer(PEER_PUBKEY), Ok(()));
        // Unknown channel: typed not-found for closes, all-None estimate.
        assert_eq!(
            node.close_channel(&"22".repeat(32), false).unwrap_err(),
            ChannelsError::ChannelNotFound
        );
        assert_eq!(
            node.close_channel(&"22".repeat(32), true).unwrap_err(),
            ChannelsError::ChannelNotFound
        );
        assert_eq!(
            node.estimate_close(&"22".repeat(32)),
            CloseEstimate::unavailable()
        );
        node.stop().unwrap();
    }
}
