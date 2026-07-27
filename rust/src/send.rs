//! U6 — unified send engine (R5, KTD-6): one `classify` entry point covering
//! all six input families, plus async `resolve` for the two families that
//! need the network (BIP353 names and LNURL-pay).
//!
//! The classification DISPATCH ORDER, regexes, and error strings are the
//! PWA's (`payment-input.ts`), preserved verbatim per KTD-6: `bitcoin:` URIs
//! (BIP321, preference `lno` > `lightning` > address — AE5), `lightning:`
//! strip-and-recurse, BOLT11 (`^ln(bc|tb|tbs|bcrt)/i`), BOLT12 (`lno1`),
//! BIP353 names (`/^[₿]?[a-z0-9._-]+@[a-z0-9.-]+\.[a-z]{2,}$/i`), bech32 /
//! legacy on-chain addresses, and "Unrecognized payment format" otherwise.
//! Network and expiry checks happen at classify time; inputs are capped at
//! 2,000 chars (the PWA's scan/input cap).
//!
//! Resolution follows the PWA's `Send.tsx` flow: BIP353 over DNSSEC-verified
//! DoH first (5 s budget; `bitcoin-payment-instructions`' `HTTPHrnResolver`
//! builds and verifies a full DNSSEC proof — strictly stronger than the
//! PWA's `AD`-flag check), then LNURL-pay (LUD-16) as fallback on a miss
//! with a FRESH 5 s budget, and the PWA's "No Lightning Address or BIP 353
//! record found for {raw}" when both miss. LNURL validation is hand-rolled
//! to the PWA's `resolve-lnurl.ts` semantics (HTTPS callback, callback
//! domain binding, min/max, first `text/plain` metadata entry) because the
//! crate's LUD-16 flow performs none of the callback-binding checks; the
//! fetched invoice additionally gets the KTD-6 `description_hash` and
//! amount-match enforcement the plan requires (the crate agrees; the PWA is
//! laxer — deviations documented on `validate_lnurl_invoice`).

use std::fmt;
use std::future::Future;
use std::str::FromStr;
use std::time::Duration;

use bitcoin::constants::ChainHash;
use bitcoin::hashes::{sha256, Hash as _};
use bitcoin::Network;
use bitcoin_payment_instructions::hrn_resolution::{HrnResolution, HrnResolver as _};
use bitcoin_payment_instructions::http_resolver::HTTPHrnResolver;
use lightning::offers::offer::{Amount as OfferAmount, Offer};
use lightning::onion_message::dns_resolution::HumanReadableName;
use lightning_invoice::{Bolt11Invoice, Bolt11InvoiceDescriptionRef};

use crate::util::{hex_str, unix_now};

/// The PWA's input cap (`Send.tsx:613` / `maxLength={2000}`).
pub const MAX_INPUT_CHARS: usize = 2000;

/// The PWA's per-step resolution budget (`Send.tsx RESOLVE_TIMEOUT_MS`).
pub(crate) const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

// ---- PWA classification error strings, verbatim (payment-input.ts) ----
const ERR_UNRECOGNIZED: &str = "Unrecognized payment format";
const ERR_INVALID_INVOICE: &str = "Invalid Lightning invoice";
const ERR_INVOICE_NETWORK: &str = "Invoice is for a different Bitcoin network";
const ERR_INVOICE_EXPIRED: &str = "Invoice has expired";
const ERR_INVALID_OFFER: &str = "Invalid BOLT 12 offer";
const ERR_OFFER_NETWORK: &str = "Offer is for a different Bitcoin network";
const ERR_OFFER_EXPIRED: &str = "Offer has expired";
const ERR_INVALID_ADDRESS_FORMAT: &str = "Invalid address format";
const ERR_EMPTY_URI: &str = "Empty Bitcoin URI";
const ERR_MALFORMED_URI: &str = "Malformed Bitcoin URI";
const ERR_URI_NO_METHOD: &str = "Bitcoin URI has no payment method";
const ERR_URI_ADDRESS_NETWORK: &str = "Address is for a different Bitcoin network";
/// The PWA's over-length message (`Send.tsx:614`).
const ERR_INPUT_TOO_LONG: &str = "Scanned input is too long";

/// LUD-16 metadata for a resolved Lightning Address (the PWA's
/// `LnurlPayMetadata` plus the KTD-6 `description_hash` commitment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LnurlPayMetadata {
    pub domain: String,
    pub user: String,
    pub callback: String,
    pub min_sendable_msat: u64,
    pub max_sendable_msat: u64,
    pub description: String,
    /// sha256 of the raw LUD-06 `metadata` string; the fetched invoice's
    /// `description_hash` must match (KTD-6). `None` when the server sent no
    /// metadata field (nothing to verify against).
    pub expected_description_hash: Option<[u8; 32]>,
}

impl LnurlPayMetadata {
    /// Min sendable in sats, rounded UP (`Send.tsx:320` — never send less
    /// than the server requires).
    pub fn min_sats(&self) -> u64 {
        self.min_sendable_msat.div_ceil(1000)
    }

    /// Max sendable in sats, rounded DOWN (`Send.tsx:321` — never exceed the
    /// server's limit).
    pub fn max_sats(&self) -> u64 {
        self.max_sendable_msat / 1000
    }

    /// Fixed-amount LNURL: the shells skip amount entry (`Send.tsx:324`).
    pub fn skip_amount_entry(&self) -> bool {
        self.min_sats() == self.max_sats()
    }
}

/// One classified send input — the PWA's `ParsedPaymentInput` union.
/// `Lnurl` is only ever produced by [`resolve`], exactly like the PWA
/// (LNURL resolution happens asynchronously after classification).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classified {
    /// A validated (mainnet, unexpired) BOLT11 invoice.
    Bolt11 {
        raw: String,
        amount_msat: Option<u64>,
        description: Option<String>,
        /// The invoice's payment hash as lowercase hex, taken from the parse
        /// the classifier already did. The shells need it to match a
        /// `PaymentSuccessful`/`PaymentFailed` event to their own dispatch.
        payment_hash: String,
    },
    /// A validated (mainnet, unexpired) BOLT12 offer. `amount_msat` is
    /// `None` for amountless and fiat-denominated offers (PWA parity).
    Bolt12 {
        raw: String,
        amount_msat: Option<u64>,
        description: Option<String>,
    },
    /// A BIP321 `bitcoin:` URI: the preferred method per AE5 (`lno` >
    /// `lightning` > address) plus the preserved ordered fallbacks.
    Bip321 {
        preferred: Box<Classified>,
        /// The URI's on-chain address when present AND mainnet-valid.
        onchain_fallback: Option<String>,
        /// The URI's `amount` (BTC fixed-point), in sats.
        amount_sats: Option<u64>,
    },
    /// An unresolved BIP353 human-readable name (resolve with [`resolve`]).
    Bip353 {
        user: String,
        domain: String,
        raw: String,
    },
    /// A resolved LNURL-pay Lightning Address (produced by [`resolve`]).
    Lnurl {
        metadata: LnurlPayMetadata,
        raw: String,
    },
    /// A mainnet on-chain address.
    Onchain {
        address: String,
        amount_sats: Option<u64>,
    },
    /// Anything else; `reason` is the PWA's error string, verbatim.
    Invalid { reason: String },
}

impl Classified {
    /// The dispatchable classification: unwraps a BIP321 wrapper to its
    /// preferred method (what the PWA's `parseBip321` returns directly).
    pub fn effective(&self) -> &Classified {
        match self {
            Classified::Bip321 { preferred, .. } => preferred.effective(),
            other => other,
        }
    }
}

/// Classifies a send input at the current time.
pub fn classify(input: &str) -> Classified {
    classify_at(input, unix_now())
}

/// Classifies a send input with an injectable `now` (expiry checks happen at
/// classify time, PWA parity). Dispatch order is `payment-input.ts:56-93`,
/// preserved exactly.
pub fn classify_at(raw: &str, now: Duration) -> Classified {
    // Send.tsx:613-615 — the scan/input cap, before anything else.
    if raw.chars().count() > MAX_INPUT_CHARS {
        return invalid(ERR_INPUT_TOO_LONG);
    }
    let input = raw.trim();
    let lower = input.to_lowercase();

    // BIP 321 unified URI (payment-input.ts:61-63).
    if has_prefix_ci(input, "bitcoin:") {
        return parse_bip321(input, now);
    }

    // lightning: URI scheme — strip and recurse (payment-input.ts:66-68).
    if has_prefix_ci(input, "lightning:") {
        return classify_at(&input["lightning:".len()..], now);
    }

    // BOLT 11 — broad /^ln(bc|tb|tbs|bcrt)/i match; non-mainnet prefixes are
    // rejected by the network check inside (payment-input.ts:71-73).
    if has_prefix_ci(input, "lnbc") || has_prefix_ci(input, "lntb") {
        return parse_bolt11(input, now);
    }

    // BOLT 12 offer (payment-input.ts:76-78).
    if has_prefix_ci(input, "lno1") {
        return parse_bolt12(input, now);
    }

    // BIP 353 human-readable name (payment-input.ts:81-83).
    if is_bip353_candidate(input) {
        return parse_bip353(input);
    }

    // On-chain fallback: bech32 against the lowercased input (BIP 173 is
    // case-insensitive), legacy base58 against the original
    // (payment-input.ts:86-90).
    if is_bech32_mainnet(&lower) || is_legacy_mainnet(input) {
        return Classified::Onchain {
            address: input.to_string(),
            amount_sats: None,
        };
    }

    invalid(ERR_UNRECOGNIZED)
}

fn invalid(reason: &str) -> Classified {
    Classified::Invalid {
        reason: reason.to_string(),
    }
}

