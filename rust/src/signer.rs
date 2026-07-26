//! Custom `SignerProvider` (U1, KTD-4, R2): all signing key material comes
//! from LDK's `KeysManager`, but `channel_keys_id` generation and destination
//! scripts reproduce the PWA (`zinq/src/ldk/traits/bdk-signer-provider.ts`)
//! byte-for-byte so channel state and close funds recover identically on
//! either client:
//!
//! - `generate_channel_keys_id` = HMAC-SHA256(hmac_key, `[inbound u8]`
//!   `[user_channel_id low 64 BE][high 64 BE]`) — deterministic, so recovery
//!   re-derives the same ids; the hmac key is purpose-limited (the master
//!   seed is never held here).
//! - `get_destination_script` = the bdk external address at index
//!   `BE(first 4 bytes of channel_keys_id) mod 10_000` — deterministic, so a
//!   restored wallet watches the same close scripts.
//! - `get_shutdown_scriptpubkey` = bdk `next_unused_address` —
//!   non-deterministic by design (PWA parity): shutdown scripts are recorded
//!   at channel open and replayed from serialized state, so determinism is
//!   scoped to destination scripts only.

use std::sync::Arc;

use bitcoin::hashes::Hash;
use bitcoin::{ScriptBuf, WPubkeyHash};
use lightning::ln::script::ShutdownScript;
use lightning::log_error;
use lightning::sign::{InMemorySigner, KeysManager, SignerProvider};
use lightning::util::logger::Logger as _;
use zeroize::Zeroize;

use crate::keys::hmac_sha256;
use crate::types::Logger;
use crate::wallet::OnchainWallet;

/// The destination-index space: `u32 mod 10_000` (PWA parity). Bounds how many
/// addresses `reveal_addresses_to` must track; collisions are harmless (both
/// channels pay the same wallet-owned address) but reduce privacy.
const DESTINATION_INDEX_SPACE: u32 = 10_000;

/// The node's `SignerProvider`: wraps [`KeysManager`] for signing, overrides
/// id generation and close destinations per KTD-4.
pub(crate) struct WalletSignerProvider {
    keys_manager: Arc<KeysManager>,
    wallet: Arc<OnchainWallet>,
    channel_keys_id_hmac_key: [u8; 32],
    logger: Arc<Logger>,
}

impl WalletSignerProvider {
    pub(crate) fn new(
        keys_manager: Arc<KeysManager>,
        wallet: Arc<OnchainWallet>,
        channel_keys_id_hmac_key: [u8; 32],
        logger: Arc<Logger>,
    ) -> Self {
        Self {
            keys_manager,
            wallet,
            channel_keys_id_hmac_key,
            logger,
        }
    }
}

impl Drop for WalletSignerProvider {
    fn drop(&mut self) {
        self.channel_keys_id_hmac_key.zeroize();
    }
}

/// Deterministic destination index for a `channel_keys_id`: big-endian u32
/// from the first 4 bytes, mod 10,000 (`zinq/src/onchain/address-utils.ts`).
pub(crate) fn destination_index(channel_keys_id: &[u8; 32]) -> u32 {
    let raw = u32::from_be_bytes(
        channel_keys_id[0..4]
            .try_into()
            .expect("4 bytes from a 32-byte id"),
    );
    raw % DESTINATION_INDEX_SPACE
}

impl SignerProvider for WalletSignerProvider {
    type EcdsaSigner = InMemorySigner;

    fn generate_channel_keys_id(&self, inbound: bool, user_channel_id: u128) -> [u8; 32] {
        // PWA wire format (bdk-signer-provider.ts:43-68), reproduced exactly:
        // [1 byte inbound][8 bytes ucid low 64 BE][8 bytes ucid high 64 BE].
        let mut msg = [0u8; 17];
        msg[0] = u8::from(inbound);
        msg[1..9].copy_from_slice(&(user_channel_id as u64).to_be_bytes());
        msg[9..17].copy_from_slice(&((user_channel_id >> 64) as u64).to_be_bytes());
        hmac_sha256(&self.channel_keys_id_hmac_key, &msg)
    }

    fn derive_channel_signer(&self, channel_keys_id: [u8; 32]) -> InMemorySigner {
        self.keys_manager.derive_channel_keys(&channel_keys_id)
    }

    fn get_destination_script(&self, channel_keys_id: [u8; 32]) -> Result<ScriptBuf, ()> {
        // No fallback to KeysManager: its destination script is an address the
        // bdk wallet does not watch, so falling back would make close funds
        // appear lost. An error fails the channel operation gracefully.
        self.wallet
            .destination_script_for_index(destination_index(&channel_keys_id))
    }

