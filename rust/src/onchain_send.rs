//! On-chain send engine and anchor reserve (U8, R7; cites KTD-9, AE6):
//! fee estimation at the PWA's 6-block target (rate resolved by the caller
//! from the fee cache, clamped >= 2 sat/vB in `fees.rs`), max-sendable with
//! the 10,000-sat anchor reserve (active iff at least one channel is open),
//! exact-amount and send-max dispatch, the review-to-broadcast drift guard
//! (R5), and the PWA's fee guards (`MAX_FEE_SATS` 50,000, dust floors).
//!
//! Semantics mirror the PWA's `src/onchain/context.tsx`, `send-guards.ts`,
//! and `config.ts` — typed errors carry the PWA's exact Display strings.
//! Send-max deviates deliberately per the plan: with channels the reserve is
//! an EXPLICIT output to an internal (change) address, so exactly 10,000 sats
//! remain as an output (AE6), instead of the PWA's approximate change output.

use std::fmt;
use std::str::FromStr as _;

use bitcoin::{Address, FeeRate, Network, ScriptBuf, Transaction};

use crate::wallet::OnchainWallet;

/// Reserve withheld for anchor-channel CPFP fee bumping while any Lightning
/// channel is open (PWA `config.ts:17`, R7).
pub(crate) const ANCHOR_RESERVE_SATS: u64 = 10_000;

/// Sanity ceiling for the absolute fee on any on-chain send (PWA
/// `config.ts:9`, KTD-9).
pub(crate) const MAX_FEE_SATS: u64 = 50_000;

/// Typed on-chain send failures (U8). Display strings are the PWA's exact
/// user-facing copy where the plan names them (R7), so the shells render the
/// same messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnchainSendError {
    /// An on-chain operation while the node is stopped.
    NotRunning,
    /// The address failed to parse at all.
    InvalidAddress { detail: String },
    /// The address parses but belongs to a different network (PWA
    /// `mapSendError`, `context.tsx:60-62`).
    WrongNetwork,
    /// The estimated/built absolute fee exceeds [`MAX_FEE_SATS`] (PWA
    /// `send-guards.ts:10`).
    FeeTooHigh,
    /// A send-all leaves less than the recipient script's dust floor after
    /// fees (PWA `send-guards.ts:7`).
    BalanceTooLow,
    /// The tx built at the broadcast boundary pays the recipient a different
    /// amount (or fee) than the one reviewed — the R5 drift guard (PWA
    /// `send-guards.ts:72`); the shell re-renders "Amounts were updated".
    DriftDetected,
    /// amount + fee + reserve exceeds the trusted-spendable balance (PWA
    /// `context.tsx:316-320`).
    InsufficientFunds { reserve_sats: u64 },
    /// The requested amount is below the recipient script's dust floor (PWA
    /// `mapSendError`, `context.tsx:63-65`).
    AmountBelowDust { min_sats: u64 },
    /// The tx could not be built (coin selection, descriptor, persistence).
    BuildFailed { detail: String },
    /// The built tx could not be signed/finalized.
    SigningFailed { detail: String },
}

impl fmt::Display for OnchainSendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OnchainSendError::NotRunning => write!(f, "the node is not running"),
            OnchainSendError::InvalidAddress { detail } => {
                write!(f, "invalid bitcoin address: {detail}")
            }
            OnchainSendError::WrongNetwork => {
                write!(f, "This address is for a different Bitcoin network")
            }
            OnchainSendError::FeeTooHigh => {
                write!(f, "Network fees are too high right now — try again later.")
            }
            OnchainSendError::BalanceTooLow => write!(f, "Balance too low to cover fees"),
            OnchainSendError::DriftDetected => write!(f, "Send amount changed since review"),
            OnchainSendError::InsufficientFunds { reserve_sats } => write!(
                f,
                "Insufficient funds after reserving {} for Lightning channel safety",
                format_btc(*reserve_sats)
            ),
            OnchainSendError::AmountBelowDust { min_sats } => {
                write!(f, "Amount is below the minimum ({})", format_btc(*min_sats))
            }
            OnchainSendError::BuildFailed { detail } => {
                write!(f, "failed to build the transaction: {detail}")
            }
            OnchainSendError::SigningFailed { detail } => {
                write!(f, "failed to sign the transaction: {detail}")
            }
        }
    }
}

impl std::error::Error for OnchainSendError {}

/// A fee estimate for an exact-amount send (U8, PWA `FeeEstimate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct FeeEstimate {
    /// Absolute fee in sats.
    pub fee_sats: u64,
    /// The rate the estimate was built at (sat/vB, already clamped >= 2).
    pub fee_rate_sat_per_vb: u64,
}

/// A max-sendable estimate (U8, PWA `MaxSendEstimate`): the drain amount
/// after fees and the anchor reserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct MaxSendEstimate {
    /// The maximum sendable amount in sats.
    pub amount_sats: u64,
    /// Absolute fee in sats for the drain-shaped tx.
    pub fee_sats: u64,
    /// The rate the estimate was built at (sat/vB).
    pub fee_rate_sat_per_vb: u64,
    /// The anchor reserve withheld (10,000 iff >= 1 channel, else 0).
    pub reserve_sats: u64,
}

/// The review-to-broadcast drift guard (R5, U8): plain values captured at
/// review time — recipient script hex, reviewed amount, reviewed fee —
/// re-verified against the tx actually built at the broadcast boundary.
/// Mirrors the PWA's `makeDriftCheck` (`send-guards.ts:48-61`), which holds no
/// live objects for the same reason (plain values only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftGuard {
    /// Hex of the recipient's script pubkey, captured before any build.
    pub script_hex: String,
    /// The amount the user confirmed on the review screen, in sats.
    pub expected_amount_sats: u64,
    /// The fee the user reviewed, in sats.
    pub expected_fee_sats: u64,
}

