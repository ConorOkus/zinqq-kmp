//! Outbound BOLT11 payment flow (U5).
//!
//! `send` parses and validates the invoice (mainnet, unexpired, fixed-amount)
//! and pays it through the `ChannelManager` with a stable [`PaymentId`]
//! derived from the payment hash. LDK persists pending outbound payments
//! inside the channel manager, so after a restart a re-send of the same
//! invoice is rejected with `RetryableSendFailure::DuplicatePayment` instead
//! of double-paying — the derivation IS the idempotency key.
//!
//! Outcomes surface through the persisted event queue: LDK's `PaymentSent` /
//! `PaymentFailed` events map to the public `PaymentSuccessful` /
//! `PaymentFailed { reason }` (see `node::handle_ldk_event`). Failures of the
//! initial attempt (e.g. route-not-found) are returned synchronously by LDK
//! WITHOUT an event, so `Node::send_payment` pushes `PaymentFailed` itself
//! for those.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use bitcoin::hashes::Hash as _;
use bitcoin::Network;
use lightning::events::PaymentFailureReason;
use lightning::ln::channelmanager::{Bolt11PaymentError, PaymentId, Retry, RetryableSendFailure};
use lightning::routing::router::RouteParametersConfig;
use lightning_invoice::Bolt11Invoice;

use crate::types::ChannelManager;

/// Retry budget for one send: LDK keeps retrying failed paths for this long
/// (à la ldk-node's `LDK_PAYMENT_RETRY_TIMEOUT`).
pub(crate) const SEND_RETRY_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// The invoice carries no amount; the spike sends fixed-amount only
    /// (there is no amount argument to supply one).
    AmountMissing,
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
            SendError::AmountMissing => write!(
                f,
                "the invoice has no amount; amountless invoices are not supported"
            ),
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
/// offline, e.g. `DuplicatePayment`).
pub(crate) trait Bolt11Payer {
    fn pay(
        &self,
        invoice: &Bolt11Invoice,
        payment_id: PaymentId,
        retry: Retry,
    ) -> Result<(), Bolt11PaymentError>;
}

impl Bolt11Payer for ChannelManager {
    fn pay(
        &self,
        invoice: &Bolt11Invoice,
        payment_id: PaymentId,
        retry: Retry,
    ) -> Result<(), Bolt11PaymentError> {
        // No amount override (fixed-amount invoices only) and default route
        // params; routing runs over the RGS-fed graph + scorer (U2's router).
        self.pay_for_bolt11_invoice(
            invoice,
            payment_id,
            None,
            RouteParametersConfig::default(),
            retry,
        )
    }
}

/// Parses and validates a bolt11 string: well-formed, right network,
/// unexpired at `now_since_epoch`, and carrying a fixed amount. Each
/// rejection is a distinct [`SendError`].
pub(crate) fn parse_and_validate(
    bolt11: &str,
    network: Network,
    now_since_epoch: Duration,
) -> Result<Bolt11Invoice, SendError> {
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
    if invoice.amount_milli_satoshis().is_none() {
        return Err(SendError::AmountMissing);
    }
    Ok(invoice)
}

