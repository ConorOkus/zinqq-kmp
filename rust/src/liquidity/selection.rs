//! Client-side selection over an LSPS2 `lsps2.get_info` fee menu: pick the
//! cheapest non-expired opening-fee params the amount fits into, enforcing
//! the client-side fee floor, plus the typed [`Lsps2Error`] every LSPS2
//! failure in the flow renders through.

use std::fmt;
use std::time::Duration;

use lightning_liquidity::lsps0::ser::{LSPSDateTime, LSPSResponseError};
use lightning_liquidity::lsps2::msgs::LSPS2OpeningFeeParams;
use lightning_liquidity::lsps2::utils::compute_opening_fee;

/// Typed LSPS2 failures. Every variant renders to a DISTINCT reason string
/// (see `Display`), which is what `Event::Lsps2Failed { reason }` carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lsps2Error {
    /// The node is not running.
    NotRunning,
    /// The node shut down while the request was in flight.
    Shutdown,
    /// The LSP peer connection could not be established.
    ConnectFailed,
    /// The LSP never answered within the request timeout. The `&'static str`
    /// names the phase (`"get_info"` / `"buy"`).
    RequestTimeout(&'static str),
    /// The LSP answered `lsps2.get_info` with an error.
    GetInfoFailed(String),
    /// The LSP answered `lsps2.buy` with an error.
    BuyFailed(String),
    /// The LSP returned an empty opening-fee-params menu.
    EmptyMenu,
    /// Every offered opening-fee-params entry was already expired.
    AllParamsExpired,
    /// The amount is below every offer's `min_payment_size_msat` (pre-empts
    /// LSP error 202).
    AmountBelowMinimum {
        amount_msat: u64,
        min_payment_size_msat: u64,
    },
    /// The amount is above every offer's `max_payment_size_msat` (pre-empts
    /// LSP error 203).
    AmountAboveMaximum {
        amount_msat: u64,
        max_payment_size_msat: u64,
    },
    /// The cheapest valid opening fee would consume the whole payment.
    OpeningFeeExceedsAmount {
        opening_fee_msat: u64,
        amount_msat: u64,
    },
    /// Registering or signing the invoice failed.
    InvoiceCreationFailed,
    /// `jit_accept` with a quote token that was never issued or was already
    /// consumed (U7: quotes are single-use; a buy commits the LSP).
    QuoteNotFound,
    /// `jit_accept` with an amount that differs from the quoted one (U7: the
    /// signed fee promise is bound to the quoted payment size).
    QuoteAmountMismatch {
        quoted_msat: u64,
        requested_msat: u64,
    },
    /// The quote's `valid_until` leaves less than 60 s of payable invoice
    /// life after the 30 s flight margin — the re-quote signal (U7, R6),
    /// raised BEFORE any `buy` so no LSP-side reservation is orphaned.
    QuoteExpired,
}

impl fmt::Display for Lsps2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Lsps2Error::NotRunning => write!(f, "the node is not running"),
            Lsps2Error::Shutdown => write!(f, "the node shut down during the LSPS2 request"),
            Lsps2Error::ConnectFailed => write!(f, "could not connect to the LSP peer"),
            Lsps2Error::RequestTimeout(phase) => {
                write!(f, "the LSP did not answer lsps2.{phase} in time")
            }
            Lsps2Error::GetInfoFailed(reason) => write!(f, "lsps2.get_info failed: {reason}"),
            Lsps2Error::BuyFailed(reason) => write!(f, "lsps2.buy failed: {reason}"),
            Lsps2Error::EmptyMenu => write!(f, "the LSP offered no opening fee params"),
            Lsps2Error::AllParamsExpired => {
                write!(f, "all LSP-offered opening fee params are expired")
            }
            Lsps2Error::AmountBelowMinimum {
                amount_msat,
                min_payment_size_msat,
            } => write!(
                f,
                "amount {amount_msat}msat is below the LSP minimum payment size of \
                 {min_payment_size_msat}msat"
            ),
            Lsps2Error::AmountAboveMaximum {
                amount_msat,
                max_payment_size_msat,
            } => write!(
                f,
                "amount {amount_msat}msat is above the LSP maximum payment size of \
                 {max_payment_size_msat}msat"
            ),
            Lsps2Error::OpeningFeeExceedsAmount {
                opening_fee_msat,
                amount_msat,
            } => write!(
                f,
                "the channel opening fee of {opening_fee_msat}msat would consume the whole \
                 {amount_msat}msat payment"
            ),
            Lsps2Error::InvoiceCreationFailed => write!(f, "failed to create the invoice"),
            Lsps2Error::QuoteNotFound => {
                write!(
                    f,
                    "the fee quote is no longer available, request a new quote"
                )
            }
            Lsps2Error::QuoteAmountMismatch {
                quoted_msat,
                requested_msat,
            } => write!(
                f,
                "the quote was for {quoted_msat}msat but {requested_msat}msat was requested"
            ),
            // The PWA's `JitQuoteFreshnessError` copy, verbatim
            // (`src/ldk/context.tsx` `computeJitInvoiceExpirySecs`).
            Lsps2Error::QuoteExpired => write!(f, "Fee quote expired, please try again"),
        }
    }
}

