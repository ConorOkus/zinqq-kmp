//! Receive engine (U7, R6, F2): the unified BIP321 URI, the capacity
//! decision, the JIT floor/clamp math, standard-invoice parameters, and the
//! BOLT12 offer lifecycle helpers.
//!
//! Everything here mirrors the PWA's receive path code-for-code:
//! - [`build_bip321_uri`] mirrors `src/onchain/bip321.ts::buildBip321Uri`
//!   (address uppercased, amount as fixed 8-decimal BTC only when > 0, param
//!   order `amount` → `lightning` → `lno`; the *whole-URI* uppercase form is
//!   a QR-display concern — `Receive.tsx:640` — surfaced as `qr_value` on
//!   [`ReceiveBundle`]).
//! - [`needs_jit`] mirrors `Receive.tsx:200-207`.
//! - [`compute_min_receive_sats`] mirrors
//!   `src/ldk/lsps2/types.ts::computeMinReceiveSats`, with
//!   [`MIN_JIT_RECEIVE_SATS`] as the static fallback (AE4's numpad floor).
//! - [`compute_jit_invoice_expiry_secs`] mirrors
//!   `src/ldk/context.tsx::computeJitInvoiceExpirySecs` (R6: expiry clamped
//!   to quote validity minus 30 s, minimum 60 s else re-quote), applied
//!   BEFORE any `lsps2.buy` (KTD-10 cluster's JIT flow).
//! - The offer helpers mirror `context.tsx::loadOrCreateOffer` (3/6/12/24/48 s
//!   backoff while the graph cannot yet produce blinded paths) and
//!   `src/ldk/storage/offer.ts` (one stable persistence key).

use std::time::Duration;

use lightning::ln::channelmanager::Bolt11InvoiceParameters;
use lightning::util::persist::KVStoreSync as _;
use lightning_invoice::{Bolt11InvoiceDescription, Description};
use lightning_liquidity::lsps2::msgs::LSPS2OpeningFeeParams;
use lightning_persister::fs_store::FilesystemStore;

use crate::channels::ChannelView;
use crate::config::RECEIVE_INVOICE_DESCRIPTION;
use crate::liquidity::{datetime_unix_secs, Lsps2Error};

/// Static fallback floor (sats) for a JIT receive, used whenever the live
/// menu fetch failed, returned an empty/degenerate menu, or has not resolved
/// (PWA `MIN_JIT_RECEIVE_SATS`, `src/ldk/lsps2/types.ts:131`).
pub const MIN_JIT_RECEIVE_SATS: u64 = 3_000;

/// Upper bound on any invoice expiry — the pre-clamp default and the standard
/// invoice's fixed expiry (PWA `JIT_INVOICE_MAX_EXPIRY_SECS` and the
/// `createInvoice` 3600 s).
pub(crate) const INVOICE_MAX_EXPIRY_SECS: u32 = 3_600;

/// Subtracted from the quote's `valid_until` headroom: the HTLC must ARRIVE
/// at the LSP before `valid_until`, so leave room for scan + payer wallet
/// pathfinding + HTLC flight (PWA `JIT_INVOICE_FLIGHT_MARGIN_SECS`).
pub(crate) const JIT_INVOICE_FLIGHT_MARGIN_SECS: u64 = 30;

/// Minimum useful invoice life after the flight margin; less than this means
/// the QR would die in the user's hand — re-quote instead (PWA
/// `JIT_INVOICE_MIN_EXPIRY_SECS`).
pub(crate) const JIT_INVOICE_MIN_EXPIRY_SECS: u64 = 60;

/// A quote is "fresh enough" to display when its `valid_until` is at least
/// this far away (PWA `getJitQuote`'s ≥ 30 s sanity gate).
pub(crate) const QUOTE_FRESHNESS_MARGIN_SECS: u64 = 30;

