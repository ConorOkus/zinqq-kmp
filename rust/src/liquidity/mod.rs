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
//! trusted-LSP set only (U12/KTD-10: a set + predicate, with the shared JIT
//! overrides from `config` so the skimmed opening fee is claimable), and
//! [`ClaimTracker`] guards the skim at `PaymentClaimable` time before
//! `claim_funds`.
//!
//! Module layout: [`selection`] holds the fee-menu selection logic and
//! [`Lsps2Error`]; [`claim`] holds the skim guard; this module owns the
//! [`LiquiditySource`] driver wiring them to the node.

mod claim;
mod selection;

pub use selection::Lsps2Error;

pub(crate) use claim::{ClaimDecision, ClaimTracker};
pub(crate) use selection::{datetime_unix_secs, describe_lsps_error, select_cheapest_valid_params};

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bitcoin::hex::DisplayHex as _;
use bitcoin::Network;
use lightning::events::PaymentPurpose;
use lightning::ln::types::ChannelId;
use lightning::log_error;
use lightning::log_info;
use lightning::types::payment::PaymentHash;
use lightning::util::config::{
    ChannelConfigOverrides, ChannelConfigUpdate, ChannelHandshakeConfigUpdate,
};
use lightning::util::logger::Logger as _;
use lightning_invoice::Bolt11Invoice;
use lightning_liquidity::events::LiquidityEvent;
use lightning_liquidity::lsps0::ser::LSPSRequestId;
use lightning_liquidity::lsps2::event::LSPS2ClientEvent;
use lightning_liquidity::lsps2::msgs::LSPS2OpeningFeeParams;
use tokio::sync::oneshot;

use crate::config::{
    LspConfig, JIT_ACCEPT_UNDERPAYING_HTLCS, JIT_INVOICE_DESCRIPTION, JIT_MAX_INBOUND_INFLIGHT_PCT,
    LSP_CONNECT_TIMEOUT,
};
use crate::invoice::{build_jit_invoice, JitInvoiceParams, JIT_MIN_FINAL_CLTV_EXPIRY_DELTA};
use crate::receive::{
    compute_jit_invoice_expiry_secs, compute_min_receive_sats, quote_fresh_enough, JitQuote,
    MIN_JIT_RECEIVE_SATS,
};
use crate::types::{ChannelManager, LiquidityManager, Logger, PeerManager};
use crate::util::{peer_is_connected, unix_now};

/// The `lsps2.buy` outcome relayed from `InvoiceParametersReady`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BuyResponse {
    pub(crate) intercept_scid: u64,
    pub(crate) cltv_expiry_delta: u32,
}

type FeeResult = Result<Vec<LSPS2OpeningFeeParams>, Lsps2Error>;
type BuyResult = Result<BuyResponse, Lsps2Error>;

/// A quoted-but-not-bought JIT offer (U7 phase A), parked until `jit_accept`
/// consumes its token. No LSP-side commitment exists while it sits here.
struct StoredJitQuote {
    amount_msat: u64,
    opening_fee_msat: u64,
    params: LSPS2OpeningFeeParams,
}

/// A validated, clamp-checked accept, ready for the `buy` leg (U7 phase B).
struct PreparedAccept {
    amount_msat: u64,
    opening_fee_msat: u64,
    params: LSPS2OpeningFeeParams,
    /// Invoice expiry, already clamped per R6 (`valid_until` − 30 s, capped
    /// at 3600 s, ≥ 60 s or the accept was refused).
    expiry_secs: u32,
    /// `now + expiry_secs` — what the QR countdown and the event surface.
    expires_at_unix: u64,
}

/// The client-side LSPS2 driver, owning the pending-request routing and the
/// JIT claim policy (à la ldk-node's `LiquiditySource`).
pub(crate) struct LiquiditySource {
    channel_manager: Arc<ChannelManager>,
    liquidity_manager: Arc<LiquidityManager>,
    peer_manager: Arc<PeerManager>,
    lsp: LspConfig,
    /// LSP node ids trusted for 0-conf inbound channels beyond the configured
    /// LSP (U12/KTD-10: a set + predicate, never a single pubkey compare).
    trusted_lsps: Vec<bitcoin::secp256k1::PublicKey>,
    network: Network,
    node_secret: bitcoin::secp256k1::SecretKey,
    request_timeout: Duration,
    pending_fee_requests: Mutex<HashMap<LSPSRequestId, oneshot::Sender<FeeResult>>>,
    pending_buy_requests: Mutex<HashMap<LSPSRequestId, oneshot::Sender<BuyResult>>>,
    /// Phase-A quotes awaiting `jit_accept`, keyed by their single-use token
    /// (U7). Never bounded: quotes are tiny and a node restart clears them.
    pending_quotes: Mutex<HashMap<u64, StoredJitQuote>>,
    next_quote_token: AtomicU64,
    /// The session-cached JIT numpad floor in sats (U7, R6: ONE amountless
    /// `get_info` per receive session; `refresh` invalidates explicitly).
    jit_floor_cache: Mutex<Option<u64>>,
    claims: ClaimTracker,
    /// Serializes LSP dials so the node's reconnect loop and a `receive_jit`
    /// racing it cannot both open a connection (LDK drops the duplicate, and
    /// an in-flight request can be left on the dropped socket).
    connect_lock: tokio::sync::Mutex<()>,
    logger: Arc<Logger>,
}

