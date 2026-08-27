//! Advisory file locks: the single-writer authority behind attach and inbox.
//!
//! The `fs2` lock itself is the sole liveness and mutual-exclusion authority.
//! Advisory locks are released by the kernel when the holder's handle closes
//! at process death — including SIGKILL — so a lock is never "stale held",
//! only held-by-a-live-process or free; there is nothing to break. The
//! PID fingerprint written into the file is diagnostic only: it makes `basis
//! watch`/errors honest about who is attached and makes post-mortems possible,
//! and no code path ever decides anything from it, which is what keeps PID
//! reuse harmless.

use std::{
    fs::{File, OpenOptions},
    io::{self, Seek, Write},
    path::Path,
};

use fs2::{FileExt, lock_contended_error};

use super::data_dir::restrict_file;

/// An exclusively held lock file. Dropping it releases the lock; the kernel
/// releases it anyway when the process dies.
pub(crate) struct Lock {
    file: File,
}

impl Lock {
    /// Records who holds the lock: truncate-and-write, never a recreate — the
    /// lock lives on the open handle, and replacing the file would orphan it.
    pub(crate) fn write_fingerprint(&mut self) {
        let _ = self.file.set_len(0);
        let _ = self.file.rewind();
        let _ = writeln!(self.file, "{}{}", std::process::id(), start_time_suffix());
        let _ = self.file.flush();
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Tries the lock once. `None` means a live process holds it.
pub(crate) fn try_exclusive(path: &Path) -> io::Result<Option<Lock>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    restrict_file(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(Lock { file })),
        Err(error) if is_lock_contended(&error) => Ok(None),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("{}: {error}", path.display()),
        )),
    }
}

/// Blocks until the lock is held. Used only for the inbox lock, which every
/// holder takes for one bounded rewrite — never across a model turn.
pub(crate) fn exclusive(path: &Path) -> io::Result<Lock> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    restrict_file(path)?;
    file.lock_exclusive()
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))?;
    Ok(Lock { file })
}

/// Whether a live executor currently holds `path`, without disturbing it.
pub(crate) fn is_held(path: &Path) -> bool {
    matches!(try_exclusive(path), Ok(None))
}

pub(crate) fn is_lock_contended(error: &io::Error) -> bool {
    let expected = lock_contended_error();
    match (error.raw_os_error(), expected.raw_os_error()) {
        // The OS code is the discriminant on Windows (ERROR_LOCK_VIOLATION)
        // and Unix (EWOULDBLOCK/EAGAIN). Prefer it whenever both are present
        // so an unrelated `Uncategorized` error cannot look like contention.
        (Some(actual), Some(expected)) => actual == expected,
        // Keep the kind-only fallback for targets where fs2 cannot expose an
        // OS code. `lock_contended_error` is the crate's contract in that case.
        _ => error.kind() == expected.kind(),
    }
}

/// Best-effort process start time, for the fingerprint's diagnostic value
/// only. Field 22 of `/proc/self/stat` on Linux; omitted elsewhere because
/// nothing depends on it.
#[cfg(target_os = "linux")]
fn start_time_suffix() -> String {
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|stat| {
            // The command name (field 2) may contain anything, including
            // spaces; fields count from after its closing parenthesis.
            let (_, after) = stat.rsplit_once(')')?;
            // `starttime` is field 22 overall, so field 20 of the remainder.
            let start = after.split_whitespace().nth(19)?;
            Some(format!(" {start}"))
        })
        .unwrap_or_default()
}

#[cfg(not(target_os = "linux"))]
fn start_time_suffix() -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_contention_error_is_recognized_across_platforms() {
        assert!(is_lock_contended(&lock_contended_error()));
    }

    #[test]
    fn a_held_lock_is_contended_and_a_dropped_one_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attach.lock");

        let held = try_exclusive(&path).unwrap().expect("first holder");
        assert!(is_held(&path));
        drop(held);
        assert!(!is_held(&path));
        assert!(try_exclusive(&path).unwrap().is_some());
    }

    #[test]
    fn the_fingerprint_is_written_without_recreating_the_lock_file() {
        use std::io::Read as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attach.lock");
        let mut held = try_exclusive(&path).unwrap().expect("holder");
        held.write_fingerprint();

        held.file.rewind().unwrap();
        let mut written = String::new();
        held.file.read_to_string(&mut written).unwrap();
        assert!(
            written.starts_with(&std::process::id().to_string()),
            "{written}"
        );
        // Still held by the same handle after the write.
        assert!(is_held(&path));
    }
}
