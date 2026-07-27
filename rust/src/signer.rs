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

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

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
    /// Every `channel_keys_id` LDK has asked this provider to derive a signer
    /// for. Interior-mutable because `SignerProvider` takes `&self`.
    ///
    /// This is how the startup destination reveal
    /// ([`WalletSignerProvider::reveal_derived_destinations`]) learns which
    /// channels exist: `lightning` 0.2.4 exposes NO public accessor for a
    /// deserialized `ChannelMonitor`'s `channel_keys_id`
    /// (`ChannelMonitor::do_mut_signer_call`, the only route to the signer, is
    /// `#[cfg(any(test, feature = "_test_utils"))]`-gated inside the crate), so
    /// the ids are recorded as they flow past. Both
    /// `read_channel_monitors` (via `OnchainTxHandler`'s `read`) and
    /// `ChannelManager`'s `read` call `derive_channel_signer` exactly once per
    /// channel they load, so after startup's reads this set is precisely the
    /// set of channels this boot knows about.
    derived_channel_keys_ids: Mutex<BTreeSet<[u8; 32]>>,
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
            derived_channel_keys_ids: Mutex::new(BTreeSet::new()),
            logger,
        }
    }

    /// How many distinct channels this provider has derived a signer for.
    pub(crate) fn derived_channel_count(&self) -> usize {
        self.derived_channel_keys_ids.lock().unwrap().len()
    }

    /// The deterministic destination index of every channel this provider has
    /// derived a signer for, computed with [`destination_index`] — the SAME
    /// helper [`SignerProvider::get_destination_script`] uses, so there is one
    /// source of truth for "where can this channel's close funds land".
    pub(crate) fn derived_destination_indexes(&self) -> Vec<u32> {
        let mut indexes: Vec<u32> = self
            .derived_channel_keys_ids
            .lock()
            .unwrap()
            .iter()
            .map(destination_index)
            .collect();
        indexes.sort_unstable();
        indexes.dedup();
        indexes
    }

    /// Reveals the deterministic close/sweep destination of every channel this
    /// provider has derived a signer for to the bdk wallet, and persists the
    /// reveal. Returns the highest revealed EXTERNAL index, or `None` when
    /// there is nothing to reveal (a wallet with no channels reveals nothing —
    /// no gratuitous address inflation).
    ///
    /// Called from `builder::build` on EVERY boot, after the monitors and the
    /// channel manager are read and BEFORE the first bdk chain scan.
    ///
    /// WHY: `get_destination_script` derives close destinations at
    /// `BE(channel_keys_id[0..4]) mod 10_000` (KTD-4, PWA parity), so close
    /// funds can sit at ANY external index up to 9 999. A restored wallet's
    /// bdk changeset starts with `last_revealed` at 0, and
    /// `BDK_CLIENT_STOP_GAP` means the full scan never looks anywhere near
    /// those indices — the on-chain balance of a cross-client restore comes
    /// back EMPTY even though the closed channels' funds are on chain. This is
    /// the reveal-on-restore half of the PWA's own
    /// `bdk-ldk-force-close-destination-script-interop` learning; the
    /// deterministic-derivation half was already honored. Not gated on the
    /// restore path: plain restarts and `vss::startup::silent_recovery` need
    /// the same guarantee, and re-revealing is a monotone no-op.
    ///
    /// COST: bdk can only watch an index by REVEALING it, and a reveal is
    /// inclusive — one channel at index 9 970 means 9 971 tracked external
    /// addresses and a proportionally larger persisted changeset. That cost is
    /// inherent to the KTD-4 destination scheme and a wallet that OPENED the
    /// channel here already pays it (`destination_script_for_index` reveals the
    /// same index at channel-open time); this only stops a restored wallet from
    /// silently paying less and seeing less. What it no longer costs is a
    /// per-tick Esplora query for all 9 971: the incremental sync watches the
    /// pinned destinations plus a bounded window at each end of the revealed
    /// range, never the inclusive interior (see
    /// [`crate::wallet::OnchainWallet::bounded_sync_request`], and
    /// [`crate::config::ONCHAIN_SYNC_KEYCHAIN_WINDOW`] for the 13-minute sync
    /// this regression cost before it was bounded).
    ///
    /// NOT a brute force of the 10 000-index space. Revealing all of it would
    /// bloat the persisted changeset a hundredfold and force the FULL SCAN to
    /// walk 10 000 addresses, for a wallet that has at most a handful of
    /// channels.
    /// RESIDUAL: a channel whose monitor was fully archived AND deleted leaves
    /// no `channel_keys_id` to derive from, so its destination index is
    /// unrecoverable by derivation. That is a real, known gap for the
    /// maintainer — not something a 10k-address scan should paper over.
    pub(crate) fn reveal_derived_destinations(&self) -> Option<u32> {
        let indexes = self.derived_destination_indexes();
        // Revealing an index is not enough to keep it WATCHED: the incremental
        // sync deliberately queries a bounded SPK set, so every destination has
        // to be pinned into it by index (see
        // `OnchainWallet::bounded_sync_request`). Pin before the reveal — a
        // failed persist below is a reason to keep watching, not to stop.
        self.wallet
            .watch_destination_indexes(indexes.iter().copied());
        // Destinations are EXTERNAL-only and `reveal_addresses_to` is
        // monotone, so revealing to the single highest index covers every
        // recorded channel in one persist.
        let max_index = indexes.into_iter().max()?;
        if self.wallet.reveal_external_addresses_to(max_index).is_err() {
            // The wallet logs the underlying persistence error; an unpersisted
            // reveal would be lost on the next start (the PWA's
            // `bdk-address-reveal-not-persisted` learning), so report failure
            // rather than claim the destinations are watched.
            log_error!(
                self.logger,
                "Failed to reveal channel destination addresses up to external index {max_index}; \
                 close funds may stay invisible until the next successful start"
            );
            return None;
        }
        Some(max_index)
    }

    /// The wallet these destinations are revealed into (tests only).
    #[cfg(test)]
    pub(crate) fn wallet(&self) -> &Arc<OnchainWallet> {
        &self.wallet
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
        // Record the id: this is the only hook that sees the `channel_keys_id`
        // of every channel LDK loads (see `derived_channel_keys_ids`), and the
        // startup destination reveal depends on it.
        self.derived_channel_keys_ids
            .lock()
            .unwrap()
            .insert(channel_keys_id);
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

    use bdk_wallet::KeychainKind;
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

    // ---------- startup destination reveal (cross-client restore fix) ----------

    /// The bug: a restored wallet never revealed the deterministic close
    /// destinations, so a channel whose `channel_keys_id` maps to a HIGH index
    /// (here the PWA vector's 9 970, two orders of magnitude past
    /// `BDK_CLIENT_STOP_GAP`) was unwatched and its close funds invisible.
    ///
    /// `derive_channel_signer` is the exact call LDK makes for every channel it
    /// loads (`OnchainTxHandler::read` for monitors, `ChannelManager::read` for
    /// the manager), which is what the provider records. The real-monitor half
    /// of this — that deserializing an actual serialized `ChannelMonitor`
    /// records its id — is pinned in `restore::tests`; the fixture monitors'
    /// harness-generated ids happen to sit at indices 0 and 1, so the
    /// high-index case is driven from our own PWA-vector id here.
    #[test]
    fn startup_reveal_covers_a_high_destination_index() {
        let (_dir, store) = fresh_store();
        let provider = test_provider(store);

        let keys_id = provider.generate_channel_keys_id(false, BIG_UCID);
        let _signer = provider.derive_channel_signer(keys_id);
        assert_eq!(provider.derived_channel_count(), 1);

        let max_index = provider
            .reveal_derived_destinations()
            .expect("one loaded channel must produce a reveal");
        // ONE source of truth: the index the reveal walks to is the index
        // `get_destination_script` derives for the same id.
        assert_eq!(max_index, destination_index(&keys_id));
        assert_eq!(max_index, EXPECTED_INDEX_OUTBOUND_BIG);
        assert!(
            max_index as usize > crate::config::BDK_CLIENT_STOP_GAP,
            "the regression only bites past the stop gap"
        );

        // The wallet's revealed EXTERNAL index now covers it...
        assert_eq!(
            crate::wallet::test_support::derivation_index(
                provider.wallet(),
                KeychainKind::External
            ),
            Some(max_index),
        );
        // ...and the SPK a sync request would query is the destination script
        // the signer hands LDK.
        let destination = provider.get_destination_script(keys_id).unwrap();
        assert!(
            provider.wallet().sync_request_spks().contains(&destination),
            "the next chain sync must query the close destination's SPK"
        );
    }

    /// The reveal is worthless unless persisted: an unpersisted reveal is lost
    /// on the next start (the PWA's `bdk-address-reveal-not-persisted`
    /// learning), which would resurrect the empty-on-chain-balance bug on the
    /// very next boot.
    #[test]
    fn startup_reveal_survives_a_restart() {
        let (_dir, store) = fresh_store();
        let max_index = {
            let provider = test_provider(Arc::clone(&store));
            // Index 735 (the inbound-42 vector): still 36x the stop gap, but a
            // far cheaper reveal than 9 970 for a persistence assertion.
            let keys_id = provider.generate_channel_keys_id(true, 42);
            let _signer = provider.derive_channel_signer(keys_id);
            let max_index = provider.reveal_derived_destinations().unwrap();
            assert_eq!(max_index, EXPECTED_INDEX_INBOUND_42);
            max_index
        };

        // Restart: a fresh provider over the same store, with no channels
        // loaded yet, still finds the destination revealed.
        let restarted = test_provider(store);
        assert_eq!(restarted.derived_channel_count(), 0);
        assert_eq!(
            crate::wallet::test_support::derivation_index(
                restarted.wallet(),
                KeychainKind::External
            ),
            Some(max_index),
            "the revealed destination index must survive persistence"
        );
    }

    /// No channels means no extra addresses: the reveal must not inflate the
    /// external keychain on a fresh (or channel-less) wallet, which would cost
    /// every later revealed-SPK sync for nothing.
    #[test]
    fn wallet_with_no_channels_reveals_nothing_extra() {
        let (_dir, store) = fresh_store();
        let provider = test_provider(store);
        assert_eq!(provider.derived_channel_count(), 0);
        assert!(provider.derived_destination_indexes().is_empty());
        assert!(
            provider.reveal_derived_destinations().is_none(),
            "nothing to reveal"
        );
        assert_eq!(
            crate::wallet::test_support::derivation_index(
                provider.wallet(),
                KeychainKind::External
            ),
            None,
            "no address may be revealed on a wallet with no channels"
        );
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
