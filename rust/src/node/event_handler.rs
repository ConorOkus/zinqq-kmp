//! LDK event handling and the background processor.
//!
//! `handle_ldk_event` is the single dispatch point for every LDK event the node
//! observes, and the three `settle_*`/`record_*` helpers above it own the
//! persist-then-ack ordering that keeps a public event from ever outrunning its
//! durable history row.
//!
//! These are free functions, not `Node` methods, so this module is a plain
//! relocation out of `node.rs` (see that module's header). Named
//! `event_handler` rather than `events` so it does not read as the crate-root
//! `crate::events` module.

use std::sync::Arc;
use std::time::Duration;

use lightning::events::{Event, PaymentFailureReason, ReplayEvent};
use lightning::ln::channelmanager::PaymentId;
use lightning::log_error;
use lightning::log_info;
use lightning::types::payment::PaymentHash;
use lightning::util::logger::Logger as _;
use lightning_background_processor::{process_events_async_with_kv_store_sync, GossipSync};
use tokio::runtime::Runtime;
use tokio::sync::watch;

use crate::builder::NodeComponents;
use crate::channels::{self, ChannelEventContext, FundingStore};
use crate::close_records::{
    self, classify_closure_reason, CloseOutpoint, CloseRecordStore, FundingTxoEntry,
};
use crate::history::{PaymentStatus, PaymentStore};
use crate::liquidity::LiquiditySource;
use crate::node::{CoreEvent, EventSink};
use crate::payment::describe_failure_reason;
use crate::recovery::{self, RecoveryStore};
use crate::sweep::SweepEngine;
use crate::types::{Logger, Sweeper};
use crate::util::{hex_str, now_ms, unix_now};

/// Settles the history row for a `PaymentSent` and emits the public success —
/// row FIRST (U5 persist-then-ack): if the settle cannot be made durable the
/// event is replayed and nothing is emitted, so a consumer never sees a
/// success the store doesn't know about. The row key is LDK's `payment_id`
/// when present, else the payment hash (PWA `event-handler.ts:297-300`);
/// replays are absorbed by the store's idempotent settle.
pub(crate) fn settle_payment_sent(
    payment_store: &PaymentStore,
    event_sink: &dyn EventSink,
    logger: &Arc<Logger>,
    payment_id: Option<PaymentId>,
    payment_hash: PaymentHash,
    fee_paid_msat: Option<u64>,
) -> Result<(), ReplayEvent> {
    let row_key = payment_id
        .map(|id| hex_str(&id.0))
        .unwrap_or_else(|| hex_str(&payment_hash.0));
    payment_store
        .settle(&row_key, PaymentStatus::Succeeded, fee_paid_msat, None)
        .map_err(|e| {
            log_error!(logger, "Deferring PaymentSent {row_key}: {e}");
            ReplayEvent()
        })?;
    log_info!(
        logger,
        "Outbound payment {row_key} succeeded (fee paid: {fee_paid_msat:?} msat)"
    );
    event_sink.emit(CoreEvent::PaymentSuccessful {
        payment_hash: hex_str(&payment_hash.0),
        fee_paid_msat,
    });
    Ok(())
}

/// Settles the history row for a terminal `PaymentFailed` and emits the
/// public failure — row first, exactly like [`settle_payment_sent`].
pub(crate) fn settle_payment_failed(
    payment_store: &PaymentStore,
    event_sink: &dyn EventSink,
    logger: &Arc<Logger>,
    payment_id: PaymentId,
    payment_hash: Option<PaymentHash>,
    reason: Option<PaymentFailureReason>,
) -> Result<(), ReplayEvent> {
    let row_key = hex_str(&payment_id.0);
    let reason = describe_failure_reason(reason);
    payment_store
        .settle(&row_key, PaymentStatus::Failed, None, Some(reason.clone()))
        .map_err(|e| {
            log_error!(logger, "Deferring PaymentFailed {row_key}: {e}");
            ReplayEvent()
        })?;
    log_error!(logger, "Outbound payment {row_key} failed: {reason}");
    event_sink.emit(CoreEvent::PaymentFailed {
        payment_hash: payment_hash.map(|hash| hex_str(&hash.0)),
        reason,
    });
    Ok(())
}

