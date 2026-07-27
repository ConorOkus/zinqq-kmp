//! VSS dual-write persistence (U3; R3; KTD-3): the fund-safety core.
//!
//! Split monitor/CM semantics per KTD-3:
//!
//! - **Monitors** go through [`VssBackedStore`]'s custom
//!   [`Persist`] implementation: `persist_new_channel` /
//!   `update_persisted_channel` return [`ChannelMonitorUpdateStatus::InProgress`]
//!   and spawn a per-channel serialized write chain doing VSS put → (for new
//!   channels) `_monitor_keys` manifest put → local [`FilesystemStore`] write →
//!   `channel_monitor_updated`. Transient failures retry with indefinite
//!   exponential backoff (500 ms → 60 s) and a [`CoreEvent::BackupDegraded`]
//!   after 10 s; LDK halts channel operations until the completion signal,
//!   which is exactly the fund-safe outcome.
//! - **Channel manager** writes route through [`DualWriteKvStore`] (the
//!   `KVStoreSync` handed to the background processor): ONE bounded VSS
//!   attempt, then the local write ALWAYS happens; failure sets a dirty flag
//!   the node's timer tick retries. CM persistence never gates the event loop.
//! - **Version conflicts (409) on fund-critical keys are content-compared,
//!   never blindly retried**: refetch, compare decrypted plaintexts (retries
//!   resend the same plaintext buffer, and the U2 client re-sends identical
//!   ciphertext, so the compare is sound); identical → short-circuit success
//!   at the server version; divergent → **fence**: durable `fenced` flag file,
//!   [`CoreEvent::Fenced`], zero further puts, node halt via the fence watch.
//!   Un-fencing is user-owned (wipe + restore); restart refuses with a typed
//!   error while the flag exists.
//! - `_monitor_keys` manifest: PWA format (JSON array of `{txid_hex}:{index}`
//!   keys, raw-byte txid order, ≤ 1000 entries, dedup) with merge-on-conflict —
//!   never fence semantics — and the same indefinite backoff when it gates a
//!   `persist_new_channel` completion.
//! - `_known_peers` and other LWW keys use [`VssBackedStore::put_lww`]:
//!   conflict → adopt server version → retry once with our bytes (PWA parity).
//!
//! Source of truth on restart is LOCAL storage; remote may lead local at a
//! crash seam, which is benign because completion was never signalled and LDK
//! re-persists (the startup version seeding in [`super::startup`] adopts the
//! server version so no fence trips).

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use bitcoin::hashes::Hash as _;
use lightning::chain::chainmonitor::Persist;
use lightning::chain::channelmonitor::{ChannelMonitor, ChannelMonitorUpdate};
use lightning::chain::ChannelMonitorUpdateStatus;
use lightning::ln::types::ChannelId;
use lightning::sign::ecdsa::EcdsaChannelSigner;
use lightning::util::logger::Logger as _;
use lightning::util::persist::{
    KVStoreSync, MonitorName, ARCHIVED_CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
    ARCHIVED_CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE, CHANNEL_MANAGER_PERSISTENCE_KEY,
    CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE, CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
    CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE, CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
};
use lightning::util::ser::Writeable;
use lightning::{log_error, log_info};
use lightning_persister::fs_store::FilesystemStore;
use tokio::runtime::Handle;
use tokio::sync::watch;

use super::{client::VssWireClient, VssError};
use crate::node::{CoreEvent, EventSink};
use crate::types::Logger;
use crate::util::hex_str;

/// The VSS plaintext key of the monitor manifest (PWA `MONITOR_MANIFEST_KEY`).
pub(crate) const MONITOR_MANIFEST_KEY: &str = "_monitor_keys";

/// The VSS plaintext key of the whole-map known-peers blob.
pub(crate) const KNOWN_PEERS_VSS_KEY: &str = "_known_peers";

/// The close-records singleton map (U10, R9): the whole channelId → record
/// map lives under ONE key because per-record keys can never be enumerated on
/// restore (keys are HMAC-obfuscated) — PWA `close-records/store.ts:35`.
pub(crate) const CLOSE_RECORDS_VSS_KEY: &str = "close_records";

/// The force-close recovery state blob (U10, R9) — PWA `recovery-state.ts:8`.
pub(crate) const FORCE_CLOSE_RECOVERY_VSS_KEY: &str = "force_close_recovery";

/// The VSS plaintext key of the channel manager (PWA `CM_VSS_KEY`). The LOCAL
/// key stays LDK's `("", "", "manager")` constants — only the remote name is
/// PWA-shaped.
pub(crate) const CHANNEL_MANAGER_VSS_KEY: &str = "channel_manager";

/// Manifest entry cap (PWA `MAX_MANIFEST_ENTRIES`).
pub(crate) const MAX_MANIFEST_ENTRIES: usize = 1_000;

/// Durable fenced flag in the data dir: written when a divergent-content 409
/// proves another client owns the VSS store; its presence makes `Node::start`
/// refuse with [`crate::builder::BuildError::Fenced`] until the user wipes and
/// restores (KTD-3: no automatic un-fence).
pub(crate) const FENCED_FLAG_FILE_NAME: &str = "fenced";

/// Backoff/threshold tuning (KTD-3 values by default; tests inject
/// millisecond-scale values so failure paths run instantly).
#[derive(Clone, Copy, Debug)]
pub(crate) struct RetryTuning {
    /// First retry delay (500 ms), doubling per retry.
    pub initial_backoff: Duration,
    /// Backoff cap (60 s).
    pub max_backoff: Duration,
    /// Cumulative failure time after which `BackupDegraded` fires (10 s).
    pub degraded_after: Duration,
    /// Upper bound on the channel manager's single bounded VSS attempt.
    pub cm_attempt_timeout: Duration,
}

impl Default for RetryTuning {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(60),
            degraded_after: Duration::from_secs(10),
            cm_attempt_timeout: Duration::from_secs(10),
        }
    }
}

/// Boxed future alias for the object-safe transport seam.
pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A fetched `(plaintext bytes, server version)` pair, `None` when the key
/// does not exist.
pub(crate) type VersionedValue = Option<(Vec<u8>, i64)>;

/// U10 field-wise merge callback: takes the remote bytes fetched on a 409,
/// folds them into the caller's local store (base = local), returns the
/// merged bytes to rewrite.
pub(crate) type MergeFn = Arc<dyn Fn(&[u8]) -> Vec<u8> + Send + Sync>;

/// The transport seam over the U2 wire client, at the PLAINTEXT level
/// (obfuscation/encryption live below it, in the wire client). Tests inject a
/// deterministic in-memory implementation to drive failures, conflicts, and
/// crash seams without a network.
pub(crate) trait VssTransport: Send + Sync {
    /// Fetch + decrypt; `None` when the key does not exist.
    fn get<'a>(&'a self, plaintext_key: &'a str)
        -> BoxFuture<'a, Result<VersionedValue, VssError>>;
    /// Versioned put; returns the new version.
    fn put<'a>(
        &'a self,
        plaintext_key: &'a str,
        value: &'a [u8],
        version: i64,
    ) -> BoxFuture<'a, Result<i64, VssError>>;
    /// One transactional multi-item put (the migration batch).
    fn put_many<'a>(
        &'a self,
        items: Vec<(String, Vec<u8>, i64)>,
    ) -> BoxFuture<'a, Result<(), VssError>>;
    /// Versioned delete.
    fn delete<'a>(
        &'a self,
        plaintext_key: &'a str,
        version: i64,
    ) -> BoxFuture<'a, Result<(), VssError>>;
    /// All (obfuscated key, version) pairs in the namespace.
    fn list_key_versions<'a>(&'a self) -> BoxFuture<'a, Result<Vec<(String, i64)>, VssError>>;
    /// The obfuscated wire form of `plaintext_key`, for matching listing
    /// entries against known keys.
    fn obfuscate(&self, plaintext_key: &str) -> String;
}

impl VssTransport for VssWireClient {
    fn get<'a>(
        &'a self,
        plaintext_key: &'a str,
    ) -> BoxFuture<'a, Result<VersionedValue, VssError>> {
        Box::pin(self.get_object(plaintext_key))
    }

    fn put<'a>(
        &'a self,
        plaintext_key: &'a str,
        value: &'a [u8],
        version: i64,
    ) -> BoxFuture<'a, Result<i64, VssError>> {
        Box::pin(self.put_object(plaintext_key, value, version))
    }

    fn put_many<'a>(
        &'a self,
        items: Vec<(String, Vec<u8>, i64)>,
    ) -> BoxFuture<'a, Result<(), VssError>> {
        Box::pin(self.put_objects(items))
    }

    fn delete<'a>(
        &'a self,
        plaintext_key: &'a str,
        version: i64,
    ) -> BoxFuture<'a, Result<(), VssError>> {
        Box::pin(self.delete_object(plaintext_key, version))
    }

    fn list_key_versions<'a>(&'a self) -> BoxFuture<'a, Result<Vec<(String, i64)>, VssError>> {
        Box::pin(VssWireClient::list_key_versions(self))
    }

    fn obfuscate(&self, plaintext_key: &str) -> String {
        self.obfuscated_key(plaintext_key)
    }
}

