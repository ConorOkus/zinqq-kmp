use std::sync::OnceLock;
use std::time::Duration;

use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use tokio::runtime::{Builder, Runtime};

pub mod builder;
mod chain;
pub mod config;
mod fees;
pub mod node;
mod types;
mod wallet;

pub use builder::BuildError;
pub use config::{Config, PeerInfo};
pub use node::Node;

uniffi::setup_scaffolding!();

/// The core-owned tokio runtime (KTD-3): multi-threaded, 2 workers, lazily
/// initialized and owned by the Rust side. Exported async fns spawn onto this
/// runtime explicitly instead of relying on
/// `#[uniffi::export(async_runtime = "tokio")]`, which is ignored on
/// trait-object async methods (mozilla/uniffi-rs#2576).
fn runtime() -> &'static Runtime {
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
