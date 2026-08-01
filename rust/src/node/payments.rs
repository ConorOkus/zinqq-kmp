//! `Node`'s payment surfaces: JIT receive, BOLT12 offers, async receive,
//! standard invoices, and the outbound send/pay paths — plus the two private
//! helpers that keep the history row and the LDK attempt in step.
//!
//! Split out of `node.rs` (see that module's header) so the fund-safety
//! lifecycle stays legible. Pure move: no behavior, signature, or visibility
//! change beyond `pub(super)` on the helpers `node.rs` itself calls.

use std::sync::Arc;

use bitcoin::hashes::Hash as _;
use lightning::ln::channelmanager::PaymentId;
use lightning::log_error;
use lightning::sign::EntropySource as _;
use lightning::util::logger::Logger as _;

use crate::history::{PaymentDirection, PaymentStatus};
use crate::liquidity::{LiquiditySource, Lsps2Error};
use crate::node::{spawn_and_wait, CoreEvent, Node};
use crate::payment::{
    parse_and_validate, payment_id_for, resolve_amount, send_bolt11, send_bolt12, validate_offer,
    SendError,
};
use crate::receive::AsyncReceiveView;
use crate::types::Logger;
use crate::util::{hex_str, now_ms, unix_now};

impl Node {
    /// Requests a JIT invoice for `amount_msat` from the configured LSP,
    /// driving connect → get_info → buy → invoice in one blocking call (call
    /// from a background dispatcher, like `start`). On success the invoice is
    /// ALSO pushed as `InvoiceReady`; every failure is pushed as
    /// `Lsps2Failed` with a distinct reason.
    pub fn receive_jit(&self, amount_msat: u64) -> Result<(String, u64), Lsps2Error> {
        let (liquidity_source, runtime_handle) = self.liquidity_handles()?;

        // Run the flow on the node runtime and wait outside the state lock,
        // so a concurrent stop() can't deadlock; a dropped runtime surfaces
        // as a closed channel, not a hang.
        let result = spawn_and_wait(&runtime_handle, async move {
            liquidity_source.receive_jit(amount_msat).await
        })
        .unwrap_or(Err(Lsps2Error::Shutdown));

        match result {
            Ok((invoice, expiry_unix_secs)) => {
                let bolt11 = invoice.to_string();
                self.event_sink.emit(CoreEvent::InvoiceReady {
                    bolt11: bolt11.clone(),
                    expiry_unix_secs,
                });
                Ok((bolt11, expiry_unix_secs))
            }
            Err(error) => {
                self.event_sink.emit(CoreEvent::Lsps2Failed {
                    reason: error.to_string(),
                });
                Err(error)
            }
        }
    }

    /// Clones the LSPS2 handles out of the state lock (the receive_jit
    /// pattern: spawn on the node runtime, wait outside the lock).
    fn liquidity_handles(
        &self,
    ) -> Result<(Arc<LiquiditySource>, tokio::runtime::Handle), Lsps2Error> {
        let state_lock = self.state.lock().unwrap();
        let state = state_lock.as_ref().ok_or(Lsps2Error::NotRunning)?;
        Ok((
            Arc::clone(&state.liquidity_source),
            state.runtime.handle().clone(),
        ))
    }

    /// U7 phase A (F2 quote step): `get_info` + cheapest-valid selection
    /// against the configured LSP — NO `buy`, no LSP-side commitment. The
    /// quote carries fee/net/validity/freshness for the review screen and a
    /// single-use token for [`Node::jit_accept`]. AE4: below-floor amounts
    /// fail here with a typed error, so no buy can ever follow them.
    /// Blocking (LSP round-trip): call from a background dispatcher.
    ///
    /// Quote failures return typed errors WITHOUT queueing `Lsps2Failed`:
    /// below-minimum is a review-screen state in the PWA, not an incident.
    pub fn jit_quote(&self, amount_msat: u64) -> Result<crate::receive::JitQuote, Lsps2Error> {
        let (liquidity_source, runtime_handle) = self.liquidity_handles()?;
        spawn_and_wait(&runtime_handle, async move {
            liquidity_source.jit_quote(amount_msat).await
        })
        .unwrap_or(Err(Lsps2Error::Shutdown))
    }

    /// U7 phase B (F2 buy step): consumes the quote token, clamps the
    /// invoice expiry to the quote's remaining validity (R6: `valid_until`
    /// − 30 s, capped at 3600 s, under 60 s → the typed
    /// [`Lsps2Error::QuoteExpired`] re-quote signal BEFORE any `buy`), then
    /// buys and mints the wrapped invoice. On success the invoice is ALSO
    /// pushed as `InvoiceReady` with the clamped expiry; failures are pushed
    /// as `Lsps2Failed`. Blocking: call from a background dispatcher.
    pub fn jit_accept(
        &self,
        quote_token: u64,
        amount_msat: u64,
    ) -> Result<crate::receive::JitInvoice, Lsps2Error> {
        let (liquidity_source, runtime_handle) = self.liquidity_handles()?;
        let result = spawn_and_wait(&runtime_handle, async move {
            liquidity_source.jit_accept(quote_token, amount_msat).await
        })
        .unwrap_or(Err(Lsps2Error::Shutdown));

        match result {
            Ok((invoice, expires_at_unix, opening_fee_msat)) => {
                let bolt11 = invoice.to_string();
                self.event_sink.emit(CoreEvent::InvoiceReady {
                    bolt11: bolt11.clone(),
                    expiry_unix_secs: expires_at_unix,
                });
                Ok(crate::receive::JitInvoice {
                    bolt11,
                    payment_hash: hex_str(&invoice.payment_hash().to_byte_array()),
                    opening_fee_msat,
                    expires_at_unix,
                })
            }
            Err(error) => {
                self.event_sink.emit(CoreEvent::Lsps2Failed {
                    reason: error.to_string(),
                });
                Err(error)
            }
        }
    }

