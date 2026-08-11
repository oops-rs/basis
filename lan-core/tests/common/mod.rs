//! What every suite here needs and none of them is about.

use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

/// A directory for one test's persisted conversations.
///
/// mentra picks its default store by the **process's** current directory
/// rather than by the workspace lan opened, so a suite that says nothing
/// writes to the real database under the user's data directory — every test
/// binary in one `cargo test`, into one file (`docs/REDESIGN.md`, footnote 6).
/// Saying where is the fix, and
/// [`WorkspaceBuilder::with_store_dir`](lan_core::WorkspaceBuilder::with_store_dir)
/// is how; a suite that builds a mentra `Runtime` itself passes the same path
/// to `with_store`.
///
/// Under the system temp directory rather than inside the workspace, because a
/// workspace that is not a git repository fingerprints by walking its own
/// tree, and a live database is not something a test meant to put there. Unique
/// per call, so tests running at once never queue behind each other for one
/// SQLite file.
pub fn scratch_store() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    std::env::temp_dir().join(format!(
        "lan-store-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}
