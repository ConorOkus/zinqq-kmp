//! Restore-from-seed & silent-recovery engine (U4; F3; R1 restore half, R4;
//! KTD-3).
//!
//! One engine, two callers:
//!
//! - **Explicit restore** ([`run_restore`], surfaced as `Wallet::restore`):
//!   derive keys from the ENTERED mnemonic → probe `listKeyVersions` (empty →
//!   typed [`RestoreError::NoBackupFound`], local state untouched) → manifest
//!   reconciliation (any remote key not explained by the manifest or the
//!   fixed key set → typed [`RestoreError::BackupInconsistent`], abort before
//!   any write) → download CM and manifest→monitors (chunks of
//!   [`RESTORE_CHUNK_SIZE`] in parallel, [`RESTORE_DOWNLOAD_BUDGET`] overall)
//!   and known peers → validate EVERY blob by deserialization (monitors with
//!   the restored wallet's `SignerProvider`) BEFORE any local write →
//!   two-phase write: durable `restore_in_progress` marker → clear local
//!   state → ordered writes (mnemonic, CM before monitors, monitors, peers)
//!   → remove marker. Progress steps surface as `RestoreProgress` events
//!   matching the PWA's copy exactly (`zinq/src/pages/Restore.tsx`).
//! - **Silent recovery** (U3's `vss::startup` branch for an empty/voided
//!   local store) delegates the download/validate/write mechanics to the same
//!   [`fetch_manifest`] / [`download_and_validate`] / [`write_plan_local`]
//!   engine and keeps its own branch logic and fatality rules.
//!
//! Crash-prefix safety: the marker is written BEFORE anything destructive and
//! contains the full restore context (`{mnemonic, started_at_ms}` as JSON),
//! so EVERY crash prefix — after marker, after clear, after mnemonic, after
//! CM, mid-monitors — resumes: startup ([`prepare_marker_resume`], wired in
//! `builder::build`) re-adopts the marker's mnemonic, voids local LDK state,
//! and re-runs silent recovery (idempotent — everything is still remote).
//! While the marker exists a missing mnemonic never auto-generates fresh
//! words (U1, `keys::read_or_generate_mnemonic`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bitcoin::BlockHash;
use lightning::chain::channelmonitor::ChannelMonitor;
use lightning::sign::{InMemorySigner, KeysManager};
use lightning::util::logger::Logger as _;
use lightning::util::persist::{
    KVStoreSync, CHANNEL_MANAGER_PERSISTENCE_KEY, CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
    CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE, CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
    CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
};
use lightning::util::ser::ReadableArgs;
use lightning::{log_error, log_info};
use lightning_persister::fs_store::FilesystemStore;
use serde::{Deserialize, Serialize};

use crate::builder::{make_vss_transport, BuildError, KV_STORE_SUBDIR};
use crate::config::Config;
use crate::keys::{
    derive_wallet_keys, parse_mnemonic, write_mnemonic, MNEMONIC_FILE_NAME,
    RESTORE_IN_PROGRESS_FILE_NAME,
};
use crate::lock::DataDirLock;
use crate::node::{CoreEvent, EventSink};
use crate::signer::WalletSignerProvider;
use crate::types::Logger;
use crate::util::unix_now;
use crate::vss::known_peers::{
    parse_known_peers, write_local_known_peers, KnownPeer, KNOWN_PEERS_LOCAL_KEY,
    KNOWN_PEERS_PRIMARY_NAMESPACE, KNOWN_PEERS_SECONDARY_NAMESPACE,
};
use crate::vss::store::{
    parse_monitor_manifest, VersionedValue, VssTransport, CHANNEL_MANAGER_VSS_KEY,
    FENCED_FLAG_FILE_NAME, FIXED_REMOTE_KEYS, KNOWN_PEERS_VSS_KEY, MONITOR_MANIFEST_KEY,
};
use crate::vss::VssError;
use crate::wallet::OnchainWallet;

/// Monitors are downloaded in parallel chunks of this size (PWA
/// `VSS_RECOVERY_CHUNK_SIZE`).
pub(crate) const RESTORE_CHUNK_SIZE: usize = 10;

/// Overall monitor-download budget (PWA `VSS_RECOVERY_TIMEOUT_MS`).
pub(crate) const RESTORE_DOWNLOAD_BUDGET: Duration = Duration::from_secs(120);

// Progress-step copy, byte-identical to the PWA's Restore.tsx messages.
pub(crate) const STEP_DERIVING_KEYS: &str = "Deriving keys...";
pub(crate) const STEP_CHECKING_SERVER: &str = "Checking backup server...";
pub(crate) const STEP_STOPPING_WALLET: &str = "Stopping wallet...";
pub(crate) const STEP_CLEARING_DATA: &str = "Clearing local data...";
pub(crate) const STEP_WRITING_DATA: &str = "Writing restored data...";
pub(crate) const STEP_RESTARTING: &str = "Restarting wallet...";

/// The PWA's `Downloading ${keys.length} item(s)...` step.
pub(crate) fn step_downloading(item_count: usize) -> String {
    format!("Downloading {item_count} item(s)...")
}

/// Typed restore failures (U4). Everything before the two-phase write leaves
/// local state UNTOUCHED; a failure after the marker is written leaves a
/// resumable marker, never a partial boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreError {
    /// `restore()` while the node is running — restore is only valid from the
    /// stopped state (F3: stop-and-flush before clearing).
    NodeRunning,
    /// The entered words are not a valid BIP39 English 12-word mnemonic.
    InvalidMnemonic,
    /// VSS is disabled by configuration; there is no backup to restore from.
    VssDisabled,
    /// Restore plumbing (runtime, VSS client, scratch wallet) failed to set
    /// up. Nothing was touched.
    Setup {
        /// What failed to set up.
        detail: String,
    },
    /// `listKeyVersions` returned empty: no backup exists for these words.
    /// Local state untouched.
    NoBackupFound,
    /// The remote key set is not explained by the manifest plus the fixed key
    /// set (or the manifest/CM relationship is broken): restoring could lose
    /// a monitor another client tracks. Aborted before any write.
    BackupInconsistent {
        /// What is inconsistent.
        detail: String,
    },
    /// A download failed (network, server, or the overall time budget).
    DownloadFailed {
        /// The failing download.
        detail: String,
    },
    /// A downloaded blob failed validation-by-deserialization. Nothing was
    /// written locally.
    ValidationFailed {
        /// The failing blob.
        detail: String,
    },
    /// A local write in the two-phase phase failed. The durable marker (if
    /// already written) makes the next start resume the restore.
    LocalWriteFailed {
        /// The failing write.
        detail: String,
    },
    /// Test-only: a simulated process death injected via
    /// [`CrashPoint`]. Production callers never pass a crash point.
    Interrupted,
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RestoreError::NodeRunning => {
                write!(f, "the node is running; stop it before restoring")
            }
            RestoreError::InvalidMnemonic => write!(
                f,
                "the mnemonic is not a valid BIP39 English 12-word mnemonic"
            ),
            RestoreError::VssDisabled => {
                write!(f, "cloud backup is disabled; there is no backup to restore")
            }
            RestoreError::Setup { detail } => write!(f, "restore setup failed: {detail}"),
            RestoreError::NoBackupFound => write!(
                f,
                "No backup found for this wallet. Make sure you entered the correct seed phrase."
            ),
            RestoreError::BackupInconsistent { detail } => {
                write!(f, "backup inconsistent: {detail}")
            }
            RestoreError::DownloadFailed { detail } => {
                write!(f, "backup download failed: {detail}")
            }
            RestoreError::ValidationFailed { detail } => {
                write!(f, "backup validation failed: {detail}")
            }
            RestoreError::LocalWriteFailed { detail } => {
                write!(f, "local write during restore failed: {detail}")
            }
            RestoreError::Interrupted => write!(f, "restore interrupted (simulated crash)"),
        }
    }
}

