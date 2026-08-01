//! `Node`'s on-chain surface: fee and max-send estimates, the two send paths,
//! and the next receive address — plus the handle bundle a single send clones
//! out of the state lock.
//!
//! Split out of `node.rs` (see that module's header). Pure move: no behavior,
//! signature, or public visibility change.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use lightning::chain::chaininterface::BroadcasterInterface as _;

use crate::node::{Node, OnchainSyncPause};
use crate::onchain_send::{self, OnchainSendError};

/// The handles a single on-chain send/estimate needs, cloned out of the
/// state lock so the send never holds it (U8).
struct OnchainHandles {
    wallet: Arc<crate::wallet::OnchainWallet>,
    broadcaster: Arc<crate::chain::Broadcaster>,
    /// 6-block rate, ceil'd, clamped >= 2 sat/vB (KTD-9).
    fee_rate_sat_per_vb: u64,
    /// 10,000 sats iff at least one channel is open (R7), read from the
    /// channel manager at call time.
    reserve_sats: u64,
    sync_paused: Arc<AtomicBool>,
    sync_now: Arc<tokio::sync::Notify>,
}

impl Node {
    /// Clones the U8 send handles out of the state lock; reserve and fee
    /// rate are read at call time (channel count from the channel manager,
    /// rate from the fee cache).
    fn onchain_handles(&self) -> Result<OnchainHandles, OnchainSendError> {
        let state_lock = self.state.lock().unwrap();
        let state = state_lock.as_ref().ok_or(OnchainSendError::NotRunning)?;
        Ok(OnchainHandles {
            wallet: Arc::clone(&state.components.onchain_wallet),
            broadcaster: Arc::clone(&state.components.broadcaster),
            fee_rate_sat_per_vb: state
                .components
                .chain_source
                .onchain_send_fee_rate_sat_per_vb(),
            reserve_sats: onchain_send::anchor_reserve_sats(
                state.components.channel_manager.list_channels().len(),
            ),
            sync_paused: Arc::clone(&state.onchain_sync_paused),
            sync_now: Arc::clone(&state.onchain_sync_now),
        })
    }

    /// Broadcasts a built-and-signed on-chain send via the persist-first
    /// Broadcaster (U12/KTD-9 sentinels), then wakes the immediate wallet
    /// sync (the PWA's post-broadcast `syncNow`). Returns the txid.
    fn dispatch_onchain_tx(handles: &OnchainHandles, tx: &bitcoin::Transaction) -> String {
        handles.broadcaster.broadcast_transactions(&[tx]);
        handles.sync_now.notify_one();
        tx.compute_txid().to_string()
    }

    /// Fee estimate for an exact-amount on-chain send (U8, R7): builds the
    /// tx at the 6-block rate WITHOUT broadcasting; fees above 50,000 sats
    /// are the typed too-high error (KTD-9).
    pub fn estimate_onchain_fee(
        &self,
        address: &str,
        amount_sats: u64,
    ) -> Result<crate::onchain_send::FeeEstimate, OnchainSendError> {
        let handles = self.onchain_handles()?;
        onchain_send::estimate_fee(
            &handles.wallet,
            self.config.network,
            address,
            amount_sats,
            handles.fee_rate_sat_per_vb,
        )
    }

    /// Max-sendable estimate (U8, R7): drain build minus the anchor reserve
    /// when channels exist; dust floor from the recipient script.
    pub fn estimate_max_sendable(
        &self,
        address: &str,
    ) -> Result<crate::onchain_send::MaxSendEstimate, OnchainSendError> {
        let handles = self.onchain_handles()?;
        onchain_send::estimate_max_sendable(
            &handles.wallet,
            self.config.network,
            address,
            handles.reserve_sats,
            handles.fee_rate_sat_per_vb,
        )
    }