    /// The JIT numpad floor in sats (U7, R6, AE4): one amountless `get_info`
    /// per receive session (`refresh = true` starts a new session), cached
    /// and NEVER an error — failures and empty menus degrade to the static
    /// 3,000-sat floor, as does a stopped node. Mirrors the PWA's
    /// only-when-it-can-matter gate (`Receive.tsx:158-161`): when usable
    /// inbound capacity already covers the static floor the fetch is skipped
    /// entirely — any below-floor amount is served by existing capacity.
    /// Blocking on a fetch: call from a background dispatcher.
    pub fn min_receive_sats(&self, refresh: bool) -> u64 {
        let (liquidity_source, runtime_handle, channel_manager) = {
            let state_lock = self.state.lock().unwrap();
            match state_lock.as_ref() {
                None => return crate::receive::MIN_JIT_RECEIVE_SATS,
                Some(state) => (
                    Arc::clone(&state.liquidity_source),
                    state.runtime.handle().clone(),
                    Arc::clone(&state.components.channel_manager),
                ),
            }
        };
        let usable_inbound_msat: u64 = channel_manager
            .list_channels()
            .iter()
            .filter(|details| details.is_usable)
            .map(|details| details.inbound_capacity_msat)
            .sum();
        if usable_inbound_msat >= crate::receive::MIN_JIT_RECEIVE_SATS * 1_000 {
            return crate::receive::MIN_JIT_RECEIVE_SATS;
        }
        spawn_and_wait(&runtime_handle, async move {
            liquidity_source.min_receive_sats(refresh).await
        })
        .unwrap_or(crate::receive::MIN_JIT_RECEIVE_SATS)
    }

    /// A standard (non-JIT) BOLT11 invoice via the channel manager's
    /// `create_inbound_payment`-based builder (U7): description
    /// `Zinqq Wallet`, 3600 s expiry, amountless allowed — the PWA's
    /// `createInvoice` verbatim. Returns `(bolt11, payment_hash_hex)`; paid
    /// detection rides the payment store (U5).
    pub fn standard_invoice(
        &self,
        amount_msat: Option<u64>,
    ) -> Result<(String, String), crate::receive::ReceiveError> {
        let channel_manager = {
            let state_lock = self.state.lock().unwrap();
            let state = state_lock
                .as_ref()
                .ok_or(crate::receive::ReceiveError::NotRunning)?;
            Arc::clone(&state.components.channel_manager)
        };
        let invoice = channel_manager
            .create_bolt11_invoice(crate::receive::standard_invoice_params(amount_msat))
            .map_err(|_| crate::receive::ReceiveError::InvoiceCreationFailed)?;
        Ok((
            invoice.to_string(),
            hex_str(&invoice.payment_hash().to_byte_array()),
        ))
    }

