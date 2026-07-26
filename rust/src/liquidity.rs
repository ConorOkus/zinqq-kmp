//! LSPS2 (bLIP-52) client flow (U4), mirroring ldk-node `src/liquidity.rs`:
//! connect to the LSP on demand → `lsps2.get_info` → pick the cheapest valid
//! opening-fee params (client-side fee-floor enforcement pre-empts LSP error
//! 202) → `lsps2.buy` → build the wrapped invoice (see [`crate::invoice`]).
//!
//! Responses arrive as [`LSPS2ClientEvent`]s pumped from the
//! `LiquidityManager`; each in-flight request parks a oneshot sender keyed by
//! its `LSPSRequestId`, so success, LSP error, and timeout all resolve the
//! same await with a distinct [`Lsps2Error`].
//!
//! The KTD-9 0-conf cluster's per-channel half also lives here:
//! [`LiquiditySource::on_open_channel_request`] accepts 0-conf from the
//! configured LSP only (with the underpaying-HTLC override so the skimmed
//! opening fee is claimable), and [`ClaimTracker`] guards the skim at
//! `PaymentClaimable` time before `claim_funds`.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use bitcoin::Network;
use lightning::events::PaymentPurpose;
use lightning::ln::types::ChannelId;
use lightning::log_error;
use lightning::log_info;
use lightning::types::payment::{PaymentHash, PaymentPreimage};
use lightning::util::config::{
    ChannelConfigOverrides, ChannelConfigUpdate, ChannelHandshakeConfigUpdate,
};
use lightning::util::logger::Logger as _;
use lightning_invoice::Bolt11Invoice;
use lightning_liquidity::events::LiquidityEvent;
use lightning_liquidity::lsps0::ser::{LSPSDateTime, LSPSRequestId, LSPSResponseError};
use lightning_liquidity::lsps2::event::LSPS2ClientEvent;
use lightning_liquidity::lsps2::msgs::LSPS2OpeningFeeParams;
use lightning_liquidity::lsps2::utils::compute_opening_fee;
use tokio::sync::oneshot;

use crate::config::{LspConfig, LSPS2_REQUEST_TIMEOUT, LSP_CONNECT_TIMEOUT};
use crate::invoice::{build_jit_invoice, JitInvoiceParams, JIT_MIN_FINAL_CLTV_EXPIRY_DELTA};
use crate::types::{ChannelManager, LiquidityManager, Logger, PeerManager};

/// Fixed description on the spike's JIT invoices.
const JIT_INVOICE_DESCRIPTION: &str = "zinqq";

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

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system time before UNIX epoch")
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

/// What to do with a `PaymentClaimable` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClaimDecision {
    /// Claim with this preimage (`claim_funds` is idempotent in LDK, so a
    /// replayed claimable simply claims again).
    Claim(PaymentPreimage),
    /// Fail the HTLC back with this reason.
    FailBack(String),
}

/// Skim bookkeeping across the claim lifecycle. At invoice creation the
/// agreed opening fee is registered per payment hash; at `PaymentClaimable`
/// the observed `counterparty_skimmed_fee_msat` is checked against it (the
/// ldk-node guard: the LSP must not take more than agreed) and recorded; at
/// `PaymentClaimed` the recorded skim is consumed for the public
/// `PaymentReceived` event.
///
/// In-memory only: after a process restart a replayed claimable for a JIT
/// invoice from a previous session has no registered fee and a nonzero skim
/// is refused (HTLC failed back; the payer retries). Acceptable for the
/// foreground-only spike flow.
#[derive(Default)]
pub(crate) struct ClaimTracker {
    /// payment_hash -> max skim agreed at invoice creation.
    expected_fee_msat: Mutex<HashMap<PaymentHash, u64>>,
    /// payment_hash -> skim observed on the (latest) claimable event.
    observed_skim_msat: Mutex<HashMap<PaymentHash, u64>>,
}

impl ClaimTracker {
    /// Registers the opening fee agreed for a JIT invoice.
    pub(crate) fn register_expected_fee(&self, payment_hash: PaymentHash, fee_msat: u64) {
        self.expected_fee_msat
            .lock()
            .unwrap()
            .insert(payment_hash, fee_msat);
    }

