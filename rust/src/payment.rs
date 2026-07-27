//! Outbound Lightning payment flow (U5, extended by U6 for R5).
//!
//! BOLT11: `send_bolt11` parses and validates the invoice (mainnet,
//! unexpired) and pays it through the `ChannelManager` with a stable
//! [`PaymentId`] derived from the payment hash. LDK persists pending
//! outbound payments inside the channel manager, so after a restart a
//! re-send of the same invoice is rejected with
//! `RetryableSendFailure::DuplicatePayment` instead of double-paying — the
//! derivation IS the idempotency key. U6 adds the amount override for
//! amountless invoices (PWA `sendBolt11Payment`): an override is REQUIRED
//! for an amountless invoice and REJECTED on an amounted one.
//!
//! BOLT12 (U6): `send_bolt12` validates the offer (mainnet, unexpired, the
//! same amount-override matrix) and pays via `pay_for_offer` with a caller-
//! supplied 32-byte random [`PaymentId`] and optional payer note (PWA
//! `sendBolt12Payment`). Retries are `Retry::Attempts(3)` for both flows —
//! the PWA's `Retry.constructor_attempts(3)`.
//!
//! Outcomes surface through the persisted event queue: LDK's `PaymentSent` /
//! `PaymentFailed` events map to the public `PaymentSuccessful` /
//! `PaymentFailed { reason }` (see `node::handle_ldk_event`), with reasons
//! rendered by [`describe_failure_reason`] — the PWA's
//! `describePaymentFailure` strings VERBATIM (`event-handler.ts:919-942`).
//! Failures of the initial attempt (e.g. route-not-found) are returned
//! synchronously by LDK WITHOUT an event, so `Node::send_payment` /
//! `Node::pay_offer` push `PaymentFailed` themselves for those.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use bitcoin::constants::ChainHash;
use bitcoin::hashes::Hash as _;
use bitcoin::Network;
use lightning::events::PaymentFailureReason;
use lightning::ln::channelmanager::{
    Bolt11PaymentError, OptionalOfferPaymentParams, PaymentId, Retry, RetryableSendFailure,
};
use lightning::offers::offer::{Amount as OfferAmount, Offer};
use lightning::offers::parse::Bolt12SemanticError;
use lightning::routing::router::RouteParametersConfig;
use lightning_invoice::Bolt11Invoice;

use crate::types::ChannelManager;

/// Retry strategy for one send: three attempts, the PWA's
/// `Retry.constructor_attempts(3)` (`context.tsx:997,1063`; F1 "retry ×3").
pub(crate) const SEND_RETRY: Retry = Retry::Attempts(3);

/// Typed send failures. Every variant renders to a DISTINCT message (see
/// `Display`); the attempt-phase ones also feed `PaymentFailed { reason }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// The node is not running.
    NotRunning,
    /// The bolt11 string failed to parse or verify.
    InvalidInvoice(String),
    /// The invoice is already expired (or expired while sending).
    InvoiceExpired,
    /// The invoice is for a different network than the node's (mainnet).
    WrongNetwork { expected: Network, found: Network },
    /// An amountless invoice/offer with no amount override supplied.
    AmountMissing,
    /// An amount override supplied for an invoice/offer that already carries
    /// an amount (U6: overrides are for amountless requests only).
    AmountOverrideNotAllowed,
    /// The offer string failed to parse or verify (U6).
    InvalidOffer(String),
    /// The offer is for a different network than the node's (U6).
    OfferWrongNetwork,
    /// The offer is already expired (U6).
    OfferExpired,
    /// A payment for the same payment hash is already pending in the channel
    /// manager — paying again would risk paying twice.
    DuplicatePayment,
    /// No route to the recipient was found.
    RouteNotFound,
    /// The send attempt failed for another reason.
    SendFailed(String),
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendError::NotRunning => write!(f, "the node is not running"),
            SendError::InvalidInvoice(message) => {
                write!(f, "invalid bolt11 invoice: {message}")
            }
            SendError::InvoiceExpired => write!(f, "the invoice is expired"),
            SendError::WrongNetwork { expected, found } => write!(
                f,
                "the invoice is for the {found} network, this wallet only pays {expected} invoices"
            ),
            // The PWA's copy, verbatim (context.tsx:981).
            SendError::AmountMissing => write!(
                f,
                "Amount is required for invoices without an embedded amount"
            ),
            SendError::AmountOverrideNotAllowed => write!(
                f,
                "an amount override is only allowed for requests without an embedded amount"
            ),
            SendError::InvalidOffer(message) => write!(f, "invalid bolt12 offer: {message}"),
            SendError::OfferWrongNetwork => write!(
                f,
                "the offer is for a different network, this wallet only pays bitcoin offers"
            ),
            SendError::OfferExpired => write!(f, "the offer is expired"),
            SendError::DuplicatePayment => {
                write!(f, "a payment for this invoice is already pending")
            }
            SendError::RouteNotFound => write!(f, "no route to the recipient was found"),
            SendError::SendFailed(reason) => write!(f, "sending the payment failed: {reason}"),
        }
    }
}

impl std::error::Error for SendError {}

impl SendError {
    /// Whether this failure happened at the pay attempt (after validation),
    /// meaning LDK abandoned synchronously WITHOUT queueing a `PaymentFailed`
    /// event — the caller must push one itself. Validation failures never
    /// attempted anything, and a duplicate's outcome belongs to the original
    /// in-flight attempt.
    pub(crate) fn is_attempt_failure(&self) -> bool {
        matches!(self, SendError::RouteNotFound | SendError::SendFailed(_))
    }
}