impl std::error::Error for RestoreError {}

/// Test-only crash injection points for the two-phase write (the crash-prefix
/// matrix). Production passes `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CrashPoint {
    /// Die right after the durable marker write.
    Marker,
    /// Die right after clearing local state.
    Clear,
    /// Die right after the mnemonic write.
    Mnemonic,
    /// Die right after the channel-manager write, before any monitor.
    Manager,
}

/// The durable restore context, stored INSIDE the marker file as JSON so
/// every crash prefix can resume — including the prefix where the old
/// mnemonic was cleared but the new one was never written. The data dir is
/// app-private and the mnemonic file itself is plaintext, so the marker
/// holding the words adds no exposure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RestoreMarker {
    /// The TARGET wallet's 12 words (normalized, space-separated).
    pub mnemonic: String,
    /// When the restore started (UNIX ms) — diagnostics only.
    pub started_at_ms: u64,
}

/// Writes the marker durably (write + fsync + best-effort dir sync).
pub(crate) fn write_marker(storage_dir: &Path, marker: &RestoreMarker) -> std::io::Result<()> {
    use std::io::Write as _;
    let path = storage_dir.join(RESTORE_IN_PROGRESS_FILE_NAME);
    let mut file = fs::File::create(&path)?;
    file.write_all(&serde_json::to_vec(marker).expect("marker always serializes"))?;
    file.sync_all()?;
    if let Ok(dir) = fs::File::open(storage_dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Reads the marker's restore context. `None` when the marker is absent OR
/// holds no parsable context (a legacy/void marker still voids local LDK
/// state via the existing startup branch; it just cannot supply a mnemonic).
pub(crate) fn read_marker(storage_dir: &Path) -> Option<RestoreMarker> {
    let bytes = fs::read(storage_dir.join(RESTORE_IN_PROGRESS_FILE_NAME)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Removes the marker (restore complete).
pub(crate) fn remove_marker(storage_dir: &Path) -> std::io::Result<()> {
    match fs::remove_file(storage_dir.join(RESTORE_IN_PROGRESS_FILE_NAME)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// One downloaded-and-validated monitor, ready for its ordered local write.
pub(crate) struct ValidatedMonitor {
    /// PWA VSS key `{txid_hex}:{index}` (raw txid byte order).
    pub vss_key: String,
    /// Local `FilesystemStore` key (`MonitorName::to_string()`), derived from
    /// the DESERIALIZED monitor so `read_channel_monitors` finds it again.
    pub local_key: String,
    /// The raw monitor bytes exactly as fetched.
    pub bytes: Vec<u8>,
    /// Server version, seeding the post-restore version cache.
    pub version: i64,
}

/// Everything a restore/recovery downloads, fully validated BEFORE any local
/// write (R4).
pub(crate) struct RestorePlan {
    /// `_monitor_keys` server version, when a manifest exists.
    pub manifest_version: Option<i64>,
    /// The channel manager blob + version, when one exists remotely.
    pub cm: Option<(Vec<u8>, i64)>,
    /// Validated monitors, in manifest order.
    pub monitors: Vec<ValidatedMonitor>,
    /// Parsed known peers + version, when the blob exists remotely.
    pub peers: Option<(BTreeMap<String, KnownPeer>, i64)>,
}

impl RestorePlan {
    /// Version-cache seeds for [`crate::vss::store::VssBackedStore`].
    pub(crate) fn versions(&self) -> HashMap<String, i64> {
        let mut versions = HashMap::new();
        if let Some(version) = self.manifest_version {
            versions.insert(MONITOR_MANIFEST_KEY.to_string(), version);
        }
        if let Some((_, version)) = &self.cm {
            versions.insert(CHANNEL_MANAGER_VSS_KEY.to_string(), *version);
        }
        for monitor in &self.monitors {
            versions.insert(monitor.vss_key.clone(), monitor.version);
        }
        if let Some((_, version)) = &self.peers {
            versions.insert(KNOWN_PEERS_VSS_KEY.to_string(), *version);
        }
        versions
    }

    /// The `_monitor_keys` set the plan carries.
    pub(crate) fn monitor_keys(&self) -> BTreeSet<String> {
        self.monitors
            .iter()
            .map(|monitor| monitor.vss_key.clone())
            .collect()
    }
}

fn download_err(e: VssError) -> RestoreError {
    RestoreError::DownloadFailed {
        detail: e.to_string(),
    }
}

/// Downloads and parses the `_monitor_keys` manifest. `Ok(None)` when no
/// manifest exists (a zero-channel backup).
pub(crate) async fn fetch_manifest(
    transport: &dyn VssTransport,
) -> Result<Option<(Vec<String>, i64)>, RestoreError> {
    match transport
        .get(MONITOR_MANIFEST_KEY)
        .await
        .map_err(download_err)?
    {
        None => Ok(None),
        Some((bytes, version)) => {
            let keys = parse_monitor_manifest(&bytes)
                .map_err(|detail| RestoreError::ValidationFailed { detail })?;
            Ok(Some((keys, version)))
        }
    }
}

/// How many unexplained obfuscated keys the error names before eliding the
/// rest. Obfuscated keys are HMACs of key NAMES, never of secrets, so quoting
/// a few is safe — and without at least one the failure is un-triageable.
const UNEXPLAINED_KEYS_IN_ERROR: usize = 3;

/// Manifest reconciliation (U4, adversarially reviewed — load-bearing):
/// every key `listKeyVersions` reports must be EXPLAINED — the obfuscated
/// form of a manifest entry or of one of [`FIXED_REMOTE_KEYS`]. Any
/// unexplained remote key means the manifest undercounts the monitors (or the
/// store holds foreign data): restoring would silently drop fund-safety
/// state, so the restore aborts BEFORE any write with a typed error.
///
/// The error text is a triage report, not just a hash: it names how big the
/// listing was, how much of it the manifest and the fixed keys each accounted
/// for, WHICH fixed keys were present versus absent, and the first few
/// unexplained obfuscated keys. All of that is derived from key NAMES and
/// counts — no blob is fetched or decrypted here, and the mnemonic,
/// encryption key and plaintext values never enter the message.
pub(crate) fn reconcile_backup_keys(
    listing: &[(String, i64)],
    manifest_keys: &[String],
    transport: &dyn VssTransport,
) -> Result<(), RestoreError> {
    // Obfuscated keys are deterministic HMACs, so the obfuscated form of
    // every EXPECTED plaintext key is computable client-side and set-diffed
    // against the (obfuscated) listing.
    let fixed: Vec<(&str, String)> = FIXED_REMOTE_KEYS
        .iter()
        .map(|key| (*key, transport.obfuscate(key)))
        .collect();
    let manifest_obfuscated: HashSet<String> = manifest_keys
        .iter()
        .map(|key| transport.obfuscate(key))
        .collect();
    let expected: HashSet<&String> = fixed
        .iter()
        .map(|(_, obfuscated)| obfuscated)
        .chain(manifest_obfuscated.iter())
        .collect();
    let unexplained: Vec<&str> = listing
        .iter()
        .filter(|(obfuscated_key, _)| !expected.contains(obfuscated_key))
        .map(|(obfuscated_key, _)| obfuscated_key.as_str())
        .collect();
    if unexplained.is_empty() {
        return Ok(());
    }

    // Which side of the comparison came up short: the plaintext names are the
    // only actionable fact a user or developer can relay.
    let listed: HashSet<&str> = listing.iter().map(|(key, _)| key.as_str()).collect();
    let mut present: Vec<&str> = Vec::new();
    let mut absent: Vec<&str> = Vec::new();
    for (plaintext, obfuscated) in &fixed {
        if listed.contains(obfuscated.as_str()) {
            present.push(plaintext);
        } else {
            absent.push(plaintext);
        }
    }
    let explained_by_manifest = listing
        .iter()
        .filter(|(key, _)| manifest_obfuscated.contains(key))
        .count();
    let shown: Vec<&str> = unexplained
        .iter()
        .take(UNEXPLAINED_KEYS_IN_ERROR)
        .copied()
        .collect();
    let elided = unexplained.len().saturating_sub(shown.len());
    Err(RestoreError::BackupInconsistent {
        detail: format!(
            "{} of the {} key(s) on the backup server are not explained. The monitor manifest \
             declares {} monitor key(s) and accounts for {} of the listing; the expected wallet \
             keys account for {} more. Expected keys found: [{}]. Expected keys absent: [{}]. \
             Unexplained obfuscated key(s): {}{}. This is usually a channel-monitor backup that \
             was uploaded but never listed in the manifest, so restoring from it could silently \
             drop channel state — nothing was written locally.",
            unexplained.len(),
            listing.len(),
            manifest_keys.len(),
            explained_by_manifest,
            present.len(),
            present.join(", "),
            absent.join(", "),
            shown.join(", "),
            if elided > 0 {
                format!(" (+{elided} more)")
            } else {
                String::new()
            }
        ),
    })
}

/// Downloads CM + monitors + known peers and validates EVERY blob by
/// deserialization before returning (R4). Monitors download in parallel
/// chunks of [`RESTORE_CHUNK_SIZE`] with `budget` as the overall time box
/// (PWA `init.ts` recovery loop). Nothing is written anywhere.
pub(crate) async fn download_and_validate(
    transport: &Arc<dyn VssTransport>,
    manifest: Option<(Vec<String>, i64)>,
    keys_manager: &KeysManager,
    signer_provider: &WalletSignerProvider,
    budget: Duration,
) -> Result<RestorePlan, RestoreError> {
    let (manifest_keys, manifest_version) = match manifest {
        Some((keys, version)) => (keys, Some(version)),
        None => (Vec::new(), None),
    };

    let cm = transport
        .get(CHANNEL_MANAGER_VSS_KEY)
        .await
        .map_err(download_err)?;
    if !manifest_keys.is_empty() && cm.is_none() {
        return Err(RestoreError::BackupInconsistent {
            detail: "monitors present remotely but channel_manager missing".to_string(),
        });
    }
    if let Some((bytes, _)) = &cm {
        // PWA sanity floor for a serialized ChannelManager; the full
        // deserialization happens when the node boots on the restored state.
        if bytes.len() < 32 {
            return Err(RestoreError::ValidationFailed {
                detail: format!(
                    "channel_manager from VSS is too small ({} bytes) — likely corrupt",
                    bytes.len()
                ),
            });
        }
    }

    let started = tokio::time::Instant::now();
    let mut monitors: Vec<ValidatedMonitor> = Vec::with_capacity(manifest_keys.len());
    for chunk in manifest_keys.chunks(RESTORE_CHUNK_SIZE) {
        if started.elapsed() > budget {
            return Err(RestoreError::DownloadFailed {
                detail: format!(
                    "VSS recovery timeout: downloaded {}/{} monitors in {}s. Retry on a faster \
                     connection.",
                    monitors.len(),
                    manifest_keys.len(),
                    started.elapsed().as_secs()
                ),
            });
        }
        // Parallel chunk download (PWA Promise.all over the chunk).
        let mut join_set = tokio::task::JoinSet::new();
        for (index, vss_key) in chunk.iter().enumerate() {
            let transport = Arc::clone(transport);
            let vss_key = vss_key.clone();
            join_set.spawn(async move { (index, transport.get(&vss_key).await) });
        }
        let mut results: Vec<Option<Result<VersionedValue, VssError>>> =
            (0..chunk.len()).map(|_| None).collect();
        while let Some(joined) = join_set.join_next().await {
            let (index, result) = joined.map_err(|e| RestoreError::Setup {
                detail: format!("monitor download task failed: {e}"),
            })?;
            results[index] = Some(result);
        }
        for (index, result) in results.into_iter().enumerate() {
            let vss_key = &chunk[index];
            let value = result
                .expect("every spawned download reports back")
                .map_err(download_err)?;
            let (bytes, version) = value.ok_or_else(|| RestoreError::BackupInconsistent {
                detail: format!("monitor \"{vss_key}\" listed in manifest but missing from VSS"),
            })?;
            // Validate by deserialization with the RESTORED wallet's signer
            // BEFORE anything is written (R4).
            let (_block_hash, monitor) = <(BlockHash, ChannelMonitor<InMemorySigner>)>::read(
                &mut Cursor::new(&bytes),
                (keys_manager, signer_provider),
            )
            .map_err(|e| RestoreError::ValidationFailed {
                detail: format!("monitor \"{vss_key}\" from VSS failed deserialization: {e:?}"),
            })?;
            monitors.push(ValidatedMonitor {
                vss_key: vss_key.clone(),
                local_key: monitor.persistence_key().to_string(),
                bytes,
                version,
            });
        }
    }

    let peers = match transport
        .get(KNOWN_PEERS_VSS_KEY)
        .await
        .map_err(download_err)?
    {
        Some((bytes, version)) => {
            let map = parse_known_peers(&bytes)
                .map_err(|detail| RestoreError::ValidationFailed { detail })?;
            Some((map, version))
        }
        None => None,
    };

    Ok(RestorePlan {
        manifest_version,
        cm,
        monitors,
        peers,
    })
}

/// One entry of the ordered local write log — returned so tests can assert
/// the CM-before-monitors ordering and so callers can roll back exactly what
/// was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalWrite {
    /// The channel manager, under LDK's persist key constants.
    Manager,
    /// One monitor, under its `MonitorName` local key.
    Monitor(String),
    /// The known-peers local mirror.
    Peers,
}

/// Ordered local writes of a validated plan: CM BEFORE monitors, then
/// monitors, then peers (F3). Appends each completed write to `log` so a
/// failure leaves the caller an exact rollback list. `stop_after_manager` is
/// test-only crash injection.
pub(crate) fn write_plan_local(
    kv_store: &FilesystemStore,
    plan: &RestorePlan,
    log: &mut Vec<LocalWrite>,
    stop_after_manager: bool,
) -> Result<(), lightning::io::Error> {
    if let Some((bytes, _)) = &plan.cm {
        kv_store.write(
            CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
            CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
            CHANNEL_MANAGER_PERSISTENCE_KEY,
            bytes.clone(),
        )?;
        log.push(LocalWrite::Manager);
    }
    if stop_after_manager {
        return Ok(());
    }
    for monitor in &plan.monitors {
        kv_store.write(
            CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
            CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
            &monitor.local_key,
            monitor.bytes.clone(),
        )?;
        log.push(LocalWrite::Monitor(monitor.local_key.clone()));
    }
    if let Some((peers, _)) = &plan.peers {
        write_local_known_peers(kv_store, peers)?;
        log.push(LocalWrite::Peers);
    }
    Ok(())
}

/// Removes exactly the writes `log` records (silent recovery's
/// never-fresh-over-backup rollback).
pub(crate) fn rollback_local_writes(kv_store: &FilesystemStore, log: &[LocalWrite]) {
    for entry in log {
        let _ = match entry {
            LocalWrite::Manager => kv_store.remove(
                CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
                CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
                CHANNEL_MANAGER_PERSISTENCE_KEY,
                false,
            ),
            LocalWrite::Monitor(local_key) => kv_store.remove(
                CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
                CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
                local_key,
                false,
            ),
            LocalWrite::Peers => kv_store.remove(
                KNOWN_PEERS_PRIMARY_NAMESPACE,
                KNOWN_PEERS_SECONDARY_NAMESPACE,
                KNOWN_PEERS_LOCAL_KEY,
                false,
            ),
        };
    }
}

fn remove_file_if_exists(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Clears the wallet's local state: the `store/` KV directory (LDK state,
/// history, event queue, bdk changeset), the mnemonic file, and the fenced
/// flag (restore IS the documented un-fence path — KTD-3). The restore
/// marker and the data-dir lock survive.
fn clear_local_wallet_state(storage_dir: &Path) -> std::io::Result<()> {
    let store_dir = storage_dir.join(KV_STORE_SUBDIR);
    if store_dir.exists() {
        fs::remove_dir_all(&store_dir)?;
    }
    remove_file_if_exists(&storage_dir.join(MNEMONIC_FILE_NAME))?;
    remove_file_if_exists(&storage_dir.join(FENCED_FLAG_FILE_NAME))?;
    Ok(())
}

/// Startup half of crash-prefix safety, called by `builder::build` BEFORE the
/// fence check and mnemonic load: when the marker holds a restore context and
/// the on-disk mnemonic is not already the marker's target, the interrupted
/// clear is redone (idempotent) and the TARGET mnemonic is written — so the
/// normal marker branch (`vss::startup`) resumes silent recovery under the
/// restored identity, whatever prefix the crash cut.
pub(crate) fn prepare_marker_resume(
    storage_dir: &Path,
    logger: &Arc<Logger>,
) -> Result<(), BuildError> {
    let Some(marker) = read_marker(storage_dir) else {
        return Ok(());
    };
    let Ok(target) = parse_mnemonic(&marker.mnemonic) else {
        log_error!(
            logger,
            "Restore marker holds an invalid mnemonic; treating it as a void-only marker"
        );
        return Ok(());
    };
    let current_matches = fs::read_to_string(storage_dir.join(MNEMONIC_FILE_NAME))
        .ok()
        .and_then(|raw| parse_mnemonic(&raw).ok())
        .is_some_and(|current| current == target);
    if current_matches {
        return Ok(());
    }
    log_info!(
        logger,
        "Resuming an interrupted restore: redoing the local clear and adopting the marker's \
         mnemonic"
    );
    clear_local_wallet_state(storage_dir).map_err(|_| BuildError::WriteFailed)?;
    write_mnemonic(storage_dir, &target)?;
    Ok(())
}

/// RAII scratch directory for the validation-only signer stack (the real
/// data dir must stay untouched until the two-phase write).
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn create() -> Result<Self, RestoreError> {
        let path = std::env::temp_dir().join(format!(
            "zinqq-restore-scratch-{}-{}",
            std::process::id(),
            unix_now().as_nanos()
        ));
        fs::create_dir_all(&path).map_err(|e| RestoreError::Setup {
            detail: format!("failed to create the validation scratch dir: {e}"),
        })?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The explicit restore flow (F3), valid only with the node stopped —
/// `Node::restore` enforces that and holds the node's state lock; this
/// function additionally takes the data-dir lock so no other process can
/// boot mid-restore.
///
/// `crash_after` is test-only crash injection for the crash-prefix matrix.
pub(crate) fn run_restore(
    config: &Config,
    mnemonic_raw: &str,
    event_sink: &dyn EventSink,
    crash_after: Option<CrashPoint>,
) -> Result<(), RestoreError> {
    let logger = Arc::new(Logger);
    let storage_dir = PathBuf::from(&config.storage_dir);
    fs::create_dir_all(&storage_dir).map_err(|e| RestoreError::LocalWriteFailed {
        detail: format!("failed to create the storage dir: {e}"),
    })?;
    let _dir_lock = DataDirLock::acquire(&storage_dir).map_err(|e| match e {
        BuildError::InstanceAlreadyRunning => RestoreError::NodeRunning,
        other => RestoreError::Setup {
            detail: other.to_string(),
        },
    })?;

    let progress = |step: &str| {
        event_sink.emit(CoreEvent::RestoreProgress {
            step: step.to_string(),
        })
    };

    progress(STEP_DERIVING_KEYS);
    let mnemonic = parse_mnemonic(mnemonic_raw).map_err(|_| RestoreError::InvalidMnemonic)?;
    let keys = derive_wallet_keys(&mnemonic, config.network);
    let transport = make_vss_transport(config, &keys)
        .map_err(|e| RestoreError::Setup {
            detail: e.to_string(),
        })?
        .ok_or(RestoreError::VssDisabled)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("wallet-core-restore")
        .enable_all()
        .build()
        .map_err(|e| RestoreError::Setup {
            detail: format!("failed to create the restore runtime: {e}"),
        })?;

    progress(STEP_CHECKING_SERVER);
    let listing = runtime
        .block_on(transport.list_key_versions())
        .map_err(download_err)?;
    if listing.is_empty() {
        // The PWA's "No backup found" outcome: local state UNTOUCHED.
        return Err(RestoreError::NoBackupFound);
    }

    progress(&step_downloading(listing.len()));
    let manifest = runtime.block_on(fetch_manifest(&*transport))?;
    let manifest_keys: Vec<String> = manifest
        .as_ref()
        .map(|(keys, _)| keys.clone())
        .unwrap_or_default();
    reconcile_backup_keys(&listing, &manifest_keys, &*transport)?;

    // Validation signer stack from the ENTERED mnemonic, over a throwaway
    // scratch store: the real data dir stays untouched until the plan is
    // fully validated.
    let scratch = ScratchDir::create()?;
    let now = unix_now();
    let keys_manager = Arc::new(KeysManager::new(
        &keys.ldk_seed,
        now.as_secs(),
        now.subsec_nanos(),
        false,
    ));
    let scratch_store = Arc::new(FilesystemStore::new(scratch.path().join(KV_STORE_SUBDIR)));
    let onchain_wallet = Arc::new(
        OnchainWallet::new(
            &keys.descriptor_external,
            &keys.descriptor_internal,
            config.network,
            scratch_store,
            Arc::clone(&logger),
        )
        .map_err(|e| RestoreError::Setup {
            detail: format!("failed to set up the validation wallet: {e}"),
        })?,
    );
    let signer_provider = WalletSignerProvider::new(
        Arc::clone(&keys_manager),
        onchain_wallet,
        keys.channel_keys_id_hmac_key,
        Arc::clone(&logger),
    );
    let plan = runtime.block_on(download_and_validate(
        &transport,
        manifest,
        &keys_manager,
        &signer_provider,
        RESTORE_DOWNLOAD_BUDGET,
    ))?;
    drop(signer_provider);
    drop(scratch);
    drop(keys); // WalletKeys::drop scrubs the derived key material.

    // ---- Two-phase destructive write (everything above touched nothing) ----

    // The node is stopped by contract (Node::restore holds the state lock and
    // this function holds the data-dir lock); the step is emitted anyway for
    // exact PWA copy parity.
    progress(STEP_STOPPING_WALLET);
    let marker = RestoreMarker {
        mnemonic: mnemonic.to_string(),
        started_at_ms: crate::util::now_ms(),
    };
    write_marker(&storage_dir, &marker).map_err(|e| RestoreError::LocalWriteFailed {
        detail: format!("restore marker write failed: {e}"),
    })?;
    if crash_after == Some(CrashPoint::Marker) {
        return Err(RestoreError::Interrupted);
    }

    progress(STEP_CLEARING_DATA);
    clear_local_wallet_state(&storage_dir).map_err(|e| RestoreError::LocalWriteFailed {
        detail: format!("clearing local state failed: {e}"),
    })?;
    if crash_after == Some(CrashPoint::Clear) {
        return Err(RestoreError::Interrupted);
    }

    progress(STEP_WRITING_DATA);
    write_mnemonic(&storage_dir, &mnemonic).map_err(|e| RestoreError::LocalWriteFailed {
        detail: format!("mnemonic write failed: {e}"),
    })?;
    if crash_after == Some(CrashPoint::Mnemonic) {
        return Err(RestoreError::Interrupted);
    }

    let kv_store = FilesystemStore::new(storage_dir.join(KV_STORE_SUBDIR));
    let mut write_log = Vec::new();
    write_plan_local(
        &kv_store,
        &plan,
        &mut write_log,
        crash_after == Some(CrashPoint::Manager),
    )
    .map_err(|e| RestoreError::LocalWriteFailed {
        detail: format!("restored-data write failed: {e}"),
    })?;
    if crash_after == Some(CrashPoint::Manager) {
        return Err(RestoreError::Interrupted);
    }

    remove_marker(&storage_dir).map_err(|e| RestoreError::LocalWriteFailed {
        detail: format!("marker removal failed: {e}"),
    })?;
    log_info!(
        logger,
        "Restore complete: {} monitor(s), channel_manager: {}, peers: {}",
        plan.monitors.len(),
        plan.cm.is_some(),
        plan.peers.is_some()
    );
    progress(STEP_RESTARTING);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::config::VssTransportOverride;
    use crate::history::{PaymentDirection, PaymentStore};
    use crate::node::{EventSink, LoggingEventSink, Node};
    use crate::vss::known_peers::read_local_known_peers;
    use crate::vss::test_support::MockTransport;

    #[derive(Default)]
    struct CapturingSink(Mutex<Vec<CoreEvent>>);

    impl EventSink for CapturingSink {
        fn emit(&self, event: CoreEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    impl CapturingSink {
        fn steps(&self) -> Vec<String> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter_map(|event| match event {
                    CoreEvent::RestoreProgress { step } => Some(step.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    fn offline_config(dir: &Path) -> Config {
        let mut config = Config::new(dir.to_str().unwrap().to_string());
        config.esplora_url = "http://127.0.0.1:1".to_string();
        config.rgs_url = "http://127.0.0.1:1/snapshot".to_string();
        config.vss_disabled = true;
        config
    }

    fn vss_config(dir: &Path, transport: &Arc<MockTransport>) -> Config {
        let mut config = offline_config(dir);
        config.vss_disabled = false;
        config.vss_transport_override = Some(VssTransportOverride(
            Arc::clone(transport) as Arc<dyn VssTransport>
        ));
        config
    }

    /// A local-only wallet in `dir`; returns (node id, mnemonic words).
    fn create_local_wallet(dir: &Path) -> (String, String) {
        let node = Node::new(offline_config(dir));
        node.start().expect("offline degraded start");
        let node_id = node.node_id().unwrap().to_string();
        node.stop().unwrap();
        let mnemonic = fs::read_to_string(dir.join(MNEMONIC_FILE_NAME)).unwrap();
        (node_id, mnemonic)
    }

    /// A wallet created WITH the mock transport, so its channel manager lands
    /// on "VSS" (the migration/dual-write path). The wallet's own dir is
    /// dropped: the backup lives in the returned transport.
    fn seeded_backup() -> (Arc<MockTransport>, String, String) {
        let transport = Arc::new(MockTransport::new());
        let dir = tempfile::tempdir().unwrap();
        let node = Node::new(vss_config(dir.path(), &transport));
        node.start().expect("fresh VSS-enabled start");
        let node_id = node.node_id().unwrap().to_string();
        node.stop().unwrap();
        let mnemonic = fs::read_to_string(dir.path().join(MNEMONIC_FILE_NAME)).unwrap();
        (transport, node_id, mnemonic)
    }

    fn kv_store(dir: &Path) -> FilesystemStore {
        FilesystemStore::new(dir.join(KV_STORE_SUBDIR))
    }

    fn local_cm(dir: &Path) -> Result<Vec<u8>, lightning::io::Error> {
        kv_store(dir).read(
            CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
            CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
            CHANNEL_MANAGER_PERSISTENCE_KEY,
        )
    }

    const PEER_PUBKEY: &str = "034066e29e402d9cf55af1ae1026cc5adf92eed1e0e421785442f53717ad1453b0";
    const PEERS_JSON: &str = r#"{"034066e29e402d9cf55af1ae1026cc5adf92eed1e0e421785442f53717ad1453b0": {"host": "64.23.159.177", "port": 9735}}"#;

    // ---------- scenario 1 (AE3 offline half) + progress copy ----------

    /// Full explicit restore over an EXISTING different wallet: identity,
    /// peers, and un-fencing all adopt the backup; the pre-restore payment
    /// history is gone; progress steps match the PWA copy exactly, in order.
    #[test]
    fn restore_rebuilds_the_backed_up_wallet_and_emits_the_exact_pwa_steps() {
        let (transport, backup_node_id, backup_mnemonic) = seeded_backup();
        transport.seed(KNOWN_PEERS_VSS_KEY, PEERS_JSON.as_bytes(), 2);

        // The victim dir holds a DIFFERENT wallet, a payment row, and a
        // fenced flag (restore is the documented un-fence path).
        let dir = tempfile::tempdir().unwrap();
        let (old_node_id, old_mnemonic) = create_local_wallet(dir.path());
        assert_ne!(old_mnemonic, backup_mnemonic);
        let row_id = "aa".repeat(32);
        PaymentStore::new(Arc::new(kv_store(dir.path())), Arc::new(Logger))
            .record_pending(&row_id, PaymentDirection::Outbound, 1_000, 1)
            .unwrap();
        fs::write(dir.path().join(FENCED_FLAG_FILE_NAME), b"divergent").unwrap();

        let sink = Arc::new(CapturingSink::default());
        let node =
            Node::with_event_sink(vss_config(dir.path(), &transport), Arc::clone(&sink) as _);
        assert!(node.payment_detail(&row_id).is_some());

        node.restore(&backup_mnemonic)
            .expect("restore must succeed");

        // The words were replaced, the marker cleared, the fence lifted.
        assert_eq!(
            fs::read_to_string(dir.path().join(MNEMONIC_FILE_NAME)).unwrap(),
            parse_mnemonic(&backup_mnemonic).unwrap().to_string()
        );
        assert!(!dir.path().join(RESTORE_IN_PROGRESS_FILE_NAME).exists());
        assert!(!dir.path().join(FENCED_FLAG_FILE_NAME).exists());
        // Pre-restore history is gone, in memory and on disk.
        assert!(node.payment_detail(&row_id).is_none());
        // Known peers were restored into the local mirror.
        let peers = read_local_known_peers(&kv_store(dir.path()));
        assert_eq!(peers.len(), 1);
        assert!(peers.contains_key(PEER_PUBKEY));

        // The node restarts with the RESTORED identity (AE3 offline half).
        node.start().expect("post-restore start");
        let restored_id = node.node_id().unwrap().to_string();
        node.stop().unwrap();
        assert_eq!(restored_id, backup_node_id);
        assert_ne!(restored_id, old_node_id);

        // Progress steps: the PWA's copy, exactly, in order. The listing held
        // channel_manager + _known_peers → "2 item(s)".
        assert_eq!(
            sink.steps(),
            vec![
                "Deriving keys...",
                "Checking backup server...",
                "Downloading 2 item(s)...",
                "Stopping wallet...",
                "Clearing local data...",
                "Writing restored data...",
                "Restarting wallet...",
            ]
        );
    }

    // ---------- scenario 5: no backup ----------

    /// Empty `listKeyVersions` → typed NoBackupFound; the current wallet is
    /// completely untouched and no destructive step ever ran.
    #[test]
    fn restore_with_empty_namespace_is_no_backup_found_and_local_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let (node_id, mnemonic) = create_local_wallet(dir.path());
        let cm_before = local_cm(dir.path()).unwrap();

        let transport = Arc::new(MockTransport::new());
        let sink = Arc::new(CapturingSink::default());
        let node =
            Node::with_event_sink(vss_config(dir.path(), &transport), Arc::clone(&sink) as _);
        assert_eq!(
            node.restore(crate::keys::tests::TEST_MNEMONIC).unwrap_err(),
            RestoreError::NoBackupFound
        );

        assert_eq!(
            fs::read_to_string(dir.path().join(MNEMONIC_FILE_NAME)).unwrap(),
            mnemonic,
            "the original words must survive a no-backup restore attempt"
        );
        assert_eq!(local_cm(dir.path()).unwrap(), cm_before);
        assert!(!dir.path().join(RESTORE_IN_PROGRESS_FILE_NAME).exists());
        // The flow stopped at the probe: no destructive step was announced.
        assert_eq!(
            sink.steps(),
            vec!["Deriving keys...", "Checking backup server..."]
        );

        // The original wallet still boots (offline, local-only).
        let node = Node::new(offline_config(dir.path()));
        node.start().unwrap();
        assert_eq!(node.node_id().unwrap().to_string(), node_id);
        node.stop().unwrap();
    }

    // ---------- scenario 4: manifest reconciliation ----------

    /// A remote monitor-shaped key that no manifest explains → typed
    /// BackupInconsistent, aborted before ANY write (no marker, words
    /// intact, store intact).
    #[test]
    fn unexplained_remote_key_aborts_with_backup_inconsistent_before_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let (_, mnemonic) = create_local_wallet(dir.path());
        let cm_before = local_cm(dir.path()).unwrap();

        // Backup with a CM and a rogue monitor key but NO manifest at all.
        let transport = Arc::new(MockTransport::new());
        transport.seed(CHANNEL_MANAGER_VSS_KEY, &[7u8; 64], 1);
        let rogue = format!("{}:0", "ab".repeat(32));
        transport.seed(&rogue, b"orphan monitor bytes", 1);

        let node = Node::new(vss_config(dir.path(), &transport));
        let err = node.restore(crate::keys::tests::TEST_MNEMONIC).unwrap_err();
        assert!(
            matches!(err, RestoreError::BackupInconsistent { .. }),
            "expected BackupInconsistent, got {err:?}"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(MNEMONIC_FILE_NAME)).unwrap(),
            mnemonic
        );
        assert_eq!(local_cm(dir.path()).unwrap(), cm_before);
        assert!(!dir.path().join(RESTORE_IN_PROGRESS_FILE_NAME).exists());
    }

    /// Same with a manifest present: a monitor key OUTSIDE the manifest is
    /// unexplained even though other monitor keys are listed.
    #[test]
    fn monitor_key_absent_from_manifest_aborts_with_backup_inconsistent() {
        let dir = tempfile::tempdir().unwrap();
        create_local_wallet(dir.path());

        let listed = format!("{}:0", "cd".repeat(32));
        let unlisted = format!("{}:1", "ef".repeat(32));
        let transport = Arc::new(MockTransport::new());
        transport.seed(CHANNEL_MANAGER_VSS_KEY, &[7u8; 64], 1);
        transport.seed(
            MONITOR_MANIFEST_KEY,
            &serde_json::to_vec(&vec![listed.clone()]).unwrap(),
            1,
        );
        transport.seed(&listed, b"listed monitor bytes", 1);
        transport.seed(&unlisted, b"unlisted monitor bytes", 1);

        let node = Node::new(vss_config(dir.path(), &transport));
        let err = node.restore(crate::keys::tests::TEST_MNEMONIC).unwrap_err();
        assert!(
            matches!(err, RestoreError::BackupInconsistent { .. }),
            "expected BackupInconsistent, got {err:?}"
        );
        assert!(!dir.path().join(RESTORE_IN_PROGRESS_FILE_NAME).exists());
    }

    /// The fixed key set (CM, manifest, peers, close records, recovery
    /// state) is always explained — a backup containing all of them plus
    /// manifest-listed monitors reconciles cleanly.
    #[test]
    fn reconciliation_accepts_the_fixed_key_set_and_manifest_entries() {
        let transport = MockTransport::new();
        let monitor_key = format!("{}:0", "ab".repeat(32));
        // Driven off the shared list, so a key added there without teaching
        // reconcile about it can never pass unnoticed.
        let listing: Vec<(String, i64)> = FIXED_REMOTE_KEYS
            .iter()
            .copied()
            .chain(std::iter::once(monitor_key.as_str()))
            .map(|key| (key.to_string(), 1))
            .collect();
        reconcile_backup_keys(&listing, std::slice::from_ref(&monitor_key), &transport)
            .expect("every key is explained");

        let mut with_rogue = listing.clone();
        with_rogue.push(("something_else".to_string(), 1));
        assert!(matches!(
            reconcile_backup_keys(&with_rogue, std::slice::from_ref(&monitor_key), &transport),
            Err(RestoreError::BackupInconsistent { .. })
        ));
    }

    /// A bare hash is not a bug report: `BackupInconsistent` must name what
    /// was actually COMPARED — the listing size, how much of it the manifest
    /// and the expected keys each accounted for, and which expected plaintext
    /// keys were present versus absent — so a user's screenshot is
    /// triageable. It must stay leak-free: obfuscated keys, plaintext KEY
    /// NAMES and counts only, never a value.
    #[test]
    fn backup_inconsistent_names_what_was_compared_without_leaking_values() {
        let transport = MockTransport::new();
        let monitor_key = format!("{}:0", "ab".repeat(32));
        let orphan_a = format!("{}:0", "cd".repeat(32));
        let orphan_b = format!("{}:1", "cd".repeat(32));
        let orphan_c = format!("{}:2", "cd".repeat(32));
        let orphan_d = format!("{}:3", "cd".repeat(32));
        // A partially-populated backup: CM + manifest + one listed monitor,
        // and four monitor blobs the manifest never declared.
        let listing: Vec<(String, i64)> = [
            CHANNEL_MANAGER_VSS_KEY,
            MONITOR_MANIFEST_KEY,
            monitor_key.as_str(),
            orphan_a.as_str(),
            orphan_b.as_str(),
            orphan_c.as_str(),
            orphan_d.as_str(),
        ]
        .iter()
        .map(|key| (key.to_string(), 1))
        .collect();

        let detail =
            match reconcile_backup_keys(&listing, std::slice::from_ref(&monitor_key), &transport) {
                Err(RestoreError::BackupInconsistent { detail }) => detail,
                other => panic!("expected BackupInconsistent, got {other:?}"),
            };

        // The comparison, in numbers.
        assert!(detail.contains("4 of the 7 key(s)"), "{detail}");
        assert!(detail.contains("declares 1 monitor key(s)"), "{detail}");
        assert!(detail.contains("accounts for 1 of the listing"), "{detail}");
        // Which expected keys the server actually had, by plaintext name.
        assert!(
            detail.contains("found: [channel_manager, _monitor_keys]"),
            "{detail}"
        );
        assert!(
            detail.contains("absent: [_known_peers, close_records, force_close_recovery]"),
            "{detail}"
        );
        // The offenders, capped, with the remainder counted.
        assert!(detail.contains(&orphan_a), "{detail}");
        assert!(detail.contains("(+1 more)"), "{detail}");
        assert!(
            !detail.contains(&orphan_d),
            "the listing is truncated, not dumped: {detail}"
        );
    }

    // ---------- scenario 2: rollback / original intact ----------

    /// A corrupt monitor blob in the set fails validation BEFORE any local
    /// write: typed error, no partial writes, the ORIGINAL wallet (words +
    /// manager) still loads.
    #[test]
    fn corrupt_monitor_blob_leaves_the_original_wallet_fully_intact() {
        let dir = tempfile::tempdir().unwrap();
        let (node_id, mnemonic) = create_local_wallet(dir.path());
        let cm_before = local_cm(dir.path()).unwrap();

        let monitor_key = format!("{}:0", "cd".repeat(32));
        let transport = Arc::new(MockTransport::new());
        transport.seed(CHANNEL_MANAGER_VSS_KEY, &[7u8; 64], 1);
        transport.seed(
            MONITOR_MANIFEST_KEY,
            &serde_json::to_vec(&vec![monitor_key.clone()]).unwrap(),
            1,
        );
        transport.seed(&monitor_key, b"not a channel monitor", 1);

        let node = Node::new(vss_config(dir.path(), &transport));
        let err = node.restore(crate::keys::tests::TEST_MNEMONIC).unwrap_err();
        assert!(
            matches!(err, RestoreError::ValidationFailed { .. }),
            "expected ValidationFailed, got {err:?}"
        );

        // Nothing was cleared or written: words, manager, and marker state
        // are exactly as before.
        assert_eq!(
            fs::read_to_string(dir.path().join(MNEMONIC_FILE_NAME)).unwrap(),
            mnemonic
        );
        assert_eq!(local_cm(dir.path()).unwrap(), cm_before);
        assert!(!dir.path().join(RESTORE_IN_PROGRESS_FILE_NAME).exists());
        assert!(kv_store(dir.path())
            .list(
                CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
                CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
            )
            .unwrap_or_default()
            .is_empty());

        // The pre-restore wallet still boots with its identity.
        let node = Node::new(offline_config(dir.path()));
        node.start().unwrap();
        assert_eq!(node.node_id().unwrap().to_string(), node_id);
        node.stop().unwrap();
    }

    // ---------- scenario 6: ordering + refused while running ----------

    /// CM is written strictly BEFORE any monitor, monitors before peers
    /// (write-log assertion), and a mid-write failure rolls back exactly
    /// what was written.
    #[test]
    fn write_plan_local_orders_manager_before_monitors_before_peers() {
        let dir = tempfile::tempdir().unwrap();
        let store = kv_store(dir.path());
        let plan = RestorePlan {
            manifest_version: Some(1),
            cm: Some((vec![1u8; 40], 3)),
            monitors: vec![
                ValidatedMonitor {
                    vss_key: format!("{}:0", "aa".repeat(32)),
                    local_key: format!("{}_0", "aa".repeat(32)),
                    bytes: vec![2u8; 16],
                    version: 4,
                },
                ValidatedMonitor {
                    vss_key: format!("{}:1", "bb".repeat(32)),
                    local_key: format!("{}_1", "bb".repeat(32)),
                    bytes: vec![3u8; 16],
                    version: 5,
                },
            ],
            peers: Some((parse_known_peers(PEERS_JSON.as_bytes()).unwrap(), 2)),
        };

        let mut log = Vec::new();
        write_plan_local(&store, &plan, &mut log, false).unwrap();
        assert_eq!(
            log,
            vec![
                LocalWrite::Manager,
                LocalWrite::Monitor(format!("{}_0", "aa".repeat(32))),
                LocalWrite::Monitor(format!("{}_1", "bb".repeat(32))),
                LocalWrite::Peers,
            ],
            "F3 ordering: CM before monitors, monitors before peers"
        );

        // Rollback removes exactly the logged writes.
        rollback_local_writes(&store, &log);
        assert!(local_cm(dir.path()).is_err());
        assert!(store
            .list(
                CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
                CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
            )
            .unwrap_or_default()
            .is_empty());
        assert!(read_local_known_peers(&store).is_empty());

        // The versions/monitor-keys the plan seeds match its contents.
        let versions = plan.versions();
        assert_eq!(versions.get(CHANNEL_MANAGER_VSS_KEY), Some(&3));
        assert_eq!(versions.get(MONITOR_MANIFEST_KEY), Some(&1));
        assert_eq!(versions.get(KNOWN_PEERS_VSS_KEY), Some(&2));
        assert_eq!(versions.len(), 5);
        assert_eq!(plan.monitor_keys().len(), 2);
    }

    /// Restore is refused while the node runs — typed error, nothing touched.
    #[test]
    fn restore_is_refused_while_the_node_is_running() {
        let dir = tempfile::tempdir().unwrap();
        let transport = Arc::new(MockTransport::new());
        let node = Node::new(vss_config(dir.path(), &transport));
        node.start().expect("fresh VSS-enabled start");
        assert_eq!(
            node.restore(crate::keys::tests::TEST_MNEMONIC).unwrap_err(),
            RestoreError::NodeRunning
        );
        assert!(!dir.path().join(RESTORE_IN_PROGRESS_FILE_NAME).exists());
        node.stop().unwrap();
    }

    // ---------- scenario 3: crash-prefix matrix ----------

    /// Runs a restore of the seeded backup over an existing different wallet
    /// and kills it at `crash`. Then asserts the invariant matrix: the marker
    /// survives the crash, the next start RESUMES recovery (never boots a
    /// partial set, never generates fresh words), and the node comes back
    /// with the backup's identity and words.
    fn crash_prefix_case(crash: CrashPoint) {
        let (transport, backup_node_id, backup_mnemonic) = seeded_backup();
        let dir = tempfile::tempdir().unwrap();
        let (_, old_mnemonic) = create_local_wallet(dir.path());

        let config = vss_config(dir.path(), &transport);
        let sink = LoggingEventSink::new();
        assert_eq!(
            run_restore(&config, &backup_mnemonic, &sink, Some(crash)).unwrap_err(),
            RestoreError::Interrupted
        );
        assert!(
            dir.path().join(RESTORE_IN_PROGRESS_FILE_NAME).exists(),
            "the marker must be durable before the crash point {crash:?}"
        );
        if crash == CrashPoint::Marker {
            // The clear never ran: the OLD words are still on disk; the
            // marker's context must still win on resume.
            assert_eq!(
                fs::read_to_string(dir.path().join(MNEMONIC_FILE_NAME)).unwrap(),
                old_mnemonic
            );
        }

        // Resume: a plain start completes the restore from the marker.
        let node = Node::new(config);
        node.start()
            .unwrap_or_else(|e| panic!("start must resume the restore after {crash:?}: {e}"));
        let node_id = node.node_id().unwrap().to_string();
        node.stop().unwrap();

        assert_eq!(
            node_id, backup_node_id,
            "resume after {crash:?} must yield the backup's identity"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join(MNEMONIC_FILE_NAME)).unwrap(),
            parse_mnemonic(&backup_mnemonic).unwrap().to_string(),
            "resume after {crash:?} must adopt the marker's words, never fresh ones"
        );
        assert!(
            !dir.path().join(RESTORE_IN_PROGRESS_FILE_NAME).exists(),
            "the marker clears once the resumed recovery is durable"
        );
    }

    #[test]
    fn crash_after_marker_write_resumes_the_restore() {
        crash_prefix_case(CrashPoint::Marker);
    }

    #[test]
    fn crash_after_clear_resumes_the_restore() {
        crash_prefix_case(CrashPoint::Clear);
    }

    #[test]
    fn crash_after_mnemonic_write_resumes_the_restore() {
        crash_prefix_case(CrashPoint::Mnemonic);
    }

    #[test]
    fn crash_after_manager_write_resumes_the_restore() {
        crash_prefix_case(CrashPoint::Manager);
    }

    /// The mid-monitors crash prefix, state-constructed: marker + target
    /// mnemonic + a PARTIAL local set (CM plus a half-written garbage
    /// monitor). The node must never boot against the partial set — startup
    /// voids it and re-runs recovery.
    #[test]
    fn crash_mid_monitors_never_boots_the_partial_set_and_resumes() {
        let (transport, backup_node_id, backup_mnemonic) = seeded_backup();
        let dir = tempfile::tempdir().unwrap();
        let normalized = parse_mnemonic(&backup_mnemonic).unwrap().to_string();
        write_marker(
            dir.path(),
            &RestoreMarker {
                mnemonic: normalized.clone(),
                started_at_ms: 1,
            },
        )
        .unwrap();
        fs::write(dir.path().join(MNEMONIC_FILE_NAME), &normalized).unwrap();
        let store = kv_store(dir.path());
        let (cm_bytes, _) = transport.value(CHANNEL_MANAGER_VSS_KEY).unwrap();
        store
            .write(
                CHANNEL_MANAGER_PERSISTENCE_PRIMARY_NAMESPACE,
                CHANNEL_MANAGER_PERSISTENCE_SECONDARY_NAMESPACE,
                CHANNEL_MANAGER_PERSISTENCE_KEY,
                cm_bytes,
            )
            .unwrap();
        store
            .write(
                CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
                CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
                &format!("{}_0", "ab".repeat(32)),
                b"half-written garbage monitor".to_vec(),
            )
            .unwrap();

        let node = Node::new(vss_config(dir.path(), &transport));
        node.start()
            .expect("startup must void the partial set and resume recovery");
        assert_eq!(node.node_id().unwrap().to_string(), backup_node_id);
        node.stop().unwrap();
        assert!(!dir.path().join(RESTORE_IN_PROGRESS_FILE_NAME).exists());
        assert!(
            store
                .list(
                    CHANNEL_MONITOR_PERSISTENCE_PRIMARY_NAMESPACE,
                    CHANNEL_MONITOR_PERSISTENCE_SECONDARY_NAMESPACE,
                )
                .unwrap_or_default()
                .is_empty(),
            "the garbage partial monitor must not survive the resumed recovery"
        );
    }

    // ---------- marker + misc units ----------

    #[test]
    fn marker_round_trips_and_tolerates_legacy_void_markers() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_marker(dir.path()).is_none(), "absent marker");

        // Legacy/void marker (U3 wrote empty markers in tests): no context.
        fs::write(dir.path().join(RESTORE_IN_PROGRESS_FILE_NAME), b"").unwrap();
        assert!(read_marker(dir.path()).is_none());

        let marker = RestoreMarker {
            mnemonic: crate::keys::tests::TEST_MNEMONIC.to_string(),
            started_at_ms: 42,
        };
        write_marker(dir.path(), &marker).unwrap();
        let read = read_marker(dir.path()).expect("marker context round-trips");
        assert_eq!(read.mnemonic, marker.mnemonic);
        assert_eq!(read.started_at_ms, 42);

        remove_marker(dir.path()).unwrap();
        assert!(!dir.path().join(RESTORE_IN_PROGRESS_FILE_NAME).exists());
        remove_marker(dir.path()).expect("idempotent removal");
    }

    #[test]
    fn invalid_words_and_disabled_vss_fail_with_distinct_typed_errors() {
        let dir = tempfile::tempdir().unwrap();
        let sink = LoggingEventSink::new();

        let transport = Arc::new(MockTransport::new());
        let config = vss_config(dir.path(), &transport);
        assert_eq!(
            run_restore(&config, "not a mnemonic", &sink, None).unwrap_err(),
            RestoreError::InvalidMnemonic
        );

        let disabled = offline_config(dir.path());
        assert_eq!(
            run_restore(&disabled, crate::keys::tests::TEST_MNEMONIC, &sink, None).unwrap_err(),
            RestoreError::VssDisabled
        );
        assert!(!dir.path().join(MNEMONIC_FILE_NAME).exists());
    }

    #[test]
    fn restore_error_variants_have_distinct_display() {
        let variants = [
            RestoreError::NodeRunning,
            RestoreError::InvalidMnemonic,
            RestoreError::VssDisabled,
            RestoreError::Setup { detail: "d".into() },
            RestoreError::NoBackupFound,
            RestoreError::BackupInconsistent { detail: "d".into() },
            RestoreError::DownloadFailed { detail: "d".into() },
            RestoreError::ValidationFailed { detail: "d".into() },
            RestoreError::LocalWriteFailed { detail: "d".into() },
            RestoreError::Interrupted,
        ];
        for (i, a) in variants.iter().enumerate() {
            for b in variants.iter().skip(i + 1) {
                assert_ne!(a.to_string(), b.to_string());
            }
        }
    }
}
