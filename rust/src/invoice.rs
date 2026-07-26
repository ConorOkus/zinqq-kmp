//! JIT invoice construction (U4): a fixed-amount BOLT11 invoice wrapped with
//! the LSP route hint from an LSPS2 `buy` response, copied from ldk-node
//! `src/liquidity.rs::lsps2_create_jit_invoice`.
//!
//! KTD-7: the invoice always carries a fixed amount and `basic_mpp`, so payers
//! that split (MPP) still work — the zero-amount variant forbids MPP.
//! KTD-9: `min_final_cltv_expiry_delta` is bumped +2 over LDK's default.

use std::time::Duration;

use bitcoin::hashes::sha256;
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
use bitcoin::Network;
use lightning::ln::channelmanager::MIN_FINAL_CLTV_EXPIRY_DELTA;
use lightning::routing::gossip::RoutingFees;
use lightning::routing::router::{RouteHint, RouteHintHop};
use lightning::types::payment::{PaymentHash, PaymentSecret};
use lightning_invoice::{Bolt11Invoice, CreationError, Currency, InvoiceBuilder};

/// KTD-9: JIT invoices must ask for at least 2 more blocks of final CLTV room
/// than LDK's default, or the LSP-forwarded HTLC gets rejected. Copied from
/// ldk-node: `let min_final_cltv_expiry_delta = MIN_FINAL_CLTV_EXPIRY_DELTA + 2;`.
pub(crate) const JIT_MIN_FINAL_CLTV_EXPIRY_DELTA: u16 = MIN_FINAL_CLTV_EXPIRY_DELTA + 2;

/// Everything needed to assemble the wrapped invoice. `payment_hash` and
/// `payment_secret` come from `ChannelManager::create_inbound_payment`;
/// `intercept_scid` and `lsp_cltv_expiry_delta` from the LSPS2
/// `InvoiceParametersReady` event.
#[derive(Debug, Clone)]
pub(crate) struct JitInvoiceParams {
    pub(crate) lsp_node_id: PublicKey,
    pub(crate) intercept_scid: u64,
    pub(crate) lsp_cltv_expiry_delta: u32,
    pub(crate) amount_msat: u64,
    pub(crate) payment_hash: PaymentHash,
    pub(crate) payment_secret: PaymentSecret,
    /// Invoice validity in seconds from now, aligned to the opening params'
    /// `valid_until` (the LSP only guarantees the fee menu that long).
    pub(crate) expiry_secs: u32,
    pub(crate) network: Network,
    pub(crate) description: String,
}