/// The PWA's offer-creation retry schedule (`context.tsx`: `3000 * 2 **
/// attempt` for `MAX_OFFER_RETRIES = 5`): 3 s, 6 s, 12 s, 24 s, 48 s.
pub(crate) const OFFER_RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(3),
    Duration::from_secs(6),
    Duration::from_secs(12),
    Duration::from_secs(24),
    Duration::from_secs(48),
];

/// Stable KVStore location of the persisted BOLT12 offer (local-only, like
/// the PWA's `ldk_bolt12_offer`/`default` IndexedDB slot).
pub(crate) const OFFER_PERSISTENCE_PRIMARY_NAMESPACE: &str = "";
pub(crate) const OFFER_PERSISTENCE_SECONDARY_NAMESPACE: &str = "";
pub(crate) const OFFER_PERSISTENCE_KEY: &str = "bolt12_offer";

/// Tells LDK which static invoice server to build async receive offers with
/// (U3), returning how many paths were accepted.
///
/// Safe to call on every start: LDK overwrites the stored path list while
/// preserving its offer slots, and errors only on an empty input (which the
/// caller excludes). The offer handshake itself is driven by the background
/// processor's timer ticks, so a call made before any peer is connected still
/// converges — there is nothing here to sequence or retry.
pub(crate) fn apply_static_invoice_server_paths(
    channel_manager: &crate::types::ChannelManager,
    paths: &[lightning::blinded_path::message::BlindedMessagePath],
) -> Result<usize, ()> {
    if paths.is_empty() {
        return Ok(0);
    }
    channel_manager
        .set_paths_to_static_invoice_server(paths.to_vec())
        .map(|()| paths.len())
}

/// How far along the async payments receive setup is (U4) — the protocol that
/// lets a payer pay this wallet while it is offline, via a static invoice
/// server that serves BOLT12 static invoices on our behalf.
///
/// Three states rather than a nullable offer, because "you never configured
/// this" and "configured, still handshaking with the server" want different
/// treatment and a `None` offer cannot tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum AsyncReceiveStatus {
    /// No static invoice server configured — the shipped default. Async
    /// receive does nothing and the receive screen is unchanged.
    Disabled,
    /// Paths are configured, but LDK has not yet completed the offer/invoice
    /// handshake with the server, so there is no offer to show. LDK retries on
    /// its own background timer; nothing here needs to poll.
    AwaitingServer,
    /// An async receive offer exists and can be paid while this wallet is
    /// offline.
    Ready,
}

/// A two-phase JIT quote (U7, F2 "fee review" step): everything the review
/// screen renders, plus the single-use token `jit_accept` consumes. No
/// LSP-side commitment exists yet — refusing/abandoning a quote costs
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct JitQuote {
    /// Single-use handle for `jit_accept`.
    pub quote_token: u64,
    /// The quoted payment size (echoed back to `jit_accept`).
    pub amount_msat: u64,
    /// The LSP's channel-opening fee, skimmed off the forwarded HTLC.
    pub opening_fee_msat: u64,
    /// `amount_msat - opening_fee_msat` — the review's "You'll receive" row.
    pub receive_msat: u64,
    /// The quote's `valid_until` as UNIX seconds.
    pub valid_until_unix: u64,
    /// R6 freshness: `valid_until` is at least 30 s away. A stale quote can
    /// still be shown, but `jit_accept` will demand a re-quote.
    pub fresh_enough: bool,
}

/// The accepted (bought) JIT invoice (U7): what the QR screen renders.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct JitInvoice {
    pub bolt11: String,
    /// Lowercase hex payment hash — the payment store settles it (U5).
    pub payment_hash: String,
    /// The fee agreed at quote time (the "Setup fee" label under the QR).
    pub opening_fee_msat: u64,
    /// UNIX seconds when the displayed invoice stops being payable: the R6
    /// clamp (`valid_until` − 30 s, capped at 3600 s).
    pub expires_at_unix: u64,
}