/// The stable idempotency key (R3): `PaymentId` = payment hash bytes, so the
/// same invoice always maps to the same `PaymentId`, across restarts.
pub(crate) fn payment_id_for(invoice: &Bolt11Invoice) -> PaymentId {
    PaymentId(invoice.payment_hash().to_byte_array())
}

/// The seam between the send flow and LDK's payment machinery, so tests can
/// intercept the attempt (and fabricate LDK failures LDK won't produce
/// offline, e.g. `DuplicatePayment`). `amount_msat` is the U6 amount
/// override — `Some` exactly when the invoice is amountless.
pub(crate) trait Bolt11Payer {
    fn pay(
        &self,
        invoice: &Bolt11Invoice,
        payment_id: PaymentId,
        amount_msat: Option<u64>,
        retry: Retry,
    ) -> Result<(), Bolt11PaymentError>;
}

impl Bolt11Payer for ChannelManager {
    fn pay(
        &self,
        invoice: &Bolt11Invoice,
        payment_id: PaymentId,
        amount_msat: Option<u64>,
        retry: Retry,
    ) -> Result<(), Bolt11PaymentError> {
        // Default route params; routing runs over the RGS-fed graph + scorer
        // (U2's router). `amount_msats` is LDK's override slot, used only
        // for amountless invoices (as in the PWA, context.tsx:989-999).
        self.pay_for_bolt11_invoice(
            invoice,
            payment_id,
            amount_msat,
            RouteParametersConfig::default(),
            retry,
        )
    }
}

/// The BOLT12 counterpart of [`Bolt11Payer`] (U6).
pub(crate) trait Bolt12Payer {
    fn pay_offer(
        &self,
        offer: &Offer,
        amount_msat: Option<u64>,
        payment_id: PaymentId,
        payer_note: Option<String>,
        retry: Retry,
    ) -> Result<(), Bolt12SemanticError>;
}

impl Bolt12Payer for ChannelManager {
    fn pay_offer(
        &self,
        offer: &Offer,
        amount_msat: Option<u64>,
        payment_id: PaymentId,
        payer_note: Option<String>,
        retry: Retry,
    ) -> Result<(), Bolt12SemanticError> {
        // PWA context.tsx:1050-1061: pay_for_offer with optional amount
        // override, payer note, default route params, retry ×3.
        self.pay_for_offer(
            offer,
            amount_msat,
            payment_id,
            OptionalOfferPaymentParams {
                payer_note,
                route_params_config: RouteParametersConfig::default(),
                retry_strategy: retry,
            },
        )
    }
}

/// Parses and validates a bolt11 string: well-formed, right network,
/// unexpired at `now_since_epoch`, and a consistent amount picture (U6):
/// an override is required for amountless invoices and rejected on amounted
/// ones. Returns the invoice and the resolved amount to send. Each
/// rejection is a distinct [`SendError`].
pub(crate) fn parse_and_validate(
    bolt11: &str,
    network: Network,
    now_since_epoch: Duration,
    amount_override_msat: Option<u64>,
) -> Result<(Bolt11Invoice, u64), SendError> {
    let invoice =
        Bolt11Invoice::from_str(bolt11).map_err(|e| SendError::InvalidInvoice(e.to_string()))?;
    let found = invoice.network();
    if found != network {
        return Err(SendError::WrongNetwork {
            expected: network,
            found,
        });
    }
    if invoice.would_expire(now_since_epoch) {
        return Err(SendError::InvoiceExpired);
    }
    let amount_msat = resolve_amount(invoice.amount_milli_satoshis(), amount_override_msat)?;
    Ok((invoice, amount_msat))
}

/// The U6 amount-override matrix, shared by BOLT11 and BOLT12: embedded
/// amounts must not be overridden; amountless requests require one.
pub(crate) fn resolve_amount(
    embedded_msat: Option<u64>,
    override_msat: Option<u64>,
) -> Result<u64, SendError> {
    match (embedded_msat, override_msat) {
        (Some(_), Some(_)) => Err(SendError::AmountOverrideNotAllowed),
        (Some(embedded), None) => Ok(embedded),
        (None, Some(override_amount)) => Ok(override_amount),
        (None, None) => Err(SendError::AmountMissing),
    }
}

/// Parses and validates a bolt12 offer string (U6): well-formed, supports
/// the node's chain (an offer with no chains field implicitly targets
/// mainnet), unexpired. Returns the offer and its bitcoin-denominated
/// embedded amount (fiat-denominated offers count as amountless, PWA
/// parity).
pub(crate) fn validate_offer(
    offer_str: &str,
    network: Network,
    now_since_epoch: Duration,
) -> Result<(Offer, Option<u64>), SendError> {
    let offer =
        Offer::from_str(offer_str).map_err(|e| SendError::InvalidOffer(format!("{e:?}")))?;
    if !offer.supports_chain(ChainHash::using_genesis_block(network)) {
        return Err(SendError::OfferWrongNetwork);
    }
    if offer.is_expired_no_std(now_since_epoch) {
        return Err(SendError::OfferExpired);
    }
    let embedded_msat = match offer.amount() {
        Some(OfferAmount::Bitcoin { amount_msats }) => Some(amount_msats),
        _ => None,
    };
    Ok((offer, embedded_msat))
}