/// Whether `key` is a valid PWA monitor-manifest entry:
/// `/^[0-9a-f]{64}:\d+$/`.
pub(crate) fn is_valid_monitor_key(key: &str) -> bool {
    let Some((txid, index)) = key.split_once(':') else {
        return false;
    };
    txid.len() == 64
        && txid.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        && !index.is_empty()
        && index.bytes().all(|b| b.is_ascii_digit())
}

/// Parses and validates a monitor manifest exactly like the PWA's
/// `parseMonitorManifest`: a non-empty JSON array of ≤ 1000 regex-valid keys,
/// deduplicated. Any violation is an error (a corrupt manifest must never
/// silently drop monitors).
pub(crate) fn parse_monitor_manifest(bytes: &[u8]) -> Result<Vec<String>, String> {
    let parsed: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| format!("manifest is not JSON: {e}"))?;
    let entries = parsed
        .as_array()
        .ok_or_else(|| "monitor manifest is not an array".to_string())?;
    if entries.is_empty() {
        return Err("monitor manifest is not a non-empty array".to_string());
    }
    if entries.len() > MAX_MANIFEST_ENTRIES {
        return Err(format!(
            "monitor manifest has {} entries, exceeds max of {MAX_MANIFEST_ENTRIES}",
            entries.len()
        ));
    }
    let mut seen: Vec<String> = Vec::new();
    for entry in entries {
        let key = entry
            .as_str()
            .filter(|key| is_valid_monitor_key(key))
            .ok_or_else(|| format!("invalid monitor key in manifest: {entry}"))?;
        if !seen.iter().any(|existing| existing == key) {
            seen.push(key.to_string());
        }
    }
    Ok(seen)
}

/// The PWA's monitor storage key: `hex(funding txid raw bytes):{index}`. The
/// txid hex uses the RAW serialized byte order (what the PWA's
/// `bytesToHex(outpoint.get_txid())` produces), NOT rust-bitcoin's reversed
/// display order — cross-client restore depends on this exact form.
pub(crate) fn monitor_vss_key(funding: &lightning::chain::transaction::OutPoint) -> String {
    format!(
        "{}:{}",
        hex_str(&funding.txid.to_byte_array()),
        funding.index
    )
}

/// Where the write chain reports monitor durability. Implemented by the real
/// `ChainMonitor` (via `channel_monitor_updated`) and by test recorders. Held
/// as a `Weak` to break the `ChainMonitor` ↔ store reference cycle.
pub(crate) trait CompletionSink: Send + Sync {
    fn monitor_updated(&self, channel_id: ChannelId, update_id: u64);
}

impl CompletionSink for crate::types::ChainMonitor {
    fn monitor_updated(&self, channel_id: ChannelId, update_id: u64) {
        if let Err(e) = self.channel_monitor_updated(channel_id, update_id) {
            // Benign for archived channels; anything else deserves the log.
            log_error!(
                Logger,
                "channel_monitor_updated({channel_id}, {update_id}) rejected: {e:?}"
            );
        }
    }
}

/// Everything a monitor write chain needs, pulled out of the borrowed
/// `ChannelMonitor` BEFORE anything async runs (the monitor reference does not
/// outlive the `Persist` callback).
pub(crate) struct MonitorWrite {
    /// `MonitorName::to_string()` — the LDK-side identity, mapped to the VSS
    /// key for `archive_persisted_channel` (name-only in LDK 0.2).
    pub monitor_name: String,
    /// PWA VSS key `{txid_hex}:{index}` (raw txid byte order).
    pub vss_key: String,
    /// Local `FilesystemStore` key under LDK's monitor namespace
    /// (`MonitorName::to_string()`, so `read_channel_monitors` keeps working).
    pub local_key: String,
    pub channel_id: ChannelId,
    pub update_id: u64,
    pub bytes: Vec<u8>,
}

fn extract_monitor_write<CS: EcdsaChannelSigner>(
    monitor_name: MonitorName,
    monitor: &ChannelMonitor<CS>,
) -> MonitorWrite {
    let name = monitor_name.to_string();
    MonitorWrite {
        vss_key: monitor_vss_key(&monitor.get_funding_txo()),
        local_key: name.clone(),
        monitor_name: name,
        channel_id: monitor.channel_id(),
        update_id: monitor.get_latest_update_id(),
        bytes: monitor.encode(),
    }
}

/// Marker: the write chain stopped because the store fenced itself. The chain
/// never signals completion, so LDK keeps the channel halted — fund-safe.
struct FenceStop;

/// Runs `fut` to completion from a synchronous context. From inside the
/// runtime (background-processor thread) this uses `block_in_place`, which
/// requires the multi-thread runtime the node always creates; from a plain
/// thread it blocks on the handle directly.
fn run_blocking<F: Future>(handle: &Handle, fut: F) -> F::Output {
    if Handle::try_current().is_ok() {
        tokio::task::block_in_place(|| handle.block_on(fut))
    } else {
        handle.block_on(fut)
    }
}

fn write_fenced_flag(path: &Path, detail: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::File::create(path)?;
    file.write_all(detail.as_bytes())?;
    file.sync_all()
}

struct Inner {
    /// `None` = local-only (vss_disabled, or migration failed this session):
    /// monitors persist synchronously to the local store and complete
    /// immediately, exactly like the pre-U3 spike wiring.
    remote: Option<Arc<dyn VssTransport>>,
    local: Arc<FilesystemStore>,
    runtime: Handle,
    logger: Arc<Logger>,
    event_sink: Arc<dyn EventSink>,
    tuning: RetryTuning,
    /// In-memory version cache: plaintext key → last known server version.
    versions: Mutex<HashMap<String, i64>>,
    /// The `_monitor_keys` manifest set (dedup by construction).
    monitor_keys: Mutex<BTreeSet<String>>,
    /// `MonitorName::to_string()` → VSS key, for `archive_persisted_channel`.
    name_to_vss_key: Mutex<HashMap<String, String>>,
    /// Serializes manifest read-modify-write cycles across channels.
    manifest_lock: tokio::sync::Mutex<()>,
    /// Per-key write chain tails: a new job awaits the previous handle, so
    /// writes to one key apply strictly in submission order.
    chains: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    completion: OnceLock<Weak<dyn CompletionSink>>,
    /// `true` once fenced. Watch so the node can halt on the transition.
    fenced: watch::Sender<bool>,
    fenced_flag_path: PathBuf,
    /// Set when the bounded CM attempt failed; the node's timer tick retries.
    /// While set, further CM writes skip the remote attempt entirely so the
    /// background processor is never stalled twice by one outage.
    cm_dirty: AtomicBool,
    /// Whether `listKeyVersions` returned empty this session — the only state
    /// in which version-0 first writes of fund-critical fixed keys are
    /// expected (KTD-3). Recorded for diagnostics; the write path itself is
    /// protected by the content-compare fence regardless.
    probe_empty: bool,
}

impl Inner {
    fn is_fenced(&self) -> bool {
        *self.fenced.borrow()
    }

    fn cached_version(&self, key: &str) -> i64 {
        self.versions.lock().unwrap().get(key).copied().unwrap_or(0)
    }

    fn record_version(&self, key: &str, version: i64) {
        self.versions
            .lock()
            .unwrap()
            .insert(key.to_string(), version);
    }

    /// Chains `job` after the previous job for `chain_key`, keeping writes to
    /// one key strictly ordered (the PWA's per-channel write chains).
    fn enqueue(&self, chain_key: String, job: impl Future<Output = ()> + Send + 'static) {
        let mut chains = self.chains.lock().unwrap();
        let prev = chains.remove(&chain_key);
        let handle = self.runtime.spawn(async move {
            if let Some(prev) = prev {
                // A panicked predecessor must not wedge the chain.
                let _ = prev.await;
            }
            job.await;
        });
        chains.insert(chain_key, handle);
    }

    /// Fences the store: durable flag, `Fenced` event, poisoned puts, node
    /// halt via the watch. Idempotent. If the flag write itself fails, the
    /// in-memory fence still holds for this process and the divergent remote
    /// content re-trips it on the next start's first conflicting put.
    fn trip_fence(&self, detail: String) {
        let was_fenced = self.fenced.send_replace(true);
        if was_fenced {
            return;
        }
        log_error!(
            self.logger,
            "FENCED: another client wrote this wallet's VSS store ({detail}); halting all cloud \
             puts and signalling node halt"
        );
        if let Err(e) = write_fenced_flag(&self.fenced_flag_path, &detail) {
            log_error!(
                self.logger,
                "Failed to persist the fenced flag (fence still holds in-memory): {e}"
            );
        }
        self.event_sink.emit(CoreEvent::Fenced { detail });
    }

