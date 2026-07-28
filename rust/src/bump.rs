//! BumpTransaction/CPFP handling (U11; R9 CPFP half; KTD-9).
//!
//! Wires lightning's `BumpTransactionEventHandlerSync` with a bdk-backed
//! wallet source (the PWA's `bdk-wallet-source.ts`):
//!
//! - `list_confirmed_utxos`: CONFIRMED UTXOs only — an unconfirmed parent
//!   could drop from the mempool and invalidate the CPFP child, leaving the
//!   force-close stuck — with the P2WPKH satisfaction weight (107 wu);
//! - `sign_psbt`: `trust_witness_utxo: true` — the historic
//!   CPFP-cannot-sign bug: LDK builds anchor-CPFP PSBTs with only
//!   `witness_utxo` for our inputs, and bdk's default `SignOptions` reject
//!   exactly that (CVE-2020-14199 fee-siphon mitigation, aimed at UNTRUSTED
//!   PSBT producers; LDK produces this PSBT on our behalf from state we
//!   already trust);
//! - the fee-sanity gate (adopted from the incident review): a bump event
//!   whose requested package target rate exceeds 5x a fresh 3-block
//!   estimate is refused BEFORE any tx is built — the ~30x overpay incident
//!   happened when the urgent sweep target answered a 1-block panic rate
//!   (KTD-9 pins `UrgentOnChainSweep` to 3 blocks in `fees.rs`; this gate is
//!   the defense-in-depth on top). LDK re-yields bump events on each new
//!   block until the claim confirms, so a refusal is retried at fresh rates.

use std::sync::Arc;

use bitcoin::{Psbt, ScriptBuf, Transaction};
use lightning::events::bump_transaction::sync::{
    BumpTransactionEventHandlerSync, WalletSourceSync, WalletSync,
};
use lightning::events::bump_transaction::{BumpTransactionEvent, Utxo};
use lightning::log_error;
use lightning::sign::{ChangeDestinationSourceSync as _, KeysManager};
use lightning::util::logger::Logger as _;

use crate::chain::{check_fee_sanity, fee_sanity_max_sat_per_kw, Broadcaster, FeeSanityError};
use crate::fees::CachedFeeEstimator;
use crate::types::Logger;
use crate::wallet::OnchainWallet;

/// P2WPKH witness: ~107 weight units (DER sig + compressed pubkey) — the
/// PWA's `P2WPKH_SATISFACTION_WEIGHT` (`bdk-wallet-source.ts:17`).
pub(crate) const P2WPKH_SATISFACTION_WEIGHT: u64 = 107;

/// The concrete sync CPFP handler over our stack.
pub(crate) type BumpEventHandler = BumpTransactionEventHandlerSync<
    Arc<Broadcaster>,
    Arc<WalletSync<Arc<BdkWalletSource>, Arc<Logger>>>,
    Arc<KeysManager>,
    Arc<Logger>,
>;

/// Builds the CPFP handler (node start): broadcaster (persist-first,
/// sentinel-aware — KTD-9), bdk wallet source, and the `KeysManager` as the
/// signer provider for anchor-input re-derivation.
pub(crate) fn build_bump_handler(
    broadcaster: Arc<Broadcaster>,
    wallet_source: Arc<BdkWalletSource>,
    keys_manager: Arc<KeysManager>,
    logger: Arc<Logger>,
) -> BumpEventHandler {
    let wallet_sync = Arc::new(WalletSync::new(wallet_source, Arc::clone(&logger)));
    BumpTransactionEventHandlerSync::new(broadcaster, wallet_sync, keys_manager, logger)
}

/// The bump event's requested package/target feerate in sat/kW.
pub(crate) fn bump_event_target_sat_per_kw(event: &BumpTransactionEvent) -> u32 {
    match event {
        BumpTransactionEvent::ChannelClose {
            package_target_feerate_sat_per_1000_weight,
            ..
        } => *package_target_feerate_sat_per_1000_weight,
        BumpTransactionEvent::HTLCResolution {
            target_feerate_sat_per_1000_weight,
            ..
        } => *target_feerate_sat_per_1000_weight,
    }
}