impl LiquiditySource {
    pub(crate) fn from_components(
        components: &crate::builder::NodeComponents,
        lsp: LspConfig,
        trusted_lsps: Vec<bitcoin::secp256k1::PublicKey>,
        network: Network,
        request_timeout: Duration,
    ) -> Self {
        Self {
            channel_manager: Arc::clone(&components.channel_manager),
            liquidity_manager: Arc::clone(&components.liquidity_manager),
            peer_manager: Arc::clone(&components.peer_manager),
            lsp,
            trusted_lsps,
            network,
            node_secret: components.keys_manager.get_node_secret_key(),
            request_timeout,
            pending_fee_requests: Mutex::new(HashMap::new()),
            pending_buy_requests: Mutex::new(HashMap::new()),
            pending_quotes: Mutex::new(HashMap::new()),
            next_quote_token: AtomicU64::new(1),
            jit_floor_cache: Mutex::new(None),
            claims: ClaimTracker::default(),
            connect_lock: tokio::sync::Mutex::new(()),
            logger: Arc::clone(&components.logger),
        }
    }

    /// The full one-shot JIT-receive assembly (spike compat): connect →
    /// quote (get_info + select) → accept (clamp + buy + invoice). Returns
    /// the invoice plus its clamped expiry as UNIX seconds (R6:
    /// `valid_until` − 30 s flight margin, capped at 3600 s).
    pub(crate) async fn receive_jit(
        &self,
        amount_msat: u64,
    ) -> Result<(Bolt11Invoice, u64), Lsps2Error> {
        self.ensure_lsp_connected().await?;
        self.receive_jit_preconnected(amount_msat).await
    }

    /// [`Self::receive_jit`] minus the connect-on-demand step (split out so
    /// offline tests can drive the flow with fabricated LSPS2 events).
    /// Composes the two U7 phases back into the one-shot flow.
    pub(crate) async fn receive_jit_preconnected(
        &self,
        amount_msat: u64,
    ) -> Result<(Bolt11Invoice, u64), Lsps2Error> {
        let quote = self.jit_quote_preconnected(amount_msat).await?;
        let (invoice, expires_at_unix, _opening_fee_msat) = self
            .jit_accept_preconnected(quote.quote_token, amount_msat)
            .await?;
        Ok((invoice, expires_at_unix))
    }

    /// U7 phase A (F2 "quote" step): `get_info` + cheapest-valid selection,
    /// NO `buy` — a pure read the review screen can discard freely. Every
    /// selection failure (below/above the menu's payment-size window, fee ≥
    /// amount — AE4's core gate) is a typed error raised HERE, before any
    /// LSP-side commitment.
    pub(crate) async fn jit_quote(&self, amount_msat: u64) -> Result<JitQuote, Lsps2Error> {
        self.ensure_lsp_connected().await?;
        self.jit_quote_preconnected(amount_msat).await
    }

    /// [`Self::jit_quote`] minus the connect-on-demand step (offline tests).
    pub(crate) async fn jit_quote_preconnected(
        &self,
        amount_msat: u64,
    ) -> Result<JitQuote, Lsps2Error> {
        let menu = self.request_opening_params().await?;
        let now_unix_secs = unix_now().as_secs();
        let (opening_fee_msat, params) =
            select_cheapest_valid_params(menu, amount_msat, now_unix_secs)?;
        let valid_until_unix = datetime_unix_secs(&params.valid_until);
        log_info!(
            self.logger,
            "Chose cheapest LSPS2 offer: {opening_fee_msat}msat opening fee, valid until {}",
            params.valid_until
        );

        let quote_token = self.next_quote_token.fetch_add(1, Ordering::Relaxed);
        self.pending_quotes.lock().unwrap().insert(
            quote_token,
            StoredJitQuote {
                amount_msat,
                opening_fee_msat,
                params,
            },
        );
        Ok(JitQuote {
            quote_token,
            amount_msat,
            opening_fee_msat,
            receive_msat: amount_msat.saturating_sub(opening_fee_msat),
            valid_until_unix,
            fresh_enough: quote_fresh_enough(valid_until_unix, now_unix_secs),
        })
    }

