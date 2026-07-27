//! VSS startup phases (U3; KTD-3; R4): silent recovery, migration, and
//! mandatory version-cache seeding — resolved BEFORE any node component that
//! could write runs.
//!
//! The branch matrix (KTD-3, adversarially reviewed — every rule is
//! load-bearing):
//!
//! 1. **Local LDK state empty** (no monitors, no channel manager) — or the
//!    U4 `restore_in_progress` marker present, which voids local state:
//!    probe `listKeyVersions`.
//!    - Empty → fresh wallet. Version-0 first writes are permitted ONLY
//!      because the probe returned empty this session (`probe_empty`).
//!    - Non-empty → **silent recovery**: manifest → monitors (validated by
//!      deserialization BEFORE any local write) → channel manager → known
//!      peers → local writes. ANY failure rolls back the partial local
//!      writes and is FATAL ([`BuildError::VssRecoveryFailed`]) — never a
//!      fall-through to fresh-wallet writes over an existing backup.
//! 2. **Local data + empty namespace** → migration: ONE transactional
//!    `putObjects` batch (CM + all monitors + manifest + known peers at
//!    version 0), versions seeded to 1. Failure is non-fatal: the session
//!    continues LOCAL-ONLY with a `BackupDegraded` event (the next start
//!    retries the migration).
//! 3. **Local data + non-empty namespace** → mandatory version seeding.
//!    Versions are read from the (already fetched) `listKeyVersions` result
//!    by matching the obfuscated forms of every known key — equivalent to
//!    the plan's per-key `getObject` sweep with one request and no blob
//!    downloads, and it can never guess: a probe failure is a typed startup
//!    error ([`BuildError::VssVersionSeedFailed`]).

use std::collections::{BTreeSet, HashMap};
use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use bitcoin::BlockHash;
use lightning::chain::channelmonitor::ChannelMonitor;
use lightning::sign::{InMemorySigner, KeysManager};
use lightning::util::logger::Logger as _;
use lightning::util::persist::{
    read_channel_monitors, KVStoreSync, CHANNEL_MANAGER_PERSISTENCE_KEY,
    CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE, CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
    CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE, CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
};
use lightning::util::ser::ReadableArgs;
use lightning::{log_error, log_info};
use lightning_persister::fs_store::FilesystemStore;

use super::known_peers::{
    parse_known_peers, read_local_known_peers, serialize_known_peers, write_local_known_peers,
    KNOWN_PEERS_LOCAL_KEY, KNOWN_PEERS_PRIMARY_NAMESPACE, KNOWN_PEERS_SECONDARY_NAMESPACE,
};
use super::store::{
    monitor_vss_key, parse_monitor_manifest, VssTransport, CHANNEL_MANAGER_VSS_KEY,
    KNOWN_PEERS_VSS_KEY, MONITOR_MANIFEST_KEY,
};
use crate::builder::BuildError;
use crate::keys::RESTORE_IN_PROGRESS_FILE_NAME;
use crate::node::{CoreEvent, EventSink};
use crate::signer::WalletSignerProvider;
use crate::types::Logger;

/// What the startup phases resolved: the seeds for
/// [`super::store::VssBackedStore`].
pub(crate) struct VssStartupState {
    /// `None` = local-only for this session (vss_disabled, or the migration
    /// batch failed).
    pub remote: Option<Arc<dyn VssTransport>>,
    /// Version cache seeds: plaintext key → server version.
    pub versions: HashMap<String, i64>,
    /// The `_monitor_keys` set (from the recovered manifest or local
    /// monitors).
    pub monitor_keys: BTreeSet<String>,
    /// Whether `listKeyVersions` returned empty this session (KTD-3's
    /// version-0 write precondition).
    pub probe_empty: bool,
    /// Whether silent recovery wrote local state (the caller re-reads
    /// monitors afterwards).
    pub recovered: bool,
}

impl VssStartupState {
    /// The vss_disabled state: everything local-only, spike behavior.
    pub(crate) fn local_only() -> Self {
        Self {
            remote: None,
            versions: HashMap::new(),
            monitor_keys: BTreeSet::new(),
            probe_empty: false,
            recovered: false,
        }
    }
}

fn local_channel_manager(kv_store: &FilesystemStore) -> Result<Option<Vec<u8>>, BuildError> {
    match kv_store.read(
        CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
        CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
        CHANNEL_MANAGER_PERSISTENCE_KEY,
    ) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == lightning::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(BuildError::ReadFailed),
    }
}