    /// The fund-critical put loop (KTD-3): indefinite exponential backoff on
    /// transient failures with a `BackupDegraded` after the threshold;
    /// content-compare on 409 — identical → success at the server version,
    /// divergent → fence and stop.
    async fn put_fund_critical_with_retry(&self, key: &str, bytes: &[u8]) -> Result<(), FenceStop> {
        let Some(remote) = self.remote.as_ref() else {
            return Ok(());
        };
        let mut backoff = self.tuning.initial_backoff;
        let mut waited = Duration::ZERO;
        let mut degraded_notified = false;
        loop {
            if self.is_fenced() {
                return Err(FenceStop);
            }
            let version = self.cached_version(key);
            let failure = match remote.put(key, bytes, version).await {
                Ok(new_version) => {
                    self.record_version(key, new_version);
                    return Ok(());
                }
                Err(VssError::Conflict { .. }) => match remote.get(key).await {
                    Ok(Some((remote_bytes, remote_version))) => {
                        self.record_version(key, remote_version);
                        if remote_bytes == bytes {
                            log_info!(
                                self.logger,
                                "409 on {key} resolved: identical content at server version \
                                 {remote_version}; short-circuiting to success"
                            );
                            return Ok(());
                        }
                        self.trip_fence(format!(
                            "divergent remote content for fund-critical key {key} at server \
                             version {remote_version}"
                        ));
                        return Err(FenceStop);
                    }
                    Ok(None) => {
                        // Deleted remotely (e.g. archived by another session):
                        // not content divergence — retry as a first write.
                        self.record_version(key, 0);
                        format!("409 on {key} but the key is gone remotely; retrying at version 0")
                    }
                    Err(e) => format!("409 refetch for {key} failed: {e}"),
                },
                Err(e) => format!("VSS put for {key} failed: {e}"),
            };
            log_error!(self.logger, "{failure}; retrying in {backoff:?}");
            tokio::time::sleep(backoff).await;
            waited += backoff;
            backoff = (backoff * 2).min(self.tuning.max_backoff);
            if !degraded_notified && waited >= self.tuning.degraded_after {
                degraded_notified = true;
                self.event_sink.emit(CoreEvent::BackupDegraded {
                    detail: format!(
                        "cloud-backup writes for {key} have been failing for {waited:?}; local \
                         persistence continues, channel operations wait for the backup"
                    ),
                });
            }
        }
    }

    /// Serialized manifest write: puts the current key set, merging the
    /// server's keys on conflict (never dropping a monitor another device
    /// tracks) and retrying indefinitely — this gates `persist_new_channel`
    /// completion (KTD-3: manifest gating is normative). Caller must hold
    /// `manifest_lock`.
    async fn write_manifest_with_retry_locked(&self) -> Result<(), FenceStop> {
        let Some(remote) = self.remote.as_ref() else {
            return Ok(());
        };
        let mut backoff = self.tuning.initial_backoff;
        let mut waited = Duration::ZERO;
        let mut degraded_notified = false;
        loop {
            if self.is_fenced() {
                return Err(FenceStop);
            }
            let keys: Vec<String> = self.monitor_keys.lock().unwrap().iter().cloned().collect();
            let payload =
                serde_json::to_vec(&keys).expect("a vec of strings always serializes to JSON");
            let version = self.cached_version(MONITOR_MANIFEST_KEY);
            let failure = match remote.put(MONITOR_MANIFEST_KEY, &payload, version).await {
                Ok(new_version) => {
                    self.record_version(MONITOR_MANIFEST_KEY, new_version);
                    return Ok(());
                }
                Err(VssError::Conflict { .. }) => match remote.get(MONITOR_MANIFEST_KEY).await {
                    Ok(Some((remote_bytes, remote_version))) => {
                        self.record_version(MONITOR_MANIFEST_KEY, remote_version);
                        match parse_monitor_manifest(&remote_bytes) {
                            Ok(server_keys) => {
                                self.monitor_keys.lock().unwrap().extend(server_keys);
                            }
                            Err(e) => log_error!(
                                self.logger,
                                "Server manifest parse failed, overwriting with local keys: {e}"
                            ),
                        }
                        // Retry immediately with the merged set at the server
                        // version (the PWA's merge-on-conflict).
                        continue;
                    }
                    Ok(None) => {
                        self.record_version(MONITOR_MANIFEST_KEY, 0);
                        continue;
                    }
                    Err(e) => format!("manifest 409 refetch failed: {e}"),
                },
                Err(e) => format!("manifest put failed: {e}"),
            };
            log_error!(self.logger, "{failure}; retrying in {backoff:?}");
            tokio::time::sleep(backoff).await;
            waited += backoff;
            backoff = (backoff * 2).min(self.tuning.max_backoff);
            if !degraded_notified && waited >= self.tuning.degraded_after {
                degraded_notified = true;
                self.event_sink.emit(CoreEvent::BackupDegraded {
                    detail: format!(
                        "monitor-manifest writes have been failing for {waited:?}; a pending \
                         channel open waits for the backup"
                    ),
                });
            }
        }
    }

    /// One best-effort manifest write (archive/backfill paths — nothing gates
    /// on it): single attempt plus one merge-retry on conflict.
    async fn write_manifest_once_best_effort(&self) {
        let Some(remote) = self.remote.as_ref() else {
            return;
        };
        if self.is_fenced() {
            return;
        }
        let _guard = self.manifest_lock.lock().await;
        for _ in 0..2 {
            let keys: Vec<String> = self.monitor_keys.lock().unwrap().iter().cloned().collect();
            let payload =
                serde_json::to_vec(&keys).expect("a vec of strings always serializes to JSON");
            let version = self.cached_version(MONITOR_MANIFEST_KEY);
            match remote.put(MONITOR_MANIFEST_KEY, &payload, version).await {
                Ok(new_version) => {
                    self.record_version(MONITOR_MANIFEST_KEY, new_version);
                    return;
                }
                Err(VssError::Conflict { .. }) => match remote.get(MONITOR_MANIFEST_KEY).await {
                    Ok(Some((remote_bytes, remote_version))) => {
                        self.record_version(MONITOR_MANIFEST_KEY, remote_version);
                        if let Ok(server_keys) = parse_monitor_manifest(&remote_bytes) {
                            self.monitor_keys.lock().unwrap().extend(server_keys);
                        }
                    }
                    Ok(None) => self.record_version(MONITOR_MANIFEST_KEY, 0),
                    Err(e) => {
                        log_error!(self.logger, "Best-effort manifest refetch failed: {e}");
                        return;
                    }
                },
                Err(e) => {
                    log_error!(self.logger, "Best-effort manifest write failed: {e}");
                    return;
                }
            }
        }
        log_error!(
            self.logger,
            "Best-effort manifest write still conflicted after a merge retry; giving up"
        );
    }