    /// Exact-amount on-chain send (U8, R7): reserve post-check, then the
    /// broadcast-boundary drift + fee guards, then the persist-first
    /// broadcast; sync is paused around the build and `sync_now` follows the
    /// dispatch. `expected_*` are the review-screen values (R5 drift guard).
    pub fn send_onchain(
        &self,
        address: &str,
        amount_sats: u64,
        expected_amount_sats: u64,
        expected_fee_sats: u64,
    ) -> Result<String, OnchainSendError> {
        let handles = self.onchain_handles()?;
        let expected = onchain_send::DriftGuard::for_address(
            address,
            self.config.network,
            expected_amount_sats,
            expected_fee_sats,
        )?;
        let _pause = OnchainSyncPause::engage(&handles.sync_paused);
        let tx = onchain_send::send_to_address(
            &handles.wallet,
            self.config.network,
            address,
            amount_sats,
            &expected,
            handles.reserve_sats,
            handles.fee_rate_sat_per_vb,
        )?;
        Ok(Self::dispatch_onchain_tx(&handles, &tx))
    }

    /// On-chain send-max (U8, AE6): drains fully at zero channels; with
    /// channels the built tx leaves exactly 10,000 sats as an explicit
    /// reserve output to an internal address. Same drift guard, pause, and
    /// persist-first broadcast as [`Node::send_onchain`].
    pub fn send_onchain_max(
        &self,
        address: &str,
        expected_amount_sats: u64,
        expected_fee_sats: u64,
    ) -> Result<String, OnchainSendError> {
        let handles = self.onchain_handles()?;
        let expected = onchain_send::DriftGuard::for_address(
            address,
            self.config.network,
            expected_amount_sats,
            expected_fee_sats,
        )?;
        let _pause = OnchainSyncPause::engage(&handles.sync_paused);
        let tx = onchain_send::send_max(
            &handles.wallet,
            self.config.network,
            address,
            &expected,
            handles.reserve_sats,
            handles.fee_rate_sat_per_vb,
        )?;
        Ok(Self::dispatch_onchain_tx(&handles, &tx))
    }

    /// Next unused receive address on the external keychain (U8): the
    /// changeset is persisted after the reveal, so a restart keeps the index.
    pub fn next_receive_address(&self) -> Result<String, OnchainSendError> {
        let wallet = {
            let state_lock = self.state.lock().unwrap();
            let state = state_lock.as_ref().ok_or(OnchainSendError::NotRunning)?;
            Arc::clone(&state.components.onchain_wallet)
        };
        wallet
            .next_receive_address()
            .map_err(|()| OnchainSendError::BuildFailed {
                detail: "failed to persist the address reveal".to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::node::tests::offline_config;
    use crate::node::Node;

    /// U8 at the Node seam: every on-chain endpoint is NotRunning while
    /// stopped; once started (offline, degraded), the receive path serves a
    /// mainnet address and persists the reveal across a restart, and an
    /// empty wallet's estimates fail typed, never panic.
    #[test]
    fn onchain_endpoints_follow_the_node_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::new(offline_config(dir.path()));
        const ADDR: &str = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";

        assert_eq!(
            node.next_receive_address().unwrap_err(),
            OnchainSendError::NotRunning
        );
        assert_eq!(
            node.estimate_onchain_fee(ADDR, 10_000).unwrap_err(),
            OnchainSendError::NotRunning
        );
        assert_eq!(
            node.send_onchain(ADDR, 10_000, 10_000, 100).unwrap_err(),
            OnchainSendError::NotRunning
        );
        assert_eq!(
            node.send_onchain_max(ADDR, 10_000, 100).unwrap_err(),
            OnchainSendError::NotRunning
        );

        node.start().expect("offline degraded start");
        let address = node.next_receive_address().unwrap();
        assert!(address.starts_with("bc1q"), "BIP84 mainnet address");
        // Zero channels: the reserve is inactive, so an empty wallet's max
        // estimate fails on the balance, not the reserve (R7).
        assert_eq!(
            node.estimate_max_sendable(ADDR).unwrap_err(),
            OnchainSendError::BalanceTooLow
        );
        assert!(matches!(
            node.estimate_onchain_fee(ADDR, 10_000).unwrap_err(),
            OnchainSendError::BuildFailed { .. }
        ));
        node.stop().unwrap();

        // The reveal survives the restart (address-reveal learning).
        node.start().expect("offline degraded restart");
        assert_eq!(node.next_receive_address().unwrap(), address);
        node.stop().unwrap();
    }
}