/// Fee-sanity gate for a bump event (U11 middleware): the built package's
/// effective rate tracks the requested target by LDK's construction, so
/// gating the target IS gating the broadcast's effective rate — and it is
/// checkable BEFORE any coins are selected or signed.
pub(crate) fn check_bump_target_sanity(
    target_sat_per_kw: u32,
    fee_estimator: &CachedFeeEstimator,
) -> Result<(), FeeSanityError> {
    let max = fee_sanity_max_sat_per_kw(fee_estimator);
    // Rate-vs-rate: express the target as the fee a 1000-wu tx would pay.
    check_fee_sanity(u64::from(target_sat_per_kw), 1000, max)
}

/// The bdk-backed `WalletSourceSync` feeding LDK's CPFP coin selection.
pub(crate) struct BdkWalletSource {
    wallet: Arc<OnchainWallet>,
    logger: Arc<Logger>,
}

impl BdkWalletSource {
    pub(crate) fn new(wallet: Arc<OnchainWallet>, logger: Arc<Logger>) -> Self {
        Self { wallet, logger }
    }
}

impl WalletSourceSync for BdkWalletSource {
    fn list_confirmed_utxos(&self) -> Result<Vec<Utxo>, ()> {
        Ok(self
            .wallet
            .confirmed_utxos()
            .into_iter()
            .map(|(outpoint, output)| Utxo {
                outpoint,
                output,
                satisfaction_weight: P2WPKH_SATISFACTION_WEIGHT,
            })
            .collect())
    }

    fn get_change_script(&self) -> Result<ScriptBuf, ()> {
        self.wallet.get_change_destination_script()
    }