    /// Decides claim-or-fail for a claimable payment. Idempotent: replaying
    /// the same claimable yields the same decision and never panics.
    pub(crate) fn decide(
        &self,
        payment_hash: PaymentHash,
        skimmed_fee_msat: u64,
        preimage: Option<PaymentPreimage>,
    ) -> ClaimDecision {
        self.observed_skim_msat
            .lock()
            .unwrap()
            .insert(payment_hash, skimmed_fee_msat);

        let Some(preimage) = preimage else {
            return ClaimDecision::FailBack(
                "claimable payment carries no preimage (not created via create_inbound_payment)"
                    .to_string(),
            );
        };

        // ldk-node's guard: never let the counterparty skim more than the
        // agreed opening fee (payments we never sold a JIT channel for get an
        // allowance of zero).
        let max_skim_msat = self
            .expected_fee_msat
            .lock()
            .unwrap()
            .get(&payment_hash)
            .copied()
            .unwrap_or(0);
        if skimmed_fee_msat > max_skim_msat {
            return ClaimDecision::FailBack(format!(
                "counterparty skimmed {skimmed_fee_msat}msat, more than the agreed \
                 {max_skim_msat}msat opening fee"
            ));
        }

        ClaimDecision::Claim(preimage)
    }

    /// Consumes the recorded skim when the payment is durably claimed.
    pub(crate) fn take_skim(&self, payment_hash: &PaymentHash) -> Option<u64> {
        self.expected_fee_msat.lock().unwrap().remove(payment_hash);
        self.observed_skim_msat.lock().unwrap().remove(payment_hash)
    }
}

/// The `lsps2.buy` outcome relayed from `InvoiceParametersReady`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BuyResponse {
    pub(crate) intercept_scid: u64,
    pub(crate) cltv_expiry_delta: u32,
}

type FeeResult = Result<Vec<LSPS2OpeningFeeParams>, Lsps2Error>;
type BuyResult = Result<BuyResponse, Lsps2Error>;

/// The client-side LSPS2 driver, owning the pending-request routing and the
/// JIT claim policy (à la ldk-node's `LiquiditySource`).
pub(crate) struct LiquiditySource {
    channel_manager: Arc<ChannelManager>,
    liquidity_manager: Arc<LiquidityManager>,
    peer_manager: Arc<PeerManager>,
    lsp: LspConfig,
    network: Network,
    node_secret: bitcoin::secp256k1::SecretKey,
    request_timeout: Duration,
    pending_fee_requests: Mutex<HashMap<LSPSRequestId, oneshot::Sender<FeeResult>>>,
    pending_buy_requests: Mutex<HashMap<LSPSRequestId, oneshot::Sender<BuyResult>>>,
    claims: ClaimTracker,
    logger: Arc<Logger>,
}