    /// U7 phase B (F2 "buy" step): consumes the quote token, clamps the
    /// invoice expiry to the quote's remaining validity BEFORE issuing the
    /// `buy` (throwing here costs nothing — no reservation exists yet), then
    /// buys and assembles the wrapped invoice. Returns the invoice, its
    /// expiry as UNIX seconds, and the agreed opening fee.
    pub(crate) async fn jit_accept(
        &self,
        quote_token: u64,
        amount_msat: u64,
    ) -> Result<(Bolt11Invoice, u64, u64), Lsps2Error> {
        let prepared = self.prepare_accept(quote_token, amount_msat)?;
        self.ensure_lsp_connected().await?;
        self.finish_accept(prepared).await
    }

    /// [`Self::jit_accept`] minus the connect-on-demand step (offline tests).
    pub(crate) async fn jit_accept_preconnected(
        &self,
        quote_token: u64,
        amount_msat: u64,
    ) -> Result<(Bolt11Invoice, u64, u64), Lsps2Error> {
        let prepared = self.prepare_accept(quote_token, amount_msat)?;
        self.finish_accept(prepared).await
    }

    /// The pre-`buy` validation half of an accept: consume the (single-use)
    /// token, bind the amount to the quoted one, and apply the R6 expiry
    /// clamp — under 60 s of payable life is the typed
    /// [`Lsps2Error::QuoteExpired`] re-quote signal, raised before anything
    /// leaves the device.
    fn prepare_accept(
        &self,
        quote_token: u64,
        amount_msat: u64,
    ) -> Result<PreparedAccept, Lsps2Error> {
        let stored = self
            .pending_quotes
            .lock()
            .unwrap()
            .remove(&quote_token)
            .ok_or(Lsps2Error::QuoteNotFound)?;
        if stored.amount_msat != amount_msat {
            return Err(Lsps2Error::QuoteAmountMismatch {
                quoted_msat: stored.amount_msat,
                requested_msat: amount_msat,
            });
        }
        let now_unix_secs = unix_now().as_secs();
        let expiry_secs = compute_jit_invoice_expiry_secs(
            datetime_unix_secs(&stored.params.valid_until),
            now_unix_secs,
        )?;
        Ok(PreparedAccept {
            amount_msat,
            opening_fee_msat: stored.opening_fee_msat,
            params: stored.params,
            expiry_secs,
            expires_at_unix: now_unix_secs + expiry_secs as u64,
        })
    }