    /// The one receive call the shells render (U7, R6): on-chain address,
    /// standard invoice when capacity covers the request (`needs_jit` false),
    /// the unified BIP321 URI in copy and QR forms, the persisted offer when
    /// a usable channel exists, the session floor, and the capacity decision.
    /// Never touches the network: the floor is the session-cached value (use
    /// [`Node::min_receive_sats`] to fetch), and only the ALREADY-persisted
    /// offer is included (use [`Node::get_or_create_offer`] to mint one) —
    /// offer creation never blocks receive.
    pub fn receive_bundle(
        &self,
        amount_msat: Option<u64>,
    ) -> Result<crate::receive::ReceiveBundle, crate::receive::ReceiveError> {
        use crate::receive::{self, ReceiveError};

        let (kv_store, liquidity_source) = {
            let state_lock = self.state.lock().unwrap();
            let state = state_lock.as_ref().ok_or(ReceiveError::NotRunning)?;
            (
                Arc::clone(&state.components.kv_store),
                Arc::clone(&state.liquidity_source),
            )
        };
        let address =
            self.next_receive_address()
                .map_err(|e| ReceiveError::AddressUnavailable {
                    detail: e.to_string(),
                })?;
        let channels = self.list_channels().map_err(|_| ReceiveError::NotRunning)?;

        let needs_jit = receive::needs_jit(&channels, amount_msat);
        let (bolt11, payment_hash, invoice_error) = if needs_jit {
            // JIT path (amounted) or amountless-with-no-capacity: the PWA
            // renders the on-chain QR and drives the quote flow separately.
            (None, None, None)
        } else {
            match self.standard_invoice(amount_msat) {
                Ok((bolt11, payment_hash)) => (Some(bolt11), Some(payment_hash), None),
                // Receive.tsx:289-291: the failure copy renders only for an
                // amounted request; the on-chain QR still shows either way.
                Err(error) => (
                    None,
                    None,
                    amount_msat
                        .filter(|amount| *amount > 0)
                        .map(|_| error.to_string()),
                ),
            }
        };

        let amount_sats = amount_msat.map(|msat| msat / 1_000);
        let bip321_uri = receive::build_bip321_uri(&address, amount_sats, bolt11.as_deref());
        // QR alphanumeric mode uppercases the WHOLE URI (Receive.tsx:640).
        let qr_value = bip321_uri.to_uppercase();

        // showBolt12 gating (Receive.tsx:372): an offer page exists only
        // when an offer is persisted AND a usable channel can pay it.
        let offer = if receive::has_usable_channel(&channels) {
            receive::read_persisted_offer(&kv_store)
        } else {
            None
        };
        let offer_qr_value = offer
            .as_deref()
            .map(|offer| receive::build_bolt12_page_uri(offer).to_uppercase());

        Ok(receive::ReceiveBundle {
            address,
            bolt11,
            payment_hash,
            invoice_error,
            bip321_uri,
            qr_value,
            offer,
            offer_qr_value,
            needs_jit,
            min_receive_sats: liquidity_source
                .cached_jit_floor_sats()
                .unwrap_or(receive::MIN_JIT_RECEIVE_SATS),
        })
    }

    /// The persistent BOLT12 offer (U7, R6): returns the persisted one when
    /// it exists; otherwise creates it via `create_offer_builder` (chain
    /// mainnet, description `zinqq wallet` — the PWA's builder calls,
    /// `context.tsx:1655-1658`), retrying on the 3/6/12/24/48 s schedule
    /// because blinded paths need the RGS-synced graph. Persisted under a
    /// stable local key on success. `None` on a stopped node or when every
    /// attempt failed — offer creation NEVER blocks receive. Blocking (up to
    /// the retry schedule): call from a background dispatcher.
    pub fn get_or_create_offer(&self) -> Option<String> {
        use crate::config::BOLT12_OFFER_DESCRIPTION;

        let (channel_manager, kv_store, runtime_handle, logger) = {
            let state_lock = self.state.lock().unwrap();
            let state = state_lock.as_ref()?;
            (
                Arc::clone(&state.components.channel_manager),
                Arc::clone(&state.components.kv_store),
                state.runtime.handle().clone(),
                Arc::clone(&state.components.logger),
            )
        };
        if let Some(existing) = crate::receive::read_persisted_offer(&kv_store) {
            return Some(existing);
        }

        let network = self.config.network;
        let attempt_logger = Arc::clone(&logger);
        let offer = spawn_and_wait(&runtime_handle, async move {
            crate::receive::create_offer_with_retry(
                || {
                    let build = || -> Result<String, String> {
                        let offer = channel_manager
                            .create_offer_builder()
                            .map_err(|e| format!("create_offer_builder: {e:?}"))?
                            .chain(network)
                            .description(BOLT12_OFFER_DESCRIPTION.to_string())
                            .build()
                            .map_err(|e| format!("offer build: {e:?}"))?;
                        Ok(offer.to_string())
                    };
                    build().inspect_err(|reason| {
                        log_error!(
                            attempt_logger,
                            "BOLT12 offer creation attempt failed (graph not ready?): {reason}"
                        );
                    })
                },
                &crate::receive::OFFER_RETRY_DELAYS,
            )
            .await
        })
        .flatten()?;

        // PWA parity (context.tsx:1663): the offer is exposed only once
        // persisted, so every later session serves the SAME offer string.
        match crate::receive::persist_offer(&kv_store, &offer) {
            Ok(()) => Some(offer),
            Err(e) => {
                log_error!(logger, "Failed to persist the BOLT12 offer: {e}");
                None
            }
        }
    }

    /// Whether the BOLT12 offer pager page should exist (U7, R6): a
    /// persisted offer AND at least one usable channel (the PWA's
    /// `showBolt12`, `Receive.tsx:372`). `false` while stopped.
    pub fn offer_available(&self) -> bool {
        let (channel_manager, kv_store) = {
            let state_lock = self.state.lock().unwrap();
            match state_lock.as_ref() {
                None => return false,
                Some(state) => (
                    Arc::clone(&state.components.channel_manager),
                    Arc::clone(&state.components.kv_store),
                ),
            }
        };
        channel_manager
            .list_channels()
            .iter()
            .any(|details| details.is_usable)
            && crate::receive::read_persisted_offer(&kv_store).is_some()
    }

