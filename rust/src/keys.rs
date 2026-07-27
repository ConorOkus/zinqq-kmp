//! Mnemonic key hierarchy (U1, KTD-4, R1/R2): every wallet key derives from a
//! single BIP39 English 12-word mnemonic, byte-identically to the PWA
//! (`zinq/src/wallet/keys.ts`):
//!
//! - LDK seed              = 32-byte privkey at `m/535'/0'`
//! - VSS encryption key    = privkey at `m/535'/1'` (consumed by U2)
//! - VSS signing key       = privkey at `m/535'/2'` (consumed by U2)
//! - VSS store id          = hex(SHA-256(LDK seed)) (consumed by U2)
//! - BIP84 descriptors     = `wpkh([fp/84'/0'/0']xprv/{0,1}/*)`, same master
//! - channel_keys_id HMAC key = HMAC-SHA256(LDK seed, "zinq/channel_keys_id/v1")
//!
//! The mnemonic is stored write-once as the `mnemonic` file in the data dir,
//! replacing the spike's raw `keys_seed` (spike installs are disposable — no
//! migration, R1 storage half). Auto-generation is refused while the
//! restore-in-progress marker (written by U4's restore flow) is present: a
//! missing mnemonic mid-restore means the restore is incomplete, not that this
//! is a fresh install.

use std::fmt;
use std::fs;
use std::path::Path;

use bip39::{Language, Mnemonic};
use bitcoin::bip32::{ChildNumber, Xpriv};
use bitcoin::hashes::{sha256, Hash, HashEngine, Hmac, HmacEngine};
use bitcoin::secp256k1::rand::rngs::OsRng;
use bitcoin::secp256k1::rand::RngCore;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::Network;
use lightning::sign::{KeysManager, NodeSigner, Recipient};
use zeroize::Zeroize;

/// Write-once file inside the data dir holding the wallet's 12 words (R1).
/// Public as test-support API surface, like `KV_STORE_SUBDIR`.
pub const MNEMONIC_FILE_NAME: &str = "mnemonic";

/// Marker file U4's restore flow writes into the data dir before it wipes
/// state and rewrites the mnemonic. While it exists, a missing mnemonic is an
/// interrupted restore — never a reason to auto-generate fresh words.
pub const RESTORE_IN_PROGRESS_FILE_NAME: &str = "restore_in_progress";

/// The PWA's purpose tag for the channel_keys_id HMAC key
/// (`zinq/src/ldk/init.ts`).
const CHANNEL_KEYS_ID_HMAC_TAG: &[u8] = b"zinq/channel_keys_id/v1";

/// Typed mnemonic/key failures, mapped into [`crate::BuildError`] at start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeysError {
    /// The mnemonic file (or its directory) could not be written.
    WriteFailed,
    /// The mnemonic file exists but could not be read.
    ReadFailed,
    /// The mnemonic file exists but does not hold a valid BIP39 English
    /// 12-word mnemonic.
    InvalidMnemonic,
    /// A mnemonic file already exists; overwriting it would destroy access to
    /// the existing wallet's funds.
    MnemonicExists,
    /// The restore-in-progress marker is present and no mnemonic exists:
    /// generating fresh words now would silently abandon the wallet being
    /// restored.
    RestoreInProgress,
}

impl fmt::Display for KeysError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            KeysError::WriteFailed => "failed to write the mnemonic file",
            KeysError::ReadFailed => "failed to read the mnemonic file",
            KeysError::InvalidMnemonic => {
                "the mnemonic is not a valid BIP39 English 12-word mnemonic"
            }
            KeysError::MnemonicExists => "a mnemonic already exists; refusing to overwrite it",
            KeysError::RestoreInProgress => {
                "a restore is in progress; refusing to generate a fresh mnemonic"
            }
        };
        write!(f, "{msg}")
    }
}

impl std::error::Error for KeysError {}

/// Everything derived from the mnemonic (KTD-4). Key material is scrubbed on
/// drop; the descriptors necessarily outlive this struct inside the bdk
/// wallet, so their scrub here is best-effort only.
pub(crate) struct WalletKeys {
    /// 32-byte seed for LDK's `KeysManager` (privkey at `m/535'/0'`).
    pub(crate) ldk_seed: [u8; 32],
    /// VSS client-side encryption key (`m/535'/1'`). Consumed by U2/U3.
    pub(crate) vss_encryption_key: [u8; 32],
    /// VSS auth signing key (`m/535'/2'`). Consumed by U2/U3.
    pub(crate) vss_signing_key: [u8; 32],
    /// Deterministic VSS store id: hex(SHA-256(ldk_seed)). Consumed by U2/U3.
    pub(crate) vss_store_id: String,
    /// HMAC key for `generate_channel_keys_id` (R2): the master seed is never
    /// handed to the signer, only this purpose-limited key.
    pub(crate) channel_keys_id_hmac_key: [u8; 32],
    /// BIP84 external (receive) descriptor: `wpkh([fp/84'/0'/0']xprv/0/*)`.
    pub(crate) descriptor_external: String,
    /// BIP84 internal (change) descriptor: `wpkh([fp/84'/0'/0']xprv/1/*)`.
    pub(crate) descriptor_internal: String,
}