/// Removes local LDK state (monitors + channel manager) — used when the
/// restore-in-progress marker declares it void before silent recovery.
fn wipe_local_ldk_state(kv_store: &FilesystemStore) -> Result<(), BuildError> {
    let monitor_keys = kv_store
        .list(
            CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
            CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
        )
        .map_err(|_| BuildError::ReadFailed)?;
    for key in monitor_keys {
        kv_store
            .remove(
                CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
                CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
                &key,
                false,
            )
            .map_err(|_| BuildError::WriteFailed)?;
    }
    kv_store
        .remove(
            CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
            CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
            CHANNEL_MANAGER_PERSISTENCE_KEY,
            false,
        )
        .map_err(|_| BuildError::WriteFailed)?;
    Ok(())
}

/// Resolves the KTD-3 startup branch for a VSS-enabled wallet. Runs on the
/// builder thread, blocking on `runtime` for the network calls.
#[allow(clippy::too_many_arguments)]
pub(crate) fn establish_vss_state(
    transport: Arc<dyn VssTransport>,
    kv_store: &Arc<FilesystemStore>,
    keys_manager: &Arc<KeysManager>,
    signer_provider: &Arc<WalletSignerProvider>,
    storage_dir: &Path,
    event_sink: &Arc<dyn EventSink>,
    logger: &Arc<Logger>,
    runtime: &tokio::runtime::Runtime,
) -> Result<VssStartupState, BuildError> {
    let marker_path = storage_dir.join(RESTORE_IN_PROGRESS_FILE_NAME);
    let restore_marker = marker_path.exists();
    let cm_bytes = local_channel_manager(kv_store)?;
    let local_monitor_keys = kv_store
        .list(
            CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
            CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
        )
        .map_err(|_| BuildError::ReadFailed)?;
    let local_empty = cm_bytes.is_none() && local_monitor_keys.is_empty();
    // The restore marker voids local LDK state: resume silent recovery (U4
    // writes the marker; this is the branch condition, implemented now).
    let treat_local_as_void = restore_marker || local_empty;

    let listing = runtime
        .block_on(transport.list_key_versions())
        .map_err(|e| {
            log_error!(logger, "VSS listKeyVersions probe failed at startup: {e}");
            if treat_local_as_void {
                // Cannot prove the namespace is empty, so version-0 writes
                // (and a fresh start over a possible backup) are forbidden.
                BuildError::VssRecoveryFailed
            } else {
                // Local state exists: mandatory seeding failed — never
                // guess versions (a lost cache would false-trip the fence).
                BuildError::VssVersionSeedFailed
            }
        })?;

    if treat_local_as_void {
        if restore_marker {
            log_info!(
                logger,
                "Restore-in-progress marker present: voiding local LDK state and resuming \
                 silent recovery"
            );
            wipe_local_ldk_state(kv_store)?;
        }
        if listing.is_empty() {
            // Fresh wallet: version-0 writes are allowed ONLY because the
            // probe returned empty this session.
            if restore_marker {
                let _ = std::fs::remove_file(&marker_path);
                log_info!(
                    logger,
                    "Namespace is empty; nothing to recover — clearing the restore marker and \
                     proceeding as a fresh wallet"
                );
            }
            return Ok(VssStartupState {
                remote: Some(transport),
                versions: HashMap::new(),
                monitor_keys: BTreeSet::new(),
                probe_empty: true,
                recovered: false,
            });
        }
        let state = silent_recovery(
            transport,
            kv_store,
            keys_manager,
            signer_provider,
            logger,
            runtime,
        )?;
        if restore_marker {
            let _ = std::fs::remove_file(&marker_path);
        }
        return Ok(state);
    }

    // Local data exists: deserialize the monitors once for their funding
    // outpoints (the PWA-shaped VSS keys) and their raw local bytes.
    let monitors: Vec<(BlockHash, ChannelMonitor<InMemorySigner>)> = read_channel_monitors(
        Arc::clone(kv_store),
        Arc::clone(keys_manager),
        Arc::clone(signer_provider),
    )
    .map_err(|e| {
        log_error!(
            logger,
            "Failed to read channel monitors for VSS startup: {e}"
        );
        BuildError::InvalidMonitorData
    })?;
    let mut monitor_entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(monitors.len());
    for (_, monitor) in &monitors {
        let local_key = monitor.persistence_key().to_string();
        let raw = kv_store
            .read(
                CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
                CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
                &local_key,
            )
            .map_err(|_| BuildError::ReadFailed)?;
        monitor_entries.push((monitor_vss_key(&monitor.get_funding_txo()), raw));
    }
    let monitor_keys: BTreeSet<String> =
        monitor_entries.iter().map(|(key, _)| key.clone()).collect();

    if listing.is_empty() {
        // Branch (2): migration — one transactional batch at version 0.
        let mut items: Vec<(String, Vec<u8>, i64)> = Vec::new();
        if let Some(cm) = &cm_bytes {
            items.push((CHANNEL_MANAGER_VSS_KEY.to_string(), cm.clone(), 0));
        }
        for (vss_key, raw) in &monitor_entries {
            items.push((vss_key.clone(), raw.clone(), 0));
        }
        if !monitor_keys.is_empty() {
            let manifest: Vec<&String> = monitor_keys.iter().collect();
            items.push((
                MONITOR_MANIFEST_KEY.to_string(),
                serde_json::to_vec(&manifest).expect("strings serialize"),
                0,
            ));
        }
        let peers = read_local_known_peers(kv_store);
        if !peers.is_empty() {
            items.push((
                KNOWN_PEERS_VSS_KEY.to_string(),
                serialize_known_peers(&peers),
                0,
            ));
        }
        let migrated_keys: Vec<String> = items.iter().map(|(key, _, _)| key.clone()).collect();
        match runtime.block_on(transport.put_many(items)) {
            Ok(()) => {
                log_info!(
                    logger,
                    "Migrated {} local item(s) to VSS in one transactional batch",
                    migrated_keys.len()
                );
                let versions = migrated_keys.into_iter().map(|key| (key, 1i64)).collect();
                Ok(VssStartupState {
                    remote: Some(transport),
                    versions,
                    monitor_keys,
                    // The probe returned empty this session, which is what
                    // authorized the version-0 batch.
                    probe_empty: true,
                    recovered: false,
                })
            }
            Err(e) => {
                // Non-fatal (KTD-3): local-only for this session; the next
                // start retries the migration against the still-empty
                // namespace.
                log_error!(logger, "VSS migration failed (non-fatal, local-only): {e}");
                event_sink.emit(CoreEvent::BackupDegraded {
                    detail: format!(
                        "migrating local wallet state to the cloud backup failed: {e}; \
                         continuing local-only, will retry on next start"
                    ),
                });
                Ok(VssStartupState {
                    remote: None,
                    versions: HashMap::new(),
                    monitor_keys,
                    probe_empty: true,
                    recovered: false,
                })
            }
        }
    } else {
        // Branch (3): mandatory version seeding from the listing.
        let by_obfuscated: HashMap<String, i64> = listing.into_iter().collect();
        let mut versions = HashMap::new();
        let fixed = [
            CHANNEL_MANAGER_VSS_KEY,
            MONITOR_MANIFEST_KEY,
            KNOWN_PEERS_VSS_KEY,
        ];
        for key in fixed
            .iter()
            .map(|k| k.to_string())
            .chain(monitor_keys.iter().cloned())
        {
            if let Some(version) = by_obfuscated.get(&transport.obfuscate(&key)) {
                versions.insert(key, *version);
            }
        }
        log_info!(
            logger,
            "Seeded {} VSS version(s) from the server listing",
            versions.len()
        );
        Ok(VssStartupState {
            remote: Some(transport),
            versions,
            monitor_keys,
            probe_empty: false,
            recovered: false,
        })
    }
}