/// The one receive call the shells render (U7, R6): on-chain address, the
/// standard BOLT11 when capacity covers the request, the unified BIP321 URI
/// (copy and QR forms), the persisted offer when eligible, the JIT floor and
/// the capacity decision.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ReceiveBundle {
    /// Next unused on-chain receive address.
    pub address: String,
    /// The standard invoice; `None` when the request needs JIT (drive
    /// `jit_quote`/`jit_accept` instead) or invoice creation failed.
    pub bolt11: Option<String>,
    /// The standard invoice's payment hash (paid detection via the store).
    pub payment_hash: Option<String>,
    /// The PWA's copy when an amounted standard invoice failed
    /// (`Receive.tsx:290`); the on-chain QR still renders.
    pub invoice_error: Option<String>,
    /// The copy/share BIP321 URI (address uppercased, rest untouched).
    pub bip321_uri: String,
    /// The whole URI uppercased for QR alphanumeric mode (`Receive.tsx:640`).
    pub qr_value: String,
    /// The persisted BOLT12 offer, only when ≥ 1 usable channel exists
    /// (`Receive.tsx:372` `showBolt12`).
    pub offer: Option<String>,
    /// The offer pager page's QR value: `bitcoin:?lno=…` uppercased
    /// (`Receive.tsx:385,973`).
    pub offer_qr_value: Option<String>,
    /// R6/F2 capacity decision: the request exceeds usable inbound capacity
    /// (or no usable channel exists at all).
    pub needs_jit: bool,
    /// The session's JIT numpad floor in sats: the cached live value when a
    /// `min_receive_sats` fetch settled this session, else the static 3,000.
    pub min_receive_sats: u64,
}

/// Typed receive-engine failures (U7). Distinct Display per variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiveError {
    /// The node is not running.
    NotRunning,
    /// The on-chain wallet could not reveal (and persist) a receive address.
    AddressUnavailable { detail: String },
    /// The standard BOLT11 invoice could not be created or signed.
    InvoiceCreationFailed,
}

impl std::fmt::Display for ReceiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReceiveError::NotRunning => write!(f, "the node is not running"),
            ReceiveError::AddressUnavailable { detail } => {
                write!(f, "failed to derive a receive address: {detail}")
            }
            ReceiveError::InvoiceCreationFailed => {
                // The PWA's Receive.tsx copy, verbatim (`Receive.tsx:290`).
                write!(f, "Failed to create Lightning invoice")
            }
        }
    }
}

impl std::error::Error for ReceiveError {}

/// `sats → BTC` with FIXED eight decimals, mirroring the PWA's
/// `satsToBtcString` exactly (`bip321.ts:18-23` — no trailing-zero trim).
pub(crate) fn sats_to_btc_string(sats: u64) -> String {
    let whole = sats / 100_000_000;
    let frac = sats % 100_000_000;
    format!("{whole}.{frac:08}")
}

/// `buildBip321Uri` (`bip321.ts:33-46`), all four parts: the address is
/// uppercased (bech32 QR efficiency), the amount renders only when > 0, and
/// param order is `amount` → `lightning` → `lno`. Empty strings are treated
/// as absent (JS truthiness parity).
pub(crate) fn build_bip321_uri_parts(
    address: Option<&str>,
    amount_sats: Option<u64>,
    invoice: Option<&str>,
    lno: Option<&str>,
) -> String {
    let base = match address {
        Some(address) if !address.is_empty() => format!("bitcoin:{}", address.to_uppercase()),
        _ => "bitcoin:".to_string(),
    };
    let mut params = Vec::new();
    if let Some(amount) = amount_sats.filter(|amount| *amount > 0) {
        params.push(format!("amount={}", sats_to_btc_string(amount)));
    }
    if let Some(invoice) = invoice.filter(|invoice| !invoice.is_empty()) {
        params.push(format!("lightning={invoice}"));
    }
    if let Some(lno) = lno.filter(|lno| !lno.is_empty()) {
        params.push(format!("lno={lno}"));
    }
    if params.is_empty() {
        base
    } else {
        format!("{base}?{}", params.join("&"))
    }
}

