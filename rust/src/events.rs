//! Public FFI events and the persisted handle-then-ack event queue (KTD-8),
//! mirroring ldk-node `src/event.rs`.
//!
//! Semantics:
//! - The queue is serialized to the KVStore on EVERY push and ack, so it
//!   survives process death and node restarts.
//! - `next` returns the front event WITHOUT removing it; the same event is
//!   returned again until `ack` pops it (handle-then-ack — consumers must be
//!   idempotent, because a crash between handling and acking redelivers).
//! - Waking is runtime-independent: `tokio::sync::Notify` is a pure futures
//!   primitive that needs no reactor or runtime. `notify_one()` can be called
//!   from any plain thread (e.g. while the node's runtime is being dropped in
//!   `stop()`), and the exported async `next_event` future is polled by the
//!   FOREIGN executor via UniFFI — it never touches the node's tokio runtime.
//!   `notify_one` stores a permit when no waiter is registered, so a push
//!   racing between the queue check and the `notified().await` registration is
//!   never lost. The queue assumes a single consumer (the one Kotlin event
//!   loop); with multiple concurrent `next` callers only one is woken per
//!   push.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use lightning::log_error;
use lightning::util::logger::Logger as _;
use lightning::util::persist::KVStoreSync;
use lightning_persister::fs_store::FilesystemStore;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::node::{CoreEvent, EventSink};
use crate::types::Logger;

/// KVStore location of the serialized queue, mirroring ldk-node (top-level
/// namespace, key `events`).
pub(crate) const EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE: &str = "";
pub(crate) const EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE: &str = "";
pub(crate) const EVENT_QUEUE_PERSISTENCE_KEY: &str = "events";

/// Public wallet events, consumed from Kotlin via `next_event`/`event_handled`
/// in handle-then-ack order (KTD-8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Enum)]
pub enum Event {
    /// The node started. Emitted before chain sync completes, so the queue is
    /// observable with no network (a degraded offline start still emits it).
    NodeStarted,
    /// Terminal event: the node stopped. Completes any pending `next_event`
    /// await; the Kotlin event loop treats it as loop exit.
    NodeStopped,
    /// A chain sync pass failed; sync retries in the background.
    SyncFailed,
    /// Chain sync recovered and reached the tip (clears a `SyncFailed` state).
    SyncCompleted,
    /// A JIT invoice is ready to display (U4). `expiry_unix_secs` is the
    /// LSP-guaranteed `valid_until` as UNIX seconds.
    InvoiceReady {
        bolt11: String,
        expiry_unix_secs: u64,
    },
    /// An inbound payment was claimed (U4/U5). `skimmed_fee_msat` is the JIT
    /// channel opening fee withheld by the LSP, if any.
    PaymentReceived {
        payment_hash: String,
        amount_msat: u64,
        skimmed_fee_msat: Option<u64>,
    },
    /// An outbound payment succeeded (U5). The history row is settled
    /// durably BEFORE this event is queued (persist-then-ack).
    PaymentSuccessful {
        payment_hash: String,
        fee_paid_msat: Option<u64>,
    },
    /// An outbound payment failed (U5). `payment_hash` is `None` for BOLT12
    /// payments that failed before an invoice arrived.
    PaymentFailed {
        payment_hash: Option<String>,
        reason: String,
    },
    /// An inbound JIT channel is pending (U4/U5).
    ChannelPending { channel_id: String },
    /// An inbound JIT channel is usable (U4/U5).
    ChannelReady { channel_id: String },
    /// A channel closed (U9): `reason` is LDK's `ClosureReason` rendered.
    ChannelClosed { channel_id: String, reason: String },
    /// The LSPS2 flow failed (U4): get_info/buy errors, fee-floor rejections.
    Lsps2Failed { reason: String },
    /// Cloud backup (VSS) writes are failing; local persistence continues.
    /// (U5 defines the variant; U2's VSS path fires it.)
    BackupDegraded { detail: String },
    /// Another client took over this seed's cloud store; the node is fenced.
    /// (U5 defines the variant; the VSS fence fires it.)
    Fenced { detail: String },
    /// The sweep pipeline's state changed (U5 defines; U11 fires).
    SweepStateChanged,
    /// Force-close recovery state changed (U5 defines; U10 fires).
    RecoveryStateChanged,
    /// Restore-from-seed progress (U5 defines; U4's restore flow fires).
    RestoreProgress { step: String },
}