/// Builds and signs the Megalith-wrapped fixed-amount invoice. The route hint
/// is a single hop from the LSP over the intercept SCID with zero hint fees —
/// the opening fee is skimmed off the forwarded HTLC, not charged as routing
/// fees (mirrors ldk-node exactly).
pub(crate) fn build_jit_invoice(
    params: &JitInvoiceParams,
    node_secret: &SecretKey,
) -> Result<Bolt11Invoice, CreationError> {
    let route_hint = RouteHint(vec![RouteHintHop {
        src_node_id: params.lsp_node_id,
        short_channel_id: params.intercept_scid,
        fees: RoutingFees {
            base_msat: 0,
            proportional_millionths: 0,
        },
        cltv_expiry_delta: params.lsp_cltv_expiry_delta as u16,
        htlc_minimum_msat: None,
        htlc_maximum_msat: None,
    }]);

    let payment_hash = sha256::Hash::from_byte_array(params.payment_hash.0);

    InvoiceBuilder::new(Currency::from(params.network))
        .description(params.description.clone())
        .payment_hash(payment_hash)
        .payment_secret(params.payment_secret)
        // ldk-node uses `current_timestamp()`; that helper is behind
        // lightning-invoice's non-default `std` feature, so set the same
        // value explicitly.
        .duration_since_epoch(crate::util::unix_now())
        .min_final_cltv_expiry_delta(JIT_MIN_FINAL_CLTV_EXPIRY_DELTA as u64)
        .expiry_time(Duration::from_secs(params.expiry_secs as u64))
        .private_route(route_hint)
        // KTD-7: fixed amount + basic_mpp, never the zero-amount variant.
        .amount_milli_satoshis(params.amount_msat)
        .basic_mpp()
        .build_signed(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, node_secret))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    use crate::config::MEGALITH_LSP_NODE_ID;

    /// A fabricated `InvoiceParametersReady`-equivalent (AE1 assembly half):
    /// intercept SCID + CLTV delta as the LSP would return them.
    fn test_params() -> JitInvoiceParams {
        JitInvoiceParams {
            lsp_node_id: PublicKey::from_str(MEGALITH_LSP_NODE_ID).unwrap(),
            intercept_scid: 0x0001_2345_6789_abcd,
            lsp_cltv_expiry_delta: 144,
            amount_msat: 250_000_000,
            payment_hash: PaymentHash([7u8; 32]),
            payment_secret: PaymentSecret([9u8; 32]),
            expiry_secs: 3_600,
            network: Network::Bitcoin,
            description: "zinqq spike".to_string(),
        }
    }

    fn test_secret() -> SecretKey {
        SecretKey::from_slice(&[0x2b; 32]).unwrap()
    }

    #[test]
    fn jit_invoice_route_hint_carries_intercept_scid_and_lsp_identity() {
        let params = test_params();
        let invoice = build_jit_invoice(&params, &test_secret()).unwrap();

        let hints = invoice.route_hints();
        assert_eq!(hints.len(), 1, "exactly one private route hint");
        let hops = &hints[0].0;
        assert_eq!(hops.len(), 1, "exactly one hop: the LSP");
        let hop = &hops[0];
        assert_eq!(
            hop.src_node_id, params.lsp_node_id,
            "hint must be from Megalith"
        );
        assert_eq!(hop.short_channel_id, params.intercept_scid);
        assert_eq!(hop.fees.base_msat, 0, "hint fees must be zero");
        assert_eq!(
            hop.fees.proportional_millionths, 0,
            "hint fees must be zero"
        );
        assert_eq!(hop.cltv_expiry_delta as u32, params.lsp_cltv_expiry_delta);
        assert!(
            hop.cltv_expiry_delta as u32 >= params.lsp_cltv_expiry_delta,
            "hint CLTV must be at least the event's value"
        );
    }

    #[test]
    fn jit_invoice_amount_expiry_and_hash_match_the_request() {
        let params = test_params();
        let invoice = build_jit_invoice(&params, &test_secret()).unwrap();

        assert_eq!(invoice.amount_milli_satoshis(), Some(params.amount_msat));
        assert_eq!(
            invoice.expiry_time(),
            Duration::from_secs(params.expiry_secs as u64),
            "expiry must align to the params' valid_until window"
        );
        assert_eq!(
            invoice.payment_hash().to_byte_array(),
            params.payment_hash.0
        );
        assert_eq!(invoice.payment_secret().0, params.payment_secret.0);
        assert_eq!(invoice.network(), Network::Bitcoin);
    }

    #[test]
    fn jit_invoice_bumps_min_final_cltv_and_allows_mpp() {
        let params = test_params();
        let invoice = build_jit_invoice(&params, &test_secret()).unwrap();

        // KTD-9: at least +2 over LDK's default.
        assert_eq!(
            invoice.min_final_cltv_expiry_delta(),
            (MIN_FINAL_CLTV_EXPIRY_DELTA + 2) as u64
        );
        assert!(invoice.min_final_cltv_expiry_delta() >= (MIN_FINAL_CLTV_EXPIRY_DELTA as u64) + 2);

        // KTD-7: fixed-amount mode must permit MPP.
        assert!(
            invoice
                .features()
                .is_some_and(|features| features.supports_basic_mpp()),
            "fixed-amount JIT invoice must advertise basic_mpp"
        );
    }

    #[test]
    fn jit_invoice_is_signed_by_the_node_key_and_reparses() {
        let params = test_params();
        let secret = test_secret();
        let invoice = build_jit_invoice(&params, &secret).unwrap();

        let expected_payee = PublicKey::from_secret_key(&Secp256k1::new(), &secret);
        assert_eq!(invoice.recover_payee_pub_key(), expected_payee);

        // The displayed bolt11 string round-trips through a fresh parse.
        let reparsed = Bolt11Invoice::from_str(&invoice.to_string()).unwrap();
        assert_eq!(reparsed, invoice);
    }
}
