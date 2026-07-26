//! Small crate-internal helpers shared across modules.

use std::time::{Duration, SystemTime};

use bitcoin::secp256k1::PublicKey;

use crate::types::PeerManager;

/// The duration since the UNIX epoch. Panics if the system clock is set
/// before 1970 (the crate-wide assumption for timestamping).
pub(crate) fn unix_now() -> Duration {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system time before UNIX epoch")
}

/// Whether `node_id` is a currently connected peer (`list_peers` only reports
/// handshake-complete peers).
pub(crate) fn peer_is_connected(peer_manager: &PeerManager, node_id: PublicKey) -> bool {
    peer_manager
        .list_peers()
        .iter()
        .any(|details| details.counterparty_node_id == node_id)
}