    fn get_shutdown_scriptpubkey(&self) -> Result<ShutdownScript, ()> {
        let script = self.wallet.next_unused_address_script()?;
        if !script.is_p2wpkh() {
            // BIP84 descriptors only ever yield p2wpkh; anything else means
            // the wallet is misconfigured — refuse rather than guess.
            log_error!(
                self.logger,
                "Shutdown script from the bdk wallet is not p2wpkh: {script:?}"
            );
            return Err(());
        }
        let hash = WPubkeyHash::from_slice(&script.as_bytes()[2..]).map_err(|_| ())?;
        Ok(ShutdownScript::new_p2wpkh(&hash))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use bitcoin::Network;
    use lightning::sign::ChannelSigner;
    use lightning_persister::fs_store::FilesystemStore;

    use super::*;
    use crate::keys::{derive_wallet_keys, parse_mnemonic, tests::TEST_MNEMONIC};

    // Cross-implementation vectors from `gen_vectors.mjs` (see
    // `crate::keys::tests`): the PWA's generate_channel_keys_id and
    // destination-index scheme evaluated over the test mnemonic's hmac key.
    const EXPECTED_KEYS_ID_INBOUND_42: &str =
        "a2984f2fe007a860d9778a34e816d6b948a960977816f80b311699ff65ce44c1";
    const EXPECTED_INDEX_INBOUND_42: u32 = 735;
    const EXPECTED_SCRIPT_INBOUND_42: &str = "00147eb8e6586b605a1d0ec57a48bbca9f9481410847";
    const EXPECTED_KEYS_ID_OUTBOUND_BIG: &str =
        "5645e042f900018c605b09ca6dd779d9c0255294d48571805904eae85b94bd9d";
    const EXPECTED_INDEX_OUTBOUND_BIG: u32 = 9970;
    const EXPECTED_SCRIPT_OUTBOUND_BIG: &str = "00142da39569e31e73ac3946f25ecd28872b109202e1";
    /// user_channel_id = 2^64 + 7: exercises the low/high 64-bit split.
    const BIG_UCID: u128 = (1u128 << 64) + 7;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn test_provider(store: Arc<FilesystemStore>) -> WalletSignerProvider {
        let keys = derive_wallet_keys(&parse_mnemonic(TEST_MNEMONIC).unwrap(), Network::Bitcoin);
        let logger = Arc::new(Logger);
        let wallet = Arc::new(
            OnchainWallet::new(
                &keys.descriptor_external,
                &keys.descriptor_internal,
                Network::Bitcoin,
                store,
                Arc::clone(&logger),
            )
            .unwrap(),
        );
        let keys_manager = Arc::new(KeysManager::new(&keys.ldk_seed, 0, 0, false));
        WalletSignerProvider::new(keys_manager, wallet, keys.channel_keys_id_hmac_key, logger)
    }

    fn fresh_store() -> (tempfile::TempDir, Arc<FilesystemStore>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStore::new(PathBuf::from(dir.path())));
        (dir, store)
    }

    #[test]
    fn channel_keys_ids_match_the_pwa_vectors() {
        let (_dir, store) = fresh_store();
        let provider = test_provider(store);

        let inbound = provider.generate_channel_keys_id(true, 42);
        assert_eq!(hex(&inbound), EXPECTED_KEYS_ID_INBOUND_42);
        assert_eq!(destination_index(&inbound), EXPECTED_INDEX_INBOUND_42);

        let outbound = provider.generate_channel_keys_id(false, BIG_UCID);
        assert_eq!(hex(&outbound), EXPECTED_KEYS_ID_OUTBOUND_BIG);
        assert_eq!(destination_index(&outbound), EXPECTED_INDEX_OUTBOUND_BIG);
    }

    #[test]
    fn keys_id_is_deterministic_and_inbound_sensitive() {
        let (_dir, store) = fresh_store();
        let provider = test_provider(store);
        assert_eq!(
            provider.generate_channel_keys_id(true, 42),
            provider.generate_channel_keys_id(true, 42),
            "same inputs must re-derive the same id (cross-device recovery)"
        );
        assert_ne!(
            provider.generate_channel_keys_id(true, 42),
            provider.generate_channel_keys_id(false, 42),
            "the inbound flag is part of the derivation"
        );
    }

    #[test]
    fn destination_scripts_match_the_pwa_vectors() {
        let (_dir, store) = fresh_store();
        let provider = test_provider(store);

        let inbound = provider.generate_channel_keys_id(true, 42);
        let script = provider.get_destination_script(inbound).unwrap();
        assert_eq!(hex(script.as_bytes()), EXPECTED_SCRIPT_INBOUND_42);

        let outbound = provider.generate_channel_keys_id(false, BIG_UCID);
        let script = provider.get_destination_script(outbound).unwrap();
        assert_eq!(hex(script.as_bytes()), EXPECTED_SCRIPT_OUTBOUND_BIG);
    }

    #[test]
    fn destination_script_is_stable_across_a_wallet_rebuild() {
        // The restore path: a wallet rebuilt over the same persisted store
        // (and, per the vector pins above, ANY wallet on the same mnemonic)
        // must resolve a channel_keys_id to the same script.
        let (_dir, store) = fresh_store();
        let keys_id = {
            let provider = test_provider(Arc::clone(&store));
            provider.generate_channel_keys_id(true, 42)
        };
        let first = test_provider(Arc::clone(&store))
            .get_destination_script(keys_id)
            .unwrap();
        let second = test_provider(store)
            .get_destination_script(keys_id)
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(hex(first.as_bytes()), EXPECTED_SCRIPT_INBOUND_42);
    }

    #[test]
    fn derive_channel_signer_round_trips_the_keys_id() {
        let (_dir, store) = fresh_store();
        let provider = test_provider(store);
        let keys_id = provider.generate_channel_keys_id(true, 42);
        let signer = provider.derive_channel_signer(keys_id);
        assert_eq!(signer.channel_keys_id(), keys_id);
    }

    #[test]
    fn shutdown_script_is_a_bdk_p2wpkh_address() {
        let (_dir, store) = fresh_store();
        let provider = test_provider(store);
        let shutdown = provider.get_shutdown_scriptpubkey().unwrap();
        let script = shutdown.into_inner();
        assert!(script.is_p2wpkh());
        // next_unused_address with no history is external index 0 — the
        // BIP84 first-address vector (bc1qcr8te4...).
        assert_eq!(
            hex(script.as_bytes()),
            "0014c0cebcd6c3d3ca8c75dc5ec62ebe55330ef910e2"
        );
    }
}