impl std::error::Error for Lsps2Error {}

/// Maps an LSPS error object to a human-readable reason. The codes are from
/// bLIP-52: 200 (get_info: unrecognized/stale token), 201 (buy: invalid
/// opening_fee_params, e.g. expired promise), 202 (payment size too small),
/// 203 (payment size too large).
pub(crate) fn describe_lsps_error(error: &LSPSResponseError) -> String {
    let detail = match error.code {
        200 => "unrecognized or stale token",
        201 => "invalid opening_fee_params (promise rejected or expired)",
        202 => "payment size too small for the LSP",
        203 => "payment size too large for the LSP",
        _ => "unexpected LSP error",
    };
    format!("LSP error {}: {} ({})", error.code, detail, error.message)
}

/// The seconds-since-epoch encoded in an [`LSPSDateTime`] (post-epoch by
/// construction — the parser rejects pre-epoch datetimes).
pub(crate) fn datetime_unix_secs(datetime: &LSPSDateTime) -> u64 {
    datetime
        .duration_since(&LSPSDateTime::new_from_duration_since_epoch(Duration::ZERO))
        .as_secs()
}

/// Picks the cheapest non-expired opening-fee params the amount fits into and
/// enforces the client-side fee floor (opening fee must be strictly less than
/// the amount), so doomed `buy` requests never leave the device.
///
/// Returns the computed opening fee alongside the chosen params.
pub(crate) fn select_cheapest_valid_params(
    menu: Vec<LSPS2OpeningFeeParams>,
    amount_msat: u64,
    now_unix_secs: u64,
) -> Result<(u64, LSPS2OpeningFeeParams), Lsps2Error> {
    if menu.is_empty() {
        return Err(Lsps2Error::EmptyMenu);
    }

    let mut all_expired = true;
    let mut tightest_min: Option<u64> = None;
    let mut widest_max: Option<u64> = None;

    let cheapest = menu
        .into_iter()
        .filter_map(|params| {
            if datetime_unix_secs(&params.valid_until) <= now_unix_secs {
                return None;
            }
            all_expired = false;
            tightest_min = Some(tightest_min.map_or(params.min_payment_size_msat, |m: u64| {
                m.min(params.min_payment_size_msat)
            }));
            widest_max = Some(widest_max.map_or(params.max_payment_size_msat, |m: u64| {
                m.max(params.max_payment_size_msat)
            }));
            if amount_msat < params.min_payment_size_msat
                || amount_msat > params.max_payment_size_msat
            {
                return None;
            }
            compute_opening_fee(amount_msat, params.min_fee_msat, params.proportional as u64)
                .map(|fee| (fee, params))
        })
        .min_by_key(|(fee, _)| *fee);

    let (opening_fee_msat, params) = match cheapest {
        Some(choice) => choice,
        None if all_expired => return Err(Lsps2Error::AllParamsExpired),
        None => {
            // Valid entries existed but the amount fit none of them; report
            // the closest bound for a precise reason.
            if let Some(min) = tightest_min.filter(|min| amount_msat < *min) {
                return Err(Lsps2Error::AmountBelowMinimum {
                    amount_msat,
                    min_payment_size_msat: min,
                });
            }
            if let Some(max) = widest_max.filter(|max| amount_msat > *max) {
                return Err(Lsps2Error::AmountAboveMaximum {
                    amount_msat,
                    max_payment_size_msat: max,
                });
            }
            return Err(Lsps2Error::EmptyMenu);
        }
    };

    // Client-side fee floor (pre-empts LSP error 202 and zero-value receives).
    if opening_fee_msat >= amount_msat {
        return Err(Lsps2Error::OpeningFeeExceedsAmount {
            opening_fee_msat,
            amount_msat,
        });
    }

    Ok((opening_fee_msat, params))
}