    /// Local write with indefinite retry: a failing local disk halts the
    /// completion signal exactly like a failing remote (fund-safe).
    async fn local_write_with_retry(
        &self,
        primary: &str,
        secondary: &str,
        key: &str,
        bytes: &[u8],
    ) {
        let mut backoff = self.tuning.initial_backoff;
        loop {
            match self.local.write(primary, secondary, key, bytes.to_vec()) {
                Ok(()) => return,
                Err(e) => {
                    log_error!(
                        self.logger,
                        "Local write {primary}/{secondary}/{key} failed: {e}; retrying in \
                         {backoff:?}"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(self.tuning.max_backoff);
                }
            }
        }
    }

    /// The monitor write chain body: VSS put (fund-critical semantics) → for
    /// new channels the gating manifest put → local write →
    /// `channel_monitor_updated`. Stopping anywhere before the completion
    /// signal halts channel operations, which is the fund-safe direction.
    async fn run_monitor_job(self: Arc<Self>, write: MonitorWrite, is_new: bool) {
        if self.is_fenced() {
            return;
        }
        if self
            .put_fund_critical_with_retry(&write.vss_key, &write.bytes)
            .await
            .is_err()
        {
            return;
        }
        if is_new {
            let guard = self.manifest_lock.lock().await;
            let result = self.write_manifest_with_retry_locked().await;
            drop(guard);
            if result.is_err() {
                return;
            }
        }
        self.local_write_with_retry(
            CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
            CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
            &write.local_key,
            &write.bytes,
        )
        .await;
        match self.completion.get().and_then(Weak::upgrade) {
            Some(sink) => sink.monitor_updated(write.channel_id, write.update_id),
            None => log_error!(
                self.logger,
                "Monitor write durable but no completion sink is registered; the update stays \
                 pending until restart"
            ),
        }
    }

    /// Bounded single CM attempt (KTD-3 channel-manager semantics): one put
    /// within `cm_attempt_timeout`; failure sets the dirty flag for the timer
    /// tick; 409 gets the content-compare fence treatment (CM is
    /// fund-critical).
    async fn cm_remote_attempt(&self, bytes: &[u8]) {
        let Some(remote) = self.remote.as_ref() else {
            return;
        };
        if self.is_fenced() {
            return;
        }
        let timeout = self.tuning.cm_attempt_timeout;
        let version = self.cached_version(CHANNEL_MANAGER_VSS_KEY);
        match tokio::time::timeout(timeout, remote.put(CHANNEL_MANAGER_VSS_KEY, bytes, version))
            .await
        {
            Ok(Ok(new_version)) => {
                self.record_version(CHANNEL_MANAGER_VSS_KEY, new_version);
                self.cm_dirty.store(false, Ordering::Release);
            }
            Ok(Err(VssError::Conflict { .. })) => {
                match tokio::time::timeout(timeout, remote.get(CHANNEL_MANAGER_VSS_KEY)).await {
                    Ok(Ok(Some((remote_bytes, remote_version)))) => {
                        self.record_version(CHANNEL_MANAGER_VSS_KEY, remote_version);
                        if remote_bytes == bytes {
                            self.cm_dirty.store(false, Ordering::Release);
                            log_info!(
                                self.logger,
                                "channel_manager 409 resolved: identical content at server \
                                 version {remote_version}"
                            );
                        } else {
                            self.trip_fence(format!(
                                "divergent remote channel_manager at server version \
                                 {remote_version}"
                            ));
                        }
                    }
                    Ok(Ok(None)) => {
                        self.record_version(CHANNEL_MANAGER_VSS_KEY, 0);
                        self.cm_dirty.store(true, Ordering::Release);
                    }
                    other => {
                        self.cm_dirty.store(true, Ordering::Release);
                        log_error!(
                            self.logger,
                            "channel_manager 409 refetch failed ({other:?}); marked dirty for \
                             the tick retry"
                        );
                    }
                }
            }
            Ok(Err(e)) => {
                self.cm_dirty.store(true, Ordering::Release);
                log_error!(
                    self.logger,
                    "Bounded channel_manager VSS attempt failed: {e}; marked dirty for the tick \
                     retry (local write proceeds)"
                );
            }
            Err(_elapsed) => {
                self.cm_dirty.store(true, Ordering::Release);
                log_error!(
                    self.logger,
                    "Bounded channel_manager VSS attempt timed out after {timeout:?}; marked \
                     dirty for the tick retry (local write proceeds)"
                );
            }
        }
    }

    /// LWW write (KTD-3 `_known_peers` semantics): put at the cached version;
    /// on conflict adopt the server version and retry ONCE with OUR bytes
    /// (last-writer-wins is acceptable for peers, per the PWA). Best-effort —
    /// failures log, nothing gates.
    async fn put_lww_attempt(&self, key: &str, bytes: &[u8]) {
        let Some(remote) = self.remote.as_ref() else {
            return;
        };
        if self.is_fenced() {
            return;
        }
        let version = self.cached_version(key);
        match remote.put(key, bytes, version).await {
            Ok(new_version) => self.record_version(key, new_version),
            Err(VssError::Conflict { .. }) => {
                let server_version = match remote.get(key).await {
                    Ok(Some((_, server_version))) => server_version,
                    Ok(None) => 0,
                    Err(e) => {
                        log_error!(self.logger, "LWW refetch for {key} failed: {e}");
                        return;
                    }
                };
                self.record_version(key, server_version);
                match remote.put(key, bytes, server_version).await {
                    Ok(new_version) => self.record_version(key, new_version),
                    Err(e) => log_error!(self.logger, "LWW retry for {key} failed: {e}"),
                }
            }
            Err(e) => log_error!(self.logger, "LWW write for {key} failed: {e}"),
        }
    }

    /// Field-wise-merge write (U10/R3/KTD-3 close-records semantics, PWA
    /// `close-records/store.ts:95-115`): put at the cached version; on 409
    /// refetch the remote blob, hand it to `merge` (which folds remote facts
    /// into the LOCAL store — direction base = local — and returns the merged
    /// bytes), then rewrite at the server version. Best-effort — failures
    /// log; local storage already has the record and the reconcile pass is
    /// the designated healer for lost VSS writes.
    async fn put_merge_attempt(
        &self,
        key: &str,
        bytes: &[u8],
        merge: &(dyn Fn(&[u8]) -> Vec<u8> + Send + Sync),
    ) {
        let Some(remote) = self.remote.as_ref() else {
            return;
        };
        if self.is_fenced() {
            return;
        }
        let version = self.cached_version(key);
        match remote.put(key, bytes, version).await {
            Ok(new_version) => self.record_version(key, new_version),
            Err(VssError::Conflict { .. }) => {
                // Another device wrote first: fetch, field-wise merge, rewrite.
                let (merged_bytes, server_version) = match remote.get(key).await {
                    Ok(Some((remote_bytes, server_version))) => {
                        (merge(&remote_bytes), server_version)
                    }
                    Ok(None) => (bytes.to_vec(), 0),
                    Err(e) => {
                        log_error!(self.logger, "Merge refetch for {key} failed: {e}");
                        return;
                    }
                };
                self.record_version(key, server_version);
                match remote.put(key, &merged_bytes, server_version).await {
                    Ok(new_version) => self.record_version(key, new_version),
                    Err(e) => log_error!(self.logger, "Merged rewrite for {key} failed: {e}"),
                }
            }
            Err(e) => log_error!(self.logger, "Merge write for {key} failed: {e}"),
        }
    }
}

/// The composite store: custom monitor [`Persist`] (async, VSS-first, gating)
/// plus the version cache, manifest set, fence state, and CM/LWW write paths
/// the rest of U3 builds on.
pub struct VssBackedStore {
    inner: Arc<Inner>,
}

impl VssBackedStore {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        remote: Option<Arc<dyn VssTransport>>,
        local: Arc<FilesystemStore>,
        runtime: Handle,
        storage_dir: &Path,
        event_sink: Arc<dyn EventSink>,
        logger: Arc<Logger>,
        tuning: RetryTuning,
        versions: HashMap<String, i64>,
        monitor_keys: BTreeSet<String>,
        probe_empty: bool,
    ) -> Self {
        let fenced_flag_path = storage_dir.join(FENCED_FLAG_FILE_NAME);
        let (fenced, _) = watch::channel(fenced_flag_path.exists());
        Self {
            inner: Arc::new(Inner {
                remote,
                local,
                runtime,
                logger,
                event_sink,
                tuning,
                versions: Mutex::new(versions),
                monitor_keys: Mutex::new(monitor_keys),
                name_to_vss_key: Mutex::new(HashMap::new()),
                manifest_lock: tokio::sync::Mutex::new(()),
                chains: Mutex::new(HashMap::new()),
                completion: OnceLock::new(),
                fenced,
                fenced_flag_path,
                cm_dirty: AtomicBool::new(false),
                probe_empty,
            }),
        }
    }

    /// Registers where completed monitor updates are reported (the real
    /// `ChainMonitor`, once it exists — it is constructed WITH this store, so
    /// the sink arrives after `new`).
    pub(crate) fn set_completion_sink(&self, sink: Weak<dyn CompletionSink>) {
        if self.inner.completion.set(sink).is_err() {
            log_error!(self.inner.logger, "Completion sink was already set");
        }
    }

    /// Pre-registers a monitor restored at startup: its `MonitorName` → VSS
    /// key mapping (for `archive_persisted_channel`, name-only in LDK 0.2)
    /// and its manifest membership.
    pub(crate) fn register_loaded_monitor<CS: EcdsaChannelSigner>(
        &self,
        monitor: &ChannelMonitor<CS>,
    ) {
        let vss_key = monitor_vss_key(&monitor.get_funding_txo());
        self.inner
            .name_to_vss_key
            .lock()
            .unwrap()
            .insert(monitor.persistence_key().to_string(), vss_key.clone());
        self.inner.monitor_keys.lock().unwrap().insert(vss_key);
    }

    /// Whether the fence has tripped (durable across restarts via the flag
    /// file).
    pub(crate) fn is_fenced(&self) -> bool {
        self.inner.is_fenced()
    }

    /// A watch on the fence state; the node halts its tasks when it flips.
    pub(crate) fn subscribe_fence(&self) -> watch::Receiver<bool> {
        self.inner.fenced.subscribe()
    }

    /// Whether `listKeyVersions` returned empty this session (KTD-3's
    /// `vss_probe_empty` record — the precondition for version-0 first
    /// writes of fund-critical fixed keys).
    pub(crate) fn probe_empty_this_session(&self) -> bool {
        self.inner.probe_empty
    }

    /// Whether a bounded CM attempt failed and awaits the tick retry.
    pub(crate) fn cm_dirty(&self) -> bool {
        self.inner.cm_dirty.load(Ordering::Acquire)
    }