    /// The `buy` + invoice-assembly half of an accept.
    async fn finish_accept(
        &self,
        prepared: PreparedAccept,
    ) -> Result<(Bolt11Invoice, u64, u64), Lsps2Error> {
        let buy = self
            .send_buy_request(prepared.amount_msat, prepared.params)
            .await?;

        let (payment_hash, payment_secret) = self
            .channel_manager
            .create_inbound_payment(
                Some(prepared.amount_msat),
                prepared.expiry_secs,
                Some(JIT_MIN_FINAL_CLTV_EXPIRY_DELTA),
            )
            .map_err(|()| Lsps2Error::InvoiceCreationFailed)?;

        let invoice = build_jit_invoice(
            &JitInvoiceParams {
                lsp_node_id: self.lsp.node_id,
                intercept_scid: buy.intercept_scid,
                lsp_cltv_expiry_delta: buy.cltv_expiry_delta,
                amount_msat: prepared.amount_msat,
                payment_hash,
                payment_secret,
                expiry_secs: prepared.expiry_secs,
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
            .register_expected_fee(payment_hash, prepared.opening_fee_msat);

        log_info!(self.logger, "JIT invoice created: {invoice}");
        Ok((invoice, prepared.expires_at_unix, prepared.opening_fee_msat))
    }

    /// The session's JIT numpad floor in sats (U7, R6, AE4): ONE amountless
    /// `get_info` per receive session, cached until `refresh` explicitly
    /// invalidates it. NEVER errors — any failure (connect, RPC, timeout) and
    /// any empty/expired menu degrade to the static
    /// [`MIN_JIT_RECEIVE_SATS`], which is also cached so a failed session
    /// does not re-dial on every numpad render (the PWA's settled-attempt
    /// semantics, `Receive.tsx:157`).
    pub(crate) async fn min_receive_sats(&self, refresh: bool) -> u64 {
        self.min_receive_sats_inner(refresh, true).await
    }

    /// [`Self::min_receive_sats`] minus the connect-on-demand step (tests).
    #[cfg(test)]
    pub(crate) async fn min_receive_sats_preconnected(&self, refresh: bool) -> u64 {
        self.min_receive_sats_inner(refresh, false).await
    }

    async fn min_receive_sats_inner(&self, refresh: bool, connect: bool) -> u64 {
        if refresh {
            *self.jit_floor_cache.lock().unwrap() = None;
        } else if let Some(cached) = self.cached_jit_floor_sats() {
            return cached;
        }
        let fetched = async {
            if connect {
                self.ensure_lsp_connected().await?;
            }
            let menu = self.request_opening_params().await?;
            Ok::<u64, Lsps2Error>(compute_min_receive_sats(&menu, unix_now().as_secs()))
        }
        .await;
        let floor = match fetched {
            // 0 means an empty/degenerate (all-expired) menu — static floor.
            Ok(live) if live > 0 => live,
            Ok(_) => MIN_JIT_RECEIVE_SATS,
            Err(e) => {
                log_error!(
                    self.logger,
                    "Live JIT floor fetch failed, using the static floor: {e}"
                );
                MIN_JIT_RECEIVE_SATS
            }
        };
        *self.jit_floor_cache.lock().unwrap() = Some(floor);
        floor
    }

    /// The session-cached floor without any network touch (the
    /// `receive_bundle` read; `None` before the first settled fetch).
    pub(crate) fn cached_jit_floor_sats(&self) -> Option<u64> {
        *self.jit_floor_cache.lock().unwrap()
    }

    /// Runs `lsps2.get_info` against the configured LSP and awaits the menu.
    pub(crate) async fn request_opening_params(&self) -> FeeResult {
        let (request_id, receiver) = self.begin_fee_request();
        self.await_response(&self.pending_fee_requests, request_id, receiver, "get_info")
            .await
    }

    async fn send_buy_request(
        &self,
        amount_msat: u64,
        opening_fee_params: LSPS2OpeningFeeParams,
    ) -> BuyResult {
        let (request_id, receiver) = self.begin_buy_request(amount_msat, opening_fee_params)?;
        self.await_response(&self.pending_buy_requests, request_id, receiver, "buy")
            .await
    }

    /// Awaits a parked LSPS2 response with the request timeout: a dropped
    /// sender means shutdown; a timeout unparks the pending request and names
    /// the `phase` (`"get_info"` / `"buy"`) in the error.
    async fn await_response<T>(
        &self,
        pending: &Mutex<HashMap<LSPSRequestId, oneshot::Sender<Result<T, Lsps2Error>>>>,
        request_id: LSPSRequestId,
        receiver: oneshot::Receiver<Result<T, Lsps2Error>>,
        phase: &'static str,
    ) -> Result<T, Lsps2Error> {
        match tokio::time::timeout(self.request_timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(Lsps2Error::Shutdown),
            Err(_) => {
                pending.lock().unwrap().remove(&request_id);
                Err(Lsps2Error::RequestTimeout(phase))
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

    /// Whether `node_id` may open 0-conf channels to us (U12/KTD-10): the
    /// configured LSP or any member of the trusted set — the same semantics
    /// as `Config::is_trusted_lsp` (the set is handed over at construction).
    pub(crate) fn is_trusted_lsp(&self, node_id: &bitcoin::secp256k1::PublicKey) -> bool {
        *node_id == self.lsp.node_id || self.trusted_lsps.contains(node_id)
    }

    /// KTD-9 (copied from ldk-node's `Event::OpenChannelRequest` arm): accept
    /// 0-conf from the trusted-LSP set with the underpaying-HTLC + 100%
    /// in-flight overrides (the shared KTD-10 JIT constants); reject everyone
    /// else.
    pub(crate) fn on_open_channel_request(
        &self,
        temporary_channel_id: ChannelId,
        counterparty_node_id: bitcoin::secp256k1::PublicKey,
    ) {
        if !self.is_trusted_lsp(&counterparty_node_id) {
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
        // value in flight to 100%. Both values come from the shared KTD-10
        // constants in `config`, the same source `default_user_config` reads,
        // so the per-channel override can never drift from the global default.
        let channel_override_config = Some(ChannelConfigOverrides {
            handshake_overrides: Some(ChannelHandshakeConfigUpdate {
                max_inbound_htlc_value_in_flight_percent_of_channel: Some(
                    JIT_MAX_INBOUND_INFLIGHT_PCT,
                ),
                ..Default::default()
            }),
            update_overrides: Some(ChannelConfigUpdate {
                accept_underpaying_htlcs: Some(JIT_ACCEPT_UNDERPAYING_HTLCS),
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
                    payment_hash.0.to_lower_hex_string()
                );
                self.channel_manager.claim_funds(preimage);
            }
            ClaimDecision::FailBack(reason) => {
                log_error!(
                    self.logger,
                    "Failing back claimable payment {}: {reason}",
                    payment_hash.0.to_lower_hex_string()
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
        // One dial at a time: whoever loses the race re-checks and returns
        // instead of opening a duplicate connection.
        let _dialing = self.connect_lock.lock().await;
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

    pub(crate) fn logger(&self) -> &Arc<Logger> {
        &self.logger
    }

    fn is_lsp_connected(&self) -> bool {
        peer_is_connected(&self.peer_manager, self.lsp.node_id)
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

#[cfg(test)]
mod tests {
    use super::selection::params;
    use super::*;
    use std::path::Path;
    use std::str::FromStr;

    use bitcoin::hashes::Hash as _;
    use bitcoin::secp256k1::PublicKey;
    use lightning::types::payment::PaymentPreimage;
    use lightning_liquidity::lsps0::ser::LSPSResponseError;

    use crate::builder::build;
    use crate::config::{Config, MEGALITH_LSP_NODE_ID};

    // ---------- wired flow over real components, mocked LSPS2 events ----------

    /// Offline component assembly (same pattern as tests/restart.rs: closed
    /// local port, degraded start with zero monitors is fine).
    fn build_source(dir: &Path, rt: &tokio::runtime::Runtime) -> Arc<LiquiditySource> {
        let mut config = Config::new(dir.to_str().unwrap().to_string());
        config.esplora_url = "http://127.0.0.1:1".to_string();
        config.rgs_url = "http://127.0.0.1:1/snapshot".to_string();
        config.vss_disabled = true;
        let components = build(&config, rt, Arc::new(crate::node::LoggingEventSink::new()))
            .expect("offline build must succeed");
        Arc::new(LiquiditySource::from_components(
            &components,
            config.lsp.clone(),
            config.trusted_lsps.clone(),
            config.network,
            Duration::from_millis(200),
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

    /// U12/KTD-10: the 0-conf gate consults the trusted-LSP set, not a single
    /// pubkey compare — a set member that is NOT the configured LSP passes.
    #[test]
    fn trusted_lsp_predicate_gates_the_zero_conf_accept_path() {
        let dir = tempfile::tempdir().unwrap();
        let rt = rt();
        let extra_trusted = PublicKey::from_str(
            "02eec7245d6b7d2ccb30380bfbe2a3648cd7a942653f5aa340edcea1f283686619",
        )
        .unwrap();
        let stranger = PublicKey::from_str(
            "03864ef025fde8fb587d989186ce6a4a186895ee44a926bfc370e2c366597a3f8f",
        )
        .unwrap();

        let mut config = Config::new(dir.path().to_str().unwrap().to_string());
        config.esplora_url = "http://127.0.0.1:1".to_string();
        config.rgs_url = "http://127.0.0.1:1/snapshot".to_string();
        config.vss_disabled = true;
        config.trusted_lsps.push(extra_trusted);
        let components = build(&config, &rt, Arc::new(crate::node::LoggingEventSink::new()))
            .expect("offline build must succeed");
        let source = LiquiditySource::from_components(
            &components,
            config.lsp.clone(),
            config.trusted_lsps.clone(),
            config.network,
            Duration::from_millis(200),
        );

        assert!(source.is_trusted_lsp(&megalith()), "configured LSP");
        assert!(source.is_trusted_lsp(&extra_trusted), "trusted-set member");
        assert!(!source.is_trusted_lsp(&stranger), "unknown peer");
    }

    #[test]
    fn full_jit_flow_with_mocked_lsps2_events_produces_the_wrapped_invoice() {
        let dir = tempfile::tempdir().unwrap();
        let rt = rt();
        let source = build_source(dir.path(), &rt);
        let amount_msat = 250_000_000u64;
        let valid_until = unix_now().as_secs() + 1_800;
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
            // U7/R6 clamp: the surfaced expiry is the quote's validity minus
            // the 30 s flight margin (`expires_at = now + (valid_until - now
            // - 30)` — deterministic below the 3600 s cap).
            assert_eq!(
                expiry_unix_secs,
                valid_until - 30,
                "expiry surfaces the clamped invoice expiry"
            );
            assert_eq!(invoice.amount_milli_satoshis(), Some(amount_msat));
            let hints = invoice.route_hints();
            assert_eq!(hints.len(), 1);
            assert_eq!(hints[0].0[0].short_channel_id, intercept_scid);
            assert_eq!(hints[0].0[0].src_node_id, megalith());
            assert!(invoice.expiry_time() <= Duration::from_secs(1_770));

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

    /// U7 two-phase flow: phase A (`jit_quote`) exposes fee, net amount,
    /// validity, and freshness WITHOUT a buy; phase B (`jit_accept`) buys and
    /// mints an invoice whose expiry is clamped per R6; the token is
    /// single-use.
    #[test]
    fn two_phase_quote_exposes_the_fee_then_accept_clamps_the_invoice_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let rt = rt();
        let source = build_source(dir.path(), &rt);
        let amount_msat = 250_000_000u64;
        let valid_until = unix_now().as_secs() + 1_800;
        let intercept_scid = 0x0002_3456_789a_bcdeu64;

        rt.block_on(async {
            // Phase A: quote only — no buy request may exist afterwards.
            let quote_flow = tokio::spawn({
                let source = Arc::clone(&source);
                async move { source.jit_quote_preconnected(amount_msat).await }
            });
            let fee_id = wait_for_id(|| source.pending_fee_request_ids()).await;
            source.handle_liquidity_event(LiquidityEvent::LSPS2Client(
                LSPS2ClientEvent::OpeningParametersReady {
                    request_id: fee_id,
                    counterparty_node_id: megalith(),
                    opening_fee_params_menu: vec![params(2_000, 0, valid_until, 1_000, u64::MAX)],
                },
            ));
            let quote = quote_flow.await.unwrap().expect("quote must succeed");
            assert_eq!(quote.amount_msat, amount_msat);
            assert_eq!(quote.opening_fee_msat, 2_000);
            assert_eq!(quote.receive_msat, amount_msat - 2_000);
            assert_eq!(quote.valid_until_unix, valid_until);
            assert!(quote.fresh_enough, "1800s of validity is fresh");
            assert!(
                source.pending_buy_request_ids().is_empty(),
                "phase A must never issue a buy"
            );

            // Phase B: accept consumes the token, clamps, buys, mints.
            let accept_flow = tokio::spawn({
                let source = Arc::clone(&source);
                let token = quote.quote_token;
                async move { source.jit_accept_preconnected(token, amount_msat).await }
            });
            let buy_id = wait_for_id(|| source.pending_buy_request_ids()).await;
            source.handle_liquidity_event(LiquidityEvent::LSPS2Client(
                LSPS2ClientEvent::InvoiceParametersReady {
                    request_id: buy_id,
                    counterparty_node_id: megalith(),
                    intercept_scid,
                    cltv_expiry_delta: 144,
                    payment_size_msat: Some(amount_msat),
                },
            ));
            let (invoice, expires_at_unix, opening_fee_msat) =
                accept_flow.await.unwrap().expect("accept must succeed");
            assert_eq!(opening_fee_msat, 2_000);
            assert_eq!(
                expires_at_unix,
                valid_until - 30,
                "R6 clamp: valid_until minus the 30s flight margin"
            );
            assert!(invoice.expiry_time() <= Duration::from_secs(1_770));
            assert_eq!(invoice.amount_milli_satoshis(), Some(amount_msat));
            assert_eq!(
                invoice.route_hints()[0].0[0].short_channel_id,
                intercept_scid
            );

            // The token is single-use: a second accept is refused with a
            // typed error and no buy leaves the device.
            let replay = source
                .jit_accept_preconnected(quote.quote_token, amount_msat)
                .await;
            assert_eq!(replay.unwrap_err(), Lsps2Error::QuoteNotFound);
            assert!(source.pending_buy_request_ids().is_empty());
        });
    }

    /// R6 re-quote signal: a quote whose remaining validity leaves under 60 s
    /// of payable invoice life is refused at accept time, BEFORE any `buy` —
    /// no LSP-side reservation is ever orphaned by a doomed QR.
    #[test]
    fn stale_quote_accept_demands_a_requote_before_any_buy() {
        let dir = tempfile::tempdir().unwrap();
        let rt = rt();
        let source = build_source(dir.path(), &rt);
        let amount_msat = 250_000_000u64;
        // 80 s of validity: fresh enough to quote (≥ 30 s) but only 50 s of
        // clamped life (< 60 s) — exactly the review-then-expire window.
        let valid_until = unix_now().as_secs() + 80;

        rt.block_on(async {
            let quote_flow = tokio::spawn({
                let source = Arc::clone(&source);
                async move { source.jit_quote_preconnected(amount_msat).await }
            });
            let fee_id = wait_for_id(|| source.pending_fee_request_ids()).await;
            source.handle_liquidity_event(LiquidityEvent::LSPS2Client(
                LSPS2ClientEvent::OpeningParametersReady {
                    request_id: fee_id,
                    counterparty_node_id: megalith(),
                    opening_fee_params_menu: vec![params(2_000, 0, valid_until, 1_000, u64::MAX)],
                },
            ));
            let quote = quote_flow
                .await
                .unwrap()
                .expect("the quote itself succeeds");
            assert!(quote.fresh_enough, "80s away is still fresh at quote time");

            let err = source
                .jit_accept_preconnected(quote.quote_token, amount_msat)
                .await
                .unwrap_err();
            assert_eq!(err, Lsps2Error::QuoteExpired, "the typed re-quote signal");
            assert!(
                source.pending_buy_request_ids().is_empty(),
                "the clamp must fire before any buy request"
            );
        });
    }

    /// The accept amount is bound to the quoted one (the LSP-signed promise
    /// covers exactly that payment size).
    #[test]
    fn accept_amount_mismatch_is_typed_and_pre_buy() {
        let dir = tempfile::tempdir().unwrap();
        let rt = rt();
        let source = build_source(dir.path(), &rt);
        let amount_msat = 250_000_000u64;

        rt.block_on(async {
            let quote_flow = tokio::spawn({
                let source = Arc::clone(&source);
                async move { source.jit_quote_preconnected(amount_msat).await }
            });
            let fee_id = wait_for_id(|| source.pending_fee_request_ids()).await;
            source.handle_liquidity_event(LiquidityEvent::LSPS2Client(
                LSPS2ClientEvent::OpeningParametersReady {
                    request_id: fee_id,
                    counterparty_node_id: megalith(),
                    opening_fee_params_menu: vec![params(
                        2_000,
                        0,
                        unix_now().as_secs() + 1_800,
                        1_000,
                        u64::MAX,
                    )],
                },
            ));
            let quote = quote_flow.await.unwrap().unwrap();

            let err = source
                .jit_accept_preconnected(quote.quote_token, amount_msat + 1)
                .await
                .unwrap_err();
            assert_eq!(
                err,
                Lsps2Error::QuoteAmountMismatch {
                    quoted_msat: amount_msat,
                    requested_msat: amount_msat + 1,
                }
            );
            assert!(source.pending_buy_request_ids().is_empty());
        });
    }

    /// AE4 (core half): a below-floor amount is refused by phase A with a
    /// typed error and NO buy is ever issued — both the payment-size branch
    /// and the net-positive (fee ≥ amount) branch.
    #[test]
    fn below_floor_quote_is_refused_before_any_buy() {
        let dir = tempfile::tempdir().unwrap();
        let rt = rt();
        let source = build_source(dir.path(), &rt);
        let valid_until = unix_now().as_secs() + 1_800;

        rt.block_on(async {
            // Branch 1: below every entry's min_payment_size_msat.
            let flow = tokio::spawn({
                let source = Arc::clone(&source);
                async move { source.jit_quote_preconnected(1_000_000).await }
            });
            let fee_id = wait_for_id(|| source.pending_fee_request_ids()).await;
            source.handle_liquidity_event(LiquidityEvent::LSPS2Client(
                LSPS2ClientEvent::OpeningParametersReady {
                    request_id: fee_id,
                    counterparty_node_id: megalith(),
                    opening_fee_params_menu: vec![params(
                        1_000,
                        0,
                        valid_until,
                        10_000_000,
                        u64::MAX,
                    )],
                },
            ));
            assert_eq!(
                flow.await.unwrap().unwrap_err(),
                Lsps2Error::AmountBelowMinimum {
                    amount_msat: 1_000_000,
                    min_payment_size_msat: 10_000_000,
                }
            );
            assert!(source.pending_buy_request_ids().is_empty());

            // Branch 2: inside the window but the fee would swallow the
            // amount (floor = min_fee + 1 not reached).
            let flow = tokio::spawn({
                let source = Arc::clone(&source);
                async move { source.jit_quote_preconnected(1_500_000).await }
            });
            let fee_id = wait_for_id(|| source.pending_fee_request_ids()).await;
            source.handle_liquidity_event(LiquidityEvent::LSPS2Client(
                LSPS2ClientEvent::OpeningParametersReady {
                    request_id: fee_id,
                    counterparty_node_id: megalith(),
                    opening_fee_params_menu: vec![params(2_000_000, 0, valid_until, 1, u64::MAX)],
                },
            ));
            assert_eq!(
                flow.await.unwrap().unwrap_err(),
                Lsps2Error::OpeningFeeExceedsAmount {
                    opening_fee_msat: 2_000_000,
                    amount_msat: 1_500_000,
                }
            );
            assert!(
                source.pending_buy_request_ids().is_empty(),
                "AE4: no buy may be issued for a below-floor request"
            );
        });
    }

    /// U7/R6 floor session-caching: one amountless `get_info` per session,
    /// cached until an explicit refresh; empty menus and failures degrade to
    /// the static 3,000-sat floor.
    #[test]
    fn jit_floor_is_fetched_once_per_session_and_refresh_refetches() {
        let dir = tempfile::tempdir().unwrap();
        let rt = rt();
        let source = build_source(dir.path(), &rt);
        // Megalith-shaped entry: 2,500 sats min fee → floor 2,501 sats.
        let menu_entry = params(2_500_000, 0, unix_now().as_secs() + 1_800, 1_000, u64::MAX);

        rt.block_on(async {
            assert_eq!(source.cached_jit_floor_sats(), None, "fresh session");

            // First read fetches: computeMinReceiveSats parity → 2,501.
            let flow = tokio::spawn({
                let source = Arc::clone(&source);
                async move { source.min_receive_sats_preconnected(false).await }
            });
            let fee_id = wait_for_id(|| source.pending_fee_request_ids()).await;
            source.handle_liquidity_event(LiquidityEvent::LSPS2Client(
                LSPS2ClientEvent::OpeningParametersReady {
                    request_id: fee_id,
                    counterparty_node_id: megalith(),
                    opening_fee_params_menu: vec![menu_entry.clone()],
                },
            ));
            assert_eq!(flow.await.unwrap(), 2_501);
            assert_eq!(source.cached_jit_floor_sats(), Some(2_501));

            // Second read answers from the cache: no request is parked.
            assert_eq!(source.min_receive_sats_preconnected(false).await, 2_501);
            assert!(
                source.pending_fee_request_ids().is_empty(),
                "a cached session must not re-issue get_info"
            );

            // Refresh invalidates and refetches; an EMPTY menu degrades to
            // the static floor (and is itself cached as settled).
            let flow = tokio::spawn({
                let source = Arc::clone(&source);
                async move { source.min_receive_sats_preconnected(true).await }
            });
            let fee_id = wait_for_id(|| source.pending_fee_request_ids()).await;
            source.handle_liquidity_event(LiquidityEvent::LSPS2Client(
                LSPS2ClientEvent::OpeningParametersReady {
                    request_id: fee_id,
                    counterparty_node_id: megalith(),
                    opening_fee_params_menu: Vec::new(),
                },
            ));
            assert_eq!(flow.await.unwrap(), MIN_JIT_RECEIVE_SATS);
            assert_eq!(source.cached_jit_floor_sats(), Some(MIN_JIT_RECEIVE_SATS));

            // A refresh whose request dies (timeout, 200ms in build_source)
            // also degrades to the static floor instead of erroring.
            assert_eq!(
                source.min_receive_sats_preconnected(true).await,
                MIN_JIT_RECEIVE_SATS,
                "AE4 fallback: any failure yields the static 3,000-sat floor"
            );
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
                        unix_now().as_secs() + 600,
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
                        unix_now().as_secs() + 600,
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

    /// The FULL live receive flow against Megalith: `get_info` -> select
    /// cheapest valid params -> `buy` -> build the wrapped invoice. Proves F1's
    /// client half end to end with real LSP params (no funds move; the invoice
    /// simply expires unpaid).
    /// Run manually: `cargo test -- --ignored live_megalith_receive_jit`
    #[test]
    #[ignore]
    fn live_megalith_receive_jit() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::new(dir.path().to_str().unwrap().to_string());
        let node = crate::node::Node::new(config);
        node.start().expect("node must start (needs network)");

        // Comfortably above Megalith's observed 2_501_000 msat minimum.
        let (bolt11, expiry) = node
            .receive_jit(6_000_000)
            .expect("live receive_jit against Megalith must succeed");
        eprintln!("JIT invoice (expires {expiry}):\n{bolt11}");
        assert!(
            bolt11.starts_with("lnbc"),
            "must be a mainnet bolt11, got {bolt11}"
        );
        node.stop().unwrap();
    }

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