/// Silent recovery (branch 1, non-empty namespace): download manifest →
/// monitors (validate by deserialization BEFORE any local write) → channel
/// manager → known peers → local writes. ANY failure rolls back the partial
/// local writes and returns [`BuildError::VssRecoveryFailed`] — never a
/// fall-through to fresh-wallet writes.
fn silent_recovery(
    transport: Arc<dyn VssTransport>,
    kv_store: &Arc<FilesystemStore>,
    keys_manager: &Arc<KeysManager>,
    signer_provider: &Arc<WalletSignerProvider>,
    logger: &Arc<Logger>,
    runtime: &tokio::runtime::Runtime,
) -> Result<VssStartupState, BuildError> {
    let mut written_monitor_keys: Vec<String> = Vec::new();
    let mut wrote_channel_manager = false;
    let mut wrote_peers = false;

    let mut recover = || -> Result<VssStartupState, String> {
        let mut versions = HashMap::new();
        let mut monitor_keys = BTreeSet::new();

        let manifest = runtime
            .block_on(transport.get(MONITOR_MANIFEST_KEY))
            .map_err(|e| format!("manifest download failed: {e}"))?;
        let cm = runtime
            .block_on(transport.get(CHANNEL_MANAGER_VSS_KEY))
            .map_err(|e| format!("channel_manager download failed: {e}"))?;

        let manifest_keys = match &manifest {
            Some((bytes, version)) => {
                let keys = parse_monitor_manifest(bytes)?;
                versions.insert(MONITOR_MANIFEST_KEY.to_string(), *version);
                keys
            }
            None => Vec::new(),
        };
        if !manifest_keys.is_empty() && cm.is_none() {
            return Err(
                "backup inconsistent: monitors present remotely but channel_manager missing"
                    .to_string(),
            );
        }

        for vss_key in &manifest_keys {
            let (bytes, version) = runtime
                .block_on(transport.get(vss_key))
                .map_err(|e| format!("monitor {vss_key} download failed: {e}"))?
                .ok_or_else(|| {
                    format!("monitor \"{vss_key}\" listed in manifest but missing from VSS")
                })?;
            // Validate by deserialization BEFORE the local write (R4).
            let (_block_hash, monitor) = <(BlockHash, ChannelMonitor<InMemorySigner>)>::read(
                &mut Cursor::new(&bytes),
                (&**keys_manager, &**signer_provider),
            )
            .map_err(|e| format!("monitor \"{vss_key}\" from VSS failed deserialization: {e:?}"))?;
            let local_key = monitor.persistence_key().to_string();
            kv_store
                .write(
                    CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
                    CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
                    &local_key,
                    bytes,
                )
                .map_err(|e| format!("local write of monitor {local_key} failed: {e}"))?;
            written_monitor_keys.push(local_key);
            versions.insert(vss_key.clone(), version);
            monitor_keys.insert(vss_key.clone());
        }

        if let Some((cm_bytes, cm_version)) = &cm {
            // PWA sanity floor for a serialized ChannelManager; the full
            // deserialization happens in the build right after this phase.
            if cm_bytes.len() < 32 {
                return Err(format!(
                    "channel_manager from VSS is too small ({} bytes) — likely corrupt",
                    cm_bytes.len()
                ));
            }
            kv_store
                .write(
                    CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
                    CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
                    CHANNEL_MANAGER_PERSISTENCE_KEY,
                    cm_bytes.clone(),
                )
                .map_err(|e| format!("local write of channel_manager failed: {e}"))?;
            wrote_channel_manager = true;
            versions.insert(CHANNEL_MANAGER_VSS_KEY.to_string(), *cm_version);
        }

        if let Some((bytes, version)) = runtime
            .block_on(transport.get(KNOWN_PEERS_VSS_KEY))
            .map_err(|e| format!("known-peers download failed: {e}"))?
        {
            let peers = parse_known_peers(&bytes)?;
            write_local_known_peers(kv_store, &peers)
                .map_err(|e| format!("local write of known peers failed: {e}"))?;
            wrote_peers = true;
            versions.insert(KNOWN_PEERS_VSS_KEY.to_string(), version);
        }

        log_info!(
            Logger,
            "Silent recovery complete: {} monitor(s), channel_manager: {}, peers: {}",
            manifest_keys.len(),
            cm.is_some(),
            wrote_peers
        );
        Ok(VssStartupState {
            remote: Some(Arc::clone(&transport)),
            versions,
            monitor_keys,
            probe_empty: false,
            recovered: true,
        })
    };

    match recover() {
        Ok(state) => Ok(state),
        Err(e) => {
            log_error!(
                logger,
                "Silent recovery against a non-empty namespace FAILED ({e}); rolling back \
                 partial local writes and refusing to start (never fresh-over-backup)"
            );
            for local_key in &written_monitor_keys {
                let _ = kv_store.remove(
                    CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
                    CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
                    local_key,
                    false,
                );
            }
            if wrote_channel_manager {
                let _ = kv_store.remove(
                    CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
                    CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
                    CHANNEL_MANAGER_PERSISTENCE_KEY,
                    false,
                );
            }
            if wrote_peers {
                let _ = kv_store.remove(
                    KNOWN_PEERS_PRIMARY_NAMESPACE,
                    KNOWN_PEERS_SECONDARY_NAMESPACE,
                    KNOWN_PEERS_LOCAL_KEY,
                    false,
                );
            }
            Err(BuildError::VssRecoveryFailed)
        }
    }
}