    /// The current cached version for `key` (test/diagnostic surface).
    #[cfg(test)]
    pub(crate) fn cached_version(&self, key: &str) -> i64 {
        self.inner.cached_version(key)
    }

    /// Queues one monitor write on its per-channel chain and returns the LDK
    /// status: `InProgress` when a remote is configured (completion arrives
    /// via the chain), or the synchronous local-only result otherwise.
    pub(crate) fn queue_monitor_write(
        &self,
        write: MonitorWrite,
        is_new: bool,
    ) -> ChannelMonitorUpdateStatus {
        self.inner
            .name_to_vss_key
            .lock()
            .unwrap()
            .insert(write.monitor_name.clone(), write.vss_key.clone());
        if self.inner.remote.is_none() {
            // Local-only (vss_disabled / failed migration): the spike's
            // synchronous durable-before-Completed behavior.
            return match self.inner.local.write(
                CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
                CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
                &write.local_key,
                write.bytes,
            ) {
                Ok(()) => ChannelMonitorUpdateStatus::Completed,
                Err(e) => {
                    log_error!(
                        self.inner.logger,
                        "Local-only monitor write for {} failed: {e}",
                        write.local_key
                    );
                    ChannelMonitorUpdateStatus::UnrecoverableError
                }
            };
        }
        if is_new {
            self.inner
                .monitor_keys
                .lock()
                .unwrap()
                .insert(write.vss_key.clone());
        }
        if self.inner.is_fenced() {
            // Poisoned: zero further puts. InProgress with no completion
            // keeps LDK halted.
            log_error!(
                self.inner.logger,
                "Store is fenced; dropping monitor put for {}",
                write.vss_key
            );
            return ChannelMonitorUpdateStatus::InProgress;
        }
        let chain_key = write.vss_key.clone();
        let inner = Arc::clone(&self.inner);
        self.inner.enqueue(chain_key, async move {
            inner.run_monitor_job(write, is_new).await
        });
        ChannelMonitorUpdateStatus::InProgress
    }