/// The full BOLT11 send flow: validate → derive the stable `PaymentId` →
/// pay with retry ×3. Returns the derived `PaymentId` on a successful
/// handoff to LDK (the payment outcome itself arrives later as an LDK
/// event).
pub(crate) fn send_bolt11(
    payer: &dyn Bolt11Payer,
    bolt11: &str,
    network: Network,
    now_since_epoch: Duration,
    amount_override_msat: Option<u64>,
) -> Result<PaymentId, SendError> {
    let (invoice, _amount_msat) =
        parse_and_validate(bolt11, network, now_since_epoch, amount_override_msat)?;
    let payment_id = payment_id_for(&invoice);
    // LDK's override slot must be Some exactly for amountless invoices;
    // parse_and_validate guarantees the override is None otherwise.
    let ldk_amount = match invoice.amount_milli_satoshis() {
        Some(_) => None,
        None => amount_override_msat,
    };
    payer
        .pay(&invoice, payment_id, ldk_amount, SEND_RETRY)
        .map_err(map_pay_error)?;
    Ok(payment_id)
}

/// The full BOLT12 send flow (U6): validate → amount matrix → pay_for_offer
/// with the caller-supplied random `PaymentId`, payer note, and retry ×3.
/// Returns the resolved amount (embedded or override) for the history row.
pub(crate) fn send_bolt12(
    payer: &dyn Bolt12Payer,
    offer_str: &str,
    network: Network,
    now_since_epoch: Duration,
    amount_override_msat: Option<u64>,
    payer_note: Option<String>,
    payment_id: PaymentId,
) -> Result<u64, SendError> {
    let (offer, embedded_msat) = validate_offer(offer_str, network, now_since_epoch)?;
    let resolved_msat = resolve_amount(embedded_msat, amount_override_msat)?;
    // The pay-time override slot is Some exactly for amountless offers
    // (PWA context.tsx:754-757 passes the entered amount only then).
    let ldk_amount = match embedded_msat {
        Some(_) => None,
        None => Some(resolved_msat),
    };
    payer
        .pay_offer(&offer, ldk_amount, payment_id, payer_note, SEND_RETRY)
        .map_err(map_offer_error)?;
    Ok(resolved_msat)
}

/// Maps LDK's synchronous `pay_for_offer` failures onto typed [`SendError`]s.
fn map_offer_error(error: Bolt12SemanticError) -> SendError {
    match error {
        Bolt12SemanticError::DuplicatePaymentId => SendError::DuplicatePayment,
        Bolt12SemanticError::AlreadyExpired => SendError::OfferExpired,
        Bolt12SemanticError::UnsupportedChain => SendError::OfferWrongNetwork,
        Bolt12SemanticError::MissingAmount => SendError::AmountMissing,
        other => SendError::SendFailed(format!("{other:?}")),
    }
}

/// Maps LDK's pay-time failures onto the typed [`SendError`]s.
fn map_pay_error(error: Bolt11PaymentError) -> SendError {
    match error {
        // Unreachable after validation (the invoice always carries an amount
        // and none is supplied), but map it honestly anyway.
        Bolt11PaymentError::InvalidAmount => SendError::AmountMissing,
        Bolt11PaymentError::SendingFailed(RetryableSendFailure::DuplicatePayment) => {
            SendError::DuplicatePayment
        }
        Bolt11PaymentError::SendingFailed(RetryableSendFailure::RouteNotFound) => {
            SendError::RouteNotFound
        }
        Bolt11PaymentError::SendingFailed(RetryableSendFailure::PaymentExpired) => {
            SendError::InvoiceExpired
        }
        Bolt11PaymentError::SendingFailed(other) => SendError::SendFailed(format!("{other:?}")),
    }
}

