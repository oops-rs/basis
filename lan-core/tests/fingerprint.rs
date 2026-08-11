//! The fingerprint against real git.
//!
//! Its whole value rests on git behaving the way [`lan_core::fingerprint`] assumes —
//! that `ls-files --cached --others --exclude-standard` sees a new file,
//! ignores an ignored one, and that `HEAD` moves when a commit lands. Assuming
//! any of that would be exactly the kind of unverified convention `AGENTS.md`
//! forbids, so these tests run the real thing against real repositories.
//!
//! The unit tests beside the module cover the stat-only digest itself; what is
//! here is only what needs a repository to be true.

use std::{path::Path, process::Command};

use lan_core::{Fingerprint, Snapshot, fingerprint};

// ---------------------------------------------------------------- fixtures

fn known(workspace: &Path) -> Fingerprint {
    match fingerprint::snapshot(workspace) {
        Snapshot::Known(fingerprint) => fingerprint,
        Snapshot::Unknown { reason } => panic!("expected a fingerprint, got: {reason}"),
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        // Identity and hooks are supplied here so the test does not depend on
        // whatever the machine's global git config happens to say.
        .args(["-c", "user.email=test@example.invalid"])
        .args(["-c", "user.name=lan tests"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .status()
        .expect("git runs");

    assert!(status.success(), "git {args:?} failed");
}

/// A repository with one committed file.
fn repository() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "--quiet"]);
    std::fs::write(dir.path().join("tracked.txt"), "one").expect("write");
    git(dir.path(), &["add", "tracked.txt"]);
    git(dir.path(), &["commit", "--quiet", "-m", "first"]);
    dir
}

// ------------------------------------------------------------ the digest

#[test]
fn a_repository_fingerprints_at_all() {
    let repo = repository();

    assert_eq!(known(repo.path()), known(repo.path()));
}

#[test]
fn an_ignored_file_does_not_count_as_a_change() {
    // The reason to ask git rather than to walk: `target/` churning must not
    // make every observation look like work.
    let repo = repository();
    std::fs::write(repo.path().join(".gitignore"), "build/\n").expect("write");
    git(repo.path(), &["add", ".gitignore"]);
    git(repo.path(), &["commit", "--quiet", "-m", "ignore build"]);
    let before = known(repo.path());

    std::fs::create_dir(repo.path().join("build")).expect("mkdir");
    std::fs::write(repo.path().join("build/artifact.bin"), "noise").expect("write");

    assert_eq!(
        known(repo.path()),
        before,
        "an ignored file is not workspace content"
    );
}

#[test]
fn an_untracked_file_does_count_as_a_change() {
    // The mirror of the above, and the one that would silently break a
    // caller's loop: a brand new source file is exactly what it should wake up
    // for.
    let repo = repository();
    let before = known(repo.path());

    std::fs::write(repo.path().join("new.rs"), "fn main() {}").expect("write");

    assert_ne!(known(repo.path()), before);
}

#[test]
fn a_commit_alone_changes_the_fingerprint() {
    // `git commit` leaves the working tree's mtimes alone, so a scheme built
    // only on stat would call this unchanged. HEAD is in the digest for
    // exactly this case.
    let repo = repository();
    std::fs::write(repo.path().join("tracked.txt"), "two").expect("write");
    git(repo.path(), &["add", "tracked.txt"]);
    let before = known(repo.path());

    git(repo.path(), &["commit", "--quiet", "-m", "second"]);

    assert_ne!(known(repo.path()), before);
}

#[test]
fn a_tracked_file_deleted_changes_the_fingerprint() {
    let repo = repository();
    let before = known(repo.path());

    std::fs::remove_file(repo.path().join("tracked.txt")).expect("remove");

    assert_ne!(known(repo.path()), before);
}

#[test]
fn a_repository_and_a_plain_directory_are_told_apart() {
    // Not a nicety: if `git` were missing at runtime the walk would take over,
    // and a fingerprint from each scheme must not be able to collide.
    let repo = repository();
    let before = known(repo.path());

    std::fs::remove_dir_all(repo.path().join(".git")).expect("remove .git");

    assert_ne!(
        known(repo.path()),
        before,
        "losing the repository is a change in how the workspace is read"
    );
}
