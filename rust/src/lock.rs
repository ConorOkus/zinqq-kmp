//! Single-instance guard over the node's storage directory.
//!
//! `Node`'s own `AlreadyRunning` check only covers one `Node` value: a second
//! `Node` (or a second `Wallet` over the FFI, or a second OS process) pointed at
//! the same `storage_dir` has its own state and would otherwise start happily.
//! Two live nodes on one seed means two `ChannelManager`s writing the same
//! monitors and manager with last-writer-wins, which is the channel-state
//! divergence the plan's fresh-wallet decision exists to avoid — force closes
//! and, with a stale monitor, penalty transactions.
//!
//! The guard is an OS advisory lock held on `<storage_dir>/node.lock` for as
//! long as the node runs. The lock lives in the kernel against the open file
//! description, so it is released when the file closes — including when the
//! process dies. That makes it self-healing: unlike a pid file, a crash leaves
//! no stale lock to clear by hand.

use std::fs::{File, OpenOptions};
use std::path::Path;

use fs4::fs_std::FileExt;

use crate::builder::BuildError;

/// The lock file's name inside the storage directory.
pub(crate) const LOCK_FILE_NAME: &str = "node.lock";

/// Holds the storage directory's exclusive lock. Dropping it releases the lock,
/// so it is kept alive by the node's running state and dropped on `stop()`.
#[derive(Debug)]
pub(crate) struct DataDirLock {
    // Held for its `Drop`; the lock is on the open file, not the path.
    _file: File,
}

impl DataDirLock {
    /// Takes the exclusive lock, or returns [`BuildError::InstanceAlreadyRunning`]
    /// when another node already holds it.
    pub(crate) fn acquire(storage_dir: &Path) -> Result<Self, BuildError> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(storage_dir.join(LOCK_FILE_NAME))
            .map_err(|_| BuildError::WriteFailed)?;

        // Non-blocking: a held lock must fail fast with a typed error rather
        // than park a start() call until the other node exits.
        match FileExt::try_lock_exclusive(&file) {
            Ok(true) => Ok(Self { _file: file }),
            Ok(false) => Err(BuildError::InstanceAlreadyRunning),
            Err(_) => Err(BuildError::InstanceAlreadyRunning),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_over_the_same_dir_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let first = DataDirLock::acquire(dir.path()).expect("first lock must succeed");

        let second = DataDirLock::acquire(dir.path());
        assert!(
            matches!(second, Err(BuildError::InstanceAlreadyRunning)),
            "a second node over the same storage dir must be rejected, got {second:?}"
        );

        // Releasing lets the next instance in, so a normal stop/start cycle and
        // a post-crash restart both work without manual cleanup.
        drop(first);
        DataDirLock::acquire(dir.path()).expect("lock must be reacquirable after release");
    }

    #[test]
    fn distinct_dirs_do_not_contend() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let _lock_a = DataDirLock::acquire(a.path()).expect("dir a");
        let _lock_b = DataDirLock::acquire(b.path()).expect("dir b");
    }

    #[test]
    fn lock_file_lives_inside_the_storage_dir() {
        let dir = tempfile::tempdir().unwrap();
        let _lock = DataDirLock::acquire(dir.path()).unwrap();
        assert!(
            dir.path().join(LOCK_FILE_NAME).exists(),
            "the lock file must sit in the storage dir so it is covered by the \
             same backup-exclusion rules as the seed and monitors (R6)"
        );
    }
}