/// Writes the inbound SUCCEEDED row for a `PaymentClaimed` and emits the
/// public receive — row first. `take_skim` is consumed only after the row is
/// durable, so a replayed claim never loses the skim to a failed persist.
pub(crate) fn record_payment_claimed(
    payment_store: &PaymentStore,
    event_sink: &dyn EventSink,
    logger: &Arc<Logger>,
    payment_hash: PaymentHash,
    amount_msat: u64,
    now_ms: u64,
    take_skim: impl FnOnce() -> Option<u64>,
) -> Result<(), ReplayEvent> {
    let hash_hex = hex_str(&payment_hash.0);
    payment_store
        .record_claimed(&hash_hex, amount_msat, now_ms)
        .map_err(|e| {
            log_error!(logger, "Deferring PaymentClaimed {hash_hex}: {e}");
            ReplayEvent()
        })?;
    event_sink.emit(CoreEvent::PaymentReceived {
        payment_hash: hash_hex,
        amount_msat,
        skimmed_fee_msat: take_skim(),
    });
    Ok(())
}

/// Handles LDK events: durable `SpendableOutputs` (U2 fund safety), the U4
/// JIT-receive cluster (0-conf channel acceptance, claimable→claim_funds,
/// claimed→PaymentReceived, channel pending/ready), the U5 payment outcomes
/// (PaymentSent/PaymentFailed/PaymentClaimed → history row settled durably,
/// THEN the public event — persist-then-ack), the U9 channel-open funding
/// flow (FundingGenerationReady → persist-then-notify,
/// FundingTxBroadcastSafe → broadcast, DiscardFunding → cleanup,
/// ChannelClosed → auto-forget), the U10 close-record/recovery observations
/// (ChannelPending → funding-txo safety net, ChannelClosed → close record,
/// BumpTransaction → commitment fact + gated recovery entry), and
/// log-and-ack for the rest. Every U10 observation is idempotent under
/// event replay (LDK replays unresolved events on every restart).
#[allow(clippy::too_many_arguments)]
fn handle_ldk_event(
    event: Event,
    sweep_engine: &SweepEngine,
    sweep_wake: &tokio::sync::Notify,
    bump_handler: &crate::bump::BumpEventHandler,
    liquidity_source: &LiquiditySource,
    payment_store: &PaymentStore,
    channels_ctx: &ChannelEventContext,
    close_records: &Arc<CloseRecordStore>,
    recovery: &RecoveryStore,
    event_sink: &Arc<dyn EventSink>,
    logger: &Arc<Logger>,
) -> Result<(), ReplayEvent> {
    match event {
        Event::SpendableOutputs {
            outputs,
            channel_id,
        } => {
            // U11/KTD-8: persist into the core-owned descriptor store —
            // wallet-owned StaticOutputs excluded pre-persist (U1's signer
            // hands LDK bdk destination scripts, so those funds are already
            // in the wallet AND unsignable by the KeysManager), replays
            // deduped by descriptor+outpoint. On persist failure the event
            // is REPLAYED rather than dropping funds. The sweep attempt
            // itself runs on the sweep task (woken below), so a slow
            // broadcast never blocks the background processor.
            let channel_id_hex = channel_id.map(|id| hex_str(&id.0));
            match sweep_engine.track_spendable_outputs(&outputs, channel_id_hex) {
                Ok(_) => {
                    sweep_wake.notify_one();
                    Ok(())
                }
                Err(e) => {
                    log_error!(
                        logger,
                        "Failed to persist spendable outputs; replaying event: {e}"
                    );
                    Err(ReplayEvent())
                }
            }
        }
        // KTD-9: 0-conf acceptance from the trusted LSP only.
        Event::OpenChannelRequest {
            temporary_channel_id,
            counterparty_node_id,
            ..
        } => {
            liquidity_source.on_open_channel_request(temporary_channel_id, counterparty_node_id);
            Ok(())
        }
        Event::PaymentClaimable {
            payment_hash,
            counterparty_skimmed_fee_msat,
            purpose,
            ..
        } => {
            // claim_funds is idempotent in LDK, so a replayed claimable after
            // an unacked claim is tolerated.
            liquidity_source.on_payment_claimable(
                payment_hash,
                counterparty_skimmed_fee_msat,
                &purpose,
            );
            Ok(())
        }
        // The durable success signal for a receive: history row, then event.
        Event::PaymentClaimed {
            payment_hash,
            amount_msat,
            ..
        } => record_payment_claimed(
            payment_store,
            &**event_sink,
            logger,
            payment_hash,
            amount_msat,
            now_ms(),
            || liquidity_source.take_skim(&payment_hash),
        ),
        Event::ChannelPending {
            channel_id,
            former_temporary_channel_id,
            funding_txo,
            ..
        } => {
            let channel_id_hex = hex_str(&channel_id.0);
            // U9: record real→temporary so DiscardFunding (which carries the
            // real id) can clean up the funding tx keyed by the temp id (the
            // PWA's `ldk_channel_id_map`, `event-handler.ts:352-356`).
            if let Some(temp_id) = former_temporary_channel_id {
                channels_ctx
                    .funding
                    .record_channel_id_map(&channel_id_hex, &hex_str(&temp_id.0));
            }
            // U10 safety net (PWA `event-handler.ts:357-380`): if this
            // channel later closes while the process is dying, reconcile
            // recreates the record from this funding outpoint. to_self_delay
            // is captured here too — unreadable once the channel closes.
            let timelock_blocks = channels_ctx
                .channel_manager
                .list_channels()
                .iter()
                .find(|details| details.channel_id == channel_id)
                .and_then(|details| details.force_close_spend_delay)
                .map(u32::from);
            close_records.record_funding_txo(
                &channel_id_hex,
                FundingTxoEntry {
                    txid: funding_txo.txid.to_string(),
                    vout: funding_txo.vout,
                    timelock_blocks,
                },
            );
            event_sink.emit(CoreEvent::ChannelPending {
                channel_id: channel_id_hex,
            });
            Ok(())
        }
        Event::ChannelReady { channel_id, .. } => {
            event_sink.emit(CoreEvent::ChannelReady {
                channel_id: hex_str(&channel_id.0),
            });
            Ok(())
        }
        // U9 funding flow: build from the on-chain wallet, persist the tx
        // BEFORE notifying LDK, and broadcast only on FundingTxBroadcastSafe.
        Event::FundingGenerationReady {
            temporary_channel_id,
            counterparty_node_id,
            channel_value_satoshis,
            output_script,
            ..
        } => {
            channels::on_funding_generation_ready(
                channels_ctx,
                temporary_channel_id,
                counterparty_node_id,
                channel_value_satoshis,
                output_script,
                logger,
            );
            Ok(())
        }
        Event::FundingTxBroadcastSafe {
            former_temporary_channel_id,
            ..
        } => {
            let temp_hex = hex_str(&former_temporary_channel_id.0);
            match channels::handle_funding_tx_broadcast_safe(
                &channels_ctx.funding,
                &channels_ctx.broadcaster,
                &temp_hex,
            ) {
                channels::BroadcastSafeOutcome::Broadcast { txid } => {
                    log_info!(logger, "Broadcast funding tx {txid} for {temp_hex}");
                }
                channels::BroadcastSafeOutcome::MissingTx => {
                    log_error!(
                        logger,
                        "FundingTxBroadcastSafe: no persisted funding tx for {temp_hex}"
                    );
                }
            }
            Ok(())
        }
        Event::DiscardFunding { channel_id, .. } => {
            let channel_id_hex = hex_str(&channel_id.0);
            if channels::handle_discard_funding(&channels_ctx.funding, &channel_id_hex) {
                log_info!(
                    logger,
                    "DiscardFunding: dropped the persisted funding tx for {channel_id_hex}"
                );
            }
            Ok(())
        }
        Event::ChannelClosed {
            channel_id,
            reason,
            counterparty_node_id,
            channel_funding_txo,
            last_local_balance_msat,
            ..
        } => {
            let channel_id_hex = hex_str(&channel_id.0);
            // U10 (PWA `event-handler.ts:397-430`): classify the closure
            // reason and record the close. NO channel-capacity fallback for
            // the balance (it would overstate the expected return by the
            // whole capacity); unknown stays unknown.
            let classification = classify_closure_reason(&reason);
            let funding_txo = channel_funding_txo.map(|txo| CloseOutpoint {
                txid: txo.txid.to_string(),
                vout: u32::from(txo.index),
            });
            close_records::on_channel_closed(
                close_records,
                &channel_id_hex,
                &classification,
                funding_txo,
                last_local_balance_msat.map(|msat| msat / 1_000),
                now_ms(),
            );
            // U9 auto-forget: the LAST channel with a peer closing drops it
            // from known peers, so the reconnect loop stops dialing it (PWA
            // `context.tsx:1233-1244`). Best-effort, never a replay.
            if let Some(counterparty) = counterparty_node_id {
                let still_has_channels = channels_ctx
                    .channel_manager
                    .list_channels()
                    .iter()
                    .any(|details| details.counterparty.node_id == counterparty);
                channels::auto_forget_on_channel_closed(
                    &channels_ctx.known_peers,
                    &counterparty.to_string(),
                    still_has_channels,
                    logger,
                );
            }
            // Funding-map cleanup (PWA `event-handler.ts:446`).
            channels_ctx.funding.remove_channel_id_map(&channel_id_hex);
            event_sink.emit(CoreEvent::ChannelClosed {
                channel_id: channel_id_hex,
                reason: reason.to_string(),
            });
            Ok(())
        }
        // The durable success signal for a send (U5): row, then event.
        Event::PaymentSent {
            payment_id,
            payment_hash,
            fee_paid_msat,
            ..
        } => settle_payment_sent(
            payment_store,
            &**event_sink,
            logger,
            payment_id,
            payment_hash,
            fee_paid_msat,
        ),
        // The terminal failure signal for a send (U5): all retries exhausted
        // or the payment was abandoned. Row first, then event.
        Event::PaymentFailed {
            payment_id,
            payment_hash,
            reason,
        } => settle_payment_failed(
            payment_store,
            &**event_sink,
            logger,
            payment_id,
            payment_hash,
            reason,
        ),
        // U10: OBSERVE the bump event for close records + gated recovery
        // entry FIRST; then U11's CPFP handler consumes it (fee-sanity
        // gated). Acking is safe either way: LDK re-yields bump events on
        // each new block until the claim confirms, so a refused or failed
        // bump is retried at fresh rates. The observation is idempotent
        // under replay.
        Event::BumpTransaction(ref bump_event) => {
            use lightning::events::bump_transaction::BumpTransactionEvent;
            let (channel_id_hex, commitment) = match bump_event {
                BumpTransactionEvent::ChannelClose {
                    channel_id,
                    commitment_tx,
                    commitment_tx_fee_satoshis,
                    ..
                } => (
                    hex_str(&channel_id.0),
                    // Only the anchor path hands us the actual commitment tx.
                    Some((
                        commitment_tx.compute_txid().to_string(),
                        *commitment_tx_fee_satoshis,
                    )),
                ),
                BumpTransactionEvent::HTLCResolution { channel_id, .. } => {
                    (hex_str(&channel_id.0), None)
                }
            };
            let onchain_wallet = &channels_ctx.onchain_wallet;
            recovery::observe_bump_transaction(
                close_records,
                recovery,
                &channel_id_hex,
                commitment,
                onchain_wallet.has_confirmed_utxo(),
                onchain_wallet.is_initial_scan_complete(),
                || onchain_wallet.next_receive_address().ok(),
                Some(channels_ctx.chain_source.onchain_send_fee_rate_sat_per_vb() as f64),
                now_ms(),
                logger,
            );
            // U11 CPFP handling (KTD-9), AFTER the U10 observation: the
            // fee-sanity middleware refuses a bump whose requested package
            // rate exceeds 5x a fresh 3-block estimate (the ~30x overpay
            // incident class) BEFORE any coins are selected or signed.
            let target_sat_per_kw = crate::bump::bump_event_target_sat_per_kw(bump_event);
            match crate::bump::check_bump_target_sanity(
                target_sat_per_kw,
                &channels_ctx.chain_source.fee_estimator(),
            ) {
                Ok(()) => {
                    log_info!(
                        logger,
                        "BumpTransaction for {channel_id_hex}: handling CPFP fee bump at \
                         {target_sat_per_kw} sat/kW"
                    );
                    bump_handler.handle_event(bump_event);
                }
                Err(e) => {
                    log_error!(
                        logger,
                        "BumpTransaction for {channel_id_hex} REFUSED: {e}; LDK re-yields \
                         on the next block"
                    );
                }
            }
            Ok(())
        }
        // Per-path telemetry: the background processor already feeds these to
        // the scorer; the terminal outcome arrives as PaymentSent/Failed.
        Event::PaymentPathSuccessful { payment_hash, .. } => {
            log_info!(logger, "A payment path for {payment_hash:?} succeeded");
            Ok(())
        }
        Event::PaymentPathFailed { payment_hash, .. } => {
            log_info!(
                logger,
                "A payment path for {payment_hash:?} failed; LDK may retry other paths"
            );
            Ok(())
        }
        other => {
            log_info!(logger, "Acking unhandled LDK event: {other:?}");
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_background_processor(
    runtime: &Runtime,
    components: &NodeComponents,
    liquidity_source: Arc<LiquiditySource>,
    payment_store: Arc<PaymentStore>,
    close_records: Arc<CloseRecordStore>,
    recovery: Arc<RecoveryStore>,
    sweep_engine: Arc<SweepEngine>,
    sweep_wake: Arc<tokio::sync::Notify>,
    event_sink: Arc<dyn EventSink>,
    bp_stop_receiver: watch::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    // U3: the background processor persists through the dual-write store —
    // channel-manager writes get the bounded VSS-then-local treatment,
    // graph/scorer/sweeper/liquidity stay local-only. Monitors do NOT come
    // through here (they use the ChainMonitor's custom async `Persist`).
    let kv_store = Arc::clone(&components.dual_kv_store);
    let chain_monitor = Arc::clone(&components.chain_monitor);
    let channel_manager = Arc::clone(&components.channel_manager);
    let liquidity_manager = Arc::clone(&components.liquidity_manager);
    let onion_messenger = Arc::clone(&components.onion_messenger);
    let gossip_sync = components.gossip_source.gossip_sync();
    let peer_manager = Arc::clone(&components.peer_manager);
    let scorer = Arc::clone(&components.scorer);
    let logger = Arc::clone(&components.logger);
    let error_logger = Arc::clone(&components.logger);

    // U11: the CPFP handler (BumpTransactionEventHandlerSync over the bdk
    // wallet source; the KeysManager is the signer provider for anchor-input
    // re-derivation).
    let bump_handler = Arc::new(crate::bump::build_bump_handler(
        Arc::clone(&components.broadcaster),
        Arc::new(crate::bump::BdkWalletSource::new(
            Arc::clone(&components.onchain_wallet),
            Arc::clone(&components.logger),
        )),
        Arc::clone(&components.keys_manager),
        Arc::clone(&components.logger),
    ));

    let event_logger = Arc::clone(&components.logger);
    // U9: the channel-event context (funding flow + auto-forget handles).
    // The funding store rides on the raw local KVStore — funding txs are
    // local-only, like the PWA's IDB stores, never on VSS.
    let channels_ctx = Arc::new(ChannelEventContext {
        channel_manager: Arc::clone(&components.channel_manager),
        onchain_wallet: Arc::clone(&components.onchain_wallet),
        broadcaster: Arc::clone(&components.broadcaster),
        chain_source: Arc::clone(&components.chain_source),
        known_peers: Arc::clone(&components.known_peers),
        funding: Arc::new(FundingStore::new(
            Arc::clone(&components.kv_store),
            Arc::clone(&components.logger),
        )),
    });
    let event_handler = move |event: Event| {
        let sweep_engine = Arc::clone(&sweep_engine);
        let sweep_wake = Arc::clone(&sweep_wake);
        let bump_handler = Arc::clone(&bump_handler);
        let liquidity_source = Arc::clone(&liquidity_source);
        let payment_store = Arc::clone(&payment_store);
        let channels_ctx = Arc::clone(&channels_ctx);
        let close_records = Arc::clone(&close_records);
        let recovery = Arc::clone(&recovery);
        let event_sink = Arc::clone(&event_sink);
        let logger = Arc::clone(&event_logger);
        async move {
            handle_ldk_event(
                event,
                &sweep_engine,
                &sweep_wake,
                &bump_handler,
                &liquidity_source,
                &payment_store,
                &channels_ctx,
                &close_records,
                &recovery,
                &event_sink,
                &logger,
            )
        }
    };

    let sleeper = move |duration: Duration| {
        let mut stop = bp_stop_receiver.clone();
        Box::pin(async move {
            tokio::select! {
                _ = stop.changed() => true,
                _ = tokio::time::sleep(duration) => false,
            }
        })
    };

    runtime.spawn(async move {
        let res = process_events_async_with_kv_store_sync(
            kv_store,
            event_handler,
            chain_monitor,
            channel_manager,
            Some(onion_messenger),
            GossipSync::rapid(gossip_sync),
            peer_manager,
            // The LiquidityManager rides in BOTH this slot (message-queue
            // polling + persistence) and the peer manager's custom message
            // handler — omitting either makes LSPS2 silently do nothing.
            Some(liquidity_manager),
            // U11/KTD-8: no OutputSweeper — the core-owned descriptor store
            // (`crate::sweep`) owns tracking, sweeping, and attribution. The
            // alias only types the empty slot.
            None::<Arc<Sweeper>>,
            logger,
            Some(scorer),
            sleeper,
            // KTD-3: mobile platform, interruptible sleeps.
            true,
            || Some(unix_now()),
        )
        .await;
        if let Err(e) = res {
            log_error!(error_logger, "Background processor exited with error: {e}");
        }
    })
}