/// The persisted event queue. Owns its own `FilesystemStore` handle so pushes
/// (e.g. the terminal `NodeStopped`) persist even while the node — and its
/// runtime — are gone.
pub(crate) struct EventQueue {
    queue: Mutex<VecDeque<Event>>,
    notify: Notify,
    kv_store: Arc<FilesystemStore>,
    logger: Arc<Logger>,
}

impl EventQueue {
    /// Loads the persisted queue from the store, starting empty when nothing
    /// was persisted yet. Corrupt queue data is logged and dropped (degrade to
    /// empty) rather than bricking the wallet: events drive UI, not funds.
    pub(crate) fn new(kv_store: Arc<FilesystemStore>, logger: Arc<Logger>) -> Self {
        let queue = match kv_store.read(
            EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE,
            EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE,
            EVENT_QUEUE_PERSISTENCE_KEY,
        ) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                log_error!(
                    logger,
                    "Persisted event queue is corrupt, starting empty: {e}"
                );
                VecDeque::new()
            }),
            Err(e) if e.kind() == lightning::io::ErrorKind::NotFound => VecDeque::new(),
            Err(e) => {
                log_error!(logger, "Failed to read event queue, starting empty: {e}");
                VecDeque::new()
            }
        };
        Self {
            queue: Mutex::new(queue),
            notify: Notify::new(),
            kv_store,
            logger,
        }
    }

    /// Appends an event, persists the queue, and wakes a pending `next`.
    /// A persistence failure is logged and degrades to in-memory delivery.
    pub(crate) fn push(&self, event: Event) {
        {
            let mut queue = self.queue.lock().unwrap();
            queue.push_back(event);
            self.persist(&queue);
        }
        self.notify.notify_one();
    }

    /// The front event without removing it, if any.
    pub(crate) fn peek(&self) -> Option<Event> {
        self.queue.lock().unwrap().front().cloned()
    }

    /// Awaits the front event without removing it. Runtime-independent: safe
    /// to poll from the foreign executor with no tokio runtime alive.
    pub(crate) async fn next(&self) -> Event {
        loop {
            if let Some(event) = self.peek() {
                return event;
            }
            // A push racing in here is not lost: notify_one stored a permit,
            // so this await completes immediately and the loop re-checks.
            self.notify.notified().await;
        }
    }

    /// Keeps only the events matching the predicate, re-persisting when
    /// anything was removed. Used by `start()` to purge a stale `NodeStopped`
    /// persisted by a previous process: that terminal event only has meaning
    /// for completing a pending `next_event` in the process that pushed it —
    /// redelivered on a later launch it would exit the consumer's event loop
    /// while the node runs.
    pub(crate) fn retain(&self, predicate: impl FnMut(&Event) -> bool) {
        let mut queue = self.queue.lock().unwrap();
        let len_before = queue.len();
        queue.retain(predicate);
        if queue.len() != len_before {
            self.persist(&queue);
        }
    }

    /// Pops the front event (the ack half of handle-then-ack) and re-persists.
    pub(crate) fn ack(&self) -> Option<Event> {
        let mut queue = self.queue.lock().unwrap();
        let popped = queue.pop_front();
        if popped.is_some() {
            self.persist(&queue);
        }
        popped
    }

    /// Serializes the queue to the KVStore (called under the queue lock, so
    /// writes are ordered). Persistence failure degrades to in-memory
    /// delivery — events drive UI, not funds.
    fn persist(&self, queue: &VecDeque<Event>) {
        let bytes = match serde_json::to_vec(queue) {
            Ok(bytes) => bytes,
            Err(e) => {
                log_error!(self.logger, "Failed to serialize event queue: {e}");
                return;
            }
        };
        if let Err(e) = self.kv_store.write(
            EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE,
            EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE,
            EVENT_QUEUE_PERSISTENCE_KEY,
            bytes,
        ) {
            log_error!(self.logger, "Failed to persist event queue: {e}");
        }
    }
}

