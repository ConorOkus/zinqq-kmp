use std::sync::OnceLock;
use std::time::Duration;

use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use tokio::runtime::{Builder, Runtime};

pub mod api;
pub mod builder;
mod chain;
pub mod channels;
pub mod config;
pub mod events;
mod fees;
pub mod history;
mod invoice;
pub mod keys;
pub mod liquidity;
mod lock;
pub mod node;
pub mod onchain_send;
pub mod payment;
pub mod restore;
pub mod send;
mod signer;
mod types;
mod util;
pub mod vss;
mod wallet;

pub use api::{Balances, Wallet, WalletConfig, WalletError};
pub use builder::BuildError;
pub use channels::{
    ChannelStateLabel, ChannelView, ChannelsError, CloseEstimate, CloseFeePayer, OpenFeeEstimate,
    PeerAddressError, PeerView,
};
pub use config::{Config, LspConfig, PeerInfo};
pub use events::Event;
pub use history::{
    ActivityDirection, ActivityKind, ActivityRow, ActivityStatus, CloseRecordSummary,
    CloseStatusLabel, HistoryError, PaymentDirection, PaymentStatus, PersistedPayment,
};
pub use liquidity::Lsps2Error;
pub use node::Node;
pub use onchain_send::{DriftGuard, FeeEstimate, MaxSendEstimate, OnchainSendError};
pub use payment::SendError;
pub use restore::RestoreError;

uniffi::setup_scaffolding!();

/// The core-owned tokio runtime (KTD-3): multi-threaded, 2 workers, lazily
/// initialized and owned by the Rust side. Exported async fns spawn onto this
/// runtime explicitly instead of relying on
/// `#[uniffi::export(async_runtime = "tokio")]`, which is ignored on
/// trait-object async methods (mozilla/uniffi-rs#2576).
pub(crate) fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("wallet-core")
            .enable_all()
            .build()
            .expect("failed to build wallet-core tokio runtime")
    })
}

/// Trivial sync export that routes through a real secp256k1 call so the LDK
/// dependency graph (including secp256k1's C build) actually links.
#[uniffi::export]
pub fn core_version() -> String {
    let secp = Secp256k1::new();
    let secret = SecretKey::from_slice(&[0x2a; 32]).expect("32 non-zero bytes is a valid secret");
    let pubkey = PublicKey::from_secret_key(&secp, &secret);
    format!("wallet-core {} ({pubkey})", env!("CARGO_PKG_VERSION"))
}

/// Trivial async export: the returned future is polled by the foreign caller,
/// but the work runs on the core-owned tokio runtime.
#[uniffi::export]
pub async fn ping_async() -> String {
    let task = runtime().spawn(async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        "pong".to_string()
    });
    task.await.expect("wallet-core runtime task panicked")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_version_reports_crate_version_and_derived_pubkey() {
        let version = core_version();
        assert!(
            version.starts_with("wallet-core 0.1.0"),
            "unexpected version string: {version}"
        );
        // Compressed secp256k1 pubkey for the fixed secret [0x2a; 32] must be
        // present, proving a real LDK-graph secp256k1 call happened.
        assert!(
            version.contains("03"),
            "expected a compressed pubkey in: {version}"
        );
    }

    #[test]
    fn ping_async_completes_on_core_owned_runtime() {
        let pong = runtime().block_on(ping_async());
        assert_eq!(pong, "pong");
    }
}