impl Drop for WalletKeys {
    fn drop(&mut self) {
        self.ldk_seed.zeroize();
        self.vss_encryption_key.zeroize();
        self.vss_signing_key.zeroize();
        self.channel_keys_id_hmac_key.zeroize();
        self.descriptor_external.zeroize();
        self.descriptor_internal.zeroize();
    }
}

/// Parses (and validates) a 12-word English mnemonic, whitespace-tolerant.
pub(crate) fn parse_mnemonic(raw: &str) -> Result<Mnemonic, KeysError> {
    let normalized = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mnemonic = Mnemonic::parse_in_normalized(Language::English, &normalized)
        .map_err(|_| KeysError::InvalidMnemonic)?;
    if mnemonic.word_count() != 12 {
        return Err(KeysError::InvalidMnemonic);
    }
    Ok(mnemonic)
}

/// Generates a fresh 12-word mnemonic from 128 bits of OS entropy.
fn generate_mnemonic() -> Mnemonic {
    let mut entropy = [0u8; 16];
    OsRng.fill_bytes(&mut entropy);
    let mnemonic = Mnemonic::from_entropy_in(Language::English, &entropy)
        .expect("16 bytes is valid BIP39 entropy");
    entropy.zeroize();
    mnemonic
}

/// Writes the mnemonic file, refusing to overwrite an existing one (R1:
/// write-once — losing the old words destroys access to the old funds).
/// `create_new` makes check-and-write one atomic filesystem operation.
pub(crate) fn write_mnemonic(storage_dir: &Path, mnemonic: &Mnemonic) -> Result<(), KeysError> {
    fs::create_dir_all(storage_dir).map_err(|_| KeysError::WriteFailed)?;
    let path = storage_dir.join(MNEMONIC_FILE_NAME);
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                KeysError::MnemonicExists
            } else {
                KeysError::WriteFailed
            }
        })?;
    use std::io::Write as _;
    let mut file = file;
    file.write_all(mnemonic.to_string().as_bytes())
        .map_err(|_| KeysError::WriteFailed)
}

/// Loads the wallet mnemonic, generating and persisting a fresh one on first
/// start (R1: no onboarding — first launch creates the wallet).
///
/// A corrupt/invalid mnemonic file is a typed error, never silently replaced.
/// If no mnemonic exists but the restore-in-progress marker does, generation
/// is refused ([`KeysError::RestoreInProgress`]).
pub(crate) fn read_or_generate_mnemonic(storage_dir: &Path) -> Result<Mnemonic, KeysError> {
    let path = storage_dir.join(MNEMONIC_FILE_NAME);
    if path.exists() {
        let bytes = fs::read(&path).map_err(|_| KeysError::ReadFailed)?;
        let raw = String::from_utf8(bytes).map_err(|_| KeysError::InvalidMnemonic)?;
        return parse_mnemonic(&raw);
    }
    if storage_dir.join(RESTORE_IN_PROGRESS_FILE_NAME).exists() {
        return Err(KeysError::RestoreInProgress);
    }
    let mnemonic = generate_mnemonic();
    write_mnemonic(storage_dir, &mnemonic)?;
    Ok(mnemonic)
}

/// HMAC-SHA256 (the PWA's `hmac(sha256, key, msg)`).
pub(crate) fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut engine = HmacEngine::<sha256::Hash>::new(key);
    engine.input(msg);
    Hmac::<sha256::Hash>::from_engine(engine).to_byte_array()
}

/// A hardened BIP32 child number.
fn hardened(index: u32) -> ChildNumber {
    ChildNumber::from_hardened_idx(index).expect("hardened index is in range")
}