/// The unified receive URI (R6): address + optional amount + optional BOLT11.
/// This is the COPY form (address-only uppercase); QR mode uppercases the
/// whole string (`Receive.tsx:640`).
pub fn build_bip321_uri(address: &str, amount_sats: Option<u64>, invoice: Option<&str>) -> String {
    build_bip321_uri_parts(Some(address), amount_sats, invoice, None)
}

/// The BOLT12 pager page's URI: `bitcoin:?lno=…` (`Receive.tsx:385`).
pub(crate) fn build_bolt12_page_uri(offer: &str) -> String {
    build_bip321_uri_parts(None, None, None, Some(offer))
}

/// Sum of inbound capacity (msat) across USABLE channels
/// (`Receive.tsx:33-39` `usableInboundMsat`).
pub(crate) fn usable_inbound_msat(channels: &[ChannelView]) -> u64 {
    channels
        .iter()
        .filter(|channel| channel.usable)
        .map(|channel| channel.inbound_msat)
        .sum()
}

/// Whether any usable channel exists (`Receive.tsx:205` `hasUsable`).
pub(crate) fn has_usable_channel(channels: &[ChannelView]) -> bool {
    channels.iter().any(|channel| channel.usable)
}

/// The PWA's capacity decision (`Receive.tsx:207`): with an amount, JIT is
/// needed when usable inbound capacity cannot cover it; without one, when no
/// usable channel exists at all.
pub(crate) fn needs_jit(channels: &[ChannelView], amount_msat: Option<u64>) -> bool {
    match amount_msat {
        Some(amount_msat) => usable_inbound_msat(channels) < amount_msat,
        None => !has_usable_channel(channels),
    }
}

/// The smallest amount, in sats (rounded up), the menu accepts AND that
/// yields net > 0 after the opening fee: per entry
/// `max(min_payment_size_msat, min_fee_msat + 1)`, minimum across the menu
/// (`computeMinReceiveSats`, `src/ldk/lsps2/types.ts:144-159`). Entries whose
/// `valid_until` has passed are skipped (the plan's "expired menu → static
/// floor"); `0` means no usable entry — callers fall back to
/// [`MIN_JIT_RECEIVE_SATS`].
pub(crate) fn compute_min_receive_sats(menu: &[LSPS2OpeningFeeParams], now_unix_secs: u64) -> u64 {
    let min_msat = menu
        .iter()
        .filter(|entry| datetime_unix_secs(&entry.valid_until) > now_unix_secs)
        .map(|entry| entry.min_payment_size_msat.max(entry.min_fee_msat + 1))
        .min();
    match min_msat {
        Some(min_msat) => min_msat.div_ceil(1_000),
        None => 0,
    }
}

/// Whether the quote's `valid_until` leaves at least the 30 s freshness
/// margin (`getJitQuote`'s sanity gate, `context.tsx:361`).
pub(crate) fn quote_fresh_enough(valid_until_unix: u64, now_unix_secs: u64) -> bool {
    valid_until_unix >= now_unix_secs + QUOTE_FRESHNESS_MARGIN_SECS
}