/// Fabricates opening-fee params for tests — shared with the flow tests in
/// the parent module.
#[cfg(test)]
pub(crate) fn params(
    min_fee_msat: u64,
    proportional: u32,
    valid_until_unix: u64,
    min_payment_size_msat: u64,
    max_payment_size_msat: u64,
) -> LSPS2OpeningFeeParams {
    LSPS2OpeningFeeParams {
        min_fee_msat,
        proportional,
        valid_until: LSPSDateTime::new_from_duration_since_epoch(Duration::from_secs(
            valid_until_unix,
        )),
        min_lifetime: 4032,
        max_client_to_self_delay: 2016,
        min_payment_size_msat,
        max_payment_size_msat,
        promise: "promise".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_753_000_000;
    const FUTURE: u64 = NOW + 3_600;
    const PAST: u64 = NOW - 1;

    // ---------- selection: cheapest / expiry / limits / fee floor ----------

    #[test]
    fn cheapest_params_win_across_a_multi_entry_menu() {
        let amount = 1_000_000; // 1000 sats
        let menu = vec![
            // fee = max(10_000, 1% of amount = 10_000) = 10_000
            params(10_000, 10_000, FUTURE, 1_000, 100_000_000_000),
            // fee = max(1_000, 0.5% = 5_000) = 5_000 <- cheapest
            params(1_000, 5_000, FUTURE, 1_000, 100_000_000_000),
            // fee = max(20_000, 0) = 20_000
            params(20_000, 0, FUTURE, 1_000, 100_000_000_000),
        ];
        let (fee, chosen) = select_cheapest_valid_params(menu, amount, NOW).unwrap();
        assert_eq!(fee, 5_000);
        assert_eq!(chosen.min_fee_msat, 1_000);
        assert_eq!(chosen.proportional, 5_000);
    }

    #[test]
    fn expired_params_are_skipped_even_when_cheaper() {
        let amount = 1_000_000;
        let menu = vec![
            params(1, 0, PAST, 1, u64::MAX), // cheapest but expired
            params(7_000, 0, FUTURE, 1, u64::MAX),
        ];
        let (fee, _) = select_cheapest_valid_params(menu, amount, NOW).unwrap();
        assert_eq!(fee, 7_000, "the expired cheaper entry must be skipped");
    }

    #[test]
    fn all_expired_menu_is_a_distinct_failure() {
        let menu = vec![
            params(1, 0, PAST, 1, u64::MAX),
            params(2, 0, PAST, 1, u64::MAX),
        ];
        assert_eq!(
            select_cheapest_valid_params(menu, 1_000_000, NOW).unwrap_err(),
            Lsps2Error::AllParamsExpired
        );
    }

    #[test]
    fn empty_menu_is_a_distinct_failure() {
        assert_eq!(
            select_cheapest_valid_params(Vec::new(), 1_000_000, NOW).unwrap_err(),
            Lsps2Error::EmptyMenu
        );
    }

    #[test]
    fn amount_below_min_payment_size_fails_fast() {
        let menu = vec![params(1_000, 0, FUTURE, 10_000_000, u64::MAX)];
        assert_eq!(
            select_cheapest_valid_params(menu, 1_000_000, NOW).unwrap_err(),
            Lsps2Error::AmountBelowMinimum {
                amount_msat: 1_000_000,
                min_payment_size_msat: 10_000_000,
            }
        );
    }

    #[test]
    fn amount_above_max_payment_size_fails_fast() {
        let menu = vec![params(1_000, 0, FUTURE, 1, 500_000)];
        assert_eq!(
            select_cheapest_valid_params(menu, 1_000_000, NOW).unwrap_err(),
            Lsps2Error::AmountAboveMaximum {
                amount_msat: 1_000_000,
                max_payment_size_msat: 500_000,
            }
        );
    }

    #[test]
    fn opening_fee_swallowing_the_amount_fails_fast() {
        // min_fee 2_000_000 >= amount 1_000_000 -> fee floor violation.
        let menu = vec![params(2_000_000, 0, FUTURE, 1, u64::MAX)];
        assert_eq!(
            select_cheapest_valid_params(menu, 1_000_000, NOW).unwrap_err(),
            Lsps2Error::OpeningFeeExceedsAmount {
                opening_fee_msat: 2_000_000,
                amount_msat: 1_000_000,
            }
        );
    }

    // ---------- error mapping: distinct reasons ----------

    #[test]
    fn lsps_error_codes_map_to_distinct_reasons() {
        let reasons: Vec<String> = [200, 201, 202, 203, 999]
            .iter()
            .map(|&code| {
                describe_lsps_error(&LSPSResponseError {
                    code,
                    message: "boom".to_string(),
                    data: None,
                })
            })
            .collect();
        for (i, a) in reasons.iter().enumerate() {
            assert!(a.contains("boom"), "LSP message must be surfaced: {a}");
            for b in reasons.iter().skip(i + 1) {
                assert_ne!(a, b, "each code must produce a distinct reason");
            }
        }
        assert!(reasons[1].contains("201"));
        assert!(reasons[2].contains("too small"));
        assert!(reasons[3].contains("too large"));
    }

    #[test]
    fn get_info_and_buy_failures_render_distinct_reasons() {
        let err = LSPSResponseError {
            code: 201,
            message: "m".to_string(),
            data: None,
        };
        let get_info = Lsps2Error::GetInfoFailed(describe_lsps_error(&err)).to_string();
        let buy = Lsps2Error::BuyFailed(describe_lsps_error(&err)).to_string();
        assert_ne!(get_info, buy);
        assert!(get_info.contains("get_info"));
        assert!(buy.contains("buy"));
    }
}