/// Renders LDK's `PaymentFailureReason` for the public
/// `PaymentFailed { reason }` event AND the U5 settle path's stored
/// `failure_reason` — the ONE failure-taxonomy mapping (U6, R5). Strings are
/// the PWA's `describePaymentFailure` VERBATIM (`event-handler.ts:919-942`),
/// including the "Payment failed" default for unknown/absent reasons.
pub(crate) fn describe_failure_reason(reason: Option<PaymentFailureReason>) -> String {
    let text = match reason {
        Some(PaymentFailureReason::RecipientRejected) => "Payment was rejected by the recipient",
        Some(PaymentFailureReason::UserAbandoned) => "Payment was cancelled",
        Some(PaymentFailureReason::RetriesExhausted) => "No route found after multiple attempts",
        Some(PaymentFailureReason::PaymentExpired) => "Payment expired",
        Some(PaymentFailureReason::RouteNotFound) => "No route found to the recipient",
        Some(PaymentFailureReason::UnexpectedError) => "An unexpected error occurred",
        Some(PaymentFailureReason::UnknownRequiredFeatures) => {
            "Recipient requires unsupported features"
        }
        Some(PaymentFailureReason::InvoiceRequestExpired) => {
            "Invoice request timed out — recipient may be offline"
        }
        Some(PaymentFailureReason::InvoiceRequestRejected) => {
            "Invoice request was rejected by the recipient"
        }
        // The PWA's default arm (event-handler.ts:939-940) — includes
        // BlindedPathCreationFailed and any reason LDK adds later.
        Some(_) | None => "Payment failed",
    };
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use bitcoin::hashes::sha256;
    use bitcoin::secp256k1::{Secp256k1, SecretKey};
    use lightning::types::payment::PaymentSecret;
    use lightning_invoice::{Currency, InvoiceBuilder};

    use crate::config::Config;
    use crate::node::{CoreEvent, EventSink, Node};

    /// Fixed invoice creation time for the pure validation tests.
    const NOW: u64 = 1_753_000_000;

    fn unix_now_secs() -> u64 {
        crate::util::unix_now().as_secs()
    }

    /// Builds and signs a minimal test invoice.
    fn test_invoice(
        currency: Currency,
        amount_msat: Option<u64>,
        created_at_unix_secs: u64,
        expiry_secs: u64,
    ) -> Bolt11Invoice {
        let secret = SecretKey::from_slice(&[0x3c; 32]).unwrap();
        let builder = InvoiceBuilder::new(currency)
            .description("u5 send test".to_string())
            .payment_hash(sha256::Hash::from_byte_array([0x11; 32]))
            .payment_secret(PaymentSecret([0x22; 32]))
            .duration_since_epoch(Duration::from_secs(created_at_unix_secs))
            .min_final_cltv_expiry_delta(144)
            .expiry_time(Duration::from_secs(expiry_secs));
        let sign = |hash: &_| Secp256k1::new().sign_ecdsa_recoverable(hash, &secret);
        match amount_msat {
            Some(amount) => builder
                .amount_milli_satoshis(amount)
                .build_signed(sign)
                .unwrap(),
            None => builder.build_signed(sign).unwrap(),
        }
    }

    fn valid_mainnet_invoice() -> Bolt11Invoice {
        test_invoice(Currency::Bitcoin, Some(25_000_000), NOW, 3_600)
    }

    fn just_after(created_at_unix_secs: u64) -> Duration {
        Duration::from_secs(created_at_unix_secs + 1)
    }

    /// Records every pay attempt and answers with a canned LDK result
    /// (consumed on first use — `Bolt11PaymentError` is not `Clone`).
    struct MockPayer {
        calls: Mutex<Vec<(PaymentId, Option<u64>, Retry)>>,
        response: Mutex<Option<Result<(), Bolt11PaymentError>>>,
    }

    impl MockPayer {
        fn answering(response: Result<(), Bolt11PaymentError>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                response: Mutex::new(Some(response)),
            }
        }

        fn calls(&self) -> Vec<(PaymentId, Option<u64>, Retry)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Bolt11Payer for MockPayer {
        fn pay(
            &self,
            _invoice: &Bolt11Invoice,
            payment_id: PaymentId,
            amount_msat: Option<u64>,
            retry: Retry,
        ) -> Result<(), Bolt11PaymentError> {
            self.calls
                .lock()
                .unwrap()
                .push((payment_id, amount_msat, retry));
            self.response
                .lock()
                .unwrap()
                .take()
                .expect("MockPayer answered more than once")
        }
    }

    /// One recorded offer-pay attempt: (offer, amount override, payment id,
    /// payer note, retry).
    type OfferCall = (String, Option<u64>, PaymentId, Option<String>, Retry);

    /// The BOLT12 counterpart: records offer-pay attempts.
    struct MockOfferPayer {
        calls: Mutex<Vec<OfferCall>>,
        response: Mutex<Option<Result<(), Bolt12SemanticError>>>,
    }

    impl MockOfferPayer {
        fn answering(response: Result<(), Bolt12SemanticError>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                response: Mutex::new(Some(response)),
            }
        }

        fn calls(&self) -> Vec<OfferCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Bolt12Payer for MockOfferPayer {
        fn pay_offer(
            &self,
            offer: &Offer,
            amount_msat: Option<u64>,
            payment_id: PaymentId,
            payer_note: Option<String>,
            retry: Retry,
        ) -> Result<(), Bolt12SemanticError> {
            self.calls.lock().unwrap().push((
                offer.to_string(),
                amount_msat,
                payment_id,
                payer_note,
                retry,
            ));
            self.response
                .lock()
                .unwrap()
                .take()
                .expect("MockOfferPayer answered more than once")
        }
    }

    // ---------- happy path: stable PaymentId derivation ----------

    #[test]
    fn send_attempt_carries_the_payment_id_derived_from_the_payment_hash() {
        let invoice = valid_mainnet_invoice();
        let payer = MockPayer::answering(Ok(()));

        let payment_id = send_bolt11(
            &payer,
            &invoice.to_string(),
            Network::Bitcoin,
            just_after(NOW),
            None,
        )
        .expect("valid invoice with an accepting payer must succeed");

        // R3: PaymentId == payment hash bytes, so a restarted app re-derives
        // the identical id and LDK's duplicate rejection kicks in.
        assert_eq!(payment_id.0, invoice.payment_hash().to_byte_array());

        let calls = payer.calls();
        assert_eq!(calls.len(), 1, "exactly one pay attempt");
        assert_eq!(calls[0].0, payment_id, "the attempt used the derived id");
        assert_eq!(
            calls[0].1, None,
            "no LDK amount override for an amounted invoice"
        );
        assert_eq!(
            calls[0].2,
            Retry::Attempts(3),
            "PWA-parity retry strategy (context.tsx:997)"
        );
    }

    #[test]
    fn payment_id_derivation_is_deterministic_across_reparses() {
        let bolt11 = valid_mainnet_invoice().to_string();
        let a = payment_id_for(&Bolt11Invoice::from_str(&bolt11).unwrap());
        let b = payment_id_for(&Bolt11Invoice::from_str(&bolt11).unwrap());
        assert_eq!(a, b, "same invoice must always derive the same PaymentId");
    }

    // ---------- validation: each rejection is distinct, nothing is paid ----------

    #[test]
    fn malformed_invoice_is_rejected_before_any_pay_attempt() {
        let payer = MockPayer::answering(Ok(()));
        let err = send_bolt11(
            &payer,
            "not an invoice",
            Network::Bitcoin,
            just_after(NOW),
            None,
        )
        .unwrap_err();
        assert!(matches!(err, SendError::InvalidInvoice(_)), "got {err:?}");
        assert!(payer.calls().is_empty(), "nothing may be paid");
    }

    #[test]
    fn expired_invoice_is_rejected_before_any_pay_attempt() {
        let invoice = test_invoice(Currency::Bitcoin, Some(25_000_000), NOW, 60);
        let payer = MockPayer::answering(Ok(()));
        let err = send_bolt11(
            &payer,
            &invoice.to_string(),
            Network::Bitcoin,
            Duration::from_secs(NOW + 61),
            None,
        )
        .unwrap_err();
        assert_eq!(err, SendError::InvoiceExpired);
        assert!(payer.calls().is_empty(), "nothing may be paid");
    }

    #[test]
    fn testnet_and_signet_invoices_are_rejected_with_the_found_network() {
        let payer = MockPayer::answering(Ok(()));
        for (currency, found) in [
            (Currency::BitcoinTestnet, Network::Testnet),
            (Currency::Signet, Network::Signet),
        ] {
            let invoice = test_invoice(currency, Some(25_000_000), NOW, 3_600);
            let err = send_bolt11(
                &payer,
                &invoice.to_string(),
                Network::Bitcoin,
                just_after(NOW),
                None,
            )
            .unwrap_err();
            assert_eq!(
                err,
                SendError::WrongNetwork {
                    expected: Network::Bitcoin,
                    found,
                }
            );
        }
        assert!(payer.calls().is_empty(), "nothing may be paid");
    }

    // ---------- U6 amount-override matrix ----------

    #[test]
    fn amountless_invoice_without_override_is_rejected_before_any_pay_attempt() {
        let invoice = test_invoice(Currency::Bitcoin, None, NOW, 3_600);
        let payer = MockPayer::answering(Ok(()));
        let err = send_bolt11(
            &payer,
            &invoice.to_string(),
            Network::Bitcoin,
            just_after(NOW),
            None,
        )
        .unwrap_err();
        assert_eq!(err, SendError::AmountMissing);
        // PWA copy, verbatim (context.tsx:981).
        assert_eq!(
            err.to_string(),
            "Amount is required for invoices without an embedded amount"
        );
        assert!(payer.calls().is_empty(), "nothing may be paid");
    }

    #[test]
    fn amountless_invoice_with_override_pays_with_the_override() {
        let invoice = test_invoice(Currency::Bitcoin, None, NOW, 3_600);
        let payer = MockPayer::answering(Ok(()));
        send_bolt11(
            &payer,
            &invoice.to_string(),
            Network::Bitcoin,
            just_after(NOW),
            Some(12_345_000),
        )
        .expect("amountless invoice + override must pay");
        let calls = payer.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].1,
            Some(12_345_000),
            "the override rides LDK's amount slot"
        );
    }

    #[test]
    fn override_on_an_amounted_invoice_is_a_typed_rejection() {
        let invoice = valid_mainnet_invoice();
        let payer = MockPayer::answering(Ok(()));
        let err = send_bolt11(
            &payer,
            &invoice.to_string(),
            Network::Bitcoin,
            just_after(NOW),
            Some(1_000),
        )
        .unwrap_err();
        assert_eq!(err, SendError::AmountOverrideNotAllowed);
        assert!(payer.calls().is_empty(), "nothing may be paid");
    }

    #[test]
    fn resolve_amount_matrix_is_exhaustive() {
        assert_eq!(resolve_amount(Some(5), None), Ok(5));
        assert_eq!(
            resolve_amount(Some(5), Some(7)),
            Err(SendError::AmountOverrideNotAllowed)
        );
        assert_eq!(resolve_amount(None, Some(7)), Ok(7));
        assert_eq!(resolve_amount(None, None), Err(SendError::AmountMissing));
    }

    #[test]
    fn parse_and_validate_returns_the_resolved_amount() {
        let amounted = valid_mainnet_invoice().to_string();
        let (_, amount) =
            parse_and_validate(&amounted, Network::Bitcoin, just_after(NOW), None).unwrap();
        assert_eq!(amount, 25_000_000);

        let amountless = test_invoice(Currency::Bitcoin, None, NOW, 3_600).to_string();
        let (_, amount) =
            parse_and_validate(&amountless, Network::Bitcoin, just_after(NOW), Some(42_000))
                .unwrap();
        assert_eq!(amount, 42_000);
    }

    #[test]
    fn send_error_messages_are_all_distinct() {
        let errors = [
            SendError::NotRunning,
            SendError::InvalidInvoice("bad checksum".to_string()),
            SendError::InvoiceExpired,
            SendError::WrongNetwork {
                expected: Network::Bitcoin,
                found: Network::Testnet,
            },
            SendError::AmountMissing,
            SendError::AmountOverrideNotAllowed,
            SendError::InvalidOffer("bad bech32".to_string()),
            SendError::OfferWrongNetwork,
            SendError::OfferExpired,
            SendError::DuplicatePayment,
            SendError::RouteNotFound,
            SendError::SendFailed("onion packet too large".to_string()),
        ];
        for (i, a) in errors.iter().enumerate() {
            for b in errors.iter().skip(i + 1) {
                assert_ne!(a.to_string(), b.to_string(), "reasons must be distinct");
            }
        }
    }

    // ---------- idempotency: LDK's duplicate rejection surfaces typed ----------

    #[test]
    fn duplicate_payment_id_maps_to_a_distinct_duplicate_error() {
        // LDK only produces DuplicatePayment when a pending payment with the
        // same PaymentId is registered, which needs a routable first attempt —
        // impossible offline. Fabricate LDK's answer at the payer seam; the
        // node-level test below covers what a channel-less node really does.
        let invoice = valid_mainnet_invoice();
        let payer = MockPayer::answering(Err(Bolt11PaymentError::SendingFailed(
            RetryableSendFailure::DuplicatePayment,
        )));
        let err = send_bolt11(
            &payer,
            &invoice.to_string(),
            Network::Bitcoin,
            just_after(NOW),
            None,
        )
        .unwrap_err();
        assert_eq!(err, SendError::DuplicatePayment);
        assert!(
            !err.is_attempt_failure(),
            "a duplicate must NOT push PaymentFailed — the original attempt owns the outcome"
        );
    }

    #[test]
    fn ldk_pay_failures_map_to_distinct_typed_errors() {
        let cases = [
            (
                Bolt11PaymentError::SendingFailed(RetryableSendFailure::RouteNotFound),
                SendError::RouteNotFound,
            ),
            (
                Bolt11PaymentError::SendingFailed(RetryableSendFailure::PaymentExpired),
                SendError::InvoiceExpired,
            ),
            (Bolt11PaymentError::InvalidAmount, SendError::AmountMissing),
            (
                Bolt11PaymentError::SendingFailed(RetryableSendFailure::OnionPacketSizeExceeded),
                SendError::SendFailed("OnionPacketSizeExceeded".to_string()),
            ),
        ];
        for (ldk_error, expected) in cases {
            let invoice = valid_mainnet_invoice();
            let payer = MockPayer::answering(Err(ldk_error));
            let err = send_bolt11(
                &payer,
                &invoice.to_string(),
                Network::Bitcoin,
                just_after(NOW),
                None,
            )
            .unwrap_err();
            assert_eq!(err, expected);
        }
    }

    // ---------- U6: BOLT12 offer sends at the payer seam ----------

    fn offer_signing_pubkey() -> bitcoin::secp256k1::PublicKey {
        let secp = Secp256k1::new();
        bitcoin::secp256k1::PublicKey::from_secret_key(
            &secp,
            &SecretKey::from_slice(&[0x3c; 32]).unwrap(),
        )
    }

    fn amounted_offer(amount_msat: u64) -> String {
        lightning::offers::offer::OfferBuilder::new(offer_signing_pubkey())
            .description("bolt12 test".to_string())
            .amount_msats(amount_msat)
            .build()
            .unwrap()
            .to_string()
    }

    fn amountless_offer() -> String {
        lightning::offers::offer::OfferBuilder::new(offer_signing_pubkey())
            .description("bolt12 test".to_string())
            .build()
            .unwrap()
            .to_string()
    }

    const RANDOM_ID: PaymentId = PaymentId([0x77; 32]);

    #[test]
    fn amounted_offer_pays_with_no_ldk_override_and_the_payer_note() {
        let offer = amounted_offer(9_000);
        let payer = MockOfferPayer::answering(Ok(()));
        let recorded = send_bolt12(
            &payer,
            &offer,
            Network::Bitcoin,
            just_after(NOW),
            None,
            Some("thanks!".to_string()),
            RANDOM_ID,
        )
        .expect("amounted offer must pay");
        assert_eq!(recorded, 9_000, "the row records the embedded amount");
        let calls = payer.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, None, "no override for an embedded amount");
        assert_eq!(calls[0].2, RANDOM_ID, "the caller-supplied random id");
        assert_eq!(calls[0].3.as_deref(), Some("thanks!"));
        assert_eq!(calls[0].4, Retry::Attempts(3), "PWA retry ×3 parity");
    }

    #[test]
    fn amountless_offer_requires_and_uses_the_override() {
        let offer = amountless_offer();
        let payer = MockOfferPayer::answering(Ok(()));
        let err = send_bolt12(
            &payer,
            &offer,
            Network::Bitcoin,
            just_after(NOW),
            None,
            None,
            RANDOM_ID,
        )
        .unwrap_err();
        assert_eq!(err, SendError::AmountMissing);
        assert!(payer.calls().is_empty(), "nothing may be paid");

        let payer = MockOfferPayer::answering(Ok(()));
        let recorded = send_bolt12(
            &payer,
            &offer,
            Network::Bitcoin,
            just_after(NOW),
            Some(5_500),
            None,
            RANDOM_ID,
        )
        .unwrap();
        assert_eq!(recorded, 5_500);
        assert_eq!(payer.calls()[0].1, Some(5_500), "override in the LDK slot");
    }

    #[test]
    fn override_on_an_amounted_offer_is_a_typed_rejection() {
        let offer = amounted_offer(9_000);
        let payer = MockOfferPayer::answering(Ok(()));
        let err = send_bolt12(
            &payer,
            &offer,
            Network::Bitcoin,
            just_after(NOW),
            Some(10_000),
            None,
            RANDOM_ID,
        )
        .unwrap_err();
        assert_eq!(err, SendError::AmountOverrideNotAllowed);
        assert!(payer.calls().is_empty(), "nothing may be paid");
    }

    #[test]
    fn offer_validation_rejects_garbage_wrong_network_and_expired() {
        let payer = MockOfferPayer::answering(Ok(()));
        let err = send_bolt12(
            &payer,
            "lno1garbage",
            Network::Bitcoin,
            just_after(NOW),
            Some(1),
            None,
            RANDOM_ID,
        )
        .unwrap_err();
        assert!(matches!(err, SendError::InvalidOffer(_)), "{err:?}");

        let testnet = lightning::offers::offer::OfferBuilder::new(offer_signing_pubkey())
            .description("t".to_string())
            .chain(Network::Testnet)
            .build()
            .unwrap()
            .to_string();
        let err = send_bolt12(
            &payer,
            &testnet,
            Network::Bitcoin,
            just_after(NOW),
            Some(1),
            None,
            RANDOM_ID,
        )
        .unwrap_err();
        assert_eq!(err, SendError::OfferWrongNetwork);

        let expired = lightning::offers::offer::OfferBuilder::new(offer_signing_pubkey())
            .description("t".to_string())
            .absolute_expiry(Duration::from_secs(NOW - 1))
            .build()
            .unwrap()
            .to_string();
        let err = send_bolt12(
            &payer,
            &expired,
            Network::Bitcoin,
            just_after(NOW),
            Some(1),
            None,
            RANDOM_ID,
        )
        .unwrap_err();
        assert_eq!(err, SendError::OfferExpired);
        assert!(payer.calls().is_empty(), "nothing may be paid");
    }

    #[test]
    fn offer_pay_failures_map_to_typed_errors() {
        let cases = [
            (
                Bolt12SemanticError::DuplicatePaymentId,
                SendError::DuplicatePayment,
            ),
            (
                Bolt12SemanticError::UnsupportedChain,
                SendError::OfferWrongNetwork,
            ),
            (Bolt12SemanticError::AlreadyExpired, SendError::OfferExpired),
            (Bolt12SemanticError::MissingAmount, SendError::AmountMissing),
            (
                Bolt12SemanticError::MissingPaths,
                SendError::SendFailed("MissingPaths".to_string()),
            ),
        ];
        for (ldk_error, expected) in cases {
            let offer = amounted_offer(9_000);
            let payer = MockOfferPayer::answering(Err(ldk_error));
            let err = send_bolt12(
                &payer,
                &offer,
                Network::Bitcoin,
                just_after(NOW),
                None,
                None,
                RANDOM_ID,
            )
            .unwrap_err();
            assert_eq!(err, expected);
        }
        assert!(
            !SendError::DuplicatePayment.is_attempt_failure(),
            "a BOLT12 duplicate must not push PaymentFailed either"
        );
    }

    // ---------- event reason rendering: the U6 failure taxonomy ----------

    #[test]
    fn failure_reasons_map_to_the_pwa_describe_payment_failure_strings_verbatim() {
        // event-handler.ts:919-942, case for case.
        let table: [(Option<PaymentFailureReason>, &str); 11] = [
            (
                Some(PaymentFailureReason::RecipientRejected),
                "Payment was rejected by the recipient",
            ),
            (
                Some(PaymentFailureReason::UserAbandoned),
                "Payment was cancelled",
            ),
            (
                Some(PaymentFailureReason::RetriesExhausted),
                "No route found after multiple attempts",
            ),
            (
                Some(PaymentFailureReason::PaymentExpired),
                "Payment expired",
            ),
            (
                Some(PaymentFailureReason::RouteNotFound),
                "No route found to the recipient",
            ),
            (
                Some(PaymentFailureReason::UnexpectedError),
                "An unexpected error occurred",
            ),
            (
                Some(PaymentFailureReason::UnknownRequiredFeatures),
                "Recipient requires unsupported features",
            ),
            (
                Some(PaymentFailureReason::InvoiceRequestExpired),
                "Invoice request timed out — recipient may be offline",
            ),
            (
                Some(PaymentFailureReason::InvoiceRequestRejected),
                "Invoice request was rejected by the recipient",
            ),
            // The default arm covers reasons the switch does not name.
            (
                Some(PaymentFailureReason::BlindedPathCreationFailed),
                "Payment failed",
            ),
            (None, "Payment failed"),
        ];
        for (reason, expected) in table {
            assert_eq!(
                describe_failure_reason(reason),
                expected,
                "PWA describePaymentFailure parity for {reason:?}"
            );
        }
    }

    #[test]
    fn failure_reasons_render_distinct_human_text() {
        let reasons = [
            None,
            Some(PaymentFailureReason::RecipientRejected),
            Some(PaymentFailureReason::UserAbandoned),
            Some(PaymentFailureReason::RetriesExhausted),
            Some(PaymentFailureReason::PaymentExpired),
            Some(PaymentFailureReason::RouteNotFound),
            Some(PaymentFailureReason::UnexpectedError),
            Some(PaymentFailureReason::UnknownRequiredFeatures),
            Some(PaymentFailureReason::InvoiceRequestExpired),
            Some(PaymentFailureReason::InvoiceRequestRejected),
        ];
        let rendered: Vec<String> = reasons.into_iter().map(describe_failure_reason).collect();
        for (i, a) in rendered.iter().enumerate() {
            assert!(!a.is_empty());
            for b in rendered.iter().skip(i + 1) {
                assert_ne!(a, b, "each reason must render distinct text");
            }
        }
    }

    // ---------- node-level: channel-less node over the real ChannelManager ----------

    #[derive(Default)]
    struct CapturingSink(Mutex<Vec<CoreEvent>>);

    impl EventSink for CapturingSink {
        fn emit(&self, event: CoreEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn offline_config(dir: &std::path::Path) -> Config {
        let mut config = Config::new(dir.to_str().unwrap().to_string());
        config.esplora_url = "http://127.0.0.1:1".to_string();
        config.rgs_url = "http://127.0.0.1:1/snapshot".to_string();
        // A closed local port so the U6 BOLT12 LSP pre-connect fails fast
        // and deterministically offline.
        config.lsp.address = "127.0.0.1:1".parse().unwrap();
        config
    }

    #[test]
    fn send_on_a_stopped_node_is_not_running_before_any_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::new(offline_config(dir.path()));
        assert_eq!(
            node.send_payment("garbage, never parsed", None),
            Err(SendError::NotRunning)
        );
        assert_eq!(
            node.pay_offer("garbage, never parsed", None, None),
            Err(SendError::NotRunning)
        );
    }

    /// U6 node-level BOLT12: validation failures return typed errors with no
    /// event; an unreachable LSP fails the pre-connect (PWA parity — the
    /// thrown connectAndTrack) as an ATTEMPT failure that settles the row
    /// and pushes PaymentFailed with a None payment hash (no invoice yet).
    #[test]
    fn offline_pay_offer_fails_at_the_lsp_preconnect_and_pushes_payment_failed() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(CapturingSink::default());
        let node = Node::with_event_sink(offline_config(dir.path()), Arc::clone(&sink) as _);
        node.start().expect("offline degraded start");

        // Validation failures first: typed errors, no events, nothing paid.
        assert!(matches!(
            node.pay_offer("lno1garbage", Some(1_000), None),
            Err(SendError::InvalidOffer(_))
        ));
        let amounted = amounted_offer(9_000);
        assert_eq!(
            node.pay_offer(&amounted, Some(2_000), None),
            Err(SendError::AmountOverrideNotAllowed)
        );
        let amountless = amountless_offer();
        assert_eq!(
            node.pay_offer(&amountless, None, None),
            Err(SendError::AmountMissing)
        );
        assert!(
            sink.0.lock().unwrap().is_empty(),
            "validation failures must not push events"
        );

        // A valid offer reaches the LSP pre-connect, which is unreachable
        // offline: the attempt fails, and exactly one PaymentFailed with no
        // payment hash (BOLT12 pre-invoice) is pushed.
        let err = node
            .pay_offer(&amounted, None, Some("note".to_string()))
            .unwrap_err();
        assert!(
            matches!(&err, SendError::SendFailed(reason) if reason.contains("LSP")),
            "got {err:?}"
        );
        let events = sink.0.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![CoreEvent::PaymentFailed {
                payment_hash: None,
                reason: err.to_string(),
            }]
        );

        node.stop().unwrap();
    }

    #[test]
    fn channelless_send_surfaces_route_not_found_and_pushes_payment_failed() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(CapturingSink::default());
        let node = Node::with_event_sink(offline_config(dir.path()), Arc::clone(&sink) as _);
        node.start().expect("offline degraded start");

        let invoice = test_invoice(Currency::Bitcoin, Some(50_000_000), unix_now_secs(), 3_600);
        let bolt11 = invoice.to_string();

        // No channels, empty graph: the initial route attempt fails and LDK
        // abandons synchronously without queueing any event — the node must
        // push PaymentFailed itself instead of panicking or staying silent.
        assert_eq!(
            node.send_payment(&bolt11, None),
            Err(SendError::RouteNotFound)
        );
        let events = sink.0.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![CoreEvent::PaymentFailed {
                payment_hash: Some("11".repeat(32)),
                reason: SendError::RouteNotFound.to_string(),
            }],
            "exactly one PaymentFailed with the invoice's hash and the route-not-found reason"
        );

        // Idempotency edge, honest offline shape: the first attempt failed
        // BEFORE LDK registered a pending payment (routing precedes
        // registration), so the second send reports RouteNotFound again — not
        // DuplicatePayment, and crucially not a double-pay. The
        // DuplicatePayment surface is proven at the payer seam above.
        assert_eq!(
            node.send_payment(&bolt11, None),
            Err(SendError::RouteNotFound)
        );

        // Validation failures return typed errors WITHOUT pushing events.
        assert!(matches!(
            node.send_payment("not an invoice", None),
            Err(SendError::InvalidInvoice(_))
        ));
        let testnet = test_invoice(
            Currency::BitcoinTestnet,
            Some(50_000_000),
            unix_now_secs(),
            3_600,
        );
        assert_eq!(
            node.send_payment(&testnet.to_string(), None),
            Err(SendError::WrongNetwork {
                expected: Network::Bitcoin,
                found: Network::Testnet,
            })
        );
        let events = sink.0.lock().unwrap().clone();
        assert_eq!(
            events.len(),
            2,
            "only the two attempt failures may push events, got {events:?}"
        );

        node.stop().unwrap();
    }
}