    /// One bounded remote CM attempt from a synchronous context (the
    /// `DualWriteKvStore` write path). Skipped entirely while dirty — the
    /// timer tick owns retries, so one outage stalls the background processor
    /// at most once.
    pub(crate) fn cm_remote_write_bounded(&self, bytes: &[u8]) {
        if self.inner.remote.is_none() || self.inner.is_fenced() {
            return;
        }
        if self.inner.cm_dirty.load(Ordering::Acquire) {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let bytes = bytes.to_vec();
        run_blocking(&self.inner.runtime.clone(), async move {
            inner.cm_remote_attempt(&bytes).await;
        });
    }

    /// Async CM attempt for the timer tick's dirty retry.
    pub(crate) async fn cm_remote_write_async(&self, bytes: &[u8]) {
        self.inner.cm_remote_attempt(bytes).await;
    }

    /// Whole-blob LWW write (serialized per key, spawned): `_known_peers`
    /// and friends.
    pub(crate) fn put_lww(&self, key: &str, bytes: Vec<u8>) {
        if self.inner.remote.is_none() {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let key_owned = key.to_string();
        self.inner.enqueue(key.to_string(), async move {
            inner.put_lww_attempt(&key_owned, &bytes).await;
        });
    }

    /// Field-wise-merge write for the close-records singleton (U10):
    /// serialized on the key's chain like every other per-key write. `merge`
    /// receives the remote bytes on a 409, folds them into the local store
    /// (base = local, incoming = remote — the direction is normative, plan
    /// U10) and returns the merged bytes to rewrite. Best-effort.
    pub(crate) fn put_with_merge(&self, key: &str, bytes: Vec<u8>, merge: MergeFn) {
        if self.inner.remote.is_none() {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let key_owned = key.to_string();
        self.inner.enqueue(key.to_string(), async move {
            inner.put_merge_attempt(&key_owned, &bytes, &*merge).await;
        });
    }

    /// One best-effort GET recording the server version on success (U10 init
    /// seeding for `close_records` / `force_close_recovery`: the PWA fetches
    /// each blob on init to seed its version ref and pull remote state into
    /// an empty local store). `None` on absence, error, or no remote.
    pub(crate) async fn fetch_versioned(&self, key: &str) -> Option<(Vec<u8>, i64)> {
        let remote = self.inner.remote.as_ref()?;
        match remote.get(key).await {
            Ok(Some((bytes, version))) => {
                self.inner.record_version(key, version);
                Some((bytes, version))
            }
            Ok(None) => None,
            Err(e) => {
                log_error!(self.inner.logger, "Seed fetch for {key} failed: {e}");
                None
            }
        }
    }

    /// Best-effort versioned delete (U10: clearing `force_close_recovery`
    /// mirrors the PWA's `clearRecoveryState` VSS delete). Serialized on the
    /// key's chain; the cached version resets to 0 either way (the PWA resets
    /// its version ref only on success, but a stale non-zero version would
    /// 409-retry the next write anyway — 0 re-seeds via the next fetch).
    pub(crate) fn delete_best_effort(&self, key: &str) {
        if self.inner.remote.is_none() {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let key_owned = key.to_string();
        self.inner.enqueue(key.to_string(), async move {
            let Some(remote) = inner.remote.as_ref() else {
                return;
            };
            if inner.is_fenced() {
                return;
            }
            let version = inner.cached_version(&key_owned);
            match remote.delete(&key_owned, version).await {
                Ok(()) => inner.record_version(&key_owned, 0),
                Err(e) => {
                    log_error!(
                        inner.logger,
                        "Best-effort delete of {key_owned} failed: {e}"
                    );
                    inner.record_version(&key_owned, 0);
                }
            }
        });
    }

    /// Backfills the `_monitor_keys` manifest for pre-manifest stores:
    /// monitors exist but no manifest version was seeded. Fire-and-forget —
    /// the next `persist_new_channel` keeps it in sync regardless.
    pub(crate) fn backfill_manifest_if_needed(&self) {
        if self.inner.remote.is_none() {
            return;
        }
        let needs = !self.inner.monitor_keys.lock().unwrap().is_empty()
            && !self
                .inner
                .versions
                .lock()
                .unwrap()
                .contains_key(MONITOR_MANIFEST_KEY);
        if !needs {
            return;
        }
        let inner = Arc::clone(&self.inner);
        self.inner.runtime.spawn(async move {
            inner.write_manifest_once_best_effort().await;
        });
    }

    /// `archive_persisted_channel` (LDK 0.2: name-only): local archive move
    /// (mirroring LDK's blanket impl), manifest removal, and fire-and-forget
    /// remote delete. No retry — orphaned remote keys waste storage but never
    /// funds (the channel is already closed), exactly the PWA's posture.
    fn archive_monitor(&self, monitor_name: MonitorName) {
        let name = monitor_name.to_string();
        // Local archive: monitors/{name} → archived_monitors/{name}.
        match self.inner.local.read(
            CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
            CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
            &name,
        ) {
            Ok(bytes) => {
                let archived = self
                    .inner
                    .local
                    .write(
                        ARCHIVED_CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
                        ARCHIVED_CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
                        &name,
                        bytes,
                    )
                    .and_then(|()| {
                        self.inner.local.remove(
                            CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
                            CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
                            &name,
                            false,
                        )
                    });
                if let Err(e) = archived {
                    log_error!(
                        self.inner.logger,
                        "Local monitor archive for {name} failed: {e}"
                    );
                }
            }
            Err(e) => log_error!(
                self.inner.logger,
                "Local monitor read for archive of {name} failed: {e}"
            ),
        }

        let Some(vss_key) = self.inner.name_to_vss_key.lock().unwrap().remove(&name) else {
            // Effectively unreachable (every archived monitor entered via
            // persist_new_channel or register_loaded_monitor). Deriving the
            // key from the MonitorName string would risk the wrong txid byte
            // order, so log and leave orphaned remote storage (fund-safe).
            log_error!(
                self.inner.logger,
                "archive_persisted_channel: no VSS key recorded for monitor {name}"
            );
            return;
        };
        self.inner.monitor_keys.lock().unwrap().remove(&vss_key);
        self.inner.chains.lock().unwrap().remove(&vss_key);
        if self.inner.remote.is_none() || self.inner.is_fenced() {
            return;
        }
        let inner = Arc::clone(&self.inner);
        self.inner.runtime.spawn(async move {
            inner.write_manifest_once_best_effort().await;
            let Some(remote) = inner.remote.as_ref() else {
                return;
            };
            if inner.is_fenced() {
                return;
            }
            let version = inner.cached_version(&vss_key);
            match remote.delete(&vss_key, version).await {
                Ok(()) => {
                    inner.versions.lock().unwrap().remove(&vss_key);
                }
                Err(e) => log_error!(
                    inner.logger,
                    "Fire-and-forget VSS delete of archived monitor {vss_key} failed: {e}"
                ),
            }
        });
    }
}

impl<ChannelSigner: EcdsaChannelSigner> Persist<ChannelSigner> for VssBackedStore {
    fn persist_new_channel(
        &self,
        monitor_name: MonitorName,
        monitor: &ChannelMonitor<ChannelSigner>,
    ) -> ChannelMonitorUpdateStatus {
        self.queue_monitor_write(extract_monitor_write(monitor_name, monitor), true)
    }

    fn update_persisted_channel(
        &self,
        monitor_name: MonitorName,
        _monitor_update: Option<&ChannelMonitorUpdate>,
        monitor: &ChannelMonitor<ChannelSigner>,
    ) -> ChannelMonitorUpdateStatus {
        // The full monitor is always re-persisted (PWA blob-format parity —
        // no delta persistence, KTD-3), so the update object is unused.
        self.queue_monitor_write(extract_monitor_write(monitor_name, monitor), false)
    }

    fn archive_persisted_channel(&self, monitor_name: MonitorName) {
        self.archive_monitor(monitor_name);
    }
}

/// The `KVStoreSync` handed to the background processor: routes CHANNEL
/// MANAGER writes through the bounded VSS-then-local path (KTD-3 CM
/// semantics — the local write ALWAYS happens, even when VSS fails) and
/// everything else (graph, scorer, sweeper, liquidity) local-only. Monitors
/// never come through here — they use the async [`Persist`] above.
pub struct DualWriteKvStore {
    vss: Arc<VssBackedStore>,
    local: Arc<FilesystemStore>,
}

impl DualWriteKvStore {
    pub(crate) fn new(vss: Arc<VssBackedStore>, local: Arc<FilesystemStore>) -> Self {
        Self { vss, local }
    }

    /// Whether a CM write failed remotely and awaits the tick retry.
    pub(crate) fn cm_dirty(&self) -> bool {
        self.vss.cm_dirty()
    }

    /// The timer tick's dirty retry: remote attempt then local refresh, both
    /// bounded, never blocking the event loop beyond one attempt.
    pub(crate) async fn retry_cm(&self, bytes: Vec<u8>) {
        self.vss.cm_remote_write_async(&bytes).await;
        if let Err(e) = self.local.write(
            CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
            CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
            CHANNEL_MANAGER_PERSISTENCE_KEY,
            bytes,
        ) {
            log_error!(
                Logger,
                "Local channel_manager write on tick retry failed: {e}"
            );
        }
    }
}

fn is_channel_manager_key(primary: &str, secondary: &str, key: &str) -> bool {
    primary == CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE
        && secondary == CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE
        && key == CHANNEL_MANAGER_PERSISTENCE_KEY
}

impl KVStoreSync for DualWriteKvStore {
    fn read(
        &self,
        primary_namespace: &str,
        secondary_namespace: &str,
        key: &str,
    ) -> Result<Vec<u8>, lightning::io::Error> {
        self.local.read(primary_namespace, secondary_namespace, key)
    }

    fn write(
        &self,
        primary_namespace: &str,
        secondary_namespace: &str,
        key: &str,
        buf: Vec<u8>,
    ) -> Result<(), lightning::io::Error> {
        if is_channel_manager_key(primary_namespace, secondary_namespace, key) {
            // VSS-first bounded attempt; failure marks dirty and NEVER blocks
            // the local write below (source of truth on restart is LOCAL).
            self.vss.cm_remote_write_bounded(&buf);
        }
        self.local
            .write(primary_namespace, secondary_namespace, key, buf)
    }

    fn remove(
        &self,
        primary_namespace: &str,
        secondary_namespace: &str,
        key: &str,
        lazy: bool,
    ) -> Result<(), lightning::io::Error> {
        self.local
            .remove(primary_namespace, secondary_namespace, key, lazy)
    }

    fn list(
        &self,
        primary_namespace: &str,
        secondary_namespace: &str,
    ) -> Result<Vec<String>, lightning::io::Error> {
        self.local.list(primary_namespace, secondary_namespace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vss::test_support::MockTransport;

    const MON_NS: &str = CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE;

    #[derive(Default)]
    struct RecordingCompletion(Mutex<Vec<(ChannelId, u64)>>);

    impl CompletionSink for RecordingCompletion {
        fn monitor_updated(&self, channel_id: ChannelId, update_id: u64) {
            self.0.lock().unwrap().push((channel_id, update_id));
        }
    }

    #[derive(Default)]
    struct CapturingSink(Mutex<Vec<CoreEvent>>);

    impl EventSink for CapturingSink {
        fn emit(&self, event: CoreEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    impl CapturingSink {
        fn degraded_count(&self) -> usize {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|e| matches!(e, CoreEvent::BackupDegraded { .. }))
                .count()
        }

        fn fenced_count(&self) -> usize {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|e| matches!(e, CoreEvent::Fenced { .. }))
                .count()
        }
    }

    fn fast_tuning() -> RetryTuning {
        RetryTuning {
            initial_backoff: Duration::from_millis(2),
            max_backoff: Duration::from_millis(10),
            degraded_after: Duration::from_millis(6),
            cm_attempt_timeout: Duration::from_millis(200),
        }
    }

    struct Harness {
        _dir: tempfile::TempDir,
        rt: tokio::runtime::Runtime,
        transport: Arc<MockTransport>,
        store: Arc<VssBackedStore>,
        completion: Arc<RecordingCompletion>,
        sink: Arc<CapturingSink>,
        local: Arc<FilesystemStore>,
        storage_dir: PathBuf,
    }

    fn harness_with_versions(versions: HashMap<String, i64>) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let storage_dir = dir.path().to_path_buf();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let transport = Arc::new(MockTransport::new());
        let local = Arc::new(FilesystemStore::new(storage_dir.join("store")));
        let sink = Arc::new(CapturingSink::default());
        let store = Arc::new(VssBackedStore::new(
            Some(Arc::clone(&transport) as Arc<dyn VssTransport>),
            Arc::clone(&local),
            rt.handle().clone(),
            &storage_dir,
            Arc::clone(&sink) as Arc<dyn EventSink>,
            Arc::new(Logger),
            fast_tuning(),
            versions,
            BTreeSet::new(),
            false,
        ));
        let completion = Arc::new(RecordingCompletion::default());
        store.set_completion_sink(Arc::downgrade(&completion) as Weak<dyn CompletionSink>);
        Harness {
            _dir: dir,
            rt,
            transport,
            store,
            completion,
            sink,
            local,
            storage_dir,
        }
    }

    fn harness() -> Harness {
        harness_with_versions(HashMap::new())
    }

    fn monitor_write(byte: u8, update_id: u64, bytes: &[u8]) -> MonitorWrite {
        let txid_hex = format!("{:02x}", byte).repeat(32);
        MonitorWrite {
            monitor_name: format!("{txid_hex}_0"),
            vss_key: format!("{txid_hex}:0"),
            local_key: format!("{txid_hex}_0"),
            channel_id: ChannelId([byte; 32]),
            update_id,
            bytes: bytes.to_vec(),
        }
    }

    fn wait_until(rt: &tokio::runtime::Runtime, mut condition: impl FnMut() -> bool) {
        rt.block_on(async {
            for _ in 0..2_000 {
                if condition() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            panic!("condition never became true within the wait budget");
        });
    }

    fn settle(rt: &tokio::runtime::Runtime, ms: u64) {
        // The Sleep must be constructed inside the runtime context.
        rt.block_on(async { tokio::time::sleep(Duration::from_millis(ms)).await });
    }

    // ---------- scenario 1: monitor persist gates channel ops ----------

    /// KTD-3: `channel_monitor_updated` is NEVER signalled while the VSS put
    /// is failing (backoff runs indefinitely, `BackupDegraded` after the
    /// threshold), and the chain resumes to completion once VSS recovers.
    #[test]
    fn monitor_completion_is_gated_on_the_vss_put_and_resumes_after_recovery() {
        let h = harness();
        h.transport.fail_puts.store(true, Ordering::SeqCst);

        let write = monitor_write(0xaa, 7, b"monitor-bytes");
        let local_key = write.local_key.clone();
        let status = h.store.queue_monitor_write(write, false);
        assert_eq!(status, ChannelMonitorUpdateStatus::InProgress);

        // While VSS fails: retries happen, but no completion, no local write.
        wait_until(&h.rt, || h.transport.put_attempt_count() >= 3);
        assert!(h.completion.0.lock().unwrap().is_empty());
        assert!(
            h.local.read(MON_NS, "", &local_key).is_err(),
            "the local write must not run ahead of the remote one"
        );
        wait_until(&h.rt, || h.sink.degraded_count() >= 1);

        // Recovery: the same chain converges to remote + local + completion.
        h.transport.fail_puts.store(false, Ordering::SeqCst);
        wait_until(&h.rt, || !h.completion.0.lock().unwrap().is_empty());
        assert_eq!(
            h.completion.0.lock().unwrap().clone(),
            vec![(ChannelId([0xaa; 32]), 7)]
        );
        assert_eq!(
            h.transport
                .value(&format!("{}:0", "aa".repeat(32)))
                .unwrap()
                .0,
            b"monitor-bytes".to_vec()
        );
        assert_eq!(
            h.local.read(MON_NS, "", &local_key).unwrap(),
            b"monitor-bytes".to_vec()
        );
        assert_eq!(h.sink.degraded_count(), 1, "one degraded event per outage");
    }

    // ---------- scenario 2: manifest gates new channels ----------

    /// KTD-3 manifest gating: with the monitor put succeeding but the
    /// manifest put stuck, `persist_new_channel` completion never fires (so
    /// LDK never broadcasts funding); it fires once the manifest lands —
    /// strictly AFTER the manifest write.
    #[test]
    fn manifest_put_gates_new_channel_completion() {
        let h = harness();
        let mon_key = format!("{}:0", "bb".repeat(32));
        h.transport.fail_puts_for(MONITOR_MANIFEST_KEY, true);

        let write = monitor_write(0xbb, 0, b"new-channel");
        let local_key = write.local_key.clone();
        assert_eq!(
            h.store.queue_monitor_write(write, true),
            ChannelMonitorUpdateStatus::InProgress
        );

        // Monitor blob is durable remotely, but the gating manifest is not:
        // no completion, no local write (the crash seam between the monitor
        // put and the manifest put never signals LDK).
        wait_until(&h.rt, || h.transport.value(&mon_key).is_some());
        wait_until(&h.rt, || {
            h.transport.put_attempts_for(MONITOR_MANIFEST_KEY) >= 3
        });
        assert!(h.completion.0.lock().unwrap().is_empty());
        assert!(h.local.read(MON_NS, "", &local_key).is_err());

        h.transport.fail_puts_for(MONITOR_MANIFEST_KEY, false);
        wait_until(&h.rt, || !h.completion.0.lock().unwrap().is_empty());
        let (manifest_bytes, _) = h.transport.value(MONITOR_MANIFEST_KEY).unwrap();
        assert_eq!(
            parse_monitor_manifest(&manifest_bytes).unwrap(),
            vec![mon_key]
        );
    }

    /// Updates to EXISTING monitors need no manifest gate: a stuck manifest
    /// key must not block them.
    #[test]
    fn monitor_updates_complete_without_touching_the_manifest() {
        let h = harness();
        h.transport.fail_puts_for(MONITOR_MANIFEST_KEY, true);

        let write = monitor_write(0xcc, 4, b"update");
        assert_eq!(
            h.store.queue_monitor_write(write, false),
            ChannelMonitorUpdateStatus::InProgress
        );
        wait_until(&h.rt, || !h.completion.0.lock().unwrap().is_empty());
        assert_eq!(
            h.transport.put_attempts_for(MONITOR_MANIFEST_KEY),
            0,
            "updates never write the manifest"
        );
    }

    // ---------- scenario 3: fence ----------

    /// KTD-3 fence: a divergent-content 409 on a monitor key persists the
    /// durable fenced flag, emits `Fenced`, never completes the update, and
    /// issues ZERO puts after detection (poisoned store).
    #[test]
    fn divergent_conflict_fences_durably_and_poisons_all_further_puts() {
        let h = harness();
        let key = format!("{}:0", "dd".repeat(32));
        // Another client wrote different bytes at version 2; our cache is
        // stale at 1 → guaranteed conflict, divergent content.
        h.transport.seed(&key, b"their-monitor", 2);
        let mut versions = HashMap::new();
        versions.insert(key.clone(), 1i64);
        *h.store.inner.versions.lock().unwrap() = versions;

        let write = monitor_write(0xdd, 9, b"our-monitor");
        h.store.queue_monitor_write(write, false);

        wait_until(&h.rt, || h.store.is_fenced());
        // The Fenced event lands just after the watch flips.
        wait_until(&h.rt, || h.sink.fenced_count() >= 1);
        assert_eq!(h.sink.fenced_count(), 1);
        assert!(
            h.storage_dir.join(FENCED_FLAG_FILE_NAME).exists(),
            "the fenced flag must be durable"
        );
        assert!(h.completion.0.lock().unwrap().is_empty());
        assert_eq!(
            h.transport.value(&key).unwrap().0,
            b"their-monitor".to_vec(),
            "the divergent remote content is never overwritten"
        );

        // Poisoned: further persists issue zero transport puts.
        let puts_before = h.transport.put_attempt_count();
        h.store
            .queue_monitor_write(monitor_write(0xee, 1, b"more"), false);
        settle(&h.rt, 50);
        assert_eq!(
            h.transport.put_attempt_count(),
            puts_before,
            "zero puts after fence detection"
        );

        // The flag survives restart: a rebuilt store starts fenced.
        let rebuilt = VssBackedStore::new(
            Some(Arc::clone(&h.transport) as Arc<dyn VssTransport>),
            Arc::clone(&h.local),
            h.rt.handle().clone(),
            &h.storage_dir,
            Arc::new(CapturingSink::default()) as Arc<dyn EventSink>,
            Arc::new(Logger),
            fast_tuning(),
            HashMap::new(),
            BTreeSet::new(),
            false,
        );
        assert!(rebuilt.is_fenced(), "the fenced flag survives restart");
    }

    /// The identical-content half: a 409 whose remote bytes equal ours is a
    /// benign replay — short-circuit success at the server version, no fence.
    #[test]
    fn identical_content_conflict_short_circuits_to_success() {
        let h = harness();
        let key = format!("{}:0", "ee".repeat(32));
        h.transport.seed(&key, b"same-bytes", 5);
        // Stale cache (0) → conflict on the first put.

        let write = monitor_write(0xee, 3, b"same-bytes");
        h.store.queue_monitor_write(write, false);
        wait_until(&h.rt, || !h.completion.0.lock().unwrap().is_empty());

        assert!(!h.store.is_fenced());
        assert_eq!(h.sink.fenced_count(), 0);
        assert_eq!(
            h.store.cached_version(&key),
            5,
            "the cache adopts the server version"
        );
        assert_eq!(h.transport.value(&key).unwrap().1, 5, "no rewrite happened");
    }

    // ---------- scenario 6: crash seam between remote put and local write ----------

    /// Remote leads local at a crash seam (put durable remotely, process died
    /// before the local write/completion): after restart with a seeded
    /// version cache, the re-persisted monitor converges with no fence trip
    /// and no data loss.
    #[test]
    fn crash_seam_between_remote_and_local_write_converges_on_restart() {
        let key = format!("{}:0", "1a".repeat(32));
        let mut seeded = HashMap::new();
        seeded.insert(key.clone(), 1i64);
        let h = harness_with_versions(seeded);
        // The pre-crash session's put landed at version 1.
        h.transport.seed(&key, b"pre-crash-bytes", 1);

        // Restart: LDK replays the un-completed update; the write chain puts
        // at the seeded version 1 → clean success at version 2.
        let write = monitor_write(0x1a, 6, b"pre-crash-bytes");
        let local_key = write.local_key.clone();
        h.store.queue_monitor_write(write, false);
        wait_until(&h.rt, || !h.completion.0.lock().unwrap().is_empty());

        assert!(!h.store.is_fenced(), "no fence trip at the crash seam");
        assert_eq!(
            h.transport.value(&key).unwrap(),
            (b"pre-crash-bytes".to_vec(), 2)
        );
        assert_eq!(
            h.local.read(MON_NS, "", &local_key).unwrap(),
            b"pre-crash-bytes".to_vec(),
            "local converges with remote"
        );
    }

    // ---------- ordering: per-channel serialized chains ----------

    /// Two updates to one channel apply strictly in submission order, so the
    /// remote can never regress to older monitor bytes.
    #[test]
    fn writes_to_one_channel_apply_in_submission_order() {
        let h = harness();
        let key = format!("{}:0", "2b".repeat(32));
        h.store
            .queue_monitor_write(monitor_write(0x2b, 1, b"update-1"), false);
        h.store
            .queue_monitor_write(monitor_write(0x2b, 2, b"update-2"), false);
        wait_until(&h.rt, || h.completion.0.lock().unwrap().len() == 2);
        assert_eq!(
            h.completion.0.lock().unwrap().clone(),
            vec![(ChannelId([0x2b; 32]), 1), (ChannelId([0x2b; 32]), 2)]
        );
        assert_eq!(
            h.transport.value(&key).unwrap(),
            (b"update-2".to_vec(), 2),
            "the final remote state is the newest update at version 2"
        );
    }

    // ---------- manifest merge-on-conflict ----------

    /// A manifest 409 merges the server's keys into ours (never dropping a
    /// monitor tracked by another device) and retries at the server version.
    #[test]
    fn manifest_conflict_merges_server_keys_and_retries() {
        let h = harness();
        let their_key = format!("{}:0", "3c".repeat(32));
        let our_key = format!("{}:1", "4d".repeat(32));
        h.transport.seed(
            MONITOR_MANIFEST_KEY,
            &serde_json::to_vec(&vec![their_key.clone()]).unwrap(),
            3,
        );
        h.store
            .inner
            .monitor_keys
            .lock()
            .unwrap()
            .insert(our_key.clone());
        // Stale manifest version (0) → conflict on the first put.

        h.rt.block_on(async {
            let _guard = h.store.inner.manifest_lock.lock().await;
            h.store
                .inner
                .write_manifest_with_retry_locked()
                .await
                .ok()
                .unwrap();
        });

        let (bytes, version) = h.transport.value(MONITOR_MANIFEST_KEY).unwrap();
        assert_eq!(version, 4, "written at the refetched server version");
        let mut merged = parse_monitor_manifest(&bytes).unwrap();
        merged.sort();
        let mut expected = vec![their_key, our_key];
        expected.sort();
        assert_eq!(merged, expected, "server keys merged, ours added");
    }

    // ---------- _known_peers LWW ----------

    /// LWW: a conflict adopts the server version and retries ONCE with OUR
    /// bytes (last writer wins, PWA parity — no fence for peers).
    #[test]
    fn put_lww_conflict_refetches_the_version_and_overwrites_once() {
        let h = harness();
        h.transport.seed(KNOWN_PEERS_VSS_KEY, b"their-peers", 4);

        h.store.put_lww(KNOWN_PEERS_VSS_KEY, b"our-peers".to_vec());
        wait_until(&h.rt, || {
            h.transport.value(KNOWN_PEERS_VSS_KEY).map(|(_, v)| v) == Some(5)
        });
        assert_eq!(
            h.transport.value(KNOWN_PEERS_VSS_KEY).unwrap().0,
            b"our-peers".to_vec(),
            "our bytes win (LWW)"
        );
        assert!(!h.store.is_fenced(), "peers conflicts never fence");
    }

    // ---------- scenario 8: CM dirty flag ----------

    /// KTD-3 CM semantics: a failed bounded attempt sets the dirty flag and
    /// the LOCAL write still happens (source of truth on restart is LOCAL);
    /// the tick retry converges and clears the flag. Graph/scorer keys never
    /// touch the transport.
    #[test]
    fn cm_write_failure_sets_dirty_local_still_written_and_tick_retry_converges() {
        let h = harness();
        let dual = DualWriteKvStore::new(Arc::clone(&h.store), Arc::clone(&h.local));
        h.transport.fail_puts.store(true, Ordering::SeqCst);

        dual.write("", "", CHANNEL_MANAGER_PERSISTENCE_KEY, b"cm-v1".to_vec())
            .unwrap();
        assert!(dual.cm_dirty(), "the failed attempt marks the CM dirty");
        assert_eq!(
            h.local
                .read("", "", CHANNEL_MANAGER_PERSISTENCE_KEY)
                .unwrap(),
            b"cm-v1".to_vec(),
            "the local write ALWAYS happens even when VSS fails"
        );
        assert!(h.transport.value(CHANNEL_MANAGER_VSS_KEY).is_none());

        // While dirty, further CM writes skip the remote attempt entirely
        // (the background processor is stalled at most once per outage).
        let attempts = h.transport.put_attempt_count();
        dual.write("", "", CHANNEL_MANAGER_PERSISTENCE_KEY, b"cm-v2".to_vec())
            .unwrap();
        assert_eq!(h.transport.put_attempt_count(), attempts);
        assert_eq!(
            h.local
                .read("", "", CHANNEL_MANAGER_PERSISTENCE_KEY)
                .unwrap(),
            b"cm-v2".to_vec()
        );

        // Graph writes are local-only: no transport traffic.
        dual.write("", "", "network_graph", b"graph".to_vec())
            .unwrap();
        assert_eq!(h.transport.put_attempt_count(), attempts);

        // The tick retry converges once VSS recovers.
        h.transport.fail_puts.store(false, Ordering::SeqCst);
        h.rt.block_on(dual.retry_cm(b"cm-v3".to_vec()));
        assert!(!dual.cm_dirty());
        assert_eq!(
            h.transport.value(CHANNEL_MANAGER_VSS_KEY).unwrap().0,
            b"cm-v3".to_vec()
        );
        assert_eq!(
            h.local
                .read("", "", CHANNEL_MANAGER_PERSISTENCE_KEY)
                .unwrap(),
            b"cm-v3".to_vec()
        );
    }

    /// The CM is fund-critical: a divergent-content 409 on `channel_manager`
    /// fences (and the local write still happens).
    #[test]
    fn cm_divergent_conflict_fences() {
        let h = harness();
        let dual = DualWriteKvStore::new(Arc::clone(&h.store), Arc::clone(&h.local));
        h.transport.seed(CHANNEL_MANAGER_VSS_KEY, b"their-cm", 7);

        dual.write("", "", CHANNEL_MANAGER_PERSISTENCE_KEY, b"our-cm".to_vec())
            .unwrap();
        assert!(h.store.is_fenced());
        assert_eq!(h.sink.fenced_count(), 1);
        assert_eq!(
            h.transport.value(CHANNEL_MANAGER_VSS_KEY).unwrap().0,
            b"their-cm".to_vec(),
            "the other client's CM is never overwritten"
        );
        assert_eq!(
            h.local
                .read("", "", CHANNEL_MANAGER_PERSISTENCE_KEY)
                .unwrap(),
            b"our-cm".to_vec(),
            "local persistence continues after the fence"
        );
    }

    // ---------- local-only (vss_disabled) ----------

    /// With no remote, the store reproduces the spike's synchronous
    /// durable-before-Completed behavior exactly.
    #[test]
    fn local_only_store_completes_synchronously() {
        let dir = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let local = Arc::new(FilesystemStore::new(dir.path().join("store")));
        let store = VssBackedStore::new(
            None,
            Arc::clone(&local),
            rt.handle().clone(),
            dir.path(),
            Arc::new(CapturingSink::default()) as Arc<dyn EventSink>,
            Arc::new(Logger),
            RetryTuning::default(),
            HashMap::new(),
            BTreeSet::new(),
            false,
        );
        let write = monitor_write(0x5e, 2, b"local-only");
        let local_key = write.local_key.clone();
        assert_eq!(
            store.queue_monitor_write(write, true),
            ChannelMonitorUpdateStatus::Completed,
            "local-only persists synchronously, like the spike"
        );
        assert_eq!(
            local.read(MON_NS, "", &local_key).unwrap(),
            b"local-only".to_vec()
        );
    }

    // ---------- manifest parsing ----------

    #[test]
    fn parse_monitor_manifest_validates_shape_regex_dedup_and_cap() {
        let key_a = format!("{}:0", "aa".repeat(32));
        let key_b = format!("{}:12", "bb".repeat(32));

        // Valid, with a duplicate removed, order preserved.
        let json = serde_json::to_vec(&vec![&key_a, &key_b, &key_a]).unwrap();
        assert_eq!(
            parse_monitor_manifest(&json).unwrap(),
            vec![key_a.clone(), key_b]
        );

        // Rejections: not JSON, not an array, empty, invalid entries, cap.
        assert!(parse_monitor_manifest(b"not json").is_err());
        assert!(parse_monitor_manifest(b"{}").is_err());
        assert!(parse_monitor_manifest(b"[]").is_err());
        assert!(parse_monitor_manifest(b"[42]").is_err());
        assert!(parse_monitor_manifest(&serde_json::to_vec(&vec!["nope"]).unwrap()).is_err());
        // Uppercase hex fails the PWA regex.
        let upper = format!("{}:0", "AA".repeat(32));
        assert!(parse_monitor_manifest(&serde_json::to_vec(&vec![upper]).unwrap()).is_err());
        // Missing index fails.
        let no_index = "cc".repeat(32);
        assert!(parse_monitor_manifest(&serde_json::to_vec(&vec![no_index]).unwrap()).is_err());
        let over_cap: Vec<String> = (0..=MAX_MANIFEST_ENTRIES)
            .map(|i| format!("{:064x}:{i}", i))
            .collect();
        assert!(parse_monitor_manifest(&serde_json::to_vec(&over_cap).unwrap()).is_err());
    }

    #[test]
    fn monitor_vss_key_uses_raw_txid_byte_order() {
        use std::str::FromStr as _;
        // Display order is the REVERSE of the raw byte order; the PWA key
        // (and thus cross-client restore) uses the raw order.
        let txid = bitcoin::Txid::from_str(
            "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f",
        )
        .unwrap();
        let outpoint = lightning::chain::transaction::OutPoint { txid, index: 1 };
        assert_eq!(
            monitor_vss_key(&outpoint),
            "6fe28c0ab6f1b372c1a6a246ae63f74f931e8365e15a089c68d6190000000000:1"
        );
        assert!(is_valid_monitor_key(&monitor_vss_key(&outpoint)));
    }
}
