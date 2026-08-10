//! Deciding whether the workspace has moved since the last successful run.
//!
//! # What "unchanged" has to mean
//!
//! A run is a function of three things: the prompt, the configuration, and the
//! workspace. The first two are fixed for the life of a watch, so if the
//! workspace is identical to what the last successful run saw, running again
//! asks the same question of the same material and pays for the same answer.
//! That — and only that — is what this module calls unchanged.
//!
//! # The invariant
//!
//! **A false "changed" costs tokens; a false "unchanged" silently stops the
//! feature working.** They are not symmetric, so every uncertain case here
//! resolves to changed: an unreadable directory, an enumeration that produced
//! nothing, a workspace that is not there. [`Snapshot::Unknown`] is the shape
//! that carries "I cannot claim unchanged", and the scheduler runs on it.
//!
//! # Why a fingerprint and not a hash of the contents
//!
//! Reading every byte of a repository on a timer is the thing a scheduler
//! exists to avoid. What is cheap is one `stat` per file, so the fingerprint
//! is a digest over `(path, length, mtime)` for every file the run could see,
//! plus git's `HEAD`. That is the trade every build system makes, and it
//! catches everything except an edit that preserves both length and modified
//! time — which requires deliberately forging a timestamp.
//!
//! Note that it is a *fingerprint*, compared for equality, never for order.
//! An mtime that moves backwards — a checkout of an older file, an rsync that
//! preserves times — is a different fingerprint, so it reads as changed. A
//! scheme that tracked the newest mtime instead would read it as unchanged.
//!
//! # Which files
//!
//! `git ls-files --cached --others --exclude-standard` when the workspace is
//! inside a work tree: it is one process, it honours `.gitignore` without lan
//! inventing an ignore convention of its own, and it keeps `.git`'s constant
//! internal churn — which would make every iteration look changed — out of the
//! answer. `HEAD` joins the digest so a commit, a merge, or a branch switch
//! registers even when it leaves no file's mtime behind.
//!
//! Otherwise a plain walk, skipping `.git` and following no symlinks.
//!
//! lan runs `git` here as itself, not on the agent's behalf: this is lan
//! reading the workspace to decide its own schedule, the same as reading
//! `AGENTS.md`. It is unrelated to the shell grant of ADR-0006, which governs
//! what the *agent* may execute.

use std::{
    collections::hash_map::DefaultHasher,
    fs::Metadata,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::UNIX_EPOCH,
};

/// Bumped when the fingerprint's inputs change, so a digest from an older lan
/// can never compare equal to one from a newer one.
const FINGERPRINT_VERSION: u32 = 1;

/// A cheap stand-in for everything in the workspace a run could see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint {
    digest: u64,
    files: usize,
}

impl Fingerprint {
    /// How many files went into the digest. Useful for telling "nothing
    /// changed" apart from "nothing was looked at".
    pub const fn files(self) -> usize {
        self.files
    }

    /// The digest, as it appears on the event stream.
    pub fn hex(self) -> String {
        format!("{:016x}", self.digest)
    }
}

/// What one look at the workspace produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Snapshot {
    /// A fingerprint two iterations can be compared by.
    Known(Fingerprint),
    /// The workspace could not be fingerprinted, so nothing may be concluded
    /// from it. The scheduler runs — see the invariant above.
    Unknown { reason: String },
}

impl Snapshot {
    /// The fingerprint, when there is one to keep as a baseline.
    pub const fn fingerprint(&self) -> Option<Fingerprint> {
        match self {
            Self::Known(fingerprint) => Some(*fingerprint),
            Self::Unknown { .. } => None,
        }
    }
}

/// Fingerprints `workspace` right now.
///
/// Blocking: it spawns `git` and stats files. The scheduler calls it on a
/// blocking thread.
pub fn snapshot(workspace: &Path) -> Snapshot {
    if !workspace.is_dir() {
        return Snapshot::Unknown {
            reason: format!("{} is not a directory", workspace.display()),
        };
    }

    let listing = git_listing(workspace).unwrap_or_else(|| walk_listing(workspace));

    fingerprint(workspace, listing)
}

/// The files that make up the workspace, and how they were found.
#[derive(Debug)]
enum Listing {
    /// git answered, so `.gitignore` is honoured and `.git` is excluded.
    /// `head` is empty on an unborn branch, which is a real state rather than
    /// a failure.
    Git { head: String, paths: Vec<PathBuf> },
    /// A plain walk, for a workspace that is not a repository.
    Walk { paths: Vec<PathBuf> },
}

impl Listing {
    /// Distinguishes the two schemes in the digest, so a workspace that gains
    /// or loses a `.git` reads as changed rather than as coincidence.
    const fn tag(&self) -> u8 {
        match self {
            Self::Git { .. } => 1,
            Self::Walk { .. } => 2,
        }
    }