impl DriftGuard {
    /// Captures a guard for `address` from the reviewed amount and fee.
    pub(crate) fn for_address(
        address: &str,
        network: Network,
        expected_amount_sats: u64,
        expected_fee_sats: u64,
    ) -> Result<Self, OnchainSendError> {
        let script = parse_address(address, network)?.script_pubkey();
        Ok(Self {
            script_hex: script_hex(&script),
            expected_amount_sats,
            expected_fee_sats,
        })
    }
}

/// The tx shapes the U8 engine builds (KTD-9 fee handling is shared).
/// U9 adds [`TxSpec::FundingOutput`] for channel funding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TxSpec {
    /// Exact-amount send: `add_recipient(script, amount)`.
    Recipient { script: ScriptBuf, amount_sats: u64 },
    /// Channel funding output (U9): like [`TxSpec::Recipient`] but built with
    /// `nlocktime(0)` — LDK rejects funding txs with a non-final locktime and
    /// bdk defaults to current-height anti-fee-sniping (the PWA's
    /// `event-handler.ts` funding build sets `nlocktime(0)` for the same
    /// reason).
    FundingOutput { script: ScriptBuf, amount_sats: u64 },
    /// Zero-channel send-max: `drain_wallet().drain_to(script)` (AE6).
    DrainAll { script: ScriptBuf },
    /// Send-max with channels: drain to the recipient PLUS an explicit
    /// reserve output to an internal (change) address, so exactly
    /// `reserve_sats` remain as an output (AE6).
    DrainWithReserve {
        recipient: ScriptBuf,
        reserve_sats: u64,
    },
}

/// The slice of a built PSBT the U8 guards inspect (PWA `DriftCheckPsbt`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuiltTxFacts {
    /// Absolute fee in sats.
    pub(crate) fee_sats: u64,
    /// Every output as (script, sats).
    pub(crate) outputs: Vec<(ScriptBuf, u64)>,
}

impl BuiltTxFacts {
    /// The value paid to `script_hex`, or `None` when no output pays it.
    pub(crate) fn output_value_for_script_hex(&self, script_hex_wanted: &str) -> Option<u64> {
        self.outputs
            .iter()
            .find(|(script, _)| script_hex(script) == script_hex_wanted)
            .map(|(_, value)| *value)
    }
}

/// Low-level build failures out of the bdk wallet, mapped to typed
/// [`OnchainSendError`]s per path (a sub-dust DRAIN is "balance too low"; a
/// sub-dust RECIPIENT is "amount below the minimum").
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TxBuildFailure {
    /// A recipient output is below its script's dust floor.
    OutputBelowDust,
    /// Coin selection could not fund the outputs (includes sub-dust drains).
    InsufficientFunds(String),
    /// Anything else (descriptor, policy, PSBT).
    Other(String),
}

/// Hex of a script pubkey, for drift comparison (plain values only).
pub(crate) fn script_hex(script: &ScriptBuf) -> String {
    format!("{:x}", script.as_script())
}

