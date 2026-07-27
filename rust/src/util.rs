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

/// Milliseconds since the UNIX epoch as a `u64` — the crate-wide timestamp
/// format for records and events.
pub(crate) fn now_ms() -> u64 {
    unix_now().as_millis() as u64
}

/// Lowercase hex of a byte slice (payment hashes/ids, channel ids) — the id
/// format the payment store and the public events share (U5).
pub(crate) fn hex_str(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Whether `node_id` is a currently connected peer (`list_peers` only reports
/// handshake-complete peers).
pub(crate) fn peer_is_connected(peer_manager: &PeerManager, node_id: PublicKey) -> bool {
    peer_manager
        .list_peers()
        .iter()
        .any(|details| details.counterparty_node_id == node_id)
}