/// The full send flow: validate → derive the stable `PaymentId` → pay with a
/// bounded retry. Returns the derived `PaymentId` on a successful handoff to
/// LDK (the payment outcome itself arrives later as an LDK event).
pub(crate) fn send_bolt11(
    payer: &dyn Bolt11Payer,
    bolt11: &str,
    network: Network,
    now_since_epoch: Duration,
) -> Result<PaymentId, SendError> {
    let invoice = parse_and_validate(bolt11, network, now_since_epoch)?;
    let payment_id = payment_id_for(&invoice);
    payer
        .pay(&invoice, payment_id, Retry::Timeout(SEND_RETRY_TIMEOUT))
        .map_err(map_pay_error)?;
    Ok(payment_id)
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
/// `PaymentFailed { reason }` event; distinct text per reason.
pub(crate) fn describe_failure_reason(reason: Option<PaymentFailureReason>) -> String {
    match reason {
        Some(PaymentFailureReason::RecipientRejected) => {
            "the recipient rejected the payment".to_string()
        }
        Some(PaymentFailureReason::UserAbandoned) => "the payment was abandoned".to_string(),
        Some(PaymentFailureReason::RetriesExhausted) => {
            "all retry attempts were exhausted".to_string()
        }
        Some(PaymentFailureReason::PaymentExpired) => {
            "the payment expired while retrying".to_string()
        }
        Some(PaymentFailureReason::RouteNotFound) => {
            "no route to the recipient was found while retrying".to_string()
        }
        Some(PaymentFailureReason::UnexpectedError) => {
            "an unexpected routing error occurred".to_string()
        }
        // BOLT12/blinded-path reasons the spike's bolt11-only flow should
        // never see; render their LDK name rather than losing information.
        Some(other) => format!("payment failed: {other:?}"),
        None => "unknown failure reason".to_string(),
    }
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
        calls: Mutex<Vec<(PaymentId, Retry)>>,
        response: Mutex<Option<Result<(), Bolt11PaymentError>>>,
    }

    impl MockPayer {
        fn answering(response: Result<(), Bolt11PaymentError>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                response: Mutex::new(Some(response)),
            }
        }

        fn calls(&self) -> Vec<(PaymentId, Retry)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Bolt11Payer for MockPayer {
        fn pay(
            &self,
            _invoice: &Bolt11Invoice,
            payment_id: PaymentId,
            retry: Retry,
        ) -> Result<(), Bolt11PaymentError> {
            self.calls.lock().unwrap().push((payment_id, retry));
            self.response
                .lock()
                .unwrap()
                .take()
                .expect("MockPayer answered more than once")
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
        )
        .expect("valid invoice with an accepting payer must succeed");

        // R3: PaymentId == payment hash bytes, so a restarted app re-derives
        // the identical id and LDK's duplicate rejection kicks in.
        assert_eq!(payment_id.0, invoice.payment_hash().to_byte_array());

        let calls = payer.calls();
        assert_eq!(calls.len(), 1, "exactly one pay attempt");
        assert_eq!(calls[0].0, payment_id, "the attempt used the derived id");
        assert_eq!(
            calls[0].1,
            Retry::Timeout(SEND_RETRY_TIMEOUT),
            "bounded retry strategy"
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
        let err =
            send_bolt11(&payer, "not an invoice", Network::Bitcoin, just_after(NOW)).unwrap_err();
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

    #[test]
    fn amountless_invoice_is_rejected_before_any_pay_attempt() {
        let invoice = test_invoice(Currency::Bitcoin, None, NOW, 3_600);
        let payer = MockPayer::answering(Ok(()));
        let err = send_bolt11(
            &payer,
            &invoice.to_string(),
            Network::Bitcoin,
            just_after(NOW),
        )
        .unwrap_err();
        assert_eq!(err, SendError::AmountMissing);
        assert!(payer.calls().is_empty(), "nothing may be paid");
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
            )
            .unwrap_err();
            assert_eq!(err, expected);
        }
    }

    // ---------- event reason rendering ----------

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
        config
    }

    #[test]
    fn send_on_a_stopped_node_is_not_running_before_any_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::new(offline_config(dir.path()));
        assert_eq!(
            node.send_payment("garbage, never parsed"),
            Err(SendError::NotRunning)
        );
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
        assert_eq!(node.send_payment(&bolt11), Err(SendError::RouteNotFound));
        let events = sink.0.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![CoreEvent::PaymentFailed {
                reason: SendError::RouteNotFound.to_string(),
            }],
            "exactly one PaymentFailed with the route-not-found reason"
        );

        // Idempotency edge, honest offline shape: the first attempt failed
        // BEFORE LDK registered a pending payment (routing precedes
        // registration), so the second send reports RouteNotFound again — not
        // DuplicatePayment, and crucially not a double-pay. The
        // DuplicatePayment surface is proven at the payer seam above.
        assert_eq!(node.send_payment(&bolt11), Err(SendError::RouteNotFound));

        // Validation failures return typed errors WITHOUT pushing events.
        assert!(matches!(
            node.send_payment("not an invoice"),
            Err(SendError::InvalidInvoice(_))
        ));
        let testnet = test_invoice(
            Currency::BitcoinTestnet,
            Some(50_000_000),
            unix_now_secs(),
            3_600,
        );
        assert_eq!(
            node.send_payment(&testnet.to_string()),
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