/// ASCII-case-insensitive prefix test (the PWA lowercases the whole input;
/// every scheme/prefix involved is ASCII).
fn has_prefix_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len()
        && s.is_char_boundary(prefix.len())
        && s[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// `/^bc1[a-z0-9]{25,87}$/` over the LOWERCASED input (payment-input.ts:15).
fn is_bech32_mainnet(lower: &str) -> bool {
    let Some(rest) = lower.strip_prefix("bc1") else {
        return false;
    };
    (25..=87).contains(&rest.len())
        && rest
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// `/^[13][a-km-zA-HJ-NP-Z1-9]{25,34}$/` — base58check without `0OIl`
/// (payment-input.ts:16).
fn is_legacy_mainnet(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() || (bytes[0] != b'1' && bytes[0] != b'3') {
        return false;
    }
    let rest = &bytes[1..];
    (25..=34).contains(&rest.len())
        && rest.iter().all(|&b| {
            (b.is_ascii_lowercase() && b != b'l')
                || (b.is_ascii_uppercase() && b != b'I' && b != b'O')
                || (b.is_ascii_digit() && b != b'0')
        })
}

/// `/^[₿]?[a-z0-9._-]+@[a-z0-9.-]+\.[a-z]{2,}$/i` (payment-input.ts:81).
fn is_bip353_candidate(input: &str) -> bool {
    let rest = input.strip_prefix('\u{20bf}').unwrap_or(input);
    let Some((user, domain)) = rest.split_once('@') else {
        return false;
    };
    // The character classes exclude '@', so a second '@' can never match.
    if domain.contains('@') || user.is_empty() {
        return false;
    }
    let user_ok = user
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-');
    if !user_ok {
        return false;
    }
    // Domain: `[a-z0-9.-]+\.[a-z]{2,}` — a non-empty labels part, a dot, and
    // a 2+ letter TLD at the end.
    let Some(last_dot) = domain.rfind('.') else {
        return false;
    };
    let (labels, tld) = (&domain[..last_dot], &domain[last_dot + 1..]);
    !labels.is_empty()
        && labels
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        && tld.len() >= 2
        && tld.bytes().all(|b| b.is_ascii_alphabetic())
}

fn parse_bolt11(raw: &str, now: Duration) -> Classified {
    // payment-input.ts:95-127.
    let Ok(invoice) = Bolt11Invoice::from_str(raw) else {
        return invalid(ERR_INVALID_INVOICE);
    };
    if invoice.network() != Network::Bitcoin {
        return invalid(ERR_INVOICE_NETWORK);
    }
    if invoice.would_expire(now) {
        return invalid(ERR_INVOICE_EXPIRED);
    }
    let description = match invoice.description() {
        Bolt11InvoiceDescriptionRef::Direct(description) => Some(description.to_string()),
        Bolt11InvoiceDescriptionRef::Hash(_) => None,
    };
    Classified::Bolt11 {
        raw: raw.to_string(),
        amount_msat: invoice.amount_milli_satoshis(),
        description,
        // Read off the invoice already parsed above — never re-decoded.
        payment_hash: hex_str(invoice.payment_hash().as_byte_array()),
    }
}

fn parse_bolt12(raw: &str, now: Duration) -> Classified {
    // payment-input.ts:129-180.
    let Ok(offer) = Offer::from_str(raw) else {
        return invalid(ERR_INVALID_OFFER);
    };
    // An offer with no chains field implicitly targets mainnet
    // (payment-input.ts:137-154); LDK's supports_chain encodes exactly that.
    if !offer.supports_chain(ChainHash::using_genesis_block(Network::Bitcoin)) {
        return invalid(ERR_OFFER_NETWORK);
    }
    if offer.is_expired_no_std(now) {
        return invalid(ERR_OFFER_EXPIRED);
    }
    // Only bitcoin-denominated amounts map (payment-input.ts:162-166);
    // fiat-denominated offers classify as amountless.
    let amount_msat = match offer.amount() {
        Some(OfferAmount::Bitcoin { amount_msats }) => Some(amount_msats),
        _ => None,
    };
    Classified::Bolt12 {
        raw: raw.to_string(),
        amount_msat,
        description: offer.description().map(|d| d.to_string()),
    }
}

fn parse_bip353(input: &str) -> Classified {
    // payment-input.ts:182-189 — strip one leading ₿, then LDK's
    // HumanReadableName validates user/domain shape and length.
    let cleaned = input.strip_prefix('\u{20bf}').unwrap_or(input);
    match HumanReadableName::from_encoded(cleaned) {
        Ok(name) => Classified::Bip353 {
            user: name.user().to_string(),
            domain: name.domain().trim_end_matches('.').to_string(),
            raw: cleaned.to_string(),
        },
        Err(_) => invalid(ERR_INVALID_ADDRESS_FORMAT),
    }
}

/// BIP 321 URI parsing (payment-input.ts:191-258): manual query parsing,
/// preference `lno` > `lightning` > address (AE5).
fn parse_bip321(input: &str, now: Duration) -> Classified {
    let without_scheme = &input["bitcoin:".len()..];
    // JS `split('?', 2)` keeps only the first two segments: everything after
    // a second '?' is dropped.
    let mut segments = without_scheme.splitn(3, '?');
    let address = segments.next().unwrap_or("").trim();
    let query = segments.next();

    // JS falsiness: a present-but-empty query counts as absent.
    let query_present = query.is_some_and(|q| !q.is_empty());
    if !query_present && address.is_empty() {
        return invalid(ERR_EMPTY_URI);
    }

    let mut lno_value: Option<String> = None;
    let mut lightning_value: Option<String> = None;
    let mut amount_btc: Option<String> = None;

    if let Some(query) = query {
        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (raw_key, raw_value) = match pair.find('=') {
                Some(eq) => (&pair[..eq], &pair[eq + 1..]),
                None => (pair, ""),
            };
            let (Some(key), Some(value)) = (percent_decode(raw_key), percent_decode(raw_value))
            else {
                return invalid(ERR_MALFORMED_URI);
            };
            match key.to_lowercase().as_str() {
                "lno" => lno_value = Some(value),
                "lightning" => lightning_value = Some(value),
                "amount" => amount_btc = Some(value),
                _ => {}
            }
        }
    }

    let amount_sats = amount_btc.as_deref().and_then(btc_string_to_sats);
    let onchain_fallback = (!address.is_empty()
        && (is_bech32_mainnet(&address.to_lowercase()) || is_legacy_mainnet(address)))
    .then(|| address.to_string());

    // Preference: BOLT 12 > BOLT 11 > on-chain (payment-input.ts:229-241).
    let preferred = if let Some(lno) = lno_value {
        parse_bolt12(&lno, now)
    } else if let Some(lightning) = lightning_value {
        parse_bolt11(&lightning, now)
    } else if address.is_empty() {
        return invalid(ERR_URI_NO_METHOD);
    } else {
        match &onchain_fallback {
            Some(address) => Classified::Onchain {
                address: address.clone(),
                amount_sats,
            },
            // payment-input.ts:243-247 — an address that fails the mainnet
            // regexes is a network error, not an unrecognized format.
            None => return invalid(ERR_URI_ADDRESS_NETWORK),
        }
    };

    Classified::Bip321 {
        preferred: Box::new(preferred),
        onchain_fallback,
        amount_sats,
    }
}

/// `decodeURIComponent` semantics: `%XX` hex escapes, decoded bytes must be
/// valid UTF-8, `+` is NOT a space. `None` is the PWA's URIError.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let high = char::from(*bytes.get(i + 1)?).to_digit(16)?;
            let low = char::from(*bytes.get(i + 2)?).to_digit(16)?;
            out.push((high * 16 + low) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// BTC fixed-point string → sats (payment-input.ts:261-269): strict
/// `^\d+(\.\d+)?$`, fraction truncated past 8 decimals.
fn btc_string_to_sats(btc: &str) -> Option<u64> {
    let trimmed = btc.trim();
    let (whole, frac) = match trimmed.split_once('.') {
        Some((whole, frac)) => (whole, frac),
        None => (trimmed, ""),
    };
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if trimmed.contains('.') && (frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit())) {
        return None;
    }
    let whole: u128 = whole.parse().ok()?;
    let mut padded = String::with_capacity(8);
    padded.push_str(&frac[..frac.len().min(8)]);
    while padded.len() < 8 {
        padded.push('0');
    }
    let frac_sats: u128 = padded.parse().ok()?;
    u64::try_from(whole.checked_mul(100_000_000)?.checked_add(frac_sats)?).ok()
}

// ---------------------------------------------------------------------------
// Resolution (BIP353 + LNURL)
// ---------------------------------------------------------------------------

/// Typed resolution failures; every variant renders the PWA's user-facing
/// string verbatim where one exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// Both BIP353 and LNURL missed (`Send.tsx:339`).
    NotFound { raw: String },
    /// The LNURL server answered `status: ERROR` (`resolve-lnurl.ts:63,123`).
    ServerError { reason: String },
    /// `resolve-lnurl.ts:75`.
    CallbackNotHttps,
    /// `resolve-lnurl.ts:83`.
    InvalidCallback,
    /// `resolve-lnurl.ts:87`.
    CallbackDomainMismatch,
    /// The LNURL callback fetch failed (`resolve-lnurl.ts:120`).
    InvoiceFetchFailed,
    /// The callback response carried no `pr` (`resolve-lnurl.ts:124`).
    NoInvoice,
    /// The fetched `pr` did not classify as a valid BOLT11 invoice
    /// (`Send.tsx:272`).
    InvalidProviderInvoice,
    /// The fetched invoice's amount is not the requested amount
    /// (`Send.tsx:261`; KTD-6 makes the match mandatory).
    InvoiceAmountMismatch,
    /// KTD-6: the fetched invoice's `description_hash` does not commit to
    /// the LUD-06 metadata (no PWA counterpart — the PWA skips this check).
    DescriptionHashMismatch,
    /// The requested amount is outside the server's min/max window (the PWA
    /// gates this in the amount screen with formatted copy; core enforces it
    /// as defense in depth).
    AmountOutOfBounds { min_msat: u64, max_msat: u64 },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::NotFound { raw } => {
                write!(f, "No Lightning Address or BIP 353 record found for {raw}")
            }
            ResolveError::ServerError { reason } => write!(f, "{reason}"),
            ResolveError::CallbackNotHttps => {
                write!(f, "Lightning Address callback is not HTTPS")
            }
            ResolveError::InvalidCallback => {
                write!(f, "Lightning Address has invalid callback URL")
            }
            ResolveError::CallbackDomainMismatch => {
                write!(f, "Lightning Address callback domain mismatch")
            }
            ResolveError::InvoiceFetchFailed => write!(f, "Failed to fetch invoice"),
            ResolveError::NoInvoice => write!(f, "No invoice in response"),
            ResolveError::InvalidProviderInvoice => {
                write!(f, "Invalid invoice from Lightning Address provider")
            }
            ResolveError::InvoiceAmountMismatch => {
                write!(f, "Invoice amount does not match requested amount")
            }
            ResolveError::DescriptionHashMismatch => {
                write!(f, "Invoice does not match the Lightning Address metadata")
            }
            ResolveError::AmountOutOfBounds { .. } => {
                write!(f, "Amount is outside the Lightning Address limits")
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// First-step resolution outcome for a BIP353 name.
#[derive(Debug, Clone)]
pub enum Bip353Outcome {
    /// A DNSSEC-verified `bitcoin:` TXT record.
    Bip353(String),
    /// The backend already fell through to LNURL and got an init response
    /// (the crate's `resolve_hrn` does this when the DoH transport fails).
    Lnurl(LnurlInit),
    /// No verified record (NXDOMAIN, unverifiable, timeout, transport
    /// failure) — the PWA's `null`.
    Miss,
}

/// A validated LUD-06 init response, pre-callback-binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LnurlInit {
    pub callback: String,
    pub min_sendable_msat: u64,
    pub max_sendable_msat: u64,
    /// First `text/plain` metadata entry (`resolve-lnurl.ts:92` takes the
    /// FIRST match); `None` falls back to `user@domain`.
    pub description: Option<String>,
    pub expected_description_hash: Option<[u8; 32]>,
}

/// The seam between the resolution flow and the network, so tests can stub
/// DNS and LNURL endpoints. The production impl is [`HttpNameResolver`].
pub trait NameResolver: Send + Sync {
    /// BIP353 step: DNSSEC-verified TXT lookup for `user@domain`.
    fn resolve_bip353(
        &self,
        user: &str,
        domain: &str,
    ) -> impl Future<Output = Bip353Outcome> + Send;

    /// LUD-16 step: fetch + validate `https://domain/.well-known/lnurlp/user`.
    /// `Ok(None)` is the PWA's `null` (no endpoint / not a payRequest);
    /// errors are validation failures that must surface.
    fn lnurl_init(
        &self,
        user: &str,
        domain: &str,
    ) -> impl Future<Output = Result<Option<LnurlInit>, ResolveError>> + Send;

    /// LUD-06 callback fetch: GET `url`, return the response body.
    fn lnurl_callback(
        &self,
        url: &str,
    ) -> impl Future<Output = Result<String, ResolveError>> + Send;
}

/// Resolves a classified input: BIP353 names resolve over DNSSEC DoH (5 s
/// budget), falling back to LNURL-pay on a miss (fresh 5 s budget); every
/// other classification passes through unchanged.
pub async fn resolve<R: NameResolver>(
    classified: Classified,
    resolver: &R,
    now: Duration,
) -> Result<Classified, ResolveError> {
    resolve_with_budget(classified, resolver, now, RESOLVE_TIMEOUT).await
}

/// [`resolve`] with an injectable per-step budget (tests use tiny budgets).
pub(crate) async fn resolve_with_budget<R: NameResolver>(
    classified: Classified,
    resolver: &R,
    now: Duration,
    budget: Duration,
) -> Result<Classified, ResolveError> {
    let Classified::Bip353 { user, domain, raw } = classified else {
        return Ok(classified);
    };

    // Step 1: BIP353 over DNSSEC-verified DoH, its own budget; a timeout is
    // a miss, exactly like the PWA's TimeoutError → null (Send.tsx:296-302).
    let outcome = tokio::time::timeout(budget, resolver.resolve_bip353(&user, &domain))
        .await
        .unwrap_or(Bip353Outcome::Miss);

    match outcome {
        Bip353Outcome::Bip353(txt) => {
            // resolve-bip353.ts:59-63 — the TXT record must classify to a
            // non-error; otherwise it is a miss and LNURL gets its turn.
            let parsed = classify_at(&txt, now);
            if !matches!(parsed.effective(), Classified::Invalid { .. }) {
                return Ok(parsed);
            }
        }
        // The backend already fell through to LNURL (crate transport-failure
        // path) — use it rather than fetching the same endpoint again.
        Bip353Outcome::Lnurl(init) => return finish_lnurl(init, user, domain, raw),
        Bip353Outcome::Miss => {}
    }

    // Step 2: LNURL-pay fallback with a FRESH budget (Send.tsx:308-315);
    // a timeout here is the PWA's null → not-found.
    let init = tokio::time::timeout(budget, resolver.lnurl_init(&user, &domain))
        .await
        .unwrap_or(Ok(None))?;
    match init {
        Some(init) => finish_lnurl(init, user, domain, raw),
        None => Err(ResolveError::NotFound { raw }),
    }
}

/// Callback binding + description default (resolve-lnurl.ts:72-104). Applied
/// to BOTH the hand-rolled LNURL path and the crate's fallback resolutions.
fn finish_lnurl(
    init: LnurlInit,
    user: String,
    domain: String,
    raw: String,
) -> Result<Classified, ResolveError> {
    if !init.callback.starts_with("https://") {
        return Err(ResolveError::CallbackNotHttps);
    }
    let host = url_hostname(&init.callback).ok_or(ResolveError::InvalidCallback)?;
    if host != domain && !host.ends_with(&format!(".{domain}")) {
        return Err(ResolveError::CallbackDomainMismatch);
    }
    let description = init
        .description
        .unwrap_or_else(|| format!("{user}@{domain}"));
    Ok(Classified::Lnurl {
        metadata: LnurlPayMetadata {
            domain,
            user,
            callback: init.callback,
            min_sendable_msat: init.min_sendable_msat,
            max_sendable_msat: init.max_sendable_msat,
            description,
            expected_description_hash: init.expected_description_hash,
        },
        raw,
    })
}

/// The hostname of an http(s) URL, lowercased — `new URL(url).hostname`
/// semantics for the URLs LNURL callbacks use (userinfo and port stripped).
fn url_hostname(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://")?.1;
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    // Strip userinfo (everything up to the last '@').
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    // Strip a port (not applicable to bracketed IPv6, which LNURL callbacks
    // do not use in practice).
    let host = if host_port.starts_with('[') {
        host_port
    } else {
        host_port.split(':').next().unwrap_or(host_port)
    };
    if host.is_empty() {
        return None;
    }
    Some(host.to_lowercase())
}

/// Fetches and validates the final BOLT11 invoice from an LNURL callback
/// (`fetchLnurlInvoice` + `Send.tsx fetchAndRouteInvoice`, plus KTD-6's
/// amount-match and `description_hash` enforcement). Returns the validated
/// invoice classification.
pub async fn fetch_lnurl_invoice<R: NameResolver>(
    resolver: &R,
    metadata: &LnurlPayMetadata,
    amount_msat: u64,
    now: Duration,
) -> Result<Classified, ResolveError> {
    // Defense in depth: the shells gate min/max with formatted copy
    // (Send.tsx:554-560); core refuses out-of-window amounts outright.
    if amount_msat < metadata.min_sendable_msat || amount_msat > metadata.max_sendable_msat {
        return Err(ResolveError::AmountOutOfBounds {
            min_msat: metadata.min_sendable_msat,
            max_msat: metadata.max_sendable_msat,
        });
    }
    // resolve-lnurl.ts:116-117 — amount appended as a query parameter.
    let separator = if metadata.callback.contains('?') {
        '&'
    } else {
        '?'
    };
    let url = format!("{}{}amount={}", metadata.callback, separator, amount_msat);
    let body = tokio::time::timeout(RESOLVE_TIMEOUT, resolver.lnurl_callback(&url))
        .await
        .map_err(|_| ResolveError::InvoiceFetchFailed)??;
    validate_lnurl_invoice(
        &body,
        amount_msat,
        metadata.expected_description_hash.as_ref(),
        now,
    )
}

/// Validates a LUD-06 init response body (`resolve-lnurl.ts:45-104`,
/// callback binding excluded — that happens in [`resolve`] so it also covers
/// crate-resolved fallbacks). `Ok(None)` mirrors the PWA's `null`.
pub(crate) fn validate_lnurl_init(body: &str) -> Result<Option<LnurlInit>, ResolveError> {
    let Ok(data) = serde_json::from_str::<serde_json::Value>(body) else {
        return Ok(None); // resolve-lnurl.ts:56-60
    };
    if data.get("status").and_then(|s| s.as_str()) == Some("ERROR") {
        // resolve-lnurl.ts:62-64.
        let reason = data
            .get("reason")
            .and_then(|r| r.as_str())
            .unwrap_or("Lightning Address returned an error")
            .to_string();
        return Err(ResolveError::ServerError { reason });
    }
    if data.get("tag").and_then(|t| t.as_str()) != Some("payRequest") {
        return Ok(None); // resolve-lnurl.ts:66
    }
    // resolve-lnurl.ts:68-70 — JS falsiness: missing, empty, or zero fields
    // are all a miss.
    let Some(callback) = data
        .get("callback")
        .and_then(|c| c.as_str())
        .filter(|c| !c.is_empty())
    else {
        return Ok(None);
    };
    let min_sendable_msat = data
        .get("minSendable")
        .and_then(|m| m.as_u64())
        .unwrap_or(0);
    let max_sendable_msat = data
        .get("maxSendable")
        .and_then(|m| m.as_u64())
        .unwrap_or(0);
    if min_sendable_msat == 0 || max_sendable_msat == 0 {
        return Ok(None);
    }
    // resolve-lnurl.ts:89-95 — the FIRST text/plain entry; parse failures
    // fall back (the user@domain default is applied by the caller). KTD-6:
    // the description hash commits to the RAW metadata string as served.
    let metadata_str = data.get("metadata").and_then(|m| m.as_str());
    let description = metadata_str
        .and_then(|m| serde_json::from_str::<Vec<Vec<serde_json::Value>>>(m).ok())
        .and_then(|entries| {
            entries
                .into_iter()
                .find_map(|entry| match entry.as_slice() {
                    [mime, value, ..] if mime.as_str() == Some("text/plain") => {
                        value.as_str().map(str::to_string)
                    }
                    _ => None,
                })
        });
    let expected_description_hash =
        metadata_str.map(|m| sha256::Hash::hash(m.as_bytes()).to_byte_array());
    Ok(Some(LnurlInit {
        callback: callback.to_string(),
        min_sendable_msat,
        max_sendable_msat,
        description,
        expected_description_hash,
    }))
}

/// Validates an LNURL callback response body and its invoice
/// (`resolve-lnurl.ts:111-127` + `Send.tsx:255-273` + KTD-6 enforcement).
///
/// Deviations from the PWA, both REQUIRED by KTD-6 (stricter, never laxer):
/// - the invoice must carry the requested amount exactly (the PWA back-fills
///   the requested amount into an amountless invoice);
/// - with server metadata present, the invoice's `description_hash` must
///   commit to it (the PWA performs no hash verification).
pub(crate) fn validate_lnurl_invoice(
    body: &str,
    requested_amount_msat: u64,
    expected_description_hash: Option<&[u8; 32]>,
    now: Duration,
) -> Result<Classified, ResolveError> {
    let data: serde_json::Value =
        serde_json::from_str(body).map_err(|_| ResolveError::InvoiceFetchFailed)?;
    if data.get("status").and_then(|s| s.as_str()) == Some("ERROR") {
        // resolve-lnurl.ts:123.
        let reason = data
            .get("reason")
            .and_then(|r| r.as_str())
            .unwrap_or("LNURL error")
            .to_string();
        return Err(ResolveError::ServerError { reason });
    }
    let Some(pr) = data
        .get("pr")
        .and_then(|pr| pr.as_str())
        .filter(|pr| !pr.is_empty())
    else {
        return Err(ResolveError::NoInvoice); // resolve-lnurl.ts:124
    };

    // Send.tsx:257-258 — the fetched invoice is re-CLASSIFIED, so network
    // and expiry checks apply.
    let classified = classify_at(pr, now);
    let Classified::Bolt11 { amount_msat, .. } = &classified else {
        return Err(ResolveError::InvalidProviderInvoice);
    };

    // KTD-6 amount-match enforcement (Send.tsx:259-262, made mandatory).
    if *amount_msat != Some(requested_amount_msat) {
        return Err(ResolveError::InvoiceAmountMismatch);
    }

    // KTD-6 description_hash commitment.
    if let Some(expected) = expected_description_hash {
        let invoice =
            Bolt11Invoice::from_str(pr).map_err(|_| ResolveError::InvalidProviderInvoice)?;
        match invoice.description() {
            Bolt11InvoiceDescriptionRef::Hash(hash) if hash.0.to_byte_array() == *expected => {}
            _ => return Err(ResolveError::DescriptionHashMismatch),
        }
    }

    Ok(classified)
}

// ---------------------------------------------------------------------------
// Production resolver
// ---------------------------------------------------------------------------

/// The production [`NameResolver`]: BIP353 through
/// `bitcoin-payment-instructions`' `HTTPHrnResolver` (full DNSSEC proof over
/// DoH to dns.google — KTD-1), LNURL over the in-tree reqwest/rustls stack
/// with the PWA's validation semantics.
pub struct HttpNameResolver {
    hrn: HTTPHrnResolver,
    http: reqwest::Client,
    /// Test hook: LUD-16 well-known scheme ("https" in production).
    lnurl_scheme: &'static str,
}

impl Default for HttpNameResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpNameResolver {
    pub fn new() -> Self {
        Self {
            hrn: HTTPHrnResolver::new(),
            // Belt-and-braces transport timeout; the per-step budgets in
            // resolve()/fetch_lnurl_invoice are the real governors.
            http: reqwest::Client::builder()
                .timeout(RESOLVE_TIMEOUT)
                .build()
                .expect("reqwest client construction cannot fail"),
            lnurl_scheme: "https",
        }
    }

    #[cfg(test)]
    pub(crate) fn with_plain_http_lnurl() -> Self {
        let mut resolver = Self::new();
        resolver.lnurl_scheme = "http";
        resolver
    }
}

impl NameResolver for HttpNameResolver {
    async fn resolve_bip353(&self, user: &str, domain: &str) -> Bip353Outcome {
        let Ok(hrn) = HumanReadableName::from_encoded(&format!("{user}@{domain}")) else {
            return Bip353Outcome::Miss;
        };
        match self.hrn.resolve_hrn(&hrn).await {
            Ok(HrnResolution::DNSSEC { result, .. }) => Bip353Outcome::Bip353(result),
            // The crate falls through to LNURL itself when the DoH transport
            // fails; keep the result rather than fetching it twice. Callback
            // binding still happens in resolve().
            Ok(HrnResolution::LNURLPay {
                min_value,
                max_value,
                expected_description_hash,
                recipient_description,
                callback,
            }) => Bip353Outcome::Lnurl(LnurlInit {
                callback,
                min_sendable_msat: min_value.milli_sats(),
                max_sendable_msat: max_value.milli_sats(),
                description: recipient_description,
                expected_description_hash: Some(expected_description_hash),
            }),
            Err(_) => Bip353Outcome::Miss,
        }
    }

    async fn lnurl_init(
        &self,
        user: &str,
        domain: &str,
    ) -> Result<Option<LnurlInit>, ResolveError> {
        let url = format!("{}://{domain}/.well-known/lnurlp/{user}", self.lnurl_scheme);
        // Network errors and HTTP errors are the PWA's `null`
        // (resolve-lnurl.ts:36-43), not validation failures.
        let Ok(response) = self.http.get(&url).send().await else {
            return Ok(None);
        };
        if !response.status().is_success() {
            return Ok(None);
        }
        let Ok(body) = response.text().await else {
            return Ok(None);
        };
        validate_lnurl_init(&body)
    }

    async fn lnurl_callback(&self, url: &str) -> Result<String, ResolveError> {
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|_| ResolveError::InvoiceFetchFailed)?;
        if !response.status().is_success() {
            return Err(ResolveError::InvoiceFetchFailed);
        }
        response
            .text()
            .await
            .map_err(|_| ResolveError::InvoiceFetchFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use bitcoin::hashes::sha256 as sha256_mod;
    use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};
    use lightning::offers::offer::OfferBuilder;
    use lightning::types::payment::PaymentSecret;
    use lightning_invoice::{Currency, InvoiceBuilder};

    /// Fixed "now" for deterministic expiry checks.
    const NOW: u64 = 1_753_000_000;

    fn now() -> Duration {
        Duration::from_secs(NOW)
    }

    fn test_invoice(
        currency: Currency,
        amount_msat: Option<u64>,
        created_at_unix_secs: u64,
        expiry_secs: u64,
        description: &str,
    ) -> Bolt11Invoice {
        let secret = SecretKey::from_slice(&[0x3c; 32]).unwrap();
        let builder = InvoiceBuilder::new(currency)
            .description(description.to_string())
            .payment_hash(sha256_mod::Hash::from_byte_array([0x11; 32]))
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

    fn hash_invoice(
        amount_msat: u64,
        created_at_unix_secs: u64,
        description_hash: sha256_mod::Hash,
    ) -> Bolt11Invoice {
        let secret = SecretKey::from_slice(&[0x3c; 32]).unwrap();
        let builder = InvoiceBuilder::new(Currency::Bitcoin)
            .description_hash(description_hash)
            .payment_hash(sha256_mod::Hash::from_byte_array([0x11; 32]))
            .payment_secret(PaymentSecret([0x22; 32]))
            .duration_since_epoch(Duration::from_secs(created_at_unix_secs))
            .min_final_cltv_expiry_delta(144)
            .expiry_time(Duration::from_secs(3_600))
            .amount_milli_satoshis(amount_msat);
        let sign = |hash: &_| Secp256k1::new().sign_ecdsa_recoverable(hash, &secret);
        builder.build_signed(sign).unwrap()
    }

    fn valid_bolt11() -> String {
        test_invoice(
            Currency::Bitcoin,
            Some(50_000_000),
            NOW,
            3_600,
            "Test invoice",
        )
        .to_string()
    }

    fn amountless_bolt11() -> String {
        test_invoice(Currency::Bitcoin, None, NOW, 3_600, "Test invoice").to_string()
    }

    fn signing_pubkey() -> PublicKey {
        let secp = Secp256k1::new();
        PublicKey::from_secret_key(&secp, &SecretKey::from_slice(&[0x3c; 32]).unwrap())
    }

    fn offer_with_amount(amount_msat: u64) -> String {
        OfferBuilder::new(signing_pubkey())
            .description("test offer".to_string())
            .amount_msats(amount_msat)
            .build()
            .unwrap()
            .to_string()
    }

    fn amountless_offer() -> String {
        OfferBuilder::new(signing_pubkey())
            .description("test offer".to_string())
            .build()
            .unwrap()
            .to_string()
    }

    fn testnet_offer() -> String {
        OfferBuilder::new(signing_pubkey())
            .description("test offer".to_string())
            .chain(Network::Testnet)
            .build()
            .unwrap()
            .to_string()
    }

    fn expired_offer() -> String {
        OfferBuilder::new(signing_pubkey())
            .description("test offer".to_string())
            .absolute_expiry(Duration::from_secs(NOW - 1))
            .build()
            .unwrap()
            .to_string()
    }

    const MAINNET_BECH32: &str = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
    const MAINNET_P2PKH: &str = "1A1zP1eP5QGefi2DMPTfTL5SLmv7DivfNa";
    const MAINNET_P2SH: &str = "3J98t1WpEZ73CNmQviecrnyiWrnqRhWNLy";
    const SIGNET_BECH32: &str = "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx";

    fn invalid_reason(classified: &Classified) -> &str {
        match classified {
            Classified::Invalid { reason } => reason,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    // =====================================================================
    // Classification matrix, ported case-for-case from the PWA's
    // payment-input.test.ts (file:line cited per test).
    // =====================================================================

    // ---- on-chain addresses (payment-input.test.ts:100-124) ----

    #[test]
    fn accepts_mainnet_bech32_address() {
        // payment-input.test.ts:101-105
        assert_eq!(
            classify_at(MAINNET_BECH32, now()),
            Classified::Onchain {
                address: MAINNET_BECH32.to_string(),
                amount_sats: None
            }
        );
    }

    #[test]
    fn accepts_mainnet_p2pkh_address() {
        // payment-input.test.ts:107-111
        assert_eq!(
            classify_at(MAINNET_P2PKH, now()),
            Classified::Onchain {
                address: MAINNET_P2PKH.to_string(),
                amount_sats: None
            }
        );
    }

    #[test]
    fn accepts_mainnet_p2sh_address() {
        // payment-input.test.ts:113-117
        assert_eq!(
            classify_at(MAINNET_P2SH, now()),
            Classified::Onchain {
                address: MAINNET_P2SH.to_string(),
                amount_sats: None
            }
        );
    }

    #[test]
    fn rejects_signet_address_on_mainnet() {
        // payment-input.test.ts:119-123 — tb1… falls through every family
        // to "Unrecognized payment format".
        assert_eq!(
            invalid_reason(&classify_at(SIGNET_BECH32, now())),
            ERR_UNRECOGNIZED
        );
    }

    #[test]
    fn uppercase_bech32_qr_form_is_accepted() {
        // payment-input.ts:86-88 — bech32 tested against the lowercased
        // input (BIP173 QR uppercase form).
        let upper = MAINNET_BECH32.to_uppercase();
        assert_eq!(
            classify_at(&upper, now()),
            Classified::Onchain {
                address: upper.clone(),
                amount_sats: None
            }
        );
    }

    // ---- BIP321 URIs (payment-input.test.ts:126-179) ----

    #[test]
    fn accepts_bip321_uri_with_mainnet_address_and_amount() {
        // payment-input.test.ts:127-137
        let result = classify_at(&format!("bitcoin:{MAINNET_BECH32}?amount=0.001"), now());
        assert_eq!(
            result.effective(),
            &Classified::Onchain {
                address: MAINNET_BECH32.to_string(),
                amount_sats: Some(100_000)
            }
        );
        match &result {
            Classified::Bip321 {
                onchain_fallback,
                amount_sats,
                ..
            } => {
                assert_eq!(onchain_fallback.as_deref(), Some(MAINNET_BECH32));
                assert_eq!(*amount_sats, Some(100_000));
            }
            other => panic!("expected Bip321 wrapper, got {other:?}"),
        }
    }

    #[test]
    fn rejects_bip321_uri_with_signet_address_on_mainnet() {
        // payment-input.test.ts:139-146
        let result = classify_at(&format!("bitcoin:{SIGNET_BECH32}"), now());
        assert_eq!(invalid_reason(&result), ERR_URI_ADDRESS_NETWORK);
        assert!(invalid_reason(&result).contains("different Bitcoin network"));
    }

    #[test]
    fn extracts_bolt11_from_bip321_uri_with_lightning_parameter() {
        // payment-input.test.ts:148-158
        let bolt11 = valid_bolt11();
        let result = classify_at(
            &format!("bitcoin:{MAINNET_BECH32}?lightning={bolt11}"),
            now(),
        );
        assert_eq!(
            result.effective(),
            &Classified::Bolt11 {
                raw: bolt11,
                amount_msat: Some(50_000_000),
                description: Some("Test invoice".to_string()),
                payment_hash: "11".repeat(32),
            }
        );
    }

    #[test]
    fn prefers_lightning_over_onchain_address_in_bip321_uri() {
        // payment-input.test.ts:160-167
        let bolt11 = valid_bolt11();
        let result = classify_at(
            &format!("bitcoin:{MAINNET_BECH32}?amount=0.001&lightning={bolt11}"),
            now(),
        );
        assert!(
            matches!(result.effective(), Classified::Bolt11 { .. }),
            "lightning takes precedence, got {result:?}"
        );
        // The ordered on-chain fallback is preserved (AE5).
        match &result {
            Classified::Bip321 {
                onchain_fallback,
                amount_sats,
                ..
            } => {
                assert_eq!(onchain_fallback.as_deref(), Some(MAINNET_BECH32));
                assert_eq!(*amount_sats, Some(100_000));
            }
            other => panic!("expected Bip321 wrapper, got {other:?}"),
        }
    }

    #[test]
    fn rejects_bip321_uri_with_malformed_percent_sequence() {
        // payment-input.test.ts:169-178
        let result = classify_at(&format!("bitcoin:{MAINNET_BECH32}?amount=0.00%ZZ"), now());
        assert_eq!(invalid_reason(&result), ERR_MALFORMED_URI);
        assert!(invalid_reason(&result).contains("Malformed"));
    }

    // ---- AE5: full BIP321 preference permutations ----

    #[test]
    fn ae5_bip321_prefers_lno_over_lightning_and_address() {
        // AE5: lno + lightning + address → BOLT12 preferred, BOLT11 and
        // on-chain as ordered fallbacks.
        let offer = offer_with_amount(25_000);
        let bolt11 = valid_bolt11();
        let result = classify_at(
            &format!("bitcoin:{MAINNET_BECH32}?amount=0.001&lightning={bolt11}&lno={offer}"),
            now(),
        );
        assert_eq!(
            result.effective(),
            &Classified::Bolt12 {
                raw: offer,
                amount_msat: Some(25_000),
                description: Some("test offer".to_string()),
            }
        );
        match &result {
            Classified::Bip321 {
                onchain_fallback, ..
            } => assert_eq!(onchain_fallback.as_deref(), Some(MAINNET_BECH32)),
            other => panic!("expected Bip321 wrapper, got {other:?}"),
        }
    }

    #[test]
    fn ae5_bip321_prefers_lno_over_lightning_without_address() {
        let offer = offer_with_amount(25_000);
        let bolt11 = valid_bolt11();
        let result = classify_at(&format!("bitcoin:?lightning={bolt11}&lno={offer}"), now());
        assert!(matches!(result.effective(), Classified::Bolt12 { .. }));
        match &result {
            Classified::Bip321 {
                onchain_fallback, ..
            } => assert_eq!(*onchain_fallback, None),
            other => panic!("expected Bip321 wrapper, got {other:?}"),
        }
    }

    #[test]
    fn ae5_bip321_lightning_beats_address_lno_beats_both_regardless_of_query_order() {
        let offer = offer_with_amount(25_000);
        let bolt11 = valid_bolt11();
        // lno last in the query string still wins (key-based, not positional).
        let result = classify_at(
            &format!("bitcoin:{MAINNET_BECH32}?lightning={bolt11}&lno={offer}"),
            now(),
        );
        assert!(matches!(result.effective(), Classified::Bolt12 { .. }));
        // lightning only + address → BOLT11 preferred.
        let result = classify_at(
            &format!("bitcoin:{MAINNET_BECH32}?lightning={bolt11}"),
            now(),
        );
        assert!(matches!(result.effective(), Classified::Bolt11 { .. }));
        // address only → on-chain.
        let result = classify_at(&format!("bitcoin:{MAINNET_BECH32}"), now());
        assert!(matches!(result.effective(), Classified::Onchain { .. }));
    }

    #[test]
    fn bip321_with_invalid_lno_is_the_offer_error_not_a_fallback() {
        // payment-input.ts:229-232 — an lno key means BOLT12, even when the
        // offer is garbage (the PWA surfaces the offer error).
        let bolt11 = valid_bolt11();
        let result = classify_at(
            &format!("bitcoin:{MAINNET_BECH32}?lightning={bolt11}&lno=lno1garbage"),
            now(),
        );
        assert_eq!(invalid_reason(result.effective()), ERR_INVALID_OFFER);
    }

    #[test]
    fn bip321_empty_uri_and_missing_method_are_distinct_errors() {
        // payment-input.ts:200-202, 239-241
        assert_eq!(
            invalid_reason(&classify_at("bitcoin:", now())),
            ERR_EMPTY_URI
        );
        assert_eq!(
            invalid_reason(&classify_at("bitcoin:?", now())),
            ERR_EMPTY_URI
        );
        assert_eq!(
            invalid_reason(&classify_at("bitcoin:?somekey=1", now())),
            ERR_URI_NO_METHOD
        );
    }

    #[test]
    fn bip321_uppercase_scheme_and_amount_parsing() {
        // payment-input.ts:58-61 — scheme match is on the lowercased input.
        let result = classify_at(&format!("BITCOIN:{MAINNET_BECH32}?amount=1"), now());
        assert_eq!(
            result.effective(),
            &Classified::Onchain {
                address: MAINNET_BECH32.to_string(),
                amount_sats: Some(100_000_000)
            }
        );
    }

    #[test]
    fn bip321_amount_parsing_is_fixed_point_btc() {
        // payment-input.ts:261-269 — fixed-point, truncated past 8 decimals,
        // invalid amounts silently ignored.
        let case = |amount: &str, expected: Option<u64>| {
            let result = classify_at(&format!("bitcoin:{MAINNET_BECH32}?amount={amount}"), now());
            match result.effective() {
                Classified::Onchain { amount_sats, .. } => assert_eq!(
                    *amount_sats, expected,
                    "amount {amount} should parse to {expected:?}"
                ),
                other => panic!("expected onchain, got {other:?}"),
            }
        };
        case("0.001", Some(100_000));
        case("1", Some(100_000_000));
        case("0.00000001", Some(1));
        case("0.000000019", Some(1)); // truncated past 8 decimals
        case("21000000", Some(2_100_000_000_000_000));
        case("1e3", None); // not ^\d+(\.\d+)?$
        case("-1", None);
        case(".5", None);
        case("1.", None);
        case("abc", None);
    }

    #[test]
    fn bip321_query_after_second_question_mark_is_dropped() {
        // payment-input.ts:197 — split('?', 2) keeps only the first segment.
        let bolt11 = valid_bolt11();
        let result = classify_at(
            &format!("bitcoin:{MAINNET_BECH32}?amount=0.001?lightning={bolt11}"),
            now(),
        );
        // The lightning key is inside the dropped third segment: on-chain wins.
        assert!(matches!(result.effective(), Classified::Onchain { .. }));
    }

    // ---- lightning: strip-and-recurse (payment-input.ts:66-68) ----

    #[test]
    fn lightning_scheme_is_stripped_and_reclassified() {
        let bolt11 = valid_bolt11();
        let result = classify_at(&format!("lightning:{bolt11}"), now());
        assert!(matches!(result, Classified::Bolt11 { .. }), "{result:?}");
        // Uppercase QR form of the scheme too.
        let result = classify_at(&format!("LIGHTNING:{bolt11}"), now());
        assert!(matches!(result, Classified::Bolt11 { .. }), "{result:?}");
        // lightning: wrapping an offer works as well.
        let result = classify_at(&format!("lightning:{}", offer_with_amount(1_000)), now());
        assert!(matches!(result, Classified::Bolt12 { .. }), "{result:?}");
    }

    // ---- BOLT11 (payment-input.ts:70-73, 95-127) ----

    #[test]
    fn valid_mainnet_bolt11_classifies_with_amount_and_description() {
        let bolt11 = valid_bolt11();
        assert_eq!(
            classify_at(&bolt11, now()),
            Classified::Bolt11 {
                raw: bolt11,
                amount_msat: Some(50_000_000),
                description: Some("Test invoice".to_string()),
                // The test invoice's payment hash, exposed for the shells'
                // event matching (U6/FFI `ClassifiedView.payment_hash`).
                payment_hash: "11".repeat(32),
            }
        );
    }

    #[test]
    fn amountless_bolt11_classifies_with_no_amount() {
        let bolt11 = amountless_bolt11();
        match classify_at(&bolt11, now()) {
            Classified::Bolt11 { amount_msat, .. } => assert_eq!(amount_msat, None),
            other => panic!("expected Bolt11, got {other:?}"),
        }
    }

    #[test]
    fn uppercase_bolt11_qr_form_is_accepted() {
        // The regex is case-insensitive (payment-input.ts:71) and bech32
        // permits the all-uppercase QR form.
        let upper = valid_bolt11().to_uppercase();
        assert!(matches!(
            classify_at(&upper, now()),
            Classified::Bolt11 { .. }
        ));
    }

    #[test]
    fn garbage_with_bolt11_prefix_is_an_invalid_invoice() {
        // payment-input.ts:96-99
        assert_eq!(
            invalid_reason(&classify_at("lnbc1garbagegarbage", now())),
            ERR_INVALID_INVOICE
        );
    }

    #[test]
    fn testnet_and_signet_bolt11_are_network_mismatches() {
        // payment-input.ts:102-105 — the broad ^ln(bc|tb|tbs|bcrt) match
        // routes non-mainnet invoices into the network check.
        for currency in [
            Currency::BitcoinTestnet,
            Currency::Signet,
            Currency::Regtest,
        ] {
            let bolt11 =
                test_invoice(currency.clone(), Some(50_000_000), NOW, 3_600, "invoice").to_string();
            assert_eq!(
                invalid_reason(&classify_at(&bolt11, now())),
                ERR_INVOICE_NETWORK,
                "{currency:?}"
            );
        }
    }

    #[test]
    fn expired_bolt11_is_rejected_at_classify_time() {
        // payment-input.ts:107-110
        let bolt11 = test_invoice(Currency::Bitcoin, Some(1_000), NOW, 60, "x").to_string();
        assert_eq!(
            invalid_reason(&classify_at(&bolt11, Duration::from_secs(NOW + 61))),
            ERR_INVOICE_EXPIRED
        );
    }

    #[test]
    fn bolt11_with_description_hash_has_no_description_text() {
        let invoice = hash_invoice(1_000, NOW, sha256_mod::Hash::hash(b"metadata"));
        match classify_at(&invoice.to_string(), now()) {
            Classified::Bolt11 { description, .. } => assert_eq!(description, None),
            other => panic!("expected Bolt11, got {other:?}"),
        }
    }

    // ---- BOLT12 (payment-input.ts:75-78, 129-180) ----

    #[test]
    fn valid_mainnet_offer_classifies_with_amount_and_description() {
        let offer = offer_with_amount(25_000);
        assert_eq!(
            classify_at(&offer, now()),
            Classified::Bolt12 {
                raw: offer,
                amount_msat: Some(25_000),
                description: Some("test offer".to_string()),
            }
        );
    }

    #[test]
    fn amountless_offer_classifies_with_no_amount() {
        let offer = amountless_offer();
        match classify_at(&offer, now()) {
            Classified::Bolt12 { amount_msat, .. } => assert_eq!(amount_msat, None),
            other => panic!("expected Bolt12, got {other:?}"),
        }
    }

    #[test]
    fn garbage_with_offer_prefix_is_an_invalid_offer() {
        // payment-input.ts:130-133
        assert_eq!(
            invalid_reason(&classify_at("lno1garbage", now())),
            ERR_INVALID_OFFER
        );
    }

    #[test]
    fn testnet_offer_is_a_network_mismatch() {
        // payment-input.ts:136-153
        assert_eq!(
            invalid_reason(&classify_at(&testnet_offer(), now())),
            ERR_OFFER_NETWORK
        );
    }

    #[test]
    fn offer_with_no_chains_field_is_implicit_mainnet() {
        // payment-input.ts:154 — empty chains = implicit mainnet, valid.
        // OfferBuilder without .chain() writes no chains field.
        let offer = offer_with_amount(1_000);
        let parsed = Offer::from_str(&offer).unwrap();
        assert!(
            parsed.supports_chain(ChainHash::using_genesis_block(Network::Bitcoin)),
            "fixture must carry no explicit chain"
        );
        assert!(matches!(
            classify_at(&offer, now()),
            Classified::Bolt12 { .. }
        ));
    }

    #[test]
    fn expired_offer_is_rejected_at_classify_time() {
        // payment-input.ts:156-159
        assert_eq!(
            invalid_reason(&classify_at(&expired_offer(), now())),
            ERR_OFFER_EXPIRED
        );
    }

    // ---- BIP353 (payment-input.test.ts:181-223) ----

    #[test]
    fn parses_user_at_domain_as_bip353() {
        // payment-input.test.ts:182-189
        assert_eq!(
            classify_at("alice@example.com", now()),
            Classified::Bip353 {
                user: "alice".to_string(),
                domain: "example.com".to_string(),
                raw: "alice@example.com".to_string(),
            }
        );
    }

    #[test]
    fn strips_btc_symbol_prefix_from_bip353_address() {
        // payment-input.test.ts:191-198
        assert_eq!(
            classify_at("\u{20bf}alice@example.com", now()),
            Classified::Bip353 {
                user: "alice".to_string(),
                domain: "example.com".to_string(),
                raw: "alice@example.com".to_string(),
            }
        );
    }

    #[test]
    fn rejects_plain_text_that_is_not_user_at_domain() {
        // payment-input.test.ts:200-204
        assert_eq!(
            invalid_reason(&classify_at("just-some-text", now())),
            ERR_UNRECOGNIZED
        );
    }

    #[test]
    fn handles_dots_and_hyphens_in_bip353_user_part() {
        // payment-input.test.ts:212-216
        assert!(matches!(
            classify_at("my.name-test@example.com", now()),
            Classified::Bip353 { .. }
        ));
    }

    #[test]
    fn handles_subdomains_in_bip353_domain_part() {
        // payment-input.test.ts:218-222
        assert_eq!(
            classify_at("alice@pay.example.co.uk", now()),
            Classified::Bip353 {
                user: "alice".to_string(),
                domain: "pay.example.co.uk".to_string(),
                raw: "alice@pay.example.co.uk".to_string(),
            }
        );
    }

    #[test]
    fn bip353_regex_gate_rejects_near_misses() {
        // The regex requires a dotted domain with a 2+ letter TLD and
        // restricted character classes (payment-input.ts:81).
        for input in [
            "alice@example",     // no TLD dot
            "alice@example.c",   // 1-letter TLD
            "alice@example.co1", // digit in TLD
            "@example.com",      // empty user
            "alice@@example.com",
            "al ice@example.com",
            "alice@exa_mple.com", // underscore not allowed in domain
        ] {
            assert_eq!(
                invalid_reason(&classify_at(input, now())),
                ERR_UNRECOGNIZED,
                "{input}"
            );
        }
        // Uppercase passes the case-insensitive gate.
        assert!(matches!(
            classify_at("Alice@Example.COM", now()),
            Classified::Bip353 { .. }
        ));
    }

    // ---- input handling ----

    #[test]
    fn input_is_trimmed_before_classification() {
        // payment-input.ts:57
        let bolt11 = valid_bolt11();
        assert!(matches!(
            classify_at(&format!("  {bolt11}\n"), now()),
            Classified::Bolt11 { .. }
        ));
    }

    #[test]
    fn unrecognized_input_yields_the_pwa_error_string() {
        // payment-input.ts:92
        assert_eq!(
            invalid_reason(&classify_at("hello world", now())),
            ERR_UNRECOGNIZED
        );
        assert_eq!(invalid_reason(&classify_at("", now())), ERR_UNRECOGNIZED);
    }

    #[test]
    fn over_length_input_is_rejected_with_the_pwa_cap_message() {
        // Send.tsx:613-615 — 2,000-char cap.
        let long = "a".repeat(2001);
        assert_eq!(
            invalid_reason(&classify_at(&long, now())),
            ERR_INPUT_TOO_LONG
        );
        // Exactly at the cap still classifies (as unrecognized here).
        let at_cap = "a".repeat(2000);
        assert_eq!(
            invalid_reason(&classify_at(&at_cap, now())),
            ERR_UNRECOGNIZED
        );
    }

    // =====================================================================
    // LNURL init validation (resolve-lnurl.test.ts, case for case)
    // =====================================================================

    fn init_body(overrides: &[(&str, serde_json::Value)]) -> String {
        let mut v = serde_json::json!({
            "tag": "payRequest",
            "callback": "https://example.com/lnurlp/alice/callback",
            "minSendable": 1000,
            "maxSendable": 100000000,
            "metadata": "[[\"text/plain\",\"Pay alice\"],[\"text/identifier\",\"alice@example.com\"]]",
        });
        for (key, value) in overrides {
            if value.is_null() {
                v.as_object_mut().unwrap().remove(*key);
            } else {
                v[*key] = value.clone();
            }
        }
        v.to_string()
    }

    #[test]
    fn resolves_a_valid_lnurl_pay_response() {
        // resolve-lnurl.test.ts:27-44
        let init = validate_lnurl_init(&init_body(&[])).unwrap().unwrap();
        assert_eq!(init.callback, "https://example.com/lnurlp/alice/callback");
        assert_eq!(init.min_sendable_msat, 1_000);
        assert_eq!(init.max_sendable_msat, 100_000_000);
        assert_eq!(init.description.as_deref(), Some("Pay alice"));
        let expected = sha256::Hash::hash(
            "[[\"text/plain\",\"Pay alice\"],[\"text/identifier\",\"alice@example.com\"]]"
                .as_bytes(),
        )
        .to_byte_array();
        assert_eq!(init.expected_description_hash, Some(expected));
    }

    #[test]
    fn lnurl_error_response_surfaces_the_server_reason() {
        // resolve-lnurl.test.ts:60-64
        let err = validate_lnurl_init(&init_body(&[
            ("status", serde_json::json!("ERROR")),
            ("reason", serde_json::json!("User not found")),
        ]))
        .unwrap_err();
        assert_eq!(
            err,
            ResolveError::ServerError {
                reason: "User not found".to_string()
            }
        );
        // Default reason when the server sends none (resolve-lnurl.ts:63).
        let err =
            validate_lnurl_init(&init_body(&[("status", serde_json::json!("ERROR"))])).unwrap_err();
        assert_eq!(err.to_string(), "Lightning Address returned an error");
    }

    #[test]
    fn wrong_tag_is_a_miss_not_an_error() {
        // resolve-lnurl.test.ts:66-71
        let result =
            validate_lnurl_init(&init_body(&[("tag", serde_json::json!("withdrawRequest"))]));
        assert_eq!(result, Ok(None));
    }

    #[test]
    fn missing_required_fields_are_a_miss() {
        // resolve-lnurl.test.ts:73-78 (callback removed) + JS falsy zero.
        assert_eq!(
            validate_lnurl_init(&init_body(&[("callback", serde_json::Value::Null)])),
            Ok(None)
        );
        assert_eq!(
            validate_lnurl_init(&init_body(&[("minSendable", serde_json::json!(0))])),
            Ok(None)
        );
        assert_eq!(
            validate_lnurl_init(&init_body(&[("maxSendable", serde_json::Value::Null)])),
            Ok(None)
        );
    }

    #[test]
    fn invalid_json_is_a_miss() {
        // resolve-lnurl.test.ts:104-112
        assert_eq!(validate_lnurl_init("not json {"), Ok(None));
    }

    #[test]
    fn invalid_metadata_falls_back_to_no_description() {
        // resolve-lnurl.test.ts:114-120 — description falls back (to
        // user@domain, applied in resolve()); the response still resolves.
        let init = validate_lnurl_init(&init_body(&[("metadata", serde_json::json!("invalid"))]))
            .unwrap()
            .unwrap();
        assert_eq!(init.description, None);
        // The hash still commits to the raw metadata string as served.
        assert_eq!(
            init.expected_description_hash,
            Some(sha256::Hash::hash(b"invalid").to_byte_array())
        );
    }

    #[test]
    fn metadata_without_text_plain_has_no_description() {
        // resolve-lnurl.test.ts:122-130
        let init = validate_lnurl_init(&init_body(&[(
            "metadata",
            serde_json::json!("[[\"text/identifier\",\"alice@example.com\"]]"),
        )]))
        .unwrap()
        .unwrap();
        assert_eq!(init.description, None);
    }

    #[test]
    fn first_text_plain_entry_wins() {
        // resolve-lnurl.ts:92 uses .find() — the FIRST text/plain entry.
        let init = validate_lnurl_init(&init_body(&[(
            "metadata",
            serde_json::json!("[[\"text/plain\",\"first\"],[\"text/plain\",\"second\"]]"),
        )]))
        .unwrap()
        .unwrap();
        assert_eq!(init.description.as_deref(), Some("first"));
    }

    // =====================================================================
    // Resolution flow (BIP353 → LNURL fallback) via the stub resolver seam
    // =====================================================================

    /// Scripted resolver: each call pops the next canned answer.
    #[derive(Default)]
    struct StubResolver {
        bip353: Mutex<Vec<Bip353Outcome>>,
        init: Mutex<Vec<Result<Option<LnurlInit>, ResolveError>>>,
        callback: Mutex<Vec<Result<String, ResolveError>>>,
        bip353_delay: Option<Duration>,
        init_delay: Option<Duration>,
        calls: Mutex<Vec<String>>,
    }

    impl StubResolver {
        fn with_bip353(outcome: Bip353Outcome) -> Self {
            let stub = Self::default();
            stub.bip353.lock().unwrap().push(outcome);
            stub
        }

        fn push_init(self, init: Result<Option<LnurlInit>, ResolveError>) -> Self {
            self.init.lock().unwrap().push(init);
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl NameResolver for StubResolver {
        async fn resolve_bip353(&self, user: &str, domain: &str) -> Bip353Outcome {
            self.calls
                .lock()
                .unwrap()
                .push(format!("bip353:{user}@{domain}"));
            if let Some(delay) = self.bip353_delay {
                tokio::time::sleep(delay).await;
            }
            self.bip353
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(Bip353Outcome::Miss)
        }

        async fn lnurl_init(
            &self,
            user: &str,
            domain: &str,
        ) -> Result<Option<LnurlInit>, ResolveError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("lnurl:{user}@{domain}"));
            if let Some(delay) = self.init_delay {
                tokio::time::sleep(delay).await;
            }
            self.init.lock().unwrap().pop().unwrap_or(Ok(None))
        }

        async fn lnurl_callback(&self, url: &str) -> Result<String, ResolveError> {
            self.calls.lock().unwrap().push(format!("callback:{url}"));
            self.callback
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(Err(ResolveError::InvoiceFetchFailed))
        }
    }

    fn bip353_classified() -> Classified {
        Classified::Bip353 {
            user: "alice".to_string(),
            domain: "example.com".to_string(),
            raw: "alice@example.com".to_string(),
        }
    }

    fn valid_init() -> LnurlInit {
        LnurlInit {
            callback: "https://example.com/lnurlp/alice/callback".to_string(),
            min_sendable_msat: 1_000,
            max_sendable_msat: 100_000_000,
            description: Some("Pay alice".to_string()),
            expected_description_hash: Some([0x42; 32]),
        }
    }

    #[tokio::test]
    async fn bip353_hit_returns_the_classified_txt_uri() {
        // Send.tsx:296-306 — a BIP353 hit routes without touching LNURL.
        let stub = StubResolver::with_bip353(Bip353Outcome::Bip353(format!(
            "bitcoin:{MAINNET_BECH32}?amount=0.001"
        )));
        let resolved = resolve(bip353_classified(), &stub, now()).await.unwrap();
        assert_eq!(
            resolved.effective(),
            &Classified::Onchain {
                address: MAINNET_BECH32.to_string(),
                amount_sats: Some(100_000)
            }
        );
        assert_eq!(stub.calls(), vec!["bip353:alice@example.com".to_string()]);
    }

    #[tokio::test]
    async fn bip353_txt_with_offer_resolves_to_bolt12() {
        let offer = offer_with_amount(25_000);
        let stub =
            StubResolver::with_bip353(Bip353Outcome::Bip353(format!("bitcoin:?lno={offer}")));
        let resolved = resolve(bip353_classified(), &stub, now()).await.unwrap();
        assert!(matches!(resolved.effective(), Classified::Bolt12 { .. }));
    }

    #[tokio::test]
    async fn bip353_miss_falls_back_to_lnurl() {
        // Send.tsx:308-315
        let stub = StubResolver::with_bip353(Bip353Outcome::Miss).push_init(Ok(Some(valid_init())));
        let resolved = resolve(bip353_classified(), &stub, now()).await.unwrap();
        match resolved {
            Classified::Lnurl { metadata, raw } => {
                assert_eq!(raw, "alice@example.com");
                assert_eq!(metadata.user, "alice");
                assert_eq!(metadata.domain, "example.com");
                assert_eq!(metadata.description, "Pay alice");
                assert_eq!(metadata.min_sats(), 1);
                assert_eq!(metadata.max_sats(), 100_000);
                assert!(!metadata.skip_amount_entry());
            }
            other => panic!("expected Lnurl, got {other:?}"),
        }
        assert_eq!(
            stub.calls(),
            vec![
                "bip353:alice@example.com".to_string(),
                "lnurl:alice@example.com".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn bip353_txt_that_fails_classification_falls_back_to_lnurl() {
        // resolve-bip353.ts:59-63 — a TXT record that classifies to error is
        // a miss.
        let stub =
            StubResolver::with_bip353(Bip353Outcome::Bip353(format!("bitcoin:{SIGNET_BECH32}")))
                .push_init(Ok(Some(valid_init())));
        let resolved = resolve(bip353_classified(), &stub, now()).await.unwrap();
        assert!(matches!(resolved, Classified::Lnurl { .. }));
    }

    #[tokio::test]
    async fn both_misses_yield_the_pwa_not_found_message() {
        // Send.tsx:339
        let stub = StubResolver::with_bip353(Bip353Outcome::Miss);
        let err = resolve(bip353_classified(), &stub, now())
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "No Lightning Address or BIP 353 record found for alice@example.com"
        );
    }

    #[tokio::test]
    async fn bip353_timeout_is_a_miss_and_lnurl_gets_a_fresh_budget() {
        // Send.tsx:297-302 — each step has its own timeout budget.
        let mut stub =
            StubResolver::with_bip353(Bip353Outcome::Bip353(format!("bitcoin:{MAINNET_BECH32}")))
                .push_init(Ok(Some(valid_init())));
        stub.bip353_delay = Some(Duration::from_millis(200));
        let resolved =
            resolve_with_budget(bip353_classified(), &stub, now(), Duration::from_millis(50))
                .await
                .unwrap();
        // The slow BIP353 answer was discarded; LNURL won.
        assert!(matches!(resolved, Classified::Lnurl { .. }));
    }

    #[tokio::test]
    async fn lnurl_timeout_is_the_not_found_error() {
        let mut stub =
            StubResolver::with_bip353(Bip353Outcome::Miss).push_init(Ok(Some(valid_init())));
        stub.init_delay = Some(Duration::from_millis(200));
        let err = resolve_with_budget(bip353_classified(), &stub, now(), Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(matches!(err, ResolveError::NotFound { .. }));
    }

    #[tokio::test]
    async fn lnurl_validation_errors_surface_instead_of_not_found() {
        // resolve-lnurl.ts:62-64 — server ERROR reasons propagate.
        let stub = StubResolver::with_bip353(Bip353Outcome::Miss).push_init(Err(
            ResolveError::ServerError {
                reason: "User not found".to_string(),
            },
        ));
        let err = resolve(bip353_classified(), &stub, now())
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "User not found");
    }

    #[tokio::test]
    async fn callback_that_is_not_https_is_rejected() {
        // resolve-lnurl.test.ts:80-84
        let mut init = valid_init();
        init.callback = "http://example.com/callback".to_string();
        let stub = StubResolver::with_bip353(Bip353Outcome::Miss).push_init(Ok(Some(init)));
        let err = resolve(bip353_classified(), &stub, now())
            .await
            .unwrap_err();
        assert_eq!(err, ResolveError::CallbackNotHttps);
        assert!(err.to_string().contains("not HTTPS"));
    }

    #[tokio::test]
    async fn callback_domain_mismatch_is_rejected() {
        // resolve-lnurl.test.ts:86-92
        let mut init = valid_init();
        init.callback = "https://evil.com/lnurlp/callback".to_string();
        let stub = StubResolver::with_bip353(Bip353Outcome::Miss).push_init(Ok(Some(init)));
        let err = resolve(bip353_classified(), &stub, now())
            .await
            .unwrap_err();
        assert_eq!(err, ResolveError::CallbackDomainMismatch);
        assert!(err.to_string().contains("domain mismatch"));
    }

    #[tokio::test]
    async fn callback_on_subdomain_of_original_domain_is_allowed() {
        // resolve-lnurl.test.ts:94-102
        let mut init = valid_init();
        init.callback = "https://api.example.com/lnurlp/callback".to_string();
        let stub = StubResolver::with_bip353(Bip353Outcome::Miss).push_init(Ok(Some(init)));
        let resolved = resolve(bip353_classified(), &stub, now()).await.unwrap();
        match resolved {
            Classified::Lnurl { metadata, .. } => {
                assert_eq!(metadata.callback, "https://api.example.com/lnurlp/callback");
            }
            other => panic!("expected Lnurl, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn suffix_lookalike_domain_is_still_a_mismatch() {
        // endsWith('.' + domain) — "evilexample.com" must not pass.
        let mut init = valid_init();
        init.callback = "https://evilexample.com/callback".to_string();
        let stub = StubResolver::with_bip353(Bip353Outcome::Miss).push_init(Ok(Some(init)));
        let err = resolve(bip353_classified(), &stub, now())
            .await
            .unwrap_err();
        assert_eq!(err, ResolveError::CallbackDomainMismatch);
    }

    #[tokio::test]
    async fn missing_description_falls_back_to_user_at_domain() {
        // resolve-lnurl.ts:92 — `${user}@${domain}` fallback.
        let mut init = valid_init();
        init.description = None;
        let stub = StubResolver::with_bip353(Bip353Outcome::Miss).push_init(Ok(Some(init)));
        let resolved = resolve(bip353_classified(), &stub, now()).await.unwrap();
        match resolved {
            Classified::Lnurl { metadata, .. } => {
                assert_eq!(metadata.description, "alice@example.com");
            }
            other => panic!("expected Lnurl, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn crate_side_lnurl_fallback_is_used_without_a_second_fetch() {
        // HTTPHrnResolver::resolve_hrn falls through to LNURL itself when
        // the DoH transport fails; that result is used directly (with the
        // callback binding still applied).
        let stub = StubResolver::with_bip353(Bip353Outcome::Lnurl(valid_init()));
        let resolved = resolve(bip353_classified(), &stub, now()).await.unwrap();
        assert!(matches!(resolved, Classified::Lnurl { .. }));
        assert_eq!(stub.calls(), vec!["bip353:alice@example.com".to_string()]);
    }

    #[tokio::test]
    async fn min_equals_max_sets_the_skip_amount_flag() {
        // Send.tsx:320-327 — ceil(min) == floor(max) skips the numpad.
        let mut init = valid_init();
        init.min_sendable_msat = 50_000;
        init.max_sendable_msat = 50_000;
        let stub = StubResolver::with_bip353(Bip353Outcome::Miss).push_init(Ok(Some(init)));
        let resolved = resolve(bip353_classified(), &stub, now()).await.unwrap();
        match resolved {
            Classified::Lnurl { metadata, .. } => {
                assert_eq!(metadata.min_sats(), 50);
                assert_eq!(metadata.max_sats(), 50);
                assert!(metadata.skip_amount_entry());
            }
            other => panic!("expected Lnurl, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rounding_is_ceil_for_min_and_floor_for_max() {
        // Send.tsx:318-321 — never send less than the server minimum, never
        // exceed the maximum.
        let mut init = valid_init();
        init.min_sendable_msat = 1_001; // → 2 sats (ceil)
        init.max_sendable_msat = 2_999; // → 2 sats (floor)
        let stub = StubResolver::with_bip353(Bip353Outcome::Miss).push_init(Ok(Some(init)));
        let resolved = resolve(bip353_classified(), &stub, now()).await.unwrap();
        match resolved {
            Classified::Lnurl { metadata, .. } => {
                assert_eq!(metadata.min_sats(), 2);
                assert_eq!(metadata.max_sats(), 2);
                assert!(metadata.skip_amount_entry());
            }
            other => panic!("expected Lnurl, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_bip353_classifications_pass_through_resolve_unchanged() {
        let stub = StubResolver::default();
        let bolt11 = classify_at(&valid_bolt11(), now());
        assert_eq!(resolve(bolt11.clone(), &stub, now()).await.unwrap(), bolt11);
        assert!(stub.calls().is_empty());
    }

    // =====================================================================
    // LNURL invoice fetch + validation (fetchLnurlInvoice + KTD-6)
    // =====================================================================

    fn metadata_for_callback(callback: &str, expected_hash: Option<[u8; 32]>) -> LnurlPayMetadata {
        LnurlPayMetadata {
            domain: "example.com".to_string(),
            user: "alice".to_string(),
            callback: callback.to_string(),
            min_sendable_msat: 1_000,
            max_sendable_msat: 100_000_000,
            description: "Pay alice".to_string(),
            expected_description_hash: expected_hash,
        }
    }

    #[tokio::test]
    async fn fetches_and_validates_an_invoice_from_the_callback() {
        let metadata_str = "[[\"text/plain\",\"Pay alice\"]]";
        let hash = sha256_mod::Hash::hash(metadata_str.as_bytes());
        let invoice = hash_invoice(50_000, NOW, hash);
        let stub = StubResolver::default();
        stub.callback
            .lock()
            .unwrap()
            .push(Ok(format!("{{\"pr\":\"{invoice}\",\"routes\":[]}}")));
        let metadata = metadata_for_callback("https://example.com/cb", Some(hash.to_byte_array()));
        let classified = fetch_lnurl_invoice(&stub, &metadata, 50_000, now())
            .await
            .unwrap();
        match classified {
            Classified::Bolt11 { amount_msat, .. } => assert_eq!(amount_msat, Some(50_000)),
            other => panic!("expected Bolt11, got {other:?}"),
        }
        // resolve-lnurl.ts:116-117 — amount appended with ? (no existing
        // query params).
        assert_eq!(
            stub.calls(),
            vec!["callback:https://example.com/cb?amount=50000".to_string()]
        );
    }

    #[tokio::test]
    async fn amount_is_appended_with_ampersand_when_callback_has_query() {
        // resolve-lnurl.test.ts:162-174
        let stub = StubResolver::default();
        stub.callback
            .lock()
            .unwrap()
            .push(Err(ResolveError::InvoiceFetchFailed));
        let metadata = metadata_for_callback("https://example.com/cb?key=val", None);
        let _ = fetch_lnurl_invoice(&stub, &metadata, 50_000, now()).await;
        assert_eq!(
            stub.calls(),
            vec!["callback:https://example.com/cb?key=val&amount=50000".to_string()]
        );
    }

    #[test]
    fn callback_error_response_surfaces_the_server_reason() {
        // resolve-lnurl.test.ts:184-193
        let err = validate_lnurl_invoice(
            "{\"status\":\"ERROR\",\"reason\":\"Amount too low\"}",
            50_000,
            None,
            now(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            ResolveError::ServerError {
                reason: "Amount too low".to_string()
            }
        );
    }

    #[test]
    fn missing_pr_field_is_no_invoice_in_response() {
        // resolve-lnurl.test.ts:195-204
        let err = validate_lnurl_invoice("{\"routes\":[]}", 50_000, None, now()).unwrap_err();
        assert_eq!(err, ResolveError::NoInvoice);
        assert_eq!(err.to_string(), "No invoice in response");
    }

    #[test]
    fn unparseable_pr_is_an_invalid_provider_invoice() {
        // Send.tsx:272
        let err =
            validate_lnurl_invoice("{\"pr\":\"lnbc1junk\"}", 50_000, None, now()).unwrap_err();
        assert_eq!(err, ResolveError::InvalidProviderInvoice);
        assert_eq!(
            err.to_string(),
            "Invalid invoice from Lightning Address provider"
        );
    }

    #[test]
    fn amount_mismatch_is_rejected() {
        // Send.tsx:259-262 + KTD-6 amount-match enforcement.
        let hash = sha256_mod::Hash::hash(b"[[\"text/plain\",\"Pay alice\"]]");
        let invoice = hash_invoice(60_000, NOW, hash);
        let err = validate_lnurl_invoice(&format!("{{\"pr\":\"{invoice}\"}}"), 50_000, None, now())
            .unwrap_err();
        assert_eq!(err, ResolveError::InvoiceAmountMismatch);
        assert_eq!(
            err.to_string(),
            "Invoice amount does not match requested amount"
        );
    }

    #[test]
    fn amountless_invoice_from_provider_is_an_amount_mismatch() {
        // KTD-6: the invoice must commit to the requested amount exactly
        // (stricter than the PWA, which back-fills the requested amount).
        let invoice = amountless_bolt11();
        let err = validate_lnurl_invoice(&format!("{{\"pr\":\"{invoice}\"}}"), 50_000, None, now())
            .unwrap_err();
        assert_eq!(err, ResolveError::InvoiceAmountMismatch);
    }

    #[test]
    fn description_hash_mismatch_is_rejected() {
        // KTD-6 metadata commitment.
        let served_hash = sha256_mod::Hash::hash(b"different metadata");
        let invoice = hash_invoice(50_000, NOW, served_hash);
        let expected = sha256_mod::Hash::hash(b"[[\"text/plain\",\"Pay alice\"]]").to_byte_array();
        let err = validate_lnurl_invoice(
            &format!("{{\"pr\":\"{invoice}\"}}"),
            50_000,
            Some(&expected),
            now(),
        )
        .unwrap_err();
        assert_eq!(err, ResolveError::DescriptionHashMismatch);
    }

    #[test]
    fn direct_description_invoice_fails_hash_verification() {
        // KTD-6: with metadata served, the invoice MUST use description_hash.
        let invoice = valid_bolt11();
        let expected = sha256_mod::Hash::hash(b"m").to_byte_array();
        let err = validate_lnurl_invoice(
            &format!("{{\"pr\":\"{invoice}\"}}"),
            50_000_000,
            Some(&expected),
            now(),
        )
        .unwrap_err();
        assert_eq!(err, ResolveError::DescriptionHashMismatch);
    }

    #[test]
    fn expired_invoice_from_provider_is_invalid() {
        // The fetched invoice is re-classified (Send.tsx:257) — expiry and
        // network checks apply.
        let invoice = test_invoice(Currency::Bitcoin, Some(50_000), NOW, 60, "x").to_string();
        let err = validate_lnurl_invoice(
            &format!("{{\"pr\":\"{invoice}\"}}"),
            50_000,
            None,
            Duration::from_secs(NOW + 61),
        )
        .unwrap_err();
        assert_eq!(err, ResolveError::InvalidProviderInvoice);
    }

    #[tokio::test]
    async fn amount_outside_the_lnurl_window_is_rejected_before_fetching() {
        let stub = StubResolver::default();
        let metadata = metadata_for_callback("https://example.com/cb", None);
        let err = fetch_lnurl_invoice(&stub, &metadata, 999, now())
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ResolveError::AmountOutOfBounds {
                min_msat: 1_000,
                max_msat: 100_000_000
            }
        );
        let err = fetch_lnurl_invoice(&stub, &metadata, 100_000_001, now())
            .await
            .unwrap_err();
        assert!(matches!(err, ResolveError::AmountOutOfBounds { .. }));
        assert!(stub.calls().is_empty(), "nothing may be fetched");
    }

    // =====================================================================
    // Local HTTP stub: the reqwest transport against a real socket
    // =====================================================================

    /// One-shot HTTP server answering with a canned response body.
    fn spawn_http_stub(
        body: &'static str,
    ) -> (std::net::SocketAddr, std::thread::JoinHandle<String>) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
            request
        });
        (addr, handle)
    }

    #[tokio::test]
    async fn lnurl_init_flow_works_end_to_end_against_a_local_http_stub() {
        static BODY: &str = "{\"tag\":\"payRequest\",\"callback\":\"https://127.0.0.1:9/cb\",\"minSendable\":1000,\"maxSendable\":2000,\"metadata\":\"[[\\\"text/plain\\\",\\\"hi\\\"]]\"}";
        let (addr, handle) = spawn_http_stub(BODY);
        let resolver = HttpNameResolver::with_plain_http_lnurl();
        let domain = format!("{addr}");
        let init = resolver
            .lnurl_init("alice", &domain)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(init.min_sendable_msat, 1_000);
        assert_eq!(init.max_sendable_msat, 2_000);
        assert_eq!(init.description.as_deref(), Some("hi"));
        let request = handle.join().unwrap();
        assert!(
            request.starts_with("GET /.well-known/lnurlp/alice HTTP/1.1"),
            "LUD-16 path, got: {request}"
        );
    }

    #[tokio::test]
    async fn lnurl_callback_transport_fetches_the_body() {
        static BODY: &str = "{\"pr\":\"lnbc1notparsedhere\"}";
        let (addr, handle) = spawn_http_stub(BODY);
        let resolver = HttpNameResolver::new();
        let body = resolver
            .lnurl_callback(&format!("http://{addr}/cb?amount=42"))
            .await
            .unwrap();
        assert_eq!(body, BODY);
        let request = handle.join().unwrap();
        assert!(
            request.starts_with("GET /cb?amount=42 HTTP/1.1"),
            "{request}"
        );
    }

    // =====================================================================
    // Live gate (#[ignore]d): a real Lightning Address resolves to a
    // payable LNURL endpoint. Run manually:
    //   cargo test --lib -- --ignored live_lightning_address_resolution
    // =====================================================================

    #[tokio::test]
    #[ignore = "live network: resolves a real Lightning Address over DoH + LNURL"]
    async fn live_lightning_address_resolution() {
        let resolver = HttpNameResolver::new();
        let classified = classify("lnurltest@bitcoin.ninja");
        assert!(matches!(classified, Classified::Bip353 { .. }));
        let resolved = resolve(classified, &resolver, unix_now())
            .await
            .expect("lnurltest@bitcoin.ninja must resolve");
        let Classified::Lnurl { metadata, .. } = resolved else {
            panic!("expected an LNURL resolution, got {resolved:?}");
        };
        assert!(metadata.min_sendable_msat >= 1);
        assert!(metadata.max_sendable_msat >= metadata.min_sendable_msat);
        // Fetch a real invoice at the minimum amount and validate it fully
        // (amount match + description_hash) — the plan's live gate.
        let amount = metadata.min_sendable_msat.max(1_000);
        let invoice = fetch_lnurl_invoice(&resolver, &metadata, amount, unix_now())
            .await
            .expect("callback must yield a valid invoice");
        match invoice {
            Classified::Bolt11 { amount_msat, .. } => assert_eq!(amount_msat, Some(amount)),
            other => panic!("expected Bolt11, got {other:?}"),
        }
    }
}