    /// Async receive state and offer together (U4) — the receive screen's one
    /// async payments call.
    ///
    /// **Reads LDK's offer cache exactly once**, which is the whole reason
    /// this is a single method: `ChannelManager::get_async_receive_offer` is a
    /// mutating read that marks the freshest unused offer `Used` and requests
    /// a `ChannelManager` persist. A separate status getter would consume a
    /// second offer per screen visit — halving the rotation pool LDK keeps to
    /// limit offer reuse — and could report `Ready` about a different offer
    /// than the one rendered.
    ///
    /// Short-circuits to [`AsyncReceiveView::disabled`] before touching LDK
    /// when no server is configured, which is every shipped build.
    ///
    /// Unlike [`Node::get_or_create_offer`] this neither retries nor persists
    /// anything locally, because LDK owns both: it refreshes the offer on its
    /// own timer and serializes the cache inside the `ChannelManager`, so the
    /// offer already rides the existing VSS backup. Non-blocking.
    pub fn async_receive(&self) -> AsyncReceiveView {
        if self.config.static_invoice_server_paths.is_empty() {
            return AsyncReceiveView::disabled();
        }
        let channel_manager = {
            let state_lock = self.state.lock().unwrap();
            match state_lock.as_ref() {
                // Stopped-but-configured is AwaitingServer, not Disabled:
                // paths ARE set, there is just no running node to serve the
                // handshake yet.
                None => return AsyncReceiveView::awaiting_server(),
                Some(state) => Arc::clone(&state.components.channel_manager),
            }
        };
        match channel_manager.get_async_receive_offer() {
            Ok(offer) => AsyncReceiveView::ready(offer.to_string()),
            Err(()) => AsyncReceiveView::awaiting_server(),
        }
    }

    /// Pays a mainnet BOLT11 invoice (U5). Blocking (route computation): call
    /// from a background dispatcher. Idempotent across restarts: the
    /// `PaymentId` is derived from the payment hash, so LDK rejects a re-send
    /// of an in-flight invoice as a duplicate instead of paying twice.
    ///
    /// The payment outcome arrives via the event queue (`PaymentSuccessful` /
    /// `PaymentFailed`). Failures of the initial attempt (e.g. no route) are
    /// abandoned synchronously by LDK without an event, so they are pushed as
    /// `PaymentFailed` here AND returned as a typed error. Validation
    /// failures and duplicates only return the typed error: nothing was
    /// attempted (or the original attempt still owns the outcome).
    pub fn send_payment(
        &self,
        bolt11: &str,
        amount_override_msat: Option<u64>,
    ) -> Result<(), SendError> {
        let channel_manager = {
            let state_lock = self.state.lock().unwrap();
            let state = state_lock.as_ref().ok_or(SendError::NotRunning)?;
            Arc::clone(&state.components.channel_manager)
        };
        let now = unix_now();

        // U5 dispatch writer: the PENDING history row is written after
        // validation and BEFORE the pay attempt, so the row exists for
        // whichever settle follows — the synchronous attempt failure below or
        // a later PaymentSent/PaymentFailed event. Validation failures write
        // nothing (nothing was attempted). History is informational, so a
        // persist failure degrades (logged) instead of blocking the send.
        // U6: the amount override (for amountless invoices) resolves here,
        // so the row records the amount actually being sent.
        let (invoice, amount_msat) =
            parse_and_validate(bolt11, self.config.network, now, amount_override_msat)?;
        let payment_id_hex = hex_str(&payment_id_for(&invoice).0);
        let payment_hash_hex = hex_str(invoice.payment_hash().as_byte_array());
        self.record_pending_outbound(&payment_id_hex, amount_msat, now_ms());

        let result = send_bolt11(
            &*channel_manager,
            bolt11,
            self.config.network,
            now,
            amount_override_msat,
        );
        match result {
            Ok(_payment_id) => Ok(()),
            Err(error) => {
                self.settle_attempt_failure(&payment_id_hex, Some(payment_hash_hex), &error);
                Err(error)
            }
        }
    }