    fn into_parts(self) -> (u8, String, Vec<PathBuf>) {
        let tag = self.tag();
        match self {
            Self::Git { head, paths } => (tag, head, paths),
            Self::Walk { paths } => (tag, String::new(), paths),
        }
    }
}

/// Digests a listing by stat'ing every path in it.
fn fingerprint(workspace: &Path, listing: Listing) -> Snapshot {
    let (tag, head, mut paths) = listing.into_parts();

    // A digest is only comparable if the order is, and `ls-files` prints
    // untracked files after tracked ones rather than merged into one sequence.
    paths.sort_unstable();
    paths.dedup();

    if paths.is_empty() {
        // An empty workspace and a broken enumeration look identical from
        // here, and only one of them is safe to treat as "unchanged".
        return Snapshot::Unknown {
            reason: format!("no files found under {}", workspace.display()),
        };
    }

    let mut hasher = DefaultHasher::new();
    FINGERPRINT_VERSION.hash(&mut hasher);
    tag.hash(&mut hasher);
    head.hash(&mut hasher);

    for path in &paths {
        path.hash(&mut hasher);

        match std::fs::symlink_metadata(workspace.join(path)) {
            // Not `metadata`: a symlink's target may sit outside the workspace,
            // and it is the link itself that belongs to the workspace.
            Ok(metadata) => {
                1u8.hash(&mut hasher);
                metadata.len().hash(&mut hasher);
                modified_nanos(&metadata).hash(&mut hasher);
            }
            // A path the listing has and the filesystem does not is a deletion,
            // which is exactly the change a newest-mtime scheme would miss.
            Err(_) => 0u8.hash(&mut hasher),
        }
    }

    Snapshot::Known(Fingerprint {
        digest: hasher.finish(),
        files: paths.len(),
    })
}

/// Modified time as signed nanoseconds around the epoch, or `None` when the
/// platform will not say. Signed because a file can be dated before 1970 and
/// two such files must still be told apart.
fn modified_nanos(metadata: &Metadata) -> Option<i128> {
    let modified = metadata.modified().ok()?;

    Some(match modified.duration_since(UNIX_EPOCH) {
        Ok(since) => since.as_nanos() as i128,
        Err(before) => -(before.duration().as_nanos() as i128),
    })
}

/// Asks git for the workspace's files, or `None` when git cannot answer —
/// not a repository, not installed, or refusing for any other reason.
fn git_listing(workspace: &Path) -> Option<Listing> {
    if git(workspace, &["rev-parse", "--is-inside-work-tree"])?.trim() != "true" {
        return None;
    }

    // A repository with no commit yet has no HEAD to resolve. That is a state,
    // not an error, so it contributes an empty component rather than aborting.
    let head = git(workspace, &["rev-parse", "HEAD"])
        .unwrap_or_default()
        .trim()
        .to_string();

    // One invocation for both halves: tracked files, and untracked files that
    // `.gitignore` does not exclude. Missing either would be a false
    // "unchanged" — a new file, or an edit to a committed one.
    let listed = git_bytes(
        workspace,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;

    let paths = listed
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .map(path_from_bytes)
        .collect();

    Some(Listing::Git { head, paths })
}

/// Runs git in `workspace`, returning stdout when it succeeded.
fn git(workspace: &Path, args: &[&str]) -> Option<String> {
    let bytes = git_bytes(workspace, args)?;
    String::from_utf8(bytes).ok()
}

fn git_bytes(workspace: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        // Nothing here is interactive, and a git that decides to ask a
        // question must fail rather than hang a scheduler on stdin.
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;

    output.status.success().then_some(output.stdout)
}

/// Walks `workspace` for a tree git cannot describe.
///
/// Nothing is filtered except `.git`, because any other exclusion list would
/// be lan inventing an opinion about which files matter. Symlinks are recorded
/// but never followed, which also means the walk cannot cycle.
fn walk_listing(workspace: &Path) -> Listing {
    let mut paths = Vec::new();
    let mut pending = vec![workspace.to_path_buf()];

    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            // Unreadable to lan means unreadable to the agent as well — same
            // process, same user. Record the directory so the digest stays
            // steady instead of flapping between iterations.
            if let Some(relative) = relative(workspace, &directory) {
                paths.push(relative);
            }
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == ".git") {
                continue;
            }

            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };

            if metadata.is_dir() {
                pending.push(path);
            } else if let Some(relative) = relative(workspace, &path) {
                paths.push(relative);
            }
        }
    }

    Listing::Walk { paths }
}