/// Clamps a JIT invoice's expiry to the quote's `valid_until` (R6, the
/// clamp learning: the LSP fails HTLCs arriving after `valid_until`, so an
/// invoice that outlives its quote looks payable but never is). Mirrors
/// `computeJitInvoiceExpirySecs` (`context.tsx:426-439`): headroom =
/// `valid_until − now − 30 s`; less than 60 s left →
/// [`Lsps2Error::QuoteExpired`] (the re-quote signal), else
/// `min(3600, headroom)`. Must run BEFORE any `buy`.
pub(crate) fn compute_jit_invoice_expiry_secs(
    valid_until_unix: u64,
    now_unix_secs: u64,
) -> Result<u32, Lsps2Error> {
    let headroom_secs = valid_until_unix
        .saturating_sub(now_unix_secs)
        .saturating_sub(JIT_INVOICE_FLIGHT_MARGIN_SECS);
    if headroom_secs < JIT_INVOICE_MIN_EXPIRY_SECS {
        return Err(Lsps2Error::QuoteExpired);
    }
    Ok(headroom_secs.min(INVOICE_MAX_EXPIRY_SECS as u64) as u32)
}

/// The standard (non-JIT) invoice parameters, mirroring the PWA's
/// `createInvoice` (`context.tsx:890-930`): description `Zinqq Wallet`,
/// 3600 s expiry, amountless allowed, LDK-default final CLTV.
pub(crate) fn standard_invoice_params(amount_msat: Option<u64>) -> Bolt11InvoiceParameters {
    Bolt11InvoiceParameters {
        amount_msats: amount_msat,
        description: Bolt11InvoiceDescription::Direct(
            Description::new(RECEIVE_INVOICE_DESCRIPTION.to_string())
                .expect("the constant receive description is valid"),
        ),
        invoice_expiry_delta_secs: Some(INVOICE_MAX_EXPIRY_SECS),
        min_final_cltv_expiry_delta: None,
        payment_hash: None,
    }
}

/// The persisted BOLT12 offer, if any (stable key, local-only).
pub(crate) fn read_persisted_offer(store: &FilesystemStore) -> Option<String> {
    match store.read(
        OFFER_PERSISTENCE_PRIMARY_NAMESPACE,
        OFFER_PERSISTENCE_SECONDARY_NAMESPACE,
        OFFER_PERSISTENCE_KEY,
    ) {
        Ok(bytes) => String::from_utf8(bytes).ok().filter(|s| !s.is_empty()),
        Err(_) => None,
    }
}

/// Persists the offer under the stable key so subsequent calls (and
/// restarts) reuse it instead of minting a new one.
pub(crate) fn persist_offer(
    store: &FilesystemStore,
    offer: &str,
) -> Result<(), lightning::io::Error> {
    store.write(
        OFFER_PERSISTENCE_PRIMARY_NAMESPACE,
        OFFER_PERSISTENCE_SECONDARY_NAMESPACE,
        OFFER_PERSISTENCE_KEY,
        offer.as_bytes().to_vec(),
    )
}