    /// Pays a mainnet BOLT12 offer (U6, R5). Blocking (LSP dial + offer
    /// machinery): call from a background dispatcher.
    ///
    /// PWA `sendBolt12Payment` parity (`context.tsx:1026-1091`): the LSP is
    /// connected first so invoice-request onion messages can route, the
    /// payment id is 32 random bytes (BOLT12 payments have no payment hash
    /// until the invoice arrives), `payer_note` rides the invoice request,
    /// and retries are ×3. The pending history row is keyed by that random
    /// payment id; `PaymentSent`/`PaymentFailed` settle it by the same id
    /// (U5's row-key rule prefers `payment_id` when present).
    pub fn pay_offer(
        &self,
        offer_str: &str,
        amount_override_msat: Option<u64>,
        payer_note: Option<String>,
    ) -> Result<(), SendError> {
        let (channel_manager, keys_manager, liquidity_source, runtime_handle) = {
            let state_lock = self.state.lock().unwrap();
            let state = state_lock.as_ref().ok_or(SendError::NotRunning)?;
            (
                Arc::clone(&state.components.channel_manager),
                Arc::clone(&state.components.keys_manager),
                Arc::clone(&state.liquidity_source),
                state.runtime.handle().clone(),
            )
        };
        let now = unix_now();

        // Validation failures return before anything is attempted or
        // recorded (same contract as send_payment).
        let (_offer, embedded_msat) = validate_offer(offer_str, self.config.network, now)?;
        let amount_msat = resolve_amount(embedded_msat, amount_override_msat)?;

        let payment_id = PaymentId(keys_manager.get_secure_random_bytes());
        let payment_id_hex = hex_str(&payment_id.0);
        self.record_pending_outbound(&payment_id_hex, amount_msat, now_ms());

        // LSP pre-connect for onion transport (PWA context.tsx:1032-1044):
        // without a connected LSP the invoice request cannot route. Run on
        // the node runtime, wait outside the state lock (receive_jit's
        // pattern). A connect failure fails the payment, like the PWA's
        // thrown connectAndTrack.
        let connected = spawn_and_wait(&runtime_handle, async move {
            liquidity_source.ensure_lsp_connected().await
        })
        .unwrap_or(Err(Lsps2Error::Shutdown));

        let result = match connected {
            Err(error) => Err(SendError::SendFailed(format!(
                "could not connect to the LSP for BOLT12 onion messaging: {error}"
            ))),
            Ok(()) => send_bolt12(
                &*channel_manager,
                offer_str,
                self.config.network,
                now,
                amount_override_msat,
                payer_note,
                payment_id,
            )
            .map(|_amount| ()),
        };
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                // No payment hash yet: BOLT12 failures before an invoice
                // arrives carry None (events.rs PaymentFailed contract).
                self.settle_attempt_failure(&payment_id_hex, None, &error);
                Err(error)
            }
        }
    }

    /// Shared U5/U6 dispatch writer: the PENDING history row for an outbound
    /// attempt. History is informational, so a persist failure degrades
    /// (logged) instead of blocking the send.
    fn record_pending_outbound(&self, payment_id_hex: &str, amount_msat: u64, now_ms: u64) {
        if let Err(e) = self.payment_store.record_pending(
            payment_id_hex,
            PaymentDirection::Outbound,
            amount_msat,
            now_ms,
        ) {
            log_error!(
                Logger,
                "Failed to write the pending history row for {payment_id_hex}: {e}"
            );
        }
    }

    /// Shared U5/U6 handling for synchronous attempt failures: LDK abandoned
    /// without queueing an event, so settle the row and push the public
    /// failure ourselves, row first (the row must never lag the event it
    /// explains). Validation failures and duplicates skip this — nothing was
    /// attempted (or the original attempt owns the outcome).
    fn settle_attempt_failure(
        &self,
        payment_id_hex: &str,
        payment_hash_hex: Option<String>,
        error: &SendError,
    ) {
        if !error.is_attempt_failure() {
            return;
        }
        if let Err(e) = self.payment_store.settle(
            payment_id_hex,
            PaymentStatus::Failed,
            None,
            Some(error.to_string()),
        ) {
            log_error!(
                Logger,
                "Failed to settle the history row for {payment_id_hex}: {e}"
            );
        }
        self.event_sink.emit(CoreEvent::PaymentFailed {
            payment_hash: payment_hash_hex,
            reason: error.to_string(),
        });
    }

    /// Test-only: one real `lsps2.get_info` round-trip (the plan's live
    /// Megalith smoke test).
    #[cfg(test)]
    pub(crate) fn lsps2_get_info_live(
        &self,
    ) -> Result<Vec<lightning_liquidity::lsps2::msgs::LSPS2OpeningFeeParams>, Lsps2Error> {
        let (liquidity_source, runtime_handle) = self.liquidity_handles()?;
        spawn_and_wait(&runtime_handle, async move {
            liquidity_source.ensure_lsp_connected().await?;
            liquidity_source.request_opening_params().await
        })
        .unwrap_or(Err(Lsps2Error::Shutdown))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr as _;
    use std::time::Duration;

    use bitcoin::hashes::sha256;
    use bitcoin::secp256k1::{Secp256k1, SecretKey};
    use lightning::types::payment::PaymentSecret;
    use lightning_invoice::{Bolt11Invoice, Currency, InvoiceBuilder};
    use lightning_persister::fs_store::FilesystemStore;

    use crate::builder::KV_STORE_SUBDIR;
    use crate::events::EventQueue;
    use crate::history::{ActivityStatus, PaymentStore, PAYMENT_HISTORY_PRIMARY_NAMESPACE};
    use crate::node::tests::{
        offline_config, payment_hash, static_invoice_server_path, store_in, CapturingSink,
    };
    use crate::node::{record_payment_claimed, settle_payment_sent};

    /// U3/U4, AE3 and R6: with no static invoice server configured — the
    /// shipped default — async receive is inert. Both endpoints degrade
    /// safely while stopped AND while running, exactly like the standard
    /// offer endpoints above them.
    #[test]
    fn async_receive_is_inert_without_a_configured_server() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::new(offline_config(dir.path()));

        assert_eq!(node.async_receive(), AsyncReceiveView::disabled());

        node.start().expect("offline degraded start");
        assert_eq!(
            node.async_receive(),
            AsyncReceiveView::disabled(),
            "the shipped default never touches LDK's offer cache"
        );
        node.stop().unwrap();
    }

    /// U3/U4, AE4: configured paths are applied at start without blocking or
    /// failing it, even with no peers connected — LDK's timer-driven refresh
    /// owns convergence. The status reports `AwaitingServer` rather than
    /// `Disabled`, because no server is reachable from an offline test and
    /// the handshake never completes. Re-running start proves KTD-3's claim
    /// that re-application is safe.
    #[test]
    fn configured_static_invoice_server_paths_apply_at_every_start() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = offline_config(dir.path());
        config.static_invoice_server_paths = vec![static_invoice_server_path()];
        let node = Node::new(config);

        assert_eq!(
            node.async_receive(),
            AsyncReceiveView::awaiting_server(),
            "stopped-but-configured is not Disabled"
        );

        node.start().expect("offline degraded start with paths");
        assert_eq!(
            node.async_receive(),
            AsyncReceiveView::awaiting_server(),
            "no server is reachable, so no offer was ever built"
        );
        // Prove LDK actually took the paths, which the status alone cannot
        // show — it only reflects that the config is non-empty. Re-applying
        // against the live channel manager is the same call `start` makes, so
        // an Ok here is also KTD-3's re-application claim under test.
        {
            let state_lock = node.state.lock().unwrap();
            let channel_manager =
                Arc::clone(&state_lock.as_ref().unwrap().components.channel_manager);
            drop(state_lock);
            assert_eq!(
                crate::receive::apply_static_invoice_server_paths(
                    &channel_manager,
                    &[static_invoice_server_path()]
                ),
                Ok(1)
            );
        }
        node.stop().unwrap();

        node.start().expect("re-applying the paths is safe");
        assert_eq!(node.async_receive(), AsyncReceiveView::awaiting_server());
        node.stop().unwrap();
    }

    /// U3: the empty case is a genuine no-op, not a rejected call — this is
    /// the path every shipped build takes on every start.
    #[test]
    fn applying_no_static_invoice_server_paths_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let node = Node::new(offline_config(dir.path()));
        node.start().expect("offline degraded start");

        let state_lock = node.state.lock().unwrap();
        let channel_manager = Arc::clone(&state_lock.as_ref().unwrap().components.channel_manager);
        drop(state_lock);
        assert_eq!(
            crate::receive::apply_static_invoice_server_paths(&channel_manager, &[]),
            Ok(0)
        );
        node.stop().unwrap();
    }

    /// U5 persist-then-ack, failure half: when the settle CANNOT be made
    /// durable, the handler asks LDK to REPLAY the event and emits NOTHING —
    /// the public event queue never runs ahead of the history store.
    #[cfg(unix)]
    #[test]
    fn payment_settles_persist_before_any_public_event_is_emitted() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let store = store_in(dir.path());
        let sink = CapturingSink::default();
        let logger = Arc::new(Logger);
        store
            .record_pending(&"77".repeat(32), PaymentDirection::Outbound, 1_000, 1)
            .unwrap();

        let namespace_dir = dir
            .path()
            .join("store")
            .join(PAYMENT_HISTORY_PRIMARY_NAMESPACE);
        let writable = std::fs::metadata(&namespace_dir).unwrap().permissions();
        std::fs::set_permissions(&namespace_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = settle_payment_sent(
            &store,
            &sink,
            &logger,
            Some(PaymentId([0x77; 32])),
            payment_hash(0x77),
            Some(21),
        );
        std::fs::set_permissions(&namespace_dir, writable).unwrap();

        assert!(
            result.is_err(),
            "a non-durable settle must request a replay"
        );
        assert!(
            sink.0.lock().unwrap().is_empty(),
            "no public event may be emitted before the settle is durable"
        );

        // The replay settles and emits once persistence recovers.
        settle_payment_sent(
            &store,
            &sink,
            &logger,
            Some(PaymentId([0x77; 32])),
            payment_hash(0x77),
            Some(21),
        )
        .unwrap();
        assert_eq!(
            store.get(&"77".repeat(32)).unwrap().status,
            PaymentStatus::Succeeded
        );
        assert_eq!(
            sink.0.lock().unwrap().clone(),
            vec![CoreEvent::PaymentSuccessful {
                payment_hash: "77".repeat(32),
                fee_paid_msat: Some(21),
            }]
        );
    }

    /// The crash-between-persist-and-ack window (U5): the settle is durable,
    /// the public event is queued but NEVER acked, the process dies. On
    /// rebuild the queue redelivers the event AND LDK replays PaymentSent —
    /// the replayed settle is a no-op, so the row settles exactly once.
    #[test]
    fn replayed_payment_sent_after_crash_before_ack_settles_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let logger = Arc::new(Logger);
        let kv = || Arc::new(FilesystemStore::new(dir.path().join("store")));

        {
            let store = PaymentStore::new(kv(), Arc::clone(&logger));
            let queue = EventQueue::new(kv(), Arc::clone(&logger));
            store
                .record_pending(&"88".repeat(32), PaymentDirection::Outbound, 5_000, 1)
                .unwrap();
            settle_payment_sent(
                &store,
                &queue,
                &logger,
                Some(PaymentId([0x88; 32])),
                payment_hash(0x88),
                Some(7),
            )
            .unwrap();
            // Crash here: the event stays in the persisted queue, unacked.
        }

        let store = PaymentStore::new(kv(), Arc::clone(&logger));
        let queue = EventQueue::new(kv(), Arc::clone(&logger));
        assert_eq!(
            store.get(&"88".repeat(32)).unwrap().status,
            PaymentStatus::Succeeded,
            "the settle was durable before the crash"
        );
        // LDK replays the unhandled event on restart; the settle is a no-op
        // and the fee recorded by the first delivery survives.
        settle_payment_sent(
            &store,
            &queue,
            &logger,
            Some(PaymentId([0x88; 32])),
            payment_hash(0x88),
            Some(999),
        )
        .unwrap();
        let row = store.get(&"88".repeat(32)).unwrap();
        assert_eq!(row.status, PaymentStatus::Succeeded);
        assert_eq!(
            row.fee_paid_msat,
            Some(7),
            "exactly-once: replay changes nothing"
        );
        // The unacked public event was redelivered from disk; the idempotent
        // consumer handles the duplicate emit (handle-then-ack contract).
        assert_eq!(
            queue.ack(),
            Some(crate::events::Event::PaymentSuccessful {
                payment_hash: "88".repeat(32),
                fee_paid_msat: Some(7),
            })
        );
    }

    /// A replayed PaymentClaimed after a crash-before-ack never duplicates
    /// the inbound row, and the row is durable before the event.
    #[test]
    fn replayed_payment_claimed_never_duplicates_the_inbound_row() {
        let dir = tempfile::tempdir().unwrap();
        let logger = Arc::new(Logger);
        let store = store_in(dir.path());
        let sink = CapturingSink::default();

        record_payment_claimed(
            &store,
            &sink,
            &logger,
            payment_hash(0x99),
            250_000,
            1_000,
            || Some(2_000),
        )
        .unwrap();
        // Replay: the skim was consumed by the first delivery (None now).
        record_payment_claimed(
            &store,
            &sink,
            &logger,
            payment_hash(0x99),
            250_000,
            2_000,
            || None,
        )
        .unwrap();

        assert_eq!(store.rows().len(), 1, "re-claiming must not duplicate");
        let row = store.get(&"99".repeat(32)).unwrap();
        assert_eq!(row.direction, PaymentDirection::Inbound);
        assert_eq!(row.status, PaymentStatus::Succeeded);
        assert_eq!(row.created_at_ms, 1_000, "first claim's facts win");
    }

    /// U7 at the Node seam: receive endpoints follow the lifecycle. An
    /// offline degraded start (fresh wallet, zero channels) serves the
    /// on-chain-only bundle; the standard invoice carries the PWA's
    /// description/expiry and allows amountless; a persisted offer stays
    /// gated on usable channels (and survives a restart under its stable
    /// key); a bogus accept token fails typed and queues `Lsps2Failed`.
    #[test]
    fn receive_endpoints_follow_the_node_lifecycle() {
        use crate::receive::{ReceiveError, MIN_JIT_RECEIVE_SATS};

        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(CapturingSink::default());
        let node = Node::with_event_sink(offline_config(dir.path()), Arc::clone(&sink) as _);

        // Stopped: typed NotRunning; the floor and offer degrade safely.
        assert_eq!(
            node.jit_quote(1_000_000).unwrap_err(),
            Lsps2Error::NotRunning
        );
        assert_eq!(
            node.jit_accept(1, 1_000_000).unwrap_err(),
            Lsps2Error::NotRunning
        );
        assert_eq!(
            node.receive_bundle(None).unwrap_err(),
            ReceiveError::NotRunning
        );
        assert_eq!(
            node.standard_invoice(None).unwrap_err(),
            ReceiveError::NotRunning
        );
        assert_eq!(node.min_receive_sats(false), MIN_JIT_RECEIVE_SATS);
        assert_eq!(node.get_or_create_offer(), None);
        assert!(!node.offer_available());

        node.start().expect("offline degraded start");

        // Fresh wallet, amountless: the on-chain-only QR state
        // (Receive.tsx:209-218) — needs_jit, no invoice, no error copy.
        let bundle = node.receive_bundle(None).unwrap();
        assert!(bundle.needs_jit, "no usable channel");
        assert_eq!(bundle.bolt11, None);
        assert_eq!(bundle.payment_hash, None);
        assert_eq!(bundle.invoice_error, None);
        assert!(bundle.address.starts_with("bc1q"), "BIP84 mainnet address");
        assert_eq!(
            bundle.bip321_uri,
            format!("bitcoin:{}", bundle.address.to_uppercase())
        );
        assert_eq!(bundle.qr_value, bundle.bip321_uri.to_uppercase());
        assert_eq!(bundle.offer, None);
        assert_eq!(bundle.offer_qr_value, None);
        assert_eq!(bundle.min_receive_sats, MIN_JIT_RECEIVE_SATS);

        // Amounted while JIT is needed: the amount rides the URI (the QR
        // stays scannable on-chain) and no lightning param exists yet.
        let bundle = node.receive_bundle(Some(5_000_000)).unwrap();
        assert!(bundle.needs_jit);
        assert_eq!(bundle.bolt11, None);
        assert!(
            bundle.bip321_uri.ends_with("?amount=0.00005000"),
            "unexpected URI: {}",
            bundle.bip321_uri
        );

        // The standard invoice mirrors the PWA's createInvoice: amountless
        // allowed, description 'Zinqq Wallet', 3600 s expiry, and the
        // returned hash matches the invoice's.
        let (bolt11, payment_hash_hex) = node.standard_invoice(None).unwrap();
        let invoice = lightning_invoice::Bolt11Invoice::from_str(&bolt11).unwrap();
        assert_eq!(invoice.amount_milli_satoshis(), None, "amountless allowed");
        assert_eq!(invoice.expiry_time(), Duration::from_secs(3_600));
        assert_eq!(invoice.description().to_string(), "Zinqq Wallet");
        assert_eq!(
            payment_hash_hex,
            hex_str(&invoice.payment_hash().to_byte_array())
        );
        let (amounted, _) = node.standard_invoice(Some(250_000)).unwrap();
        assert_eq!(
            lightning_invoice::Bolt11Invoice::from_str(&amounted)
                .unwrap()
                .amount_milli_satoshis(),
            Some(250_000)
        );

        // A persisted offer does NOT surface with zero usable channels
        // (showBolt12 gating), but get_or_create_offer serves it verbatim
        // instead of minting a new one.
        let kv_store = FilesystemStore::new(dir.path().join(KV_STORE_SUBDIR));
        crate::receive::persist_offer(&kv_store, "lno1testoffer").unwrap();
        assert!(!node.offer_available(), "zero usable channels → no page");
        assert_eq!(node.receive_bundle(None).unwrap().offer, None);
        assert_eq!(node.get_or_create_offer().as_deref(), Some("lno1testoffer"));

        // A bogus accept token: typed error, Lsps2Failed queued, no buy.
        assert_eq!(
            node.jit_accept(999, 1_000_000).unwrap_err(),
            Lsps2Error::QuoteNotFound
        );
        assert!(
            sink.0.lock().unwrap().iter().any(|event| matches!(
                event,
                CoreEvent::Lsps2Failed { reason } if reason.contains("no longer available")
            )),
            "the failure must reach the event queue"
        );

        node.stop().unwrap();

        // The offer is restart-stable under its stable key.
        node.start().expect("offline degraded restart");
        assert_eq!(node.get_or_create_offer().as_deref(), Some("lno1testoffer"));
        node.stop().unwrap();
    }

    fn signed_mainnet_invoice() -> Bolt11Invoice {
        let secret = SecretKey::from_slice(&[0x4d; 32]).unwrap();
        InvoiceBuilder::new(Currency::Bitcoin)
            .description("u5 dispatch test".to_string())
            .payment_hash(sha256::Hash::from_byte_array([0x42; 32]))
            .payment_secret(PaymentSecret([0x24; 32]))
            .duration_since_epoch(unix_now())
            .min_final_cltv_expiry_delta(144)
            .expiry_time(Duration::from_secs(3_600))
            .amount_milli_satoshis(50_000_000)
            .build_signed(|hash| Secp256k1::new().sign_ecdsa_recoverable(hash, &secret))
            .unwrap()
    }

    /// The wired dispatch (U5): send_payment writes the pending row keyed by
    /// the derived payment id, and a synchronous attempt failure settles it
    /// FAILED with the same reason the public event carries. The failed row
    /// is hidden from the activity feed but visible via payment_detail.
    #[test]
    fn send_payment_writes_and_settles_the_history_row() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(CapturingSink::default());
        let node = Node::with_event_sink(offline_config(dir.path()), Arc::clone(&sink) as _);
        node.start().expect("offline degraded start");

        let invoice = signed_mainnet_invoice();
        let payment_id_hex = "42".repeat(32); // bolt11: payment id == hash
        assert_eq!(
            node.send_payment(&invoice.to_string(), None),
            Err(SendError::RouteNotFound)
        );

        let row = node
            .payment_detail(&payment_id_hex)
            .expect("dispatch must write a history row");
        assert_eq!(row.direction, PaymentDirection::Outbound);
        assert_eq!(row.amount_msat, 50_000_000);
        assert_eq!(row.status, PaymentStatus::Failed);
        assert_eq!(
            row.failure_reason.as_deref(),
            Some(SendError::RouteNotFound.to_string().as_str()),
            "the row and the public event carry the same reason"
        );

        // Validation failures never touch the store.
        assert!(matches!(
            node.send_payment("junk", None),
            Err(SendError::InvalidInvoice(_))
        ));
        let feed = node.list_activity().unwrap();
        assert!(
            feed.iter().all(|r| r.id != payment_id_hex),
            "failed rows are hidden from the feed (KTD-7)"
        );
        assert!(
            feed.iter().all(|r| r.status != ActivityStatus::Failed),
            "the feed never exposes a failed status"
        );
        node.stop().unwrap();

        // Rows stay readable while stopped; the feed needs the node.
        assert!(node.payment_detail(&payment_id_hex).is_some());
        assert!(node.list_activity().is_none());
    }
}