impl LiquiditySource {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        channel_manager: Arc<ChannelManager>,
        liquidity_manager: Arc<LiquidityManager>,
        peer_manager: Arc<PeerManager>,
        lsp: LspConfig,
        network: Network,
        node_secret: bitcoin::secp256k1::SecretKey,
        request_timeout: Duration,
        logger: Arc<Logger>,
    ) -> Self {
        Self {
            channel_manager,
            liquidity_manager,
            peer_manager,
            lsp,
            network,
            node_secret,
            request_timeout,
            pending_fee_requests: Mutex::new(HashMap::new()),
            pending_buy_requests: Mutex::new(HashMap::new()),
            claims: ClaimTracker::default(),
            logger,
        }
    }

    pub(crate) fn from_components(
        components: &crate::builder::NodeComponents,
        lsp: LspConfig,
        network: Network,
    ) -> Self {
        Self::new(
            Arc::clone(&components.channel_manager),
            Arc::clone(&components.liquidity_manager),
            Arc::clone(&components.peer_manager),
            lsp,
            network,
            components.keys_manager.get_node_secret_key(),
            LSPS2_REQUEST_TIMEOUT,
            Arc::clone(&components.logger),
        )
    }

    /// The full HTD JIT-receive assembly: connect → get_info → select →
    /// buy → invoice. Returns the invoice plus the chosen params'
    /// `valid_until` as UNIX seconds (the surfaced expiry).
    pub(crate) async fn receive_jit(
        &self,
        amount_msat: u64,
    ) -> Result<(Bolt11Invoice, u64), Lsps2Error> {
        self.ensure_lsp_connected().await?;
        self.receive_jit_preconnected(amount_msat).await
    }

    /// [`Self::receive_jit`] minus the connect-on-demand step (split out so
    /// offline tests can drive the flow with fabricated LSPS2 events).
    pub(crate) async fn receive_jit_preconnected(
        &self,
        amount_msat: u64,
    ) -> Result<(Bolt11Invoice, u64), Lsps2Error> {
        let menu = self.request_opening_params().await?;

        let (opening_fee_msat, params) =
            select_cheapest_valid_params(menu, amount_msat, unix_now_secs())?;
        let valid_until_unix_secs = datetime_unix_secs(&params.valid_until);
        log_info!(
            self.logger,
            "Chose cheapest LSPS2 offer: {opening_fee_msat}msat opening fee, valid until {}",
            params.valid_until
        );

        let buy = self.send_buy_request(amount_msat, params).await?;

        // Invoice expiry aligns to the params' remaining validity window.
        let expiry_secs: u32 = valid_until_unix_secs
            .saturating_sub(unix_now_secs())
            .try_into()
            .unwrap_or(u32::MAX);
        let (payment_hash, payment_secret) = self
            .channel_manager
            .create_inbound_payment(
                Some(amount_msat),
                expiry_secs,
                Some(JIT_MIN_FINAL_CLTV_EXPIRY_DELTA),
            )
            .map_err(|()| Lsps2Error::InvoiceCreationFailed)?;

        let invoice = build_jit_invoice(
            &JitInvoiceParams {
                lsp_node_id: self.lsp.node_id,
                intercept_scid: buy.intercept_scid,
                lsp_cltv_expiry_delta: buy.cltv_expiry_delta,
                amount_msat,
                payment_hash,
                payment_secret,
                expiry_secs,
                network: self.network,
                description: JIT_INVOICE_DESCRIPTION.to_string(),
            },
            &self.node_secret,
        )
        .map_err(|e| {
            log_error!(self.logger, "Failed to build the JIT invoice: {e}");
            Lsps2Error::InvoiceCreationFailed
        })?;

        // Arm the claim guard: the LSP may skim exactly the agreed fee.
        self.claims
            .register_expected_fee(payment_hash, opening_fee_msat);

        log_info!(self.logger, "JIT invoice created: {invoice}");
        Ok((invoice, valid_until_unix_secs))
    }

    /// Runs `lsps2.get_info` against the configured LSP and awaits the menu.
    pub(crate) async fn request_opening_params(&self) -> FeeResult {
        let (request_id, receiver) = self.begin_fee_request();
        match tokio::time::timeout(self.request_timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(Lsps2Error::Shutdown),
            Err(_) => {
                self.pending_fee_requests
                    .lock()
                    .unwrap()
                    .remove(&request_id);
                Err(Lsps2Error::RequestTimeout("get_info"))
            }
        }
    }

    async fn send_buy_request(
        &self,
        amount_msat: u64,
        opening_fee_params: LSPS2OpeningFeeParams,
    ) -> BuyResult {
        let (request_id, receiver) = self.begin_buy_request(amount_msat, opening_fee_params)?;
        match tokio::time::timeout(self.request_timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(Lsps2Error::Shutdown),
            Err(_) => {
                self.pending_buy_requests
                    .lock()
                    .unwrap()
                    .remove(&request_id);
                Err(Lsps2Error::RequestTimeout("buy"))
            }
        }
    }

    /// Issues the `get_info` request and parks a oneshot for its response.
    fn begin_fee_request(&self) -> (LSPSRequestId, oneshot::Receiver<FeeResult>) {
        let client_handler = self
            .liquidity_manager
            .lsps2_client_handler()
            .expect("builder always configures the LSPS2 client");
        let (sender, receiver) = oneshot::channel();
        {
            // Park the sender under the lock BEFORE the response can race in.
            let mut pending = self.pending_fee_requests.lock().unwrap();
            let request_id =
                client_handler.request_opening_params(self.lsp.node_id, self.lsp.token.clone());
            pending.insert(request_id.clone(), sender);
            (request_id, receiver)
        }
    }

    /// Issues the `buy` request (fixed amount, KTD-7) and parks a oneshot.
    fn begin_buy_request(
        &self,
        amount_msat: u64,
        opening_fee_params: LSPS2OpeningFeeParams,
    ) -> Result<(LSPSRequestId, oneshot::Receiver<BuyResult>), Lsps2Error> {
        let client_handler = self
            .liquidity_manager
            .lsps2_client_handler()
            .expect("builder always configures the LSPS2 client");
        let (sender, receiver) = oneshot::channel();
        let mut pending = self.pending_buy_requests.lock().unwrap();
        let request_id = client_handler
            .select_opening_params(self.lsp.node_id, Some(amount_msat), opening_fee_params)
            .map_err(|e| {
                log_error!(self.logger, "Failed to send lsps2.buy: {e:?}");
                Lsps2Error::BuyFailed(format!("failed to send buy request: {e:?}"))
            })?;
        pending.insert(request_id.clone(), sender);
        Ok((request_id, receiver))
    }

    /// Routes `LiquidityManager` events into the parked oneshot senders.
    /// Called from the node's liquidity event pump.
    pub(crate) fn handle_liquidity_event(&self, event: LiquidityEvent) {
        match event {
            LiquidityEvent::LSPS2Client(LSPS2ClientEvent::OpeningParametersReady {
                request_id,
                counterparty_node_id,
                opening_fee_params_menu,
            }) => {
                if counterparty_node_id != self.lsp.node_id {
                    log_error!(
                        self.logger,
                        "Ignoring LSPS2 response from unexpected peer {counterparty_node_id}"
                    );
                    return;
                }
                if let Some(sender) = self
                    .pending_fee_requests
                    .lock()
                    .unwrap()
                    .remove(&request_id)
                {
                    let _ = sender.send(Ok(opening_fee_params_menu));
                }
            }
            LiquidityEvent::LSPS2Client(LSPS2ClientEvent::GetInfoFailed {
                request_id,
                error,
                ..
            }) => {
                if let Some(sender) = self
                    .pending_fee_requests
                    .lock()
                    .unwrap()
                    .remove(&request_id)
                {
                    let _ =
                        sender.send(Err(Lsps2Error::GetInfoFailed(describe_lsps_error(&error))));
                }
            }
            LiquidityEvent::LSPS2Client(LSPS2ClientEvent::InvoiceParametersReady {
                request_id,
                counterparty_node_id,
                intercept_scid,
                cltv_expiry_delta,
                ..
            }) => {
                if counterparty_node_id != self.lsp.node_id {
                    log_error!(
                        self.logger,
                        "Ignoring LSPS2 response from unexpected peer {counterparty_node_id}"
                    );
                    return;
                }
                if let Some(sender) = self
                    .pending_buy_requests
                    .lock()
                    .unwrap()
                    .remove(&request_id)
                {
                    let _ = sender.send(Ok(BuyResponse {
                        intercept_scid,
                        cltv_expiry_delta,
                    }));
                }
            }
            LiquidityEvent::LSPS2Client(LSPS2ClientEvent::BuyRequestFailed {
                request_id,
                error,
                ..
            }) => {
                if let Some(sender) = self
                    .pending_buy_requests
                    .lock()
                    .unwrap()
                    .remove(&request_id)
                {
                    let _ = sender.send(Err(Lsps2Error::BuyFailed(describe_lsps_error(&error))));
                }
            }
            other => {
                log_info!(self.logger, "Ignoring unhandled liquidity event: {other:?}");
            }
        }
    }

    /// KTD-9 (copied from ldk-node's `Event::OpenChannelRequest` arm): accept
    /// 0-conf from the configured LSP with the underpaying-HTLC + 100%
    /// in-flight overrides; reject everyone else.
    pub(crate) fn on_open_channel_request(
        &self,
        temporary_channel_id: ChannelId,
        counterparty_node_id: bitcoin::secp256k1::PublicKey,
    ) {
        if counterparty_node_id != self.lsp.node_id {
            log_error!(
                self.logger,
                "Rejecting inbound channel from untrusted peer {counterparty_node_id}"
            );
            self.channel_manager
                .force_close_broadcasting_latest_txn(
                    &temporary_channel_id,
                    &counterparty_node_id,
                    "Channel request rejected".to_string(),
                )
                .unwrap_or_else(|e| {
                    log_error!(self.logger, "Failed to reject channel: {e:?}");
                });
            return;
        }

        // When we're an LSPS2 client, allow claiming underpaying HTLCs as the
        // LSP will skim off some fee. We'll check that they don't take too
        // much before claiming. We also set the maximum allowed inbound HTLC
        // value in flight to 100% (verbatim ldk-node).
        let channel_override_config = Some(ChannelConfigOverrides {
            handshake_overrides: Some(ChannelHandshakeConfigUpdate {
                max_inbound_htlc_value_in_flight_percent_of_channel: Some(100),
                ..Default::default()
            }),
            update_overrides: Some(ChannelConfigUpdate {
                accept_underpaying_htlcs: Some(true),
                ..Default::default()
            }),
        });

        match self
            .channel_manager
            .accept_inbound_channel_from_trusted_peer_0conf(
                &temporary_channel_id,
                &counterparty_node_id,
                0,
                channel_override_config,
            ) {
            Ok(()) => {
                log_info!(
                    self.logger,
                    "Accepted inbound 0conf JIT channel from trusted LSP {counterparty_node_id}"
                );
            }
            Err(e) => {
                log_error!(
                    self.logger,
                    "Failed to accept inbound 0conf channel from {counterparty_node_id}: {e:?}"
                );
            }
        }
    }

    /// Handles `PaymentClaimable`: record the skim, guard it against the
    /// agreed opening fee, then `claim_funds` (idempotent in LDK — a replayed
    /// claimable claims again harmlessly) or fail the HTLC back.
    pub(crate) fn on_payment_claimable(
        &self,
        payment_hash: PaymentHash,
        counterparty_skimmed_fee_msat: u64,
        purpose: &PaymentPurpose,
    ) {
        match self.claims.decide(
            payment_hash,
            counterparty_skimmed_fee_msat,
            purpose.preimage(),
        ) {
            ClaimDecision::Claim(preimage) => {
                log_info!(
                    self.logger,
                    "Claiming payment {} (skimmed fee {counterparty_skimmed_fee_msat}msat)",
                    hex(&payment_hash.0)
                );
                self.channel_manager.claim_funds(preimage);
            }
            ClaimDecision::FailBack(reason) => {
                log_error!(
                    self.logger,
                    "Failing back claimable payment {}: {reason}",
                    hex(&payment_hash.0)
                );
                self.channel_manager.fail_htlc_backwards(&payment_hash);
            }
        }
    }

    /// Consumes the recorded skim for a durably claimed payment (feeds the
    /// public `PaymentReceived { skimmed_fee_msat }`).
    pub(crate) fn take_skim(&self, payment_hash: &PaymentHash) -> Option<u64> {
        self.claims.take_skim(payment_hash)
    }

    /// Connect-on-demand (the node's reconnect loop also keeps the LSP peer
    /// alive; this covers a `receive_jit` racing ahead of it).
    pub(crate) async fn ensure_lsp_connected(&self) -> Result<(), Lsps2Error> {
        if self.is_lsp_connected() {
            return Ok(());
        }
        match lightning_net_tokio::connect_outbound(
            Arc::clone(&self.peer_manager),
            self.lsp.node_id,
            self.lsp.address,
        )
        .await
        {
            Some(connection_closed) => {
                tokio::spawn(connection_closed);
            }
            None => return Err(Lsps2Error::ConnectFailed),
        }
        // Wait for the BOLT8 handshake to complete (list_peers only reports
        // handshake-complete peers).
        let started = Instant::now();
        while started.elapsed() < LSP_CONNECT_TIMEOUT {
            if self.is_lsp_connected() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err(Lsps2Error::ConnectFailed)
    }

    fn is_lsp_connected(&self) -> bool {
        self.peer_manager
            .list_peers()
            .iter()
            .any(|details| details.counterparty_node_id == self.lsp.node_id)
    }

    #[cfg(test)]
    pub(crate) fn pending_fee_request_ids(&self) -> Vec<LSPSRequestId> {
        self.pending_fee_requests
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn pending_buy_request_ids(&self) -> Vec<LSPSRequestId> {
        self.pending_buy_requests
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::str::FromStr;

    use bitcoin::hashes::Hash as _;
    use bitcoin::secp256k1::PublicKey;

    use crate::builder::build;
    use crate::config::{Config, MEGALITH_LSP_NODE_ID};

    // ---------- pure helpers: params fabrication ----------

    fn params(
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

    // ---------- claim tracker: guard + idempotency ----------

    #[test]
    fn claim_decision_claims_when_skim_is_within_the_agreed_fee() {
        let tracker = ClaimTracker::default();
        let hash = PaymentHash([1u8; 32]);
        let preimage = PaymentPreimage([2u8; 32]);
        tracker.register_expected_fee(hash, 5_000);

        assert_eq!(
            tracker.decide(hash, 5_000, Some(preimage)),
            ClaimDecision::Claim(preimage)
        );
        assert_eq!(tracker.take_skim(&hash), Some(5_000));
        assert_eq!(tracker.take_skim(&hash), None, "skim is consumed once");
    }

    #[test]
    fn replayed_claimable_after_unacked_claim_is_tolerated() {
        // KTD idempotency scenario: a crash between claim and ack replays the
        // claimable; the handler must decide identically and not panic.
        let tracker = ClaimTracker::default();
        let hash = PaymentHash([3u8; 32]);
        let preimage = PaymentPreimage([4u8; 32]);
        tracker.register_expected_fee(hash, 1_000);

        let first = tracker.decide(hash, 1_000, Some(preimage));
        let replay = tracker.decide(hash, 1_000, Some(preimage));
        assert_eq!(first, replay);
        assert_eq!(first, ClaimDecision::Claim(preimage));
    }

    #[test]
    fn overskimming_lsp_is_failed_back_not_claimed() {
        let tracker = ClaimTracker::default();
        let hash = PaymentHash([5u8; 32]);
        tracker.register_expected_fee(hash, 1_000);
        assert!(matches!(
            tracker.decide(hash, 1_001, Some(PaymentPreimage([6u8; 32]))),
            ClaimDecision::FailBack(_)
        ));

        // Unknown payment hash: zero skim allowance.
        let unknown = PaymentHash([7u8; 32]);
        assert!(matches!(
            tracker.decide(unknown, 1, Some(PaymentPreimage([6u8; 32]))),
            ClaimDecision::FailBack(_)
        ));
        assert_eq!(
            tracker.decide(unknown, 0, Some(PaymentPreimage([6u8; 32]))),
            ClaimDecision::Claim(PaymentPreimage([6u8; 32]))
        );
    }

    #[test]
    fn missing_preimage_is_failed_back() {
        let tracker = ClaimTracker::default();
        assert!(matches!(
            tracker.decide(PaymentHash([8u8; 32]), 0, None),
            ClaimDecision::FailBack(_)
        ));
    }

    // ---------- wired flow over real components, mocked LSPS2 events ----------

    /// Offline component assembly (same pattern as tests/restart.rs: closed
    /// local port, degraded start with zero monitors is fine).
    fn build_source(dir: &Path, rt: &tokio::runtime::Runtime) -> Arc<LiquiditySource> {
        let mut config = Config::new(dir.to_str().unwrap().to_string());
        config.esplora_url = "http://127.0.0.1:1".to_string();
        config.rgs_url = "http://127.0.0.1:1/snapshot".to_string();
        let components = build(&config, rt).expect("offline build must succeed");
        Arc::new(LiquiditySource::new(
            Arc::clone(&components.channel_manager),
            Arc::clone(&components.liquidity_manager),
            Arc::clone(&components.peer_manager),
            config.lsp.clone(),
            config.network,
            components.keys_manager.get_node_secret_key(),
            Duration::from_millis(200),
            Arc::clone(&components.logger),
        ))
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    async fn wait_for_id(f: impl Fn() -> Vec<LSPSRequestId>) -> LSPSRequestId {
        for _ in 0..500 {
            if let Some(id) = f().into_iter().next() {
                return id;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("request was never registered");
    }

    fn megalith() -> PublicKey {
        PublicKey::from_str(MEGALITH_LSP_NODE_ID).unwrap()
    }

    #[test]
    fn full_jit_flow_with_mocked_lsps2_events_produces_the_wrapped_invoice() {
        let dir = tempfile::tempdir().unwrap();
        let rt = rt();
        let source = build_source(dir.path(), &rt);
        let amount_msat = 250_000_000u64;
        let valid_until = unix_now_secs() + 1_800;
        let intercept_scid = 0x0002_3456_789a_bcdeu64;
        let cltv_expiry_delta = 144u32;

        rt.block_on(async {
            let flow = tokio::spawn({
                let source = Arc::clone(&source);
                async move { source.receive_jit_preconnected(amount_msat).await }
            });

            // get_info leg: answer the parked request with a two-entry menu.
            let fee_id = wait_for_id(|| source.pending_fee_request_ids()).await;
            source.handle_liquidity_event(LiquidityEvent::LSPS2Client(
                LSPS2ClientEvent::OpeningParametersReady {
                    request_id: fee_id,
                    counterparty_node_id: megalith(),
                    opening_fee_params_menu: vec![
                        params(10_000, 0, valid_until, 1_000, u64::MAX),
                        params(2_000, 0, valid_until, 1_000, u64::MAX), // cheapest
                    ],
                },
            ));

            // buy leg: answer with the intercept SCID + CLTV delta.
            let buy_id = wait_for_id(|| source.pending_buy_request_ids()).await;
            source.handle_liquidity_event(LiquidityEvent::LSPS2Client(
                LSPS2ClientEvent::InvoiceParametersReady {
                    request_id: buy_id,
                    counterparty_node_id: megalith(),
                    intercept_scid,
                    cltv_expiry_delta,
                    payment_size_msat: Some(amount_msat),
                },
            ));

            let (invoice, expiry_unix_secs) = flow.await.unwrap().expect("flow must succeed");

            // AE1 assembly half: the invoice wraps the buy response.
            assert_eq!(expiry_unix_secs, valid_until, "expiry surfaces valid_until");
            assert_eq!(invoice.amount_milli_satoshis(), Some(amount_msat));
            let hints = invoice.route_hints();
            assert_eq!(hints.len(), 1);
            assert_eq!(hints[0].0[0].short_channel_id, intercept_scid);
            assert_eq!(hints[0].0[0].src_node_id, megalith());
            assert!(invoice.expiry_time() <= Duration::from_secs(1_800));

            // The claim guard was armed with the cheapest fee.
            let payment_hash = PaymentHash(invoice.payment_hash().to_byte_array());
            assert_eq!(
                source
                    .claims
                    .decide(payment_hash, 2_000, Some(PaymentPreimage([1; 32]))),
                ClaimDecision::Claim(PaymentPreimage([1; 32])),
                "the agreed 2000msat skim must be claimable"
            );
            assert!(matches!(
                source
                    .claims
                    .decide(payment_hash, 2_001, Some(PaymentPreimage([1; 32]))),
                ClaimDecision::FailBack(_)
            ));
        });
    }

    #[test]
    fn fee_floor_fails_before_any_buy_request_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let rt = rt();
        let source = build_source(dir.path(), &rt);
        let amount_msat = 1_000u64;

        rt.block_on(async {
            let flow = tokio::spawn({
                let source = Arc::clone(&source);
                async move { source.receive_jit_preconnected(amount_msat).await }
            });

            let fee_id = wait_for_id(|| source.pending_fee_request_ids()).await;
            source.handle_liquidity_event(LiquidityEvent::LSPS2Client(
                LSPS2ClientEvent::OpeningParametersReady {
                    request_id: fee_id,
                    counterparty_node_id: megalith(),
                    // Opening fee (min 5_000) >= amount (1_000).
                    opening_fee_params_menu: vec![params(
                        5_000,
                        0,
                        unix_now_secs() + 600,
                        1,
                        u64::MAX,
                    )],
                },
            ));

            let err = flow.await.unwrap().unwrap_err();
            assert_eq!(
                err,
                Lsps2Error::OpeningFeeExceedsAmount {
                    opening_fee_msat: 5_000,
                    amount_msat: 1_000,
                }
            );
            assert!(
                source.pending_buy_request_ids().is_empty(),
                "no buy request may be issued after a fee-floor rejection"
            );
        });
    }

    #[test]
    fn get_info_error_response_resolves_the_flow_with_a_distinct_reason() {
        let dir = tempfile::tempdir().unwrap();
        let rt = rt();
        let source = build_source(dir.path(), &rt);

        rt.block_on(async {
            let flow = tokio::spawn({
                let source = Arc::clone(&source);
                async move { source.receive_jit_preconnected(42_000).await }
            });
            let fee_id = wait_for_id(|| source.pending_fee_request_ids()).await;
            source.handle_liquidity_event(LiquidityEvent::LSPS2Client(
                LSPS2ClientEvent::GetInfoFailed {
                    request_id: fee_id,
                    counterparty_node_id: megalith(),
                    error: LSPSResponseError {
                        code: 200,
                        message: "bad token".to_string(),
                        data: None,
                    },
                },
            ));
            let err = flow.await.unwrap().unwrap_err();
            match &err {
                Lsps2Error::GetInfoFailed(reason) => {
                    assert!(reason.contains("200"), "unexpected reason: {reason}");
                    assert!(reason.contains("token"), "unexpected reason: {reason}");
                }
                other => panic!("expected GetInfoFailed, got {other:?}"),
            }
        });
    }

    #[test]
    fn buy_error_response_resolves_the_flow_with_a_distinct_reason() {
        let dir = tempfile::tempdir().unwrap();
        let rt = rt();
        let source = build_source(dir.path(), &rt);
        let amount_msat = 1_000_000u64;

        rt.block_on(async {
            let flow = tokio::spawn({
                let source = Arc::clone(&source);
                async move { source.receive_jit_preconnected(amount_msat).await }
            });
            let fee_id = wait_for_id(|| source.pending_fee_request_ids()).await;
            source.handle_liquidity_event(LiquidityEvent::LSPS2Client(
                LSPS2ClientEvent::OpeningParametersReady {
                    request_id: fee_id,
                    counterparty_node_id: megalith(),
                    opening_fee_params_menu: vec![params(
                        1_000,
                        0,
                        unix_now_secs() + 600,
                        1,
                        u64::MAX,
                    )],
                },
            ));
            let buy_id = wait_for_id(|| source.pending_buy_request_ids()).await;
            source.handle_liquidity_event(LiquidityEvent::LSPS2Client(
                LSPS2ClientEvent::BuyRequestFailed {
                    request_id: buy_id,
                    counterparty_node_id: megalith(),
                    error: LSPSResponseError {
                        code: 201,
                        message: "params expired".to_string(),
                        data: None,
                    },
                },
            ));
            let err = flow.await.unwrap().unwrap_err();
            match &err {
                Lsps2Error::BuyFailed(reason) => {
                    assert!(reason.contains("201"), "unexpected reason: {reason}");
                }
                other => panic!("expected BuyFailed, got {other:?}"),
            }
        });
    }

    #[test]
    fn unanswered_get_info_times_out_with_a_distinct_reason() {
        let dir = tempfile::tempdir().unwrap();
        let rt = rt();
        // 200ms request timeout (set in build_source).
        let source = build_source(dir.path(), &rt);

        let err = rt
            .block_on(source.request_opening_params())
            .expect_err("no LSP is connected, the request must time out");
        assert_eq!(err, Lsps2Error::RequestTimeout("get_info"));
        assert!(
            source.pending_fee_request_ids().is_empty(),
            "the timed-out request must be unparked"
        );
    }

    // ---------- live smoke test (plan-required, network) ----------

    /// ONE real `lsps2.get_info` against Megalith, logging the fee menu.
    /// Run manually: `cargo test -- --ignored live_megalith_get_info`
    #[test]
    #[ignore]
    fn live_megalith_get_info() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::new(dir.path().to_str().unwrap().to_string());
        let node = crate::node::Node::new(config);
        node.start().expect("node must start (needs network)");

        let menu = node
            .lsps2_get_info_live()
            .expect("live get_info against Megalith must succeed");
        eprintln!("Megalith opening_fee_params menu ({} entries):", menu.len());
        for entry in &menu {
            eprintln!(
                "  min_fee_msat={} proportional={}ppm valid_until={} min={}msat max={}msat",
                entry.min_fee_msat,
                entry.proportional,
                entry.valid_until,
                entry.min_payment_size_msat,
                entry.max_payment_size_msat,
            );
        }
        assert!(!menu.is_empty(), "Megalith must offer at least one entry");
        node.stop().unwrap();
    }
}