/// Paths go into the digest relative to the workspace, so moving a checkout
/// does not read as every file changing at once.
fn relative(workspace: &Path, path: &Path) -> Option<PathBuf> {
    path.strip_prefix(workspace).ok().map(Path::to_path_buf)
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;

    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes a file and stamps an explicit mtime.
    ///
    /// Explicit rather than "whatever the clock says", so a test never depends
    /// on two fast writes landing in different nanoseconds, nor on the
    /// filesystem's timestamp granularity.
    fn write_at(path: &Path, body: &str, epoch_seconds: u64) {
        std::fs::write(path, body).expect("write");
        touch(path, epoch_seconds);
    }

    fn touch(path: &Path, epoch_seconds: u64) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open")
            .set_modified(UNIX_EPOCH + std::time::Duration::from_secs(epoch_seconds))
            .expect("set mtime");
    }

    /// The first write of a file, at an arbitrary fixed time.
    fn write(path: &Path, body: &str) {
        write_at(path, body, 1_700_000_000);
    }

    fn known(workspace: &Path) -> Fingerprint {
        match snapshot(workspace) {
            Snapshot::Known(fingerprint) => fingerprint,
            Snapshot::Unknown { reason } => panic!("expected a fingerprint, got: {reason}"),
        }
    }

    #[test]
    fn a_missing_workspace_is_unknown_rather_than_empty() {
        // Failing open matters most here: a workspace that vanished for a
        // moment must not read as "nothing has changed, skip forever".
        assert!(matches!(
            snapshot(Path::new("/definitely/not/a/real/path")),
            Snapshot::Unknown { .. }
        ));
    }

    #[test]
    fn an_empty_workspace_is_unknown_rather_than_unchanged() {
        let dir = tempfile::tempdir().expect("tempdir");

        assert!(matches!(snapshot(dir.path()), Snapshot::Unknown { .. }));
    }

    #[test]
    fn the_same_tree_fingerprints_the_same_twice() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("a.txt"), "one");

        assert_eq!(known(dir.path()), known(dir.path()));
    }

    #[test]
    fn an_edit_changes_the_fingerprint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        write(&file, "one");
        let before = known(dir.path());

        write_at(&file, "two", 1_700_000_060);

        assert_ne!(known(dir.path()), before);
    }

    #[test]
    fn a_same_length_edit_changes_the_fingerprint() {
        // Length alone would miss this; the mtime is what catches it.
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        write(&file, "aaa");
        let before = known(dir.path());

        write_at(&file, "bbb", 1_700_000_060);

        assert_ne!(known(dir.path()), before);
    }

    #[test]
    fn a_same_mtime_edit_of_a_different_length_changes_the_fingerprint() {
        // The mirror of the previous case: mtime alone would miss this, and
        // an editor that restores timestamps makes it real.
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        write(&file, "aaa");
        let before = known(dir.path());

        write_at(&file, "aaaa", 1_700_000_000);

        assert_ne!(known(dir.path()), before);
    }

    #[test]
    fn an_mtime_moving_backwards_still_reads_as_changed() {
        // A newest-mtime scheme would call this unchanged and stop working.
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        write(&file, "one");
        let before = known(dir.path());

        touch(&file, 1);

        assert_ne!(known(dir.path()), before);
    }

    #[test]
    fn a_new_file_changes_the_fingerprint() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("a.txt"), "one");
        let before = known(dir.path());

        write(&dir.path().join("b.txt"), "two");

        assert_ne!(known(dir.path()), before);
    }

    #[test]
    fn a_deleted_file_changes_the_fingerprint() {
        // The case a "newest mtime" scheme cannot see at all.
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("a.txt"), "one");
        write(&dir.path().join("b.txt"), "two");
        let before = known(dir.path());

        std::fs::remove_file(dir.path().join("b.txt")).expect("remove");

        assert_ne!(known(dir.path()), before);
    }

    #[test]
    fn a_new_subdirectory_of_files_changes_the_fingerprint() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("a.txt"), "one");
        let before = known(dir.path());

        std::fs::create_dir(dir.path().join("nested")).expect("mkdir");
        write(&dir.path().join("nested/b.txt"), "two");

        assert_ne!(known(dir.path()), before);
    }

    #[test]
    fn the_walk_does_not_follow_symlinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("a.txt"), "one");

        // A self-referential link would hang or overflow a walk that followed
        // links; fingerprinting it must simply terminate.
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).expect("symlink");

        let _ = known(dir.path());
    }

    #[test]
    fn the_digest_is_reported_as_stable_hex() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join("a.txt"), "one");
        let fingerprint = known(dir.path());

        assert_eq!(fingerprint.hex().len(), 16);
        assert_eq!(fingerprint.hex(), known(dir.path()).hex());
        assert_eq!(fingerprint.files(), 1);
    }

    #[test]
    fn an_unknown_snapshot_keeps_no_baseline() {
        let unknown = Snapshot::Unknown {
            reason: "gone".to_string(),
        };

        assert_eq!(unknown.fingerprint(), None);
    }
}