/// Derives the full KTD-4 hierarchy from the mnemonic (empty passphrase, like
/// the PWA's `mnemonicToSeedSync(mnemonic)`). `network` only affects the
/// descriptors' xprv serialization; the derivation math is network-free.
pub(crate) fn derive_wallet_keys(mnemonic: &Mnemonic, network: Network) -> WalletKeys {
    let secp = Secp256k1::new();
    let mut seed = mnemonic.to_seed("");
    let master = Xpriv::new_master(network, &seed).expect("a 64-byte BIP39 seed is always valid");
    seed.zeroize();

    let priv_at = |path: &[ChildNumber]| -> [u8; 32] {
        master
            .derive_priv(&secp, &path)
            .expect("hardened derivation from a valid master cannot fail")
            .private_key
            .secret_bytes()
    };

    let ldk_seed = priv_at(&[hardened(535), hardened(0)]);
    let vss_encryption_key = priv_at(&[hardened(535), hardened(1)]);
    let vss_signing_key = priv_at(&[hardened(535), hardened(2)]);
    // hex(SHA-256(ldk_seed)) — sha256's Display is forward lowercase hex.
    let vss_store_id = sha256::Hash::hash(&ldk_seed).to_string();
    let channel_keys_id_hmac_key = hmac_sha256(&ldk_seed, CHANNEL_KEYS_ID_HMAC_TAG);

    // BIP84 descriptors, string-identical to the PWA's deriveBdkDescriptors:
    // wpkh([fingerprint/84'/0'/0']accountXprv/{0,1}/*).
    let account = master
        .derive_priv(&secp, &[hardened(84), hardened(0), hardened(0)])
        .expect("hardened derivation from a valid master cannot fail");
    let origin = format!("{}/84'/0'/0'", master.fingerprint(&secp));
    let descriptor_external = format!("wpkh([{origin}]{account}/0/*)");
    let descriptor_internal = format!("wpkh([{origin}]{account}/1/*)");

    WalletKeys {
        ldk_seed,
        vss_encryption_key,
        vss_signing_key,
        vss_store_id,
        channel_keys_id_hmac_key,
        descriptor_external,
        descriptor_internal,
    }
}