impl EventSink for EventQueue {
    fn emit(&self, event: CoreEvent) {
        let event = match event {
            CoreEvent::ChainSyncCompleted => Event::SyncCompleted,
            CoreEvent::ChainSyncFailed => Event::SyncFailed,
            CoreEvent::InvoiceReady {
                bolt11,
                expiry_unix_secs,
            } => Event::InvoiceReady {
                bolt11,
                expiry_unix_secs,
            },
            CoreEvent::PaymentReceived {
                payment_hash,
                amount_msat,
                skimmed_fee_msat,
            } => Event::PaymentReceived {
                payment_hash,
                amount_msat,
                skimmed_fee_msat,
            },
            CoreEvent::ChannelPending { channel_id } => Event::ChannelPending { channel_id },
            CoreEvent::ChannelReady { channel_id } => Event::ChannelReady { channel_id },
            CoreEvent::ChannelClosed { channel_id, reason } => {
                Event::ChannelClosed { channel_id, reason }
            }
            CoreEvent::Lsps2Failed { reason } => Event::Lsps2Failed { reason },
            CoreEvent::PaymentSuccessful {
                payment_hash,
                fee_paid_msat,
            } => Event::PaymentSuccessful {
                payment_hash,
                fee_paid_msat,
            },
            CoreEvent::PaymentFailed {
                payment_hash,
                reason,
            } => Event::PaymentFailed {
                payment_hash,
                reason,
            },
            CoreEvent::BackupDegraded { detail } => Event::BackupDegraded { detail },
            CoreEvent::Fenced { detail } => Event::Fenced { detail },
            CoreEvent::SweepStateChanged => Event::SweepStateChanged,
            CoreEvent::RecoveryStateChanged => Event::RecoveryStateChanged,
            CoreEvent::RestoreProgress { step } => Event::RestoreProgress { step },
        };
        self.push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::Duration;

    fn queue_in(dir: &Path) -> EventQueue {
        EventQueue::new(
            Arc::new(FilesystemStore::new(dir.join("store"))),
            Arc::new(Logger),
        )
    }

    fn current_thread_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    #[test]
    fn pushed_event_is_returned_by_next_and_ack_advances() {
        let dir = tempfile::tempdir().unwrap();
        let queue = queue_in(dir.path());
        let rt = current_thread_rt();

        queue.push(Event::NodeStarted);
        queue.push(Event::SyncFailed);

        // Same event until acked (handle-then-ack).
        assert_eq!(rt.block_on(queue.next()), Event::NodeStarted);
        assert_eq!(rt.block_on(queue.next()), Event::NodeStarted);

        assert_eq!(queue.ack(), Some(Event::NodeStarted));
        assert_eq!(rt.block_on(queue.next()), Event::SyncFailed);
        assert_eq!(queue.ack(), Some(Event::SyncFailed));
        assert_eq!(queue.ack(), None);
        assert_eq!(queue.peek(), None);
    }

    #[test]
    fn unacked_event_is_redelivered_after_reload_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let event = Event::PaymentReceived {
            payment_hash: "11".repeat(32),
            amount_msat: 250_000,
            skimmed_fee_msat: Some(1_000),
        };

        let queue = queue_in(dir.path());
        queue.push(event.clone());
        assert_eq!(queue.peek(), Some(event.clone()));
        drop(queue);

        // No ack happened: the rebuilt queue must return the SAME event again
        // (idempotent consumers absorb the redelivery).
        let reloaded = queue_in(dir.path());
        assert_eq!(reloaded.peek(), Some(event));
    }

    #[test]
    fn queue_of_three_survives_drop_and_rebuild_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let events = [
            Event::NodeStarted,
            Event::InvoiceReady {
                bolt11: "lnbc1exampleinvoice".to_string(),
                expiry_unix_secs: 1_753_500_000,
            },
            Event::PaymentFailed {
                payment_hash: Some("22".repeat(32)),
                reason: "no route".to_string(),
            },
        ];

        let queue = queue_in(dir.path());
        for event in &events {
            queue.push(event.clone());
        }
        drop(queue);

        let reloaded = queue_in(dir.path());
        for event in &events {
            assert_eq!(reloaded.ack().as_ref(), Some(event), "order must survive");
        }
        assert_eq!(reloaded.ack(), None);

