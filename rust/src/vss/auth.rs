//! The PWA's VSS signature auth header (U2, KTD-2, R3), mirroring
//! `SignatureHeaderProvider` in `zinq/src/ldk/storage/vss-client.ts`:
//!
//! - preimage = 64-byte salt constant ‖ compressed pubkey (33) ‖ ASCII
//!   unix-seconds timestamp
//! - digest   = SHA-256(preimage), signed as a compact (64-byte) secp256k1
//!   ECDSA signature over the VSS signing key (U1's `m/535'/2'`)
//! - header   = `authorization: hex(pubkey33) + hex(sig64) + timestamp`
//!
//! The timestamp is injectable ([`SignatureHeaderProvider::header_at`]) so
//! cross-implementation vectors can pin the exact header string.

use bitcoin::hashes::{sha256, Hash};
use bitcoin::secp256k1::{Message, PublicKey, Secp256k1, SecretKey, SignOnly};

use super::VssError;
use crate::util::hex_str;

/// The Nodana VSS Signature Authorizer domain separator. The trailing dots
/// pad to exactly 64 bytes — do not modify (PWA `VSS_SIGNING_CONSTANT`).
pub const VSS_SIGNING_SALT: &[u8; 64] =
    b"VSS Signature Authorizer Signing Salt Constant..................";

/// Computes the PWA-compatible `authorization` header for each request.
pub struct SignatureHeaderProvider {
    secp: Secp256k1<SignOnly>,
    secret_key: SecretKey,
    /// Compressed (33-byte) pubkey, precomputed like the PWA's constructor.
    public_key: PublicKey,
}

impl SignatureHeaderProvider {
    /// Builds a provider over the 32-byte VSS signing key (U1's `m/535'/2'`).
    pub fn new(signing_key: &[u8; 32]) -> Result<Self, VssError> {
        let secp = Secp256k1::signing_only();
        let secret_key = SecretKey::from_slice(signing_key).map_err(|e| VssError::Auth {
            message: format!("invalid VSS signing key: {e}"),
        })?;
        let public_key = PublicKey::from_secret_key(&secp, &secret_key);
        Ok(Self {
            secp,
            secret_key,
            public_key,
        })
    }

    /// The `authorization` header value for "now" (whole unix seconds, like
    /// the PWA's `Math.floor(Date.now() / 1000)`).
    pub fn header(&self) -> String {
        self.header_at(crate::util::unix_now().as_secs())
    }

    /// [`Self::header`] with the timestamp injected — used by the vector
    /// tests to pin the exact header string.
    pub fn header_at(&self, unix_seconds: u64) -> String {
        let timestamp = unix_seconds.to_string();
        let pubkey = self.public_key.serialize();

        let mut preimage =
            Vec::with_capacity(VSS_SIGNING_SALT.len() + pubkey.len() + timestamp.len());
        preimage.extend_from_slice(VSS_SIGNING_SALT);
        preimage.extend_from_slice(&pubkey);
        preimage.extend_from_slice(timestamp.as_bytes());

        let digest = sha256::Hash::hash(&preimage);
        let message = Message::from_digest(digest.to_byte_array());
        // RFC6979 deterministic, low-S — byte-identical to the PWA's noble
        // signature with extraEntropy disabled, and verifiable against its
        // default hedged signatures (same preimage digest either way).
        let signature = self
            .secp
            .sign_ecdsa(&message, &self.secret_key)
            .serialize_compact();

        format!("{}{}{timestamp}", hex_str(&pubkey), hex_str(&signature))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Cross-implementation vector generated from the PWA's OWN dependencies
    // (zinq/node_modules: @noble/secp256k1 3.0, @noble/hashes 2.0) driven
    // through the exact `SignatureHeaderProvider` code path by
    // `gen_vss_vectors.mjs` (2026-07-26), with the timestamp fixed and
    // extraEntropy disabled (pure RFC6979 — what rust-secp256k1 emits). The
    // signing key is U1's PWA-verified `vss_signing_key` for BIP39 test
    // vector #0 (`keys.rs` EXPECTED_VSS_SIGNING_KEY).
    const VECTOR_SIGNING_KEY: [u8; 32] = [
        0x4d, 0x26, 0x77, 0x60, 0x10, 0xdb, 0xc5, 0x4f, 0xeb, 0xdc, 0x83, 0x1b, 0x15, 0xef, 0x8c,
        0xa9, 0x0d, 0x8e, 0xc6, 0x2e, 0x69, 0x3f, 0xf8, 0xec, 0x6c, 0x55, 0xa9, 0x69, 0x98, 0xa3,
        0x04, 0x46,
    ];
    const VECTOR_TIMESTAMP: u64 = 1753488000;
    const EXPECTED_PUBKEY: &str =
        "03badbeaae2362afc9b682b2715470ec8594ce420a80c4ec7720bdb7abfd51abb0";
    const EXPECTED_SIG: &str = "6aef93e48d0385e80c78fa62e787e1664d7c341c91ab58c2672130e0c49188c5\
         66d25733280cf88f0e7d46021248276f1c12077968bbac06e234713969d9b1b7";

    #[test]
    fn salt_constant_is_exactly_64_bytes() {
        assert_eq!(VSS_SIGNING_SALT.len(), 64);
    }

    #[test]
    fn auth_header_matches_the_pwa_vector_byte_for_byte() {
        let provider = SignatureHeaderProvider::new(&VECTOR_SIGNING_KEY).unwrap();
        let header = provider.header_at(VECTOR_TIMESTAMP);
        let expected_sig: String = EXPECTED_SIG.split_whitespace().collect();
        assert_eq!(
            header,
            format!("{EXPECTED_PUBKEY}{expected_sig}{VECTOR_TIMESTAMP}")
        );
    }

    #[test]
    fn header_layout_is_pubkey33_sig64_timestamp() {
        let provider = SignatureHeaderProvider::new(&VECTOR_SIGNING_KEY).unwrap();
        let header = provider.header();
        // 66 hex chars (pubkey) + 128 hex chars (compact sig) + digits.
        assert!(header.len() > 66 + 128);
        let (pubkey, rest) = header.split_at(66);
        let (sig, timestamp) = rest.split_at(128);
        assert!(pubkey.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(pubkey, EXPECTED_PUBKEY);
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!timestamp.is_empty());
        assert!(timestamp.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn invalid_signing_key_is_a_typed_auth_error() {
        // The all-zero scalar is not a valid secp256k1 secret key.
        let result = SignatureHeaderProvider::new(&[0u8; 32]);
        assert!(matches!(result, Err(VssError::Auth { .. })));
    }
}