/// AE1 debug surface: the node id this mnemonic yields — the same value the
/// PWA reports for the same 12 words (R2's cross-client identity check).
///
/// Derived through a real [`KeysManager`] over the `m/535'/0'` seed, so it is
/// the exact identity `build()` produces (the starting time only seeds
/// ephemeral material, never the node id).
pub fn derive_debug_info(mnemonic: &str) -> Result<String, KeysError> {
    let mnemonic = parse_mnemonic(mnemonic)?;
    let keys = derive_wallet_keys(&mnemonic, Network::Bitcoin);
    let keys_manager = KeysManager::new(&keys.ldk_seed, 0, 0, false);
    let node_id = keys_manager
        .get_node_id(Recipient::Node)
        .expect("Recipient::Node is always available on KeysManager");
    Ok(node_id.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // Cross-implementation vectors generated from the PWA's own code paths
    // (`zinq/src/wallet/keys.ts`, `zinq/src/ldk/init.ts`) by `gen_vectors.mjs`
    // — a one-off node script run on 2026-07-26 against the zinq repo's
    // @scure/bip39, @scure/bip32 and @noble/hashes dependencies. The mnemonic
    // is BIP39 English test vector #0; the first external script matches the
    // published BIP84 test vector address
    // bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu.
    pub(crate) const TEST_MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon \
         abandon abandon abandon abandon abandon about";
    const EXPECTED_LDK_SEED: &str =
        "c9e1abf64312a43d74b6452e5be41b6b430b777acaedaa7b1a67e077428bf9eb";
    const EXPECTED_VSS_ENCRYPTION_KEY: &str =
        "4b78cf03b10fe46e63552fccc1217d62d654542969f0d994d91440cdbb5f0ee2";
    const EXPECTED_VSS_SIGNING_KEY: &str =
        "4d26776010dbc54febdc831b15ef8ca90d8ec62e693ff8ec6c55a96998a30446";
    const EXPECTED_VSS_STORE_ID: &str =
        "753402a37283d45fc4e449aa51622b5ae76184ce450a4eaa90bce2039f4e4056";
    const EXPECTED_CHANNEL_KEYS_ID_HMAC_KEY: &str =
        "a424ae7827e5eaedc5cc6c8346ed953b1fb1dfe4d3ea7878ba52a83dbf505465";
    const EXPECTED_NODE_ID: &str =
        "0307d063a0bb85c655e519dc28c55688b39740bc3e69949f239c48ba74f581c538";
    const EXPECTED_ACCOUNT_XPRV: &str =
        "xprv9ybY78BftS5UGANki6oSifuQEjkpyAC8ZmBvBNTshQnCBcxnefjHS7buPMkkqhcRzmoGZ5bokx7GuyDAiktd5HemohAU4wV1ZPMDRmLpBMm";

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn test_keys() -> WalletKeys {
        derive_wallet_keys(&parse_mnemonic(TEST_MNEMONIC).unwrap(), Network::Bitcoin)
    }

    #[test]
    fn ldk_seed_matches_the_pwa_vector() {
        assert_eq!(hex(&test_keys().ldk_seed), EXPECTED_LDK_SEED);
    }

    #[test]
    fn vss_keys_and_store_id_match_the_pwa_vectors() {
        let keys = test_keys();
        assert_eq!(hex(&keys.vss_encryption_key), EXPECTED_VSS_ENCRYPTION_KEY);
        assert_eq!(hex(&keys.vss_signing_key), EXPECTED_VSS_SIGNING_KEY);
        assert_eq!(keys.vss_store_id, EXPECTED_VSS_STORE_ID);
    }

    #[test]
    fn channel_keys_id_hmac_key_matches_the_pwa_vector() {
        assert_eq!(
            hex(&test_keys().channel_keys_id_hmac_key),
            EXPECTED_CHANNEL_KEYS_ID_HMAC_KEY
        );
    }

    #[test]
    fn bip84_descriptors_match_the_pwa_strings_exactly() {
        let keys = test_keys();
        let origin = "[73c5da0a/84'/0'/0']";
        assert_eq!(
            keys.descriptor_external,
            format!("wpkh({origin}{EXPECTED_ACCOUNT_XPRV}/0/*)")
        );
        assert_eq!(
            keys.descriptor_internal,
            format!("wpkh({origin}{EXPECTED_ACCOUNT_XPRV}/1/*)")
        );
    }

    #[test]
    fn node_id_matches_the_pwa_vector() {
        assert_eq!(derive_debug_info(TEST_MNEMONIC).unwrap(), EXPECTED_NODE_ID);
    }

    #[test]
    fn derive_debug_info_rejects_invalid_words() {
        assert_eq!(
            derive_debug_info("not a mnemonic").unwrap_err(),
            KeysError::InvalidMnemonic
        );
    }

    #[test]
    fn mnemonic_is_generated_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let first = read_or_generate_mnemonic(dir.path()).unwrap();
        let second = read_or_generate_mnemonic(dir.path()).unwrap();
        assert_eq!(first, second, "mnemonic must be stable across restarts");
        assert_eq!(first.word_count(), 12);

        let other_dir = tempfile::tempdir().unwrap();
        let other = read_or_generate_mnemonic(other_dir.path()).unwrap();
        assert_ne!(first, other, "each data dir must get its own mnemonic");
    }

    #[test]
    fn write_mnemonic_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let existing = read_or_generate_mnemonic(dir.path()).unwrap();
        let fresh = generate_mnemonic();
        assert_eq!(
            write_mnemonic(dir.path(), &fresh).unwrap_err(),
            KeysError::MnemonicExists
        );
        // The original words survived the refused overwrite.
        assert_eq!(read_or_generate_mnemonic(dir.path()).unwrap(), existing);
    }

    #[test]
    fn corrupt_mnemonic_file_is_a_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(MNEMONIC_FILE_NAME), b"garbage words here").unwrap();
        assert_eq!(
            read_or_generate_mnemonic(dir.path()).unwrap_err(),
            KeysError::InvalidMnemonic
        );

        // Valid BIP39 but not 12 words (24-word vector) is rejected too: the
        // product's backup/restore surface is exactly 12 words (R1).
        let dir = tempfile::tempdir().unwrap();
        let twenty_four = "abandon abandon abandon abandon abandon abandon abandon abandon \
             abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon \
             abandon abandon abandon abandon abandon art";
        fs::write(dir.path().join(MNEMONIC_FILE_NAME), twenty_four).unwrap();
        assert_eq!(
            read_or_generate_mnemonic(dir.path()).unwrap_err(),
            KeysError::InvalidMnemonic
        );

        // Non-UTF8 bytes are invalid, not a read failure.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(MNEMONIC_FILE_NAME), [0xff, 0xfe, 0x00]).unwrap();
        assert_eq!(
            read_or_generate_mnemonic(dir.path()).unwrap_err(),
            KeysError::InvalidMnemonic
        );
    }

    #[test]
    fn restore_marker_blocks_mnemonic_generation() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(RESTORE_IN_PROGRESS_FILE_NAME), b"").unwrap();
        assert_eq!(
            read_or_generate_mnemonic(dir.path()).unwrap_err(),
            KeysError::RestoreInProgress
        );
        assert!(
            !dir.path().join(MNEMONIC_FILE_NAME).exists(),
            "no mnemonic may be created while a restore is incomplete"
        );
    }

    #[test]
    fn restore_marker_does_not_block_an_existing_mnemonic() {
        // Marker + mnemonic both present (restore wrote the words but crashed
        // before clearing the marker): the words win, start can proceed.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(MNEMONIC_FILE_NAME), TEST_MNEMONIC).unwrap();
        fs::write(dir.path().join(RESTORE_IN_PROGRESS_FILE_NAME), b"").unwrap();
        assert_eq!(
            read_or_generate_mnemonic(dir.path()).unwrap().to_string(),
            TEST_MNEMONIC
        );
    }

    #[test]
    fn parse_mnemonic_tolerates_surrounding_whitespace() {
        let padded = format!("  {TEST_MNEMONIC} \n");
        assert_eq!(parse_mnemonic(&padded).unwrap().to_string(), TEST_MNEMONIC);
    }
}