/// Drives `try_create` through the PWA's retry schedule (`context.tsx`:
/// `create_offer_builder` needs the message router to find blinded paths,
/// which may not exist until RGS sync completes): one attempt, then one more
/// after each delay. `None` when every attempt failed — offer creation
/// failure NEVER blocks receive.
pub(crate) async fn create_offer_with_retry<F>(
    mut try_create: F,
    delays: &[Duration],
) -> Option<String>
where
    F: FnMut() -> Result<String, String>,
{
    let mut attempt = 0;
    loop {
        match try_create() {
            Ok(offer) => return Some(offer),
            Err(_reason) => {
                if attempt >= delays.len() {
                    return None;
                }
                tokio::time::sleep(delays[attempt]).await;
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use lightning_liquidity::lsps0::ser::LSPSDateTime;

    use crate::channels::{ChannelStateLabel, ChannelView};

    const NOW: u64 = 1_753_000_000;
    const FUTURE: u64 = NOW + 3_600;

    fn params(
        min_fee_msat: u64,
        min_payment_size_msat: u64,
        valid_until_unix: u64,
    ) -> LSPS2OpeningFeeParams {
        LSPS2OpeningFeeParams {
            min_fee_msat,
            proportional: 0,
            valid_until: LSPSDateTime::new_from_duration_since_epoch(Duration::from_secs(
                valid_until_unix,
            )),
            min_lifetime: 4032,
            max_client_to_self_delay: 2016,
            min_payment_size_msat,
            max_payment_size_msat: u64::MAX,
            promise: "promise".to_string(),
        }
    }

    fn channel(usable: bool, inbound_msat: u64) -> ChannelView {
        ChannelView {
            channel_id: "00".repeat(32),
            counterparty_pubkey: "02".repeat(33),
            state: if usable {
                ChannelStateLabel::Active
            } else {
                ChannelStateLabel::Pending
            },
            capacity_sats: 100_000,
            outbound_msat: 0,
            inbound_msat,
            reserve_sats: None,
            usable,
            pending_htlc_count: 0,
        }
    }

    // ---------- BIP321 URI vectors (bip321.ts parity) ----------

    #[test]
    fn bip321_uri_uppercases_only_the_address_in_the_copy_form() {
        let uri = build_bip321_uri("bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4", None, None);
        assert_eq!(uri, "bitcoin:BC1QW508D6QEJXTDG4Y5R3ZARVARY0C5XW7KV8F3T4");
    }

    #[test]
    fn bip321_amount_is_fixed_eight_decimal_btc_only_when_positive() {
        // Zero amount → no amount param at all (bip321.ts:36 `> 0n`).
        assert_eq!(
            build_bip321_uri("bc1qtest", Some(0), None),
            "bitcoin:BC1QTEST"
        );
        // satsToBtcString pads to EIGHT decimals and never trims zeros.
        assert_eq!(
            build_bip321_uri("bc1qtest", Some(3_000), None),
            "bitcoin:BC1QTEST?amount=0.00003000"
        );
        assert_eq!(
            build_bip321_uri("bc1qtest", Some(150_000_000), None),
            "bitcoin:BC1QTEST?amount=1.50000000"
        );
        assert_eq!(
            build_bip321_uri("bc1qtest", Some(123_456_789), None),
            "bitcoin:BC1QTEST?amount=1.23456789"
        );
        assert_eq!(
            build_bip321_uri("bc1qtest", Some(1), None),
            "bitcoin:BC1QTEST?amount=0.00000001"
        );
    }

    #[test]
    fn bip321_lightning_param_present_exactly_when_an_invoice_exists() {
        // Param order is amount → lightning (bip321.ts:35-44), invoice case
        // preserved in the copy form.
        assert_eq!(
            build_bip321_uri("bc1qtest", Some(3_000), Some("lnbc30u1invoice")),
            "bitcoin:BC1QTEST?amount=0.00003000&lightning=lnbc30u1invoice"
        );
        assert_eq!(
            build_bip321_uri("bc1qtest", None, Some("lnbc30u1invoice")),
            "bitcoin:BC1QTEST?lightning=lnbc30u1invoice"
        );
        // JS truthiness: an empty invoice string is absent.
        assert_eq!(
            build_bip321_uri("bc1qtest", None, Some("")),
            "bitcoin:BC1QTEST"
        );
    }

    #[test]
    fn bolt12_pager_uri_is_a_bare_lno_param_and_qr_mode_uppercases_the_whole_uri() {
        let uri = build_bolt12_page_uri("lno1qsgqmqvgm96frzdg8m0gc6nzeqffvzsqzrxqy32afmr3jn9ggl9g");
        assert_eq!(
            uri,
            "bitcoin:?lno=lno1qsgqmqvgm96frzdg8m0gc6nzeqffvzsqzrxqy32afmr3jn9ggl9g"
        );
        // QR alphanumeric mode: the WHOLE uri uppercased (Receive.tsx:640/973).
        assert_eq!(
            uri.to_uppercase(),
            "BITCOIN:?LNO=LNO1QSGQMQVGM96FRZDG8M0GC6NZEQFFVZSQZRXQY32AFMR3JN9GGL9G"
        );
        let unified = build_bip321_uri("bc1qtest", Some(3_000), Some("lnbc30u1invoice"));
        assert_eq!(
            unified.to_uppercase(),
            "BITCOIN:BC1QTEST?AMOUNT=0.00003000&LIGHTNING=LNBC30U1INVOICE"
        );
    }

    // ---------- needs_jit decision table (Receive.tsx:200-207) ----------

    #[test]
    fn needs_jit_decision_table_matches_the_pwa() {
        let none: Vec<ChannelView> = Vec::new();
        let unusable = vec![channel(false, 10_000_000)];
        let usable_5k = vec![channel(true, 5_000_000)];
        let usable_zero_inbound = vec![channel(true, 0)];
        let mixed = vec![channel(false, 100_000_000), channel(true, 5_000_000)];

        // Amountless: JIT needed exactly when no usable channel exists.
        assert!(needs_jit(&none, None));
        assert!(needs_jit(&unusable, None));
        assert!(!needs_jit(&usable_5k, None));
        assert!(
            !needs_jit(&usable_zero_inbound, None),
            "hasUsable via usable flag"
        );

        // Amounted: JIT needed when usable inbound < amount; unusable
        // channels' inbound never counts.
        assert!(needs_jit(&none, Some(1_000)));
        assert!(needs_jit(&unusable, Some(1_000)));
        assert!(!needs_jit(&usable_5k, Some(4_000_000)));
        assert!(
            !needs_jit(&usable_5k, Some(5_000_000)),
            "equal capacity covers"
        );
        assert!(needs_jit(&usable_5k, Some(5_000_001)));
        assert!(
            needs_jit(&mixed, Some(6_000_000)),
            "unusable inbound excluded"
        );
    }

    // ---------- JIT floor math (computeMinReceiveSats parity) ----------

    #[test]
    fn floor_is_the_menu_minimum_of_max_min_payment_and_min_fee_plus_one_ceiled() {
        let menu = vec![
            // Binding constraint: min_fee + 1 = 2_500_001 msat.
            params(2_500_000, 1_000, FUTURE),
            // Binding constraint: min_payment_size = 5_000_000 msat.
            params(1_000, 5_000_000, FUTURE),
        ];
        // min(2_500_001, 5_000_000) = 2_500_001 msat → ceil → 2_501 sats.
        assert_eq!(compute_min_receive_sats(&menu, NOW), 2_501);
    }

    #[test]
    fn floor_ceils_to_whole_sats() {
        // max(1_500_500, 1_000_001) = 1_500_500 msat → 1_501 sats.
        let menu = vec![params(1_000_000, 1_500_500, FUTURE)];
        assert_eq!(compute_min_receive_sats(&menu, NOW), 1_501);
        // Exact-thousand floor does not over-ceil: 2_000_000 → 2_000 sats.
        let menu = vec![params(999, 2_000_000, FUTURE)];
        assert_eq!(compute_min_receive_sats(&menu, NOW), 2_000);
    }

    #[test]
    fn floor_is_zero_on_empty_or_fully_expired_menus() {
        assert_eq!(compute_min_receive_sats(&[], NOW), 0, "empty menu");
        let expired = vec![params(2_500_000, 1_000, NOW - 1), params(1, 1, NOW)];
        assert_eq!(compute_min_receive_sats(&expired, NOW), 0, "all expired");
        // A live entry among expired ones still yields a floor.
        let mixed = vec![params(1, 1, NOW - 1), params(2_500_000, 1_000, FUTURE)];
        assert_eq!(compute_min_receive_sats(&mixed, NOW), 2_501);
    }

    // ---------- expiry clamp (computeJitInvoiceExpirySecs parity) ----------

    #[test]
    fn clamp_bounds_expiry_to_valid_until_minus_the_flight_margin() {
        // 1800 s of quote validity → 1770 s of invoice life.
        assert_eq!(
            compute_jit_invoice_expiry_secs(NOW + 1_800, NOW).unwrap(),
            1_770
        );
    }

    #[test]
    fn clamp_caps_the_expiry_at_one_hour() {
        assert_eq!(
            compute_jit_invoice_expiry_secs(NOW + 100_000, NOW).unwrap(),
            3_600
        );
    }

    #[test]
    fn clamp_requires_sixty_seconds_of_payable_life_else_requote() {
        // Exactly 60 s of headroom is the minimum that passes.
        assert_eq!(compute_jit_invoice_expiry_secs(NOW + 90, NOW).unwrap(), 60);
        // 59 s → the typed re-quote signal.
        assert_eq!(
            compute_jit_invoice_expiry_secs(NOW + 89, NOW).unwrap_err(),
            Lsps2Error::QuoteExpired
        );
        // A quote already in the past fails closed, never panics.
        assert_eq!(
            compute_jit_invoice_expiry_secs(NOW - 1, NOW).unwrap_err(),
            Lsps2Error::QuoteExpired
        );
    }

    #[test]
    fn quote_freshness_needs_thirty_seconds_of_validity() {
        assert!(quote_fresh_enough(NOW + 30, NOW));
        assert!(!quote_fresh_enough(NOW + 29, NOW));
    }

    // ---------- standard invoice parameters (createInvoice parity) ----------

    #[test]
    fn standard_invoice_params_carry_the_pwa_description_and_expiry_and_allow_amountless() {
        let params = standard_invoice_params(None);
        assert_eq!(params.amount_msats, None, "amountless allowed");
        assert_eq!(params.invoice_expiry_delta_secs, Some(3_600));
        assert_eq!(params.min_final_cltv_expiry_delta, None);
        assert!(params.payment_hash.is_none());
        match &params.description {
            Bolt11InvoiceDescription::Direct(description) => {
                assert_eq!(description.to_string(), "Zinqq Wallet")
            }
            other => panic!("expected a direct description, got {other:?}"),
        }
        assert_eq!(
            standard_invoice_params(Some(250_000)).amount_msats,
            Some(250_000)
        );
    }

    // ---------- offer lifecycle helpers ----------

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn offer_creation_retries_through_the_backoff_schedule_until_graph_ready() {
        // Injectable error sequence: MissingPaths until the graph is "ready"
        // on the 4th attempt.
        let mut attempts = 0;
        let offer = block_on(create_offer_with_retry(
            || {
                attempts += 1;
                if attempts < 4 {
                    Err("MissingPaths".to_string())
                } else {
                    Ok("lno1offer".to_string())
                }
            },
            &[Duration::ZERO; 5],
        ));
        assert_eq!(offer.as_deref(), Some("lno1offer"));
        assert_eq!(attempts, 4);
    }

    #[test]
    fn offer_creation_gives_up_after_the_schedule_is_exhausted() {
        let mut attempts = 0;
        let offer = block_on(create_offer_with_retry(
            || {
                attempts += 1;
                Err("MissingPaths".to_string())
            },
            &[Duration::ZERO; 5],
        ));
        assert_eq!(offer, None, "failure never blocks receive");
        assert_eq!(attempts, 6, "one initial attempt + five retries");
    }

    #[test]
    fn persisted_offer_is_stable_across_a_store_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let store = FilesystemStore::new(dir.path().to_path_buf());
        assert_eq!(read_persisted_offer(&store), None, "fresh store has none");
        persist_offer(&store, "lno1persisted").unwrap();
        assert_eq!(
            read_persisted_offer(&store).as_deref(),
            Some("lno1persisted")
        );

        // A fresh handle over the same directory (restart) reads the same
        // offer back — the stable-key contract.
        let reopened = FilesystemStore::new(dir.path().to_path_buf());
        assert_eq!(
            read_persisted_offer(&reopened).as_deref(),
            Some("lno1persisted")
        );
    }
}
