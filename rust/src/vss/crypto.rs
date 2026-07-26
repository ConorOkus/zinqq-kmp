//! The PWA's exact VSS blob crypto (U2, KTD-2, R3), mirroring
//! `zinq/src/ldk/storage/vss-crypto.ts` byte for byte:
//!
//! - Key obfuscation: `hex(HMAC-SHA256(vss_encryption_key, plaintext_key))`
//!   — the deterministic wire key sent to the server.
//! - Blob encryption: `[random nonce (12)][ChaCha20-Poly1305 ciphertext+tag]`
//!   with EMPTY AAD. Decryption splits the 12-byte nonce prefix and rejects
//!   blobs shorter than nonce + tag.
//!
//! Format compatibility is the whole point (KTD-2): `vss-client-ng`'s
//! `StorableBuilder`/`KeyObfuscator` envelope is deliberately NOT used.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use bitcoin::secp256k1::rand::rngs::OsRng;
use bitcoin::secp256k1::rand::RngCore;

/// The nonce prefix length (the PWA's `NONCE_LENGTH`).
pub const NONCE_LEN: usize = 12;
/// The Poly1305 authentication tag length appended to the ciphertext.
pub const TAG_LEN: usize = 16;

/// Typed decryption failures, kept distinct so a truncated blob is never
/// mistaken for a wrong-key/tampered blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    /// The blob is shorter than nonce + tag (the PWA's "Cipher blob too
    /// short" check) — it cannot even be attempted.
    BlobTooShort,
    /// Poly1305 authentication failed: wrong key or tampered ciphertext.
    DecryptFailed,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            CryptoError::BlobTooShort => "cipher blob too short to contain nonce + auth tag",
            CryptoError::DecryptFailed => "blob decryption failed (wrong key or tampered data)",
        };
        write!(f, "{msg}")
    }
}

impl std::error::Error for CryptoError {}

/// Lowercase hex, the PWA's byte-to-hex formatting.
pub(crate) fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `hex(HMAC-SHA256(encryption_key, plaintext_key))` — the PWA's
/// `obfuscateKey`. Deterministic: the same plaintext key always maps to the
/// same wire key, so both clients address the same server-side objects.
pub fn obfuscate_key(encryption_key: &[u8; 32], plaintext_key: &str) -> String {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(encryption_key)
        .expect("HMAC-SHA256 accepts keys of any length");
    mac.update(plaintext_key.as_bytes());
    to_hex(&mac.finalize().into_bytes())
}

/// The PWA's `vssEncrypt`: fresh random 12-byte nonce, then
/// `[nonce][ciphertext+tag]`.
pub fn encrypt(key: &[u8; 32], plaintext: &[u8]) -> Vec<u8> {
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    encrypt_with_nonce(key, &nonce, plaintext)
}

/// [`encrypt`] with an injectable nonce — the cross-implementation vector
/// tests fix the nonce to compare ciphertexts byte for byte. Production code
/// must use [`encrypt`] (nonce reuse breaks ChaCha20-Poly1305).
pub fn encrypt_with_nonce(key: &[u8; 32], nonce: &[u8; NONCE_LEN], plaintext: &[u8]) -> Vec<u8> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    // `encrypt` with a bare byte slice uses empty AAD — exactly the PWA's
    // `chacha20poly1305(key, nonce).encrypt(plaintext)`.
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(nonce), plaintext)
        .expect("ChaCha20-Poly1305 encryption of in-memory buffers cannot fail");
    let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    blob.extend_from_slice(nonce);
    blob.extend_from_slice(&ciphertext);
    blob
}

