//! `lan fingerprint`: the workspace's hash, for a caller's own loop.
//!
//! Small, and a module anyway, because what is worth guarding here is a
//! failure mode rather than a computation. The point of the command is two
//! lines of somebody else's script — keep the last hash, compare, skip the
//! model when they match — and the way that script breaks is an empty string
//! compared against a previous empty string, forever reading as "unchanged".
//! So the split between "there is a hash" and "there is a reason there isn't"
//! is the whole design, and the test that holds it belongs next to the
//! function that decides it.

use std::{path::Path, process::ExitCode};

use lan_core::Snapshot;

use crate::{cli::FingerprintArgs, exit::EXIT_OK};

/// Prints the workspace fingerprint, or says why there is none.
///
/// A workspace that cannot be fingerprinted prints *nothing* on stdout and
/// exits nonzero, so a caller comparing hashes sees the gap instead of a false
/// match.
pub(crate) fn execute_fingerprint(args: FingerprintArgs) -> Result<ExitCode, String> {
    let workspace = match args.workspace {
        Some(path) => path,
        None => {
            std::env::current_dir().map_err(|error| format!("no working directory: {error}"))?
        }
    };

    println!("{}", fingerprint_line(&workspace)?);

    Ok(ExitCode::from(EXIT_OK))
}

/// The one line stdout gets, or the reason stdout gets nothing.
fn fingerprint_line(workspace: &Path) -> Result<String, String> {
    match lan_core::fingerprint::snapshot(workspace) {
        Snapshot::Known(fingerprint) => Ok(fingerprint.hex()),
        Snapshot::Unknown { reason } => Err(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workspace_that_cannot_be_fingerprinted_prints_nothing() {
        // The failure this guards against is a shell loop comparing one empty
        // string against another and concluding "unchanged" forever. There is
        // no `Ok` here to print, so nothing reaches stdout.
        let reason = fingerprint_line(Path::new("/definitely/not/a/real/path"))
            .expect_err("an absent workspace has no fingerprint");

        assert!(
            reason.contains("/definitely/not/a/real/path"),
            "the reason must name the workspace: {reason}"
        );
    }

    #[test]
    fn a_fingerprintable_workspace_prints_one_stable_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "one").expect("write");

        let printed = fingerprint_line(dir.path()).expect("a workspace with a file in it");

        assert_eq!(printed.len(), 16);
        assert_eq!(
            printed,
            fingerprint_line(dir.path()).expect("still fingerprints"),
            "a workspace nobody touched must print the same line twice"
        );
    }
}