    fn sign_psbt(&self, mut psbt: Psbt) -> Result<Transaction, ()> {
        // The trust flag is the load-bearing bit (see the module docs); the
        // finalized flag is NOT required here — the anchor input is signed
        // by LDK AFTER this call, so the PSBT legitimately isn't fully
        // finalized yet. Wallet inputs that fail to sign surface when LDK
        // verifies the resulting witnesses.
        if let Err(e) = self.wallet.sign_psbt_trusted(&mut psbt) {
            log_error!(self.logger, "CPFP wallet signing failed: {e}");
            return Err(());
        }
        psbt.extract_tx().map_err(|e| {
            log_error!(self.logger, "CPFP tx extraction failed: {e}");
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use bdk_wallet::SignOptions;
    use bitcoin::{Amount, Network, OutPoint, Sequence, TxIn, TxOut, Witness};
    use lightning_persister::fs_store::FilesystemStore;

    use super::*;
    use crate::fees::cache_from_esplora_estimates;
    use crate::keys::{derive_wallet_keys, parse_mnemonic, tests::TEST_MNEMONIC};
    use crate::wallet::test_support;

    fn funded_wallet(sats: u64) -> (tempfile::TempDir, Arc<OnchainWallet>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStore::new(dir.path().join("store")));
        let keys = derive_wallet_keys(&parse_mnemonic(TEST_MNEMONIC).unwrap(), Network::Bitcoin);
        let wallet = Arc::new(
            OnchainWallet::new(
                &keys.descriptor_external,
                &keys.descriptor_internal,
                Network::Bitcoin,
                store,
                Arc::new(Logger),
            )
            .unwrap(),
        );
        if sats > 0 {
            test_support::fund_confirmed(&wallet, sats);
        }
        (dir, wallet)
    }

    /// Builds the LDK-style CPFP PSBT: spends a wallet UTXO with ONLY
    /// `witness_utxo` populated (no `non_witness_utxo`) — exactly what
    /// `BumpTransactionEventHandler` hands the wallet source.
    fn witness_utxo_only_psbt(wallet: &OnchainWallet) -> Psbt {
        let (outpoint, txout) = wallet
            .confirmed_utxos()
            .into_iter()
            .next()
            .expect("funded wallet has a confirmed utxo");
        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: outpoint,
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: txout.value - Amount::from_sat(1_000),
                script_pubkey: txout.script_pubkey.clone(),
            }],
        };
        let mut psbt = Psbt::from_unsigned_tx(tx).unwrap();
        psbt.inputs[0].witness_utxo = Some(txout);
        psbt
    }

    /// U11 GUARD 1 (the historic CPFP-cannot-sign bug): a CPFP PSBT carrying
    /// only `witness_utxo` for the wallet input
    ///
    /// (a) is REJECTED by bdk's default `SignOptions` (CVE-2020-14199
    ///     mitigation — the regression this guard pins), and
    /// (b) SIGNS through the wallet source's `trust_witness_utxo` path,
    ///     yielding a real witness on the input.
    #[test]
    fn cpfp_psbt_with_only_witness_utxo_signs_via_trust_flag_and_not_by_default() {
        let (_dir, wallet) = funded_wallet(50_000);

        // (a) Default sign options must NOT produce a signature — this is
        // the exact failure mode that stranded CPFP txs.
        let mut default_psbt = witness_utxo_only_psbt(&wallet);
        let default_result =
            test_support::sign_with_options(&wallet, &mut default_psbt, SignOptions::default());
        let default_signed = default_result.unwrap_or(false)
            || default_psbt.inputs[0].final_script_witness.is_some()
            || !default_psbt.inputs[0].partial_sigs.is_empty();
        assert!(
            !default_signed,
            "default SignOptions must reject a witness_utxo-only PSBT; if bdk starts \
             accepting it this guard should be revisited"
        );

        // (b) The wallet source signs it (trust_witness_utxo).
        let source = BdkWalletSource::new(Arc::clone(&wallet), Arc::new(Logger));
        let psbt = witness_utxo_only_psbt(&wallet);
        let tx = source
            .sign_psbt(psbt)
            .expect("trust_witness_utxo path must sign");
        assert!(
            !tx.input[0].witness.is_empty(),
            "the signed CPFP input must carry a witness"
        );
    }

    /// The CPFP coin source offers CONFIRMED UTXOs only, at the P2WPKH
    /// satisfaction weight (PWA parity).
    #[test]
    fn wallet_source_lists_only_confirmed_utxos_with_p2wpkh_weight() {
        let (_dir, wallet) = funded_wallet(40_000);
        test_support::fund_untrusted_pending(&wallet, 90_000);

        let source = BdkWalletSource::new(Arc::clone(&wallet), Arc::new(Logger));
        let utxos = source.list_confirmed_utxos().unwrap();
        assert_eq!(utxos.len(), 1, "the unconfirmed receive must be excluded");
        assert_eq!(utxos[0].output.value.to_sat(), 40_000);
        assert_eq!(utxos[0].satisfaction_weight, P2WPKH_SATISFACTION_WEIGHT);

        let change = source.get_change_script().unwrap();
        assert!(change.is_p2wpkh());
    }

    /// U11 GUARD 3 (CPFP half): a bump event demanding 30x the fresh
    /// 3-block rate is refused; one at the urgent (3-block) rate passes.
    /// KTD-9's 3-block pin itself is tested in `fees.rs`
    /// (`fee_table_matches_pwa_floors_and_targets`).
    #[test]
    fn bump_target_sanity_blocks_a_30x_overpay_target() {
        let estimator = CachedFeeEstimator::new();
        // 3-block estimate: 100 sat/vB -> 25_000 sat/kW; ceiling 125_000.
        let estimates: HashMap<u16, f64> = [(1u16, 400.0), (3u16, 100.0), (6u16, 50.0)]
            .into_iter()
            .collect();
        estimator.set_cache(cache_from_esplora_estimates(&estimates));

        assert!(
            check_bump_target_sanity(25_000, &estimator).is_ok(),
            "the urgent 3-block rate itself always passes"
        );
        assert!(
            check_bump_target_sanity(125_000, &estimator).is_ok(),
            "the 5x boundary passes"
        );
        let refused = check_bump_target_sanity(750_000, &estimator);
        assert!(
            matches!(refused, Err(FeeSanityError::Overpay { .. })),
            "a 30x target must be refused: {refused:?}"
        );
    }

    fn dummy_outpoint() -> OutPoint {
        OutPoint {
            txid: "1111111111111111111111111111111111111111111111111111111111111111"
                .parse()
                .unwrap(),
            vout: 0,
        }
    }

    /// A PSBT whose input the wallet does not own signs nothing but does not
    /// error — LDK finalizes its own (anchor) inputs after this call.
    #[test]
    fn sign_psbt_tolerates_foreign_inputs() {
        let (_dir, wallet) = funded_wallet(30_000);
        let source = BdkWalletSource::new(Arc::clone(&wallet), Arc::new(Logger));

        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: dummy_outpoint(),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(20_000),
                script_pubkey: wallet.peek_external_script(0),
            }],
        };
        let mut psbt = Psbt::from_unsigned_tx(tx).unwrap();
        psbt.inputs[0].witness_utxo = Some(TxOut {
            value: Amount::from_sat(21_000),
            script_pubkey: ScriptBuf::new_op_return([0u8; 8]),
        });
        let extracted = source.sign_psbt(psbt).expect("foreign inputs tolerated");
        assert!(extracted.input[0].witness.is_empty());
    }
}