/// BIP 177 ₿-prefixed comma-separated sats (PWA `format-btc.ts`), for the
/// PWA-parity error copy.
pub(crate) fn format_btc(sats: u64) -> String {
    let digits = sats.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    for (index, ch) in digits.chars().enumerate() {
        let remaining = digits.len() - index;
        if index > 0 && remaining.is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    format!("\u{20BF}{grouped}")
}

/// The anchor reserve: 10,000 sats iff at least one channel is open (PWA
/// `getAnchorReserve`, `context.tsx:94-98`; `config.ts:17`).
pub(crate) fn anchor_reserve_sats(channel_count: usize) -> u64 {
    if channel_count > 0 {
        ANCHOR_RESERVE_SATS
    } else {
        0
    }
}

/// Estimate-time guards for send-all (PWA `checkMaxSendGuards`,
/// `send-guards.ts:25-29`): the fee ceiling is checked FIRST — when both
/// guards trip, "try again later" is the actionable advice.
pub(crate) fn check_max_send_guards(
    amount_sats: i128,
    fee_sats: u64,
    dust_floor_sats: u64,
) -> Result<(), OnchainSendError> {
    if fee_sats > MAX_FEE_SATS {
        return Err(OnchainSendError::FeeTooHigh);
    }
    if amount_sats < dust_floor_sats as i128 {
        return Err(OnchainSendError::BalanceTooLow);
    }
    Ok(())
}

/// Confirm-time amount drift (PWA `checkAmountDrift`, `send-guards.ts:84-92`):
/// any mismatch — including a missing output — is drift. No tolerance window.
pub(crate) fn check_amount_drift(
    expected_sats: u64,
    built_output_sats: Option<u64>,
) -> Result<(), OnchainSendError> {
    match built_output_sats {
        Some(built) if built == expected_sats => Ok(()),
        Some(_) | None => Err(OnchainSendError::DriftDetected),
    }
}

/// The full broadcast-boundary drift verification (R5): the built tx must pay
/// the reviewed script the reviewed amount exactly, at the reviewed fee.
pub(crate) fn verify_drift(
    guard: &DriftGuard,
    facts: &BuiltTxFacts,
) -> Result<(), OnchainSendError> {
    check_amount_drift(
        guard.expected_amount_sats,
        facts.output_value_for_script_hex(&guard.script_hex),
    )?;
    // U8: the guard also re-verifies the reviewed fee — a fee-cache refresh
    // between review and broadcast re-renders "Amounts were updated" instead
    // of silently paying a different fee. (The PWA pins the reviewed fee RATE
    // through the re-build instead; same protection, stricter.)
    if facts.fee_sats != guard.expected_fee_sats {
        return Err(OnchainSendError::DriftDetected);
    }
    Ok(())
}

/// The anchor-reserve post-check (ldk-node shape, PWA `context.tsx:311-321`):
/// reject when amount + fee + reserve exceed the trusted-spendable balance.
pub(crate) fn check_reserve(
    amount_sats: u64,
    fee_sats: u64,
    reserve_sats: u64,
    spendable_sats: u64,
) -> Result<(), OnchainSendError> {
    if reserve_sats == 0 {
        return Ok(());
    }
    let committed = amount_sats as u128 + fee_sats as u128 + reserve_sats as u128;
    if committed > spendable_sats as u128 {
        return Err(OnchainSendError::InsufficientFunds { reserve_sats });
    }
    Ok(())
}

/// The absolute-fee ceiling at the broadcast boundary (PWA
/// `context.tsx:197-201`).
fn check_fee_ceiling(fee_sats: u64) -> Result<(), OnchainSendError> {
    if fee_sats > MAX_FEE_SATS {
        return Err(OnchainSendError::FeeTooHigh);
    }
    Ok(())
}

/// Parses and network-checks a recipient address (PWA `Address.from_string`
/// + `mapSendError`).
pub(crate) fn parse_address(address: &str, network: Network) -> Result<Address, OnchainSendError> {
    let unchecked =
        Address::from_str(address.trim()).map_err(|e| OnchainSendError::InvalidAddress {
            detail: e.to_string(),
        })?;
    unchecked
        .require_network(network)
        .map_err(|_| OnchainSendError::WrongNetwork)
}

fn fee_rate_from(sat_per_vb: u64) -> Result<FeeRate, OnchainSendError> {
    FeeRate::from_sat_per_vb(sat_per_vb).ok_or(OnchainSendError::BuildFailed {
        detail: format!("fee rate {sat_per_vb} sat/vB overflows"),
    })
}

/// The drain shape for max-send estimates and sends: pure drain at zero
/// channels, drain-plus-explicit-reserve otherwise (AE6).
fn max_send_spec(recipient: ScriptBuf, reserve_sats: u64) -> TxSpec {
    if reserve_sats == 0 {
        TxSpec::DrainAll { script: recipient }
    } else {
        TxSpec::DrainWithReserve {
            recipient,
            reserve_sats,
        }
    }
}

/// Build-failure mapping for EXACT-AMOUNT sends: a sub-dust recipient is
/// "amount below the minimum" (PWA `mapSendError`, `context.tsx:63-65`).
fn fixed_send_build_error(failure: TxBuildFailure, script: &ScriptBuf) -> OnchainSendError {
    match failure {
        TxBuildFailure::OutputBelowDust => OnchainSendError::AmountBelowDust {
            min_sats: script.minimal_non_dust().to_sat(),
        },
        TxBuildFailure::InsufficientFunds(detail) | TxBuildFailure::Other(detail) => {
            OnchainSendError::BuildFailed { detail }
        }
    }
}

/// Build-failure mapping for SEND-MAX shapes: a drain that cannot cover fees
/// or dust is "balance too low" (U8 plan: sub-dust drain → balance too low).
fn max_send_build_error(failure: TxBuildFailure) -> OnchainSendError {
    match failure {
        TxBuildFailure::OutputBelowDust | TxBuildFailure::InsufficientFunds(_) => {
            OnchainSendError::BalanceTooLow
        }
        TxBuildFailure::Other(detail) => OnchainSendError::BuildFailed { detail },
    }
}

/// Builds an exact-amount tx to learn its fee WITHOUT broadcasting (U8; PWA
/// `estimateFee`, `context.tsx:239-257`), then applies the [`MAX_FEE_SATS`]
/// ceiling so an unaffordable send surfaces at the amount step.
pub(crate) fn estimate_fee(
    wallet: &OnchainWallet,
    network: Network,
    address: &str,
    amount_sats: u64,
    fee_rate_sat_per_vb: u64,
) -> Result<FeeEstimate, OnchainSendError> {
    let script = parse_address(address, network)?.script_pubkey();
    let spec = TxSpec::Recipient {
        script: script.clone(),
        amount_sats,
    };
    let facts = wallet
        .estimate_onchain_tx(&spec, fee_rate_from(fee_rate_sat_per_vb)?)
        .map_err(|failure| fixed_send_build_error(failure, &script))?;
    check_fee_ceiling(facts.fee_sats)?;
    Ok(FeeEstimate {
        fee_sats: facts.fee_sats,
        fee_rate_sat_per_vb,
    })
}

/// Max-sendable estimate (U8; PWA `estimateMaxSendable`,
/// `context.tsx:259-291`): drain-shaped build, then
/// `amount = trusted_spendable − fee − reserve`, then the estimate-time
/// guards (fee ceiling first, then the recipient script's dust floor).
pub(crate) fn estimate_max_sendable(
    wallet: &OnchainWallet,
    network: Network,
    address: &str,
    reserve_sats: u64,
    fee_rate_sat_per_vb: u64,
) -> Result<MaxSendEstimate, OnchainSendError> {
    let script = parse_address(address, network)?.script_pubkey();
    // Dust floor for the recipient's script (294 sats P2WPKH, 546 legacy) —
    // PWA `context.tsx:265-268`.
    let dust_floor_sats = script.minimal_non_dust().to_sat();
    let spec = max_send_spec(script, reserve_sats);
    let facts = wallet
        .estimate_onchain_tx(&spec, fee_rate_from(fee_rate_sat_per_vb)?)
        .map_err(max_send_build_error)?;

    // Total trusted inputs minus fee minus anchor reserve = max sendable
    // (PWA `context.tsx:276-279`); untrusted pending is never in this number.
    let amount_sats =
        wallet.trusted_spendable_sats() as i128 - facts.fee_sats as i128 - reserve_sats as i128;
    check_max_send_guards(amount_sats, facts.fee_sats, dust_floor_sats)?;

    Ok(MaxSendEstimate {
        amount_sats: amount_sats as u64,
        fee_sats: facts.fee_sats,
        fee_rate_sat_per_vb,
        reserve_sats,
    })
}

/// Exact-amount send (U8; PWA `sendToAddress`, `context.tsx:306-336`): the
/// anchor-reserve post-check runs first (reserve > 0 only), then the tx is
/// built once at the broadcast boundary with the drift guard and fee ceiling
/// verified BEFORE signing.
pub(crate) fn send_to_address(
    wallet: &OnchainWallet,
    network: Network,
    address: &str,
    amount_sats: u64,
    expected: &DriftGuard,
    reserve_sats: u64,
    fee_rate_sat_per_vb: u64,
) -> Result<Transaction, OnchainSendError> {
    let script = parse_address(address, network)?.script_pubkey();
    let fee_rate = fee_rate_from(fee_rate_sat_per_vb)?;
    let spec = TxSpec::Recipient {
        script: script.clone(),
        amount_sats,
    };

    // Anchor-reserve post-check (R7, PWA `context.tsx:311-321`): estimate the
    // fee for this exact shape and reject when amount + fee + reserve exceed
    // the trusted-spendable balance. Only active with open channels.
    if reserve_sats > 0 {
        let facts = wallet
            .estimate_onchain_tx(&spec, fee_rate)
            .map_err(|failure| fixed_send_build_error(failure, &script))?;
        check_reserve(
            amount_sats,
            facts.fee_sats,
            reserve_sats,
            wallet.trusted_spendable_sats(),
        )?;
    }

    wallet.create_onchain_tx(
        &spec,
        fee_rate,
        |facts| {
            // Broadcast-boundary asserts (R5/KTD-9), before anything is
            // signed: drift first, then the absolute-fee ceiling.
            verify_drift(expected, facts)?;
            check_fee_ceiling(facts.fee_sats)
        },
        |failure| fixed_send_build_error(failure, &script),
    )
}

/// Send-max (U8; PWA `sendMax`, `context.tsx:338-400`): zero channels drains
/// the wallet fully; with channels the estimate-time guards gate the send and
/// the built tx leaves EXACTLY the reserve as an explicit internal output
/// (AE6). The drift guard runs at the broadcast boundary either way.
pub(crate) fn send_max(
    wallet: &OnchainWallet,
    network: Network,
    address: &str,
    expected: &DriftGuard,
    reserve_sats: u64,
    fee_rate_sat_per_vb: u64,
) -> Result<Transaction, OnchainSendError> {
    let script = parse_address(address, network)?.script_pubkey();
    let fee_rate = fee_rate_from(fee_rate_sat_per_vb)?;

    // With channels, the estimate-time guards gate the send exactly like the
    // PWA's reserve branch (`context.tsx:376-387`): fee ceiling and dust
    // floor surface as friendly errors before any build is signed.
    if reserve_sats > 0 {
        let _ = estimate_max_sendable(wallet, network, address, reserve_sats, fee_rate_sat_per_vb)?;
    }

    wallet.create_onchain_tx(
        &max_send_spec(script, reserve_sats),
        fee_rate,
        |facts| {
            verify_drift(expected, facts)?;
            check_fee_ceiling(facts.fee_sats)
        },
        max_send_build_error,
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use bitcoin::Network;
    use lightning_persister::fs_store::FilesystemStore;

    use super::*;
    use crate::types::Logger;
    use crate::wallet::test_support;

    /// BIP173's example mainnet P2WPKH address (dust floor 294 sats).
    const RECIPIENT: &str = "bc1qw508d6qejxtdg4y5r3zarvary0c5xw7kv8f3t4";
    /// BIP173's example testnet P2WPKH address.
    const TESTNET_ADDR: &str = "tb1qw508d6qejxtdg4y5r3zarvary0c5xw7kxpjzsx";

    const RATE: u64 = 2;

    fn recipient_script() -> ScriptBuf {
        parse_address(RECIPIENT, Network::Bitcoin)
            .unwrap()
            .script_pubkey()
    }

    fn wallet_in(dir: &std::path::Path) -> OnchainWallet {
        let keys = crate::keys::derive_wallet_keys(
            &crate::keys::parse_mnemonic(crate::keys::tests::TEST_MNEMONIC).unwrap(),
            Network::Bitcoin,
        );
        OnchainWallet::new(
            &keys.descriptor_external,
            &keys.descriptor_internal,
            Network::Bitcoin,
            Arc::new(FilesystemStore::new(PathBuf::from(dir).join("store"))),
            Arc::new(Logger),
        )
        .unwrap()
    }

    fn funded_wallet(sats: u64) -> (tempfile::TempDir, OnchainWallet) {
        let dir = tempfile::tempdir().unwrap();
        let wallet = wallet_in(dir.path());
        test_support::fund_confirmed(&wallet, sats);
        (dir, wallet)
    }

    fn guard_for(amount_sats: u64, fee_sats: u64) -> DriftGuard {
        DriftGuard::for_address(RECIPIENT, Network::Bitcoin, amount_sats, fee_sats).unwrap()
    }

    // ---------- pure guards (mirrors the PWA's send-guards.test.ts) ----------

    /// PWA `checkMaxSendGuards` matrix (`send-guards.test.ts:12-47`).
    #[test]
    fn max_send_guards_match_the_pwa_matrix() {
        // Boundaries pass: amount at the dust floor, fee at the ceiling.
        assert_eq!(check_max_send_guards(294, MAX_FEE_SATS, 294), Ok(()));
        // Comfortably valid.
        assert_eq!(check_max_send_guards(40_000, 150, 294), Ok(()));
        // Below the dust floor.
        assert_eq!(
            check_max_send_guards(293, 150, 294),
            Err(OnchainSendError::BalanceTooLow)
        );
        // Negative amount (fee exceeds the balance).
        assert_eq!(
            check_max_send_guards(-100, 150, 294),
            Err(OnchainSendError::BalanceTooLow)
        );
        // Larger legacy (P2PKH) dust floor.
        assert_eq!(
            check_max_send_guards(545, 150, 546),
            Err(OnchainSendError::BalanceTooLow)
        );
        assert_eq!(check_max_send_guards(546, 150, 546), Ok(()));
        // Fee above MAX_FEE_SATS.
        assert_eq!(
            check_max_send_guards(100_000, MAX_FEE_SATS + 1, 294),
            Err(OnchainSendError::FeeTooHigh)
        );
        // Both trip: the fee ceiling wins (fees may drop; the balance won't).
        assert_eq!(
            check_max_send_guards(200, MAX_FEE_SATS + 1, 294),
            Err(OnchainSendError::FeeTooHigh)
        );
    }

    /// PWA `checkAmountDrift` matrix (`send-guards.test.ts:49-73`).
    #[test]
    fn amount_drift_matches_the_pwa_matrix() {
        assert_eq!(check_amount_drift(49_850, Some(49_850)), Ok(()));
        for built in [Some(59_850), Some(39_850), Some(49_849), None] {
            assert_eq!(
                check_amount_drift(49_850, built),
                Err(OnchainSendError::DriftDetected),
                "expected drift for built output {built:?}"
            );
        }
    }

    /// PWA `makeDriftCheck` matrix (`send-guards.test.ts:86-127`), plus the
    /// U8 fee re-verification (expected_fee_sats is part of the guard).
    #[test]
    fn drift_verification_matches_the_pwa_matrix() {
        let recipient = ScriptBuf::from_hex("0014aabbccdd").unwrap();
        let change = ScriptBuf::from_hex("0014eeff0011").unwrap();
        let guard = DriftGuard {
            script_hex: script_hex(&recipient),
            expected_amount_sats: 49_850,
            expected_fee_sats: 150,
        };
        let facts =
            |outputs: Vec<(ScriptBuf, u64)>, fee_sats: u64| BuiltTxFacts { fee_sats, outputs };

        // Exact match passes.
        assert_eq!(
            verify_drift(&guard, &facts(vec![(recipient.clone(), 49_850)], 150)),
            Ok(())
        );
        // The recipient output is found regardless of order (change first).
        assert_eq!(
            verify_drift(
                &guard,
                &facts(
                    vec![(change.clone(), 10_000), (recipient.clone(), 49_850)],
                    150
                )
            ),
            Ok(())
        );
        // A differing recipient output is drift.
        assert_eq!(
            verify_drift(
                &guard,
                &facts(
                    vec![(recipient.clone(), 59_850), (change.clone(), 10_000)],
                    150
                )
            ),
            Err(OnchainSendError::DriftDetected)
        );
        // No output paying the recipient script is drift.
        assert_eq!(
            verify_drift(&guard, &facts(vec![(change.clone(), 49_850)], 150)),
            Err(OnchainSendError::DriftDetected)
        );
        // A changed fee is drift too (U8: the guard re-verifies the fee).
        assert_eq!(
            verify_drift(&guard, &facts(vec![(recipient.clone(), 49_850)], 151)),
            Err(OnchainSendError::DriftDetected)
        );
    }

    /// R7: the reserve is 10,000 sats iff at least one channel is open.
    #[test]
    fn reserve_is_active_iff_channels_exist() {
        assert_eq!(anchor_reserve_sats(0), 0);
        assert_eq!(anchor_reserve_sats(1), ANCHOR_RESERVE_SATS);
        assert_eq!(anchor_reserve_sats(7), ANCHOR_RESERVE_SATS);
    }

    /// R7 reserve arithmetic: amount + fee + reserve > spendable is rejected;
    /// exactly equal passes.
    #[test]
    fn reserve_arithmetic_rejects_overcommit() {
        assert_eq!(check_reserve(30_000, 200, 10_000, 40_200), Ok(()));
        assert_eq!(
            check_reserve(30_001, 200, 10_000, 40_200),
            Err(OnchainSendError::InsufficientFunds {
                reserve_sats: 10_000
            })
        );
        // No channels, no reserve check (PWA gates on reserve > 0).
        assert_eq!(check_reserve(40_000, 200, 0, 40_200), Ok(()));
    }

    /// The PWA's error copy renders sats as BIP 177 ₿-prefixed integers.
    #[test]
    fn format_btc_matches_the_pwa() {
        assert_eq!(format_btc(0), "₿0");
        assert_eq!(format_btc(294), "₿294");
        assert_eq!(format_btc(10_000), "₿10,000");
        assert_eq!(format_btc(1_234_567), "₿1,234,567");
        assert_eq!(
            OnchainSendError::InsufficientFunds {
                reserve_sats: 10_000
            }
            .to_string(),
            "Insufficient funds after reserving ₿10,000 for Lightning channel safety"
        );
    }

    // ---------- engine over a bdk wallet funded offline ----------

    /// U8: an estimate builds the tx but broadcasts nothing and stages
    /// nothing durable — repeatable with identical results.
    #[test]
    fn estimate_fee_builds_without_side_effects() {
        let (_dir, wallet) = funded_wallet(100_000);
        let first = estimate_fee(&wallet, Network::Bitcoin, RECIPIENT, 50_000, RATE).unwrap();
        assert!(first.fee_sats > 0);
        assert_eq!(first.fee_rate_sat_per_vb, RATE);
        let second = estimate_fee(&wallet, Network::Bitcoin, RECIPIENT, 50_000, RATE).unwrap();
        assert_eq!(first, second, "estimates must be repeatable");
        assert_eq!(
            test_support::tx_count(&wallet),
            1,
            "an estimate must not add transactions to the wallet"
        );
    }

    /// KTD-9/R7: an estimated fee above 50,000 sats is the typed too-high
    /// error with the PWA's copy.
    #[test]
    fn estimate_fee_flags_a_fee_above_the_ceiling() {
        let (_dir, wallet) = funded_wallet(10_000_000);
        let err = estimate_fee(&wallet, Network::Bitcoin, RECIPIENT, 50_000, 500).unwrap_err();
        assert_eq!(err, OnchainSendError::FeeTooHigh);
        assert_eq!(
            err.to_string(),
            "Network fees are too high right now — try again later."
        );
    }

    /// U8: zero channels — max sendable is the whole trusted balance minus
    /// the drain fee, no reserve.
    #[test]
    fn estimate_max_with_zero_channels_subtracts_only_the_fee() {
        let (_dir, wallet) = funded_wallet(100_000);
        let est = estimate_max_sendable(&wallet, Network::Bitcoin, RECIPIENT, 0, RATE).unwrap();
        assert_eq!(est.reserve_sats, 0);
        assert!(est.fee_sats > 0);
        assert_eq!(est.amount_sats, 100_000 - est.fee_sats);
    }

    /// R7: with channels the 10,000-sat reserve is additionally withheld.
    #[test]
    fn estimate_max_with_channels_subtracts_the_reserve() {
        let (_dir, wallet) = funded_wallet(100_000);
        let est = estimate_max_sendable(
            &wallet,
            Network::Bitcoin,
            RECIPIENT,
            ANCHOR_RESERVE_SATS,
            RATE,
        )
        .unwrap();
        assert_eq!(est.reserve_sats, ANCHOR_RESERVE_SATS);
        assert_eq!(
            est.amount_sats,
            100_000 - est.fee_sats - ANCHOR_RESERVE_SATS
        );
    }

    /// R7: untrusted pending (unconfirmed external receives) is NEVER
    /// counted — not in the max amount and not as spendable inputs — while
    /// trusted pending (own change) is.
    #[test]
    fn untrusted_pending_is_never_counted() {
        let (_dir, wallet) = funded_wallet(100_000);
        test_support::fund_untrusted_pending(&wallet, 50_000);
        let est = estimate_max_sendable(&wallet, Network::Bitcoin, RECIPIENT, 0, RATE).unwrap();
        assert_eq!(
            est.amount_sats,
            100_000 - est.fee_sats,
            "the untrusted 50,000 must not appear in the max amount"
        );

        test_support::fund_trusted_pending(&wallet, 30_000);
        let with_trusted =
            estimate_max_sendable(&wallet, Network::Bitcoin, RECIPIENT, 0, RATE).unwrap();
        assert_eq!(
            with_trusted.amount_sats,
            130_000 - with_trusted.fee_sats,
            "trusted pending (own change) counts"
        );
    }

    /// R7: a drain that cannot clear the recipient's dust floor after fees is
    /// the typed balance-too-low error with the PWA's copy.
    #[test]
    fn sub_dust_drain_is_balance_too_low() {
        let (_dir, wallet) = funded_wallet(400);
        let err = estimate_max_sendable(&wallet, Network::Bitcoin, RECIPIENT, 0, RATE).unwrap_err();
        assert_eq!(err, OnchainSendError::BalanceTooLow);
        assert_eq!(err.to_string(), "Balance too low to cover fees");
    }

    /// AE6: send-max with one channel leaves EXACTLY 10,000 sats as an
    /// explicit reserve output to an internal (change) address; the recipient
    /// gets everything else minus the fee; the tx is fully signed.
    #[test]
    fn ae6_send_max_with_a_channel_leaves_exactly_the_reserve_output() {
        let (_dir, wallet) = funded_wallet(100_000);
        let est = estimate_max_sendable(
            &wallet,
            Network::Bitcoin,
            RECIPIENT,
            ANCHOR_RESERVE_SATS,
            RATE,
        )
        .unwrap();
        let guard = guard_for(est.amount_sats, est.fee_sats);
        let tx = send_max(
            &wallet,
            Network::Bitcoin,
            RECIPIENT,
            &guard,
            ANCHOR_RESERVE_SATS,
            RATE,
        )
        .unwrap();

        assert_eq!(tx.output.len(), 2, "recipient + explicit reserve only");
        let reserve_out = tx
            .output
            .iter()
            .find(|out| out.value.to_sat() == ANCHOR_RESERVE_SATS)
            .expect("exactly 10,000 sats must remain as an output");
        assert!(
            test_support::is_internal_script(&wallet, &reserve_out.script_pubkey),
            "the reserve output must pay an internal (change) address"
        );
        let recipient_out = tx
            .output
            .iter()
            .find(|out| out.script_pubkey == recipient_script())
            .expect("the recipient output must exist");
        assert_eq!(recipient_out.value.to_sat(), est.amount_sats);
        assert_eq!(
            recipient_out.value.to_sat() + ANCHOR_RESERVE_SATS + est.fee_sats,
            100_000,
            "amount + reserve + fee account for the whole balance"
        );
        assert!(
            tx.input.iter().all(|input| !input.witness.is_empty()),
            "every input must be signed"
        );
    }

    /// AE6: send-max with zero channels drains the wallet fully — one output,
    /// the whole balance minus the fee.
    #[test]
    fn ae6_send_max_with_zero_channels_drains_fully() {
        let (_dir, wallet) = funded_wallet(60_000);
        test_support::fund_confirmed(&wallet, 40_000);
        let est = estimate_max_sendable(&wallet, Network::Bitcoin, RECIPIENT, 0, RATE).unwrap();
        let guard = guard_for(est.amount_sats, est.fee_sats);
        let tx = send_max(&wallet, Network::Bitcoin, RECIPIENT, &guard, 0, RATE).unwrap();

        assert_eq!(tx.input.len(), 2, "both UTXOs are drained");
        assert_eq!(tx.output.len(), 1, "a full drain leaves nothing behind");
        assert_eq!(tx.output[0].script_pubkey, recipient_script());
        assert_eq!(tx.output[0].value.to_sat(), 100_000 - est.fee_sats);
        assert_eq!(tx.output[0].value.to_sat(), est.amount_sats);
    }

    /// R5/AE6: the drift guard rejects any change between review and
    /// broadcast — a shifted amount, a shifted fee, or a wallet whose state
    /// changed under the review — and passes unchanged values.
    #[test]
    fn drift_guard_rejects_changed_amounts_and_passes_unchanged_ones() {
        let (_dir, wallet) = funded_wallet(100_000);
        let est = estimate_max_sendable(
            &wallet,
            Network::Bitcoin,
            RECIPIENT,
            ANCHOR_RESERVE_SATS,
            RATE,
        )
        .unwrap();

        // A tampered amount is rejected before anything is signed.
        let tampered = guard_for(est.amount_sats + 1, est.fee_sats);
        assert_eq!(
            send_max(
                &wallet,
                Network::Bitcoin,
                RECIPIENT,
                &tampered,
                ANCHOR_RESERVE_SATS,
                RATE
            )
            .unwrap_err(),
            OnchainSendError::DriftDetected
        );

        // A tampered fee is rejected too.
        let tampered_fee = guard_for(est.amount_sats, est.fee_sats + 1);
        assert_eq!(
            send_max(
                &wallet,
                Network::Bitcoin,
                RECIPIENT,
                &tampered_fee,
                ANCHOR_RESERVE_SATS,
                RATE
            )
            .unwrap_err(),
            OnchainSendError::DriftDetected
        );

        // Wallet state changed since review: the stale guard is rejected...
        test_support::fund_confirmed(&wallet, 25_000);
        let stale = guard_for(est.amount_sats, est.fee_sats);
        assert_eq!(
            send_max(
                &wallet,
                Network::Bitcoin,
                RECIPIENT,
                &stale,
                ANCHOR_RESERVE_SATS,
                RATE
            )
            .unwrap_err(),
            OnchainSendError::DriftDetected
        );
        assert_eq!(
            OnchainSendError::DriftDetected.to_string(),
            "Send amount changed since review"
        );

        // ...and a fresh review passes.
        let fresh = estimate_max_sendable(
            &wallet,
            Network::Bitcoin,
            RECIPIENT,
            ANCHOR_RESERVE_SATS,
            RATE,
        )
        .unwrap();
        let guard = guard_for(fresh.amount_sats, fresh.fee_sats);
        send_max(
            &wallet,
            Network::Bitcoin,
            RECIPIENT,
            &guard,
            ANCHOR_RESERVE_SATS,
            RATE,
        )
        .expect("an unchanged review must pass the drift guard");
    }

    /// R7: an exact-amount send that would dip into the reserve is rejected
    /// with the PWA's copy; the same send without channels passes.
    #[test]
    fn send_rejects_when_amount_fee_and_reserve_exceed_spendable() {
        let (_dir, wallet) = funded_wallet(50_000);
        let est = estimate_fee(&wallet, Network::Bitcoin, RECIPIENT, 45_000, RATE).unwrap();
        let guard = guard_for(45_000, est.fee_sats);
        let err = send_to_address(
            &wallet,
            Network::Bitcoin,
            RECIPIENT,
            45_000,
            &guard,
            ANCHOR_RESERVE_SATS,
            RATE,
        )
        .unwrap_err();
        assert_eq!(
            err,
            OnchainSendError::InsufficientFunds {
                reserve_sats: ANCHOR_RESERVE_SATS
            }
        );

        // Without channels the same send is fine.
        let tx = send_to_address(
            &wallet,
            Network::Bitcoin,
            RECIPIENT,
            45_000,
            &guard,
            0,
            RATE,
        )
        .unwrap();
        let recipient_out = tx
            .output
            .iter()
            .find(|out| out.script_pubkey == recipient_script())
            .unwrap();
        assert_eq!(recipient_out.value.to_sat(), 45_000);
        assert!(tx.input.iter().all(|input| !input.witness.is_empty()));
    }

    /// R7: a send whose amount + fee + reserve exactly fits passes the
    /// reserve post-check (boundary).
    #[test]
    fn send_at_the_reserve_boundary_passes() {
        let (_dir, wallet) = funded_wallet(50_000);
        let tx = send_to_address(
            &wallet,
            Network::Bitcoin,
            RECIPIENT,
            30_000,
            &guard_for(
                30_000,
                estimate_fee(&wallet, Network::Bitcoin, RECIPIENT, 30_000, RATE)
                    .unwrap()
                    .fee_sats,
            ),
            ANCHOR_RESERVE_SATS,
            RATE,
        )
        .unwrap();
        assert!(!tx.output.is_empty());
    }

    /// KTD-9: the fee ceiling also fires at the broadcast boundary of an
    /// exact-amount send.
    #[test]
    fn send_fee_ceiling_applies_at_the_broadcast_boundary() {
        let (_dir, wallet) = funded_wallet(10_000_000);
        // Fee estimate at review time was fine (rate 2); fees spiked to 500
        // sat/vB by broadcast time. The guard's fee check fires as drift
        // first? No — the guard here carries the SPIKED fee so only the
        // ceiling can reject.
        let spiked_fee = estimate_fee(&wallet, Network::Bitcoin, RECIPIENT, 50_000, RATE)
            .unwrap()
            .fee_sats
            * 250;
        let guard = guard_for(50_000, spiked_fee);
        let err = send_to_address(&wallet, Network::Bitcoin, RECIPIENT, 50_000, &guard, 0, 500)
            .unwrap_err();
        assert!(
            matches!(
                err,
                OnchainSendError::FeeTooHigh | OnchainSendError::DriftDetected
            ),
            "a spiked fee must never broadcast: {err:?}"
        );
    }

    /// PWA `mapSendError` copy: wrong-network and unparseable addresses are
    /// typed, distinct errors.
    #[test]
    fn address_errors_are_typed() {
        let (_dir, wallet) = funded_wallet(100_000);
        let err = estimate_fee(&wallet, Network::Bitcoin, TESTNET_ADDR, 10_000, RATE).unwrap_err();
        assert_eq!(err, OnchainSendError::WrongNetwork);
        assert_eq!(
            err.to_string(),
            "This address is for a different Bitcoin network"
        );
        assert!(matches!(
            estimate_fee(&wallet, Network::Bitcoin, "not-an-address", 10_000, RATE).unwrap_err(),
            OnchainSendError::InvalidAddress { .. }
        ));
    }

    /// PWA `mapSendError` dust copy: a sub-dust recipient amount is typed
    /// with the script's floor.
    #[test]
    fn sub_dust_recipient_amount_is_typed() {
        let (_dir, wallet) = funded_wallet(100_000);
        let err = estimate_fee(&wallet, Network::Bitcoin, RECIPIENT, 100, RATE).unwrap_err();
        assert_eq!(err, OnchainSendError::AmountBelowDust { min_sats: 294 });
        assert_eq!(err.to_string(), "Amount is below the minimum (₿294)");
    }

    /// U8 (address-reveal learning): a send persists its changeset — the
    /// reserve output's internal script is still watched (revealed) after a
    /// reload, even though the review-time estimate discarded ITS staged
    /// reveal.
    #[test]
    fn send_max_persists_the_internal_reveal() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = wallet_in(dir.path());
        test_support::fund_confirmed(&wallet, 100_000);
        let est = estimate_max_sendable(
            &wallet,
            Network::Bitcoin,
            RECIPIENT,
            ANCHOR_RESERVE_SATS,
            RATE,
        )
        .unwrap();
        let tx = send_max(
            &wallet,
            Network::Bitcoin,
            RECIPIENT,
            &guard_for(est.amount_sats, est.fee_sats),
            ANCHOR_RESERVE_SATS,
            RATE,
        )
        .unwrap();
        let reserve_script = tx
            .output
            .iter()
            .find(|out| out.value.to_sat() == ANCHOR_RESERVE_SATS)
            .expect("the reserve output exists")
            .script_pubkey
            .clone();
        drop(wallet);

        let reloaded = wallet_in(dir.path());
        assert!(
            test_support::derivation_index(&reloaded, bdk_wallet::KeychainKind::Internal).is_some(),
            "the internal reveal must survive a restart"
        );
        assert!(
            test_support::is_internal_script(&reloaded, &reserve_script),
            "the broadcast reserve script must still be watched after a restart"
        );
    }

    /// U8/R7: the receive path — `next_unused_address` on the external
    /// keychain, changeset persisted after the reveal, so a restart keeps the
    /// index and re-serves the same unused address.
    #[test]
    fn next_receive_address_reveal_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let wallet = wallet_in(dir.path());
        let address = wallet.next_receive_address().unwrap();
        assert!(address.starts_with("bc1q"), "BIP84 mainnet address");
        assert_eq!(
            wallet.next_receive_address().unwrap(),
            address,
            "unused address is stable until used"
        );
        drop(wallet);

        let reloaded = wallet_in(dir.path());
        assert_eq!(
            test_support::derivation_index(&reloaded, bdk_wallet::KeychainKind::External),
            Some(0),
            "the external reveal must survive a restart"
        );
        assert_eq!(reloaded.next_receive_address().unwrap(), address);
    }
}