        // Acks persisted too: a further reload starts empty.
        drop(reloaded);
        assert_eq!(queue_in(dir.path()).peek(), None);
    }

    #[test]
    fn next_awaits_until_a_plain_thread_push_wakes_it() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Arc::new(queue_in(dir.path()));
        let rt = current_thread_rt();

        // Push from a bare std thread — no tokio runtime anywhere near it —
        // proving the wake-up path `stop()` relies on is runtime-independent.
        let pusher = {
            let queue = Arc::clone(&queue);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                queue.push(Event::SweepStateChanged);
            })
        };

        let event = rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), queue.next()).await })
            .expect("next must be woken by the push, not hang");
        assert_eq!(event, Event::SweepStateChanged);
        pusher.join().unwrap();
    }

    #[test]
    fn core_events_map_into_public_events() {
        let dir = tempfile::tempdir().unwrap();
        let queue = queue_in(dir.path());

        queue.emit(CoreEvent::ChainSyncFailed);
        queue.emit(CoreEvent::ChainSyncCompleted);
        queue.emit(CoreEvent::InvoiceReady {
            bolt11: "lnbc1example".to_string(),
            expiry_unix_secs: 1_753_500_000,
        });
        queue.emit(CoreEvent::PaymentReceived {
            payment_hash: "aa".repeat(32),
            amount_msat: 250_000,
            skimmed_fee_msat: Some(2_000),
        });
        queue.emit(CoreEvent::ChannelPending {
            channel_id: "bb".repeat(32),
        });
        queue.emit(CoreEvent::ChannelReady {
            channel_id: "bb".repeat(32),
        });
        queue.emit(CoreEvent::ChannelClosed {
            channel_id: "bb".repeat(32),
            reason: "counterparty force-closed".to_string(),
        });
        queue.emit(CoreEvent::Lsps2Failed {
            reason: "all LSP-offered opening fee params are expired".to_string(),
        });
        queue.emit(CoreEvent::PaymentSuccessful {
            payment_hash: "cc".repeat(32),
            fee_paid_msat: Some(21),
        });
        queue.emit(CoreEvent::PaymentFailed {
            payment_hash: None,
            reason: "no route to the recipient was found".to_string(),
        });
        queue.emit(CoreEvent::BackupDegraded {
            detail: "vss write failed".to_string(),
        });
        queue.emit(CoreEvent::Fenced {
            detail: "another client took over".to_string(),
        });
        queue.emit(CoreEvent::SweepStateChanged);
        queue.emit(CoreEvent::RecoveryStateChanged);
        queue.emit(CoreEvent::RestoreProgress {
            step: "monitors".to_string(),
        });

        assert_eq!(queue.ack(), Some(Event::SyncFailed));
        assert_eq!(queue.ack(), Some(Event::SyncCompleted));
        assert_eq!(
            queue.ack(),
            Some(Event::InvoiceReady {
                bolt11: "lnbc1example".to_string(),
                expiry_unix_secs: 1_753_500_000,
            })
        );
        assert_eq!(
            queue.ack(),
            Some(Event::PaymentReceived {
                payment_hash: "aa".repeat(32),
                amount_msat: 250_000,
                skimmed_fee_msat: Some(2_000),
            })
        );
        assert_eq!(
            queue.ack(),
            Some(Event::ChannelPending {
                channel_id: "bb".repeat(32),
            })
        );
        assert_eq!(
            queue.ack(),
            Some(Event::ChannelReady {
                channel_id: "bb".repeat(32),
            })
        );
        assert_eq!(
            queue.ack(),
            Some(Event::ChannelClosed {
                channel_id: "bb".repeat(32),
                reason: "counterparty force-closed".to_string(),
            })
        );
        assert_eq!(
            queue.ack(),
            Some(Event::Lsps2Failed {
                reason: "all LSP-offered opening fee params are expired".to_string(),
            })
        );
        assert_eq!(
            queue.ack(),
            Some(Event::PaymentSuccessful {
                payment_hash: "cc".repeat(32),
                fee_paid_msat: Some(21),
            })
        );
        assert_eq!(
            queue.ack(),
            Some(Event::PaymentFailed {
                payment_hash: None,
                reason: "no route to the recipient was found".to_string(),
            })
        );
        assert_eq!(
            queue.ack(),
            Some(Event::BackupDegraded {
                detail: "vss write failed".to_string(),
            })
        );
        assert_eq!(
            queue.ack(),
            Some(Event::Fenced {
                detail: "another client took over".to_string(),
            })
        );
        assert_eq!(queue.ack(), Some(Event::SweepStateChanged));
        assert_eq!(queue.ack(), Some(Event::RecoveryStateChanged));
        assert_eq!(
            queue.ack(),
            Some(Event::RestoreProgress {
                step: "monitors".to_string(),
            })
        );
    }

    #[test]
    fn corrupt_persisted_queue_degrades_to_empty_and_keeps_working() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStore::new(dir.path().join("store")));
        store
            .write(
                EVENT_QUEUE_PERSISTENCE_PRIMARY_NAMESPACE,
                EVENT_QUEUE_PERSISTENCE_SECONDARY_NAMESPACE,
                EVENT_QUEUE_PERSISTENCE_KEY,
                b"not json".to_vec(),
            )
            .unwrap();

        let queue = EventQueue::new(store, Arc::new(Logger));
        assert_eq!(
            queue.peek(),
            None,
            "corrupt queue data must degrade, not brick"
        );
        queue.push(Event::NodeStarted);
        assert_eq!(queue.ack(), Some(Event::NodeStarted));
    }
}