/// The PWA's `vssDecrypt`: split the 12-byte nonce prefix, authenticate and
/// decrypt the remainder. Blobs shorter than nonce + tag are rejected up
/// front ([`CryptoError::BlobTooShort`]).
pub fn decrypt(key: &[u8; 32], blob: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if blob.len() < NONCE_LEN + TAG_LEN {
        return Err(CryptoError::BlobTooShort);
    }
    let (nonce, ciphertext) = blob.split_at(NONCE_LEN);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| CryptoError::DecryptFailed)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // Cross-implementation vectors generated from the PWA's OWN crypto
    // dependencies (zinq/node_modules: @noble/ciphers 2.1, @noble/hashes 2.0)
    // driven through the exact `vss-crypto.ts` code paths by
    // `gen_vss_vectors.mjs` — a one-off node script run on 2026-07-26 with the
    // nonce injected. The encryption key is U1's PWA-verified
    // `vss_encryption_key` for BIP39 test vector #0 (`keys.rs`
    // EXPECTED_VSS_ENCRYPTION_KEY), tying U2's wire crypto to U1's hierarchy.
    pub(crate) const VECTOR_ENC_KEY: [u8; 32] = [
        0x4b, 0x78, 0xcf, 0x03, 0xb1, 0x0f, 0xe4, 0x6e, 0x63, 0x55, 0x2f, 0xcc, 0xc1, 0x21, 0x7d,
        0x62, 0xd6, 0x54, 0x54, 0x29, 0x69, 0xf0, 0xd9, 0x94, 0xd9, 0x14, 0x40, 0xcd, 0xbb, 0x5f,
        0x0e, 0xe2,
    ];
    const FIXED_NONCE: [u8; NONCE_LEN] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
    const VECTOR_PLAINTEXT: &[u8] = b"zinqq vss cross-impl vector v1";
    const EXPECTED_BLOB: &str = "000102030405060708090a0b3a1d84aa3ca959c848437dfdef84e577a579\
         f69d684041db89243009770d7424a20ec59152d48cdaec6b620c34b4";
    const EXPECTED_EMPTY_BLOB: &str = "000102030405060708090a0beec2d815dad737dc28dee90540d4970c";

    #[test]
    fn obfuscated_keys_match_the_pwa_vectors_byte_for_byte() {
        let cases = [
            (
                "channel_manager",
                "23f2fa842f203576c24befc717dbcb7029a36c115a494d496aed4c8291991551",
            ),
            (
                "_monitor_keys",
                "76832ae61d12bc623aefef7961b1f3a9b1fc41a54444401586dc8da9938402b4",
            ),
            (
                "_known_peers",
                "7225d9668a124ece69e9439aa2840b7fe7e647a4b5448c2e492c956c61d322e1",
            ),
        ];
        for (plaintext_key, expected) in cases {
            assert_eq!(
                obfuscate_key(&VECTOR_ENC_KEY, plaintext_key),
                expected,
                "obfuscation of {plaintext_key} diverged from the PWA"
            );
        }
    }

    #[test]
    fn encrypted_blob_with_fixed_nonce_matches_the_pwa_vector_byte_for_byte() {
        let blob = encrypt_with_nonce(&VECTOR_ENC_KEY, &FIXED_NONCE, VECTOR_PLAINTEXT);
        assert_eq!(
            to_hex(&blob),
            EXPECTED_BLOB.split_whitespace().collect::<String>()
        );
    }

    #[test]
    fn empty_plaintext_blob_matches_the_pwa_vector() {
        // nonce + tag only: the smallest valid blob (28 bytes).
        let blob = encrypt_with_nonce(&VECTOR_ENC_KEY, &FIXED_NONCE, b"");
        assert_eq!(to_hex(&blob), EXPECTED_EMPTY_BLOB);
        assert_eq!(blob.len(), NONCE_LEN + TAG_LEN);
        assert_eq!(decrypt(&VECTOR_ENC_KEY, &blob).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn encrypt_decrypt_identity_with_random_nonces() {
        let plaintext = b"the same plaintext twice";
        let blob_a = encrypt(&VECTOR_ENC_KEY, plaintext);
        let blob_b = encrypt(&VECTOR_ENC_KEY, plaintext);
        assert_ne!(blob_a, blob_b, "random nonces must differ per encryption");
        assert_eq!(decrypt(&VECTOR_ENC_KEY, &blob_a).unwrap(), plaintext);
        assert_eq!(decrypt(&VECTOR_ENC_KEY, &blob_b).unwrap(), plaintext);
    }

    #[test]
    fn decrypt_rejects_blobs_shorter_than_nonce_plus_tag() {
        for len in 0..(NONCE_LEN + TAG_LEN) {
            assert_eq!(
                decrypt(&VECTOR_ENC_KEY, &vec![0u8; len]),
                Err(CryptoError::BlobTooShort),
                "a {len}-byte blob must be rejected as too short"
            );
        }
    }

    #[test]
    fn tampered_ciphertext_fails_authentication() {
        let mut blob = encrypt(&VECTOR_ENC_KEY, b"fund-critical bytes");
        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        assert_eq!(
            decrypt(&VECTOR_ENC_KEY, &blob),
            Err(CryptoError::DecryptFailed)
        );
    }

    #[test]
    fn wrong_key_fails_authentication() {
        let blob = encrypt(&VECTOR_ENC_KEY, b"secret");
        let mut wrong_key = VECTOR_ENC_KEY;
        wrong_key[0] ^= 0xff;
        assert_eq!(decrypt(&wrong_key, &blob), Err(CryptoError::DecryptFailed));
    }

    #[test]
    fn crypto_errors_have_distinct_display() {
        assert_ne!(
            CryptoError::BlobTooShort.to_string(),
            CryptoError::DecryptFailed.to_string()
        );
    }
}
