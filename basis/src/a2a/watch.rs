//! Watching a workspace's A2A files, by polling (ADR-0031 §4).
//!
//! A poll of each source's path — does it exist, how long is it, when was it
//! last modified — rather than a filesystem notifier: it needs no dependency,
//! it is the same on every platform, and it notices a file that did not exist
//! at open, which a notifier watching that file could not. The cost is a few
//! `stat` calls per interval.
//!
//! What a poll compares against is what the last read *saw*, stamped just
//! before that read, not what the previous poll saw. So a file edited between
//! the open and the watch starting is still noticed, a host's own
//! [`reload_a2a`](crate::Workspace::reload_a2a) moves the baseline so the
//! watch does not repeat it, and a file that failed to parse is not read again
//! every interval — only once it changes again.

use std::{
    path::{Path, PathBuf},
    sync::Weak,
    time::{Duration, SystemTime},
};

use super::live::LiveA2a;

/// How often a long-running host polls its A2A files by default — what
/// `basis-acp` and the CLI's task driver pass to
/// [`Workspace::watch_a2a`](crate::Workspace::watch_a2a).
///
/// Short enough that an operator who saves the file sees the change before
/// they have switched windows; long enough that a handful of `stat` calls is
/// all it costs.
pub const DEFAULT_A2A_WATCH_INTERVAL: Duration = Duration::from_secs(2);

/// The shortest interval a watch polls at. A zero interval would spin.
const MIN_INTERVAL: Duration = Duration::from_millis(10);

/// A running watch of one workspace's A2A files. Dropping it stops the poll.
#[must_use = "the watch stops as soon as this handle is dropped"]
pub struct A2aWatch {
    poll: tokio::task::JoinHandle<()>,
}

impl Drop for A2aWatch {
    fn drop(&mut self) {
        self.poll.abort();
    }
}

impl std::fmt::Debug for A2aWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("A2aWatch")
            .field("running", &!self.poll.is_finished())
            .finish()
    }
}

/// What one path looked like at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stamp {
    /// Nothing there, or nothing this process may look at.
    Missing,
    Present {
        len: u64,
        modified: Option<SystemTime>,
    },
}

/// Every path's stamp, in order.
pub(crate) fn stamps(paths: &[PathBuf]) -> Vec<Stamp> {
    paths.iter().map(|path| stamp(path)).collect()
}

fn stamp(path: &Path) -> Stamp {
    match std::fs::metadata(path) {
        Ok(metadata) => Stamp::Present {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        },
        Err(_) => Stamp::Missing,
    }
}

/// Starts polling `live`'s sources every `interval`.
///
/// Holds the state weakly, so the watch never keeps a workspace's A2A state
/// alive; the poll ends on its own once nothing else holds it.
pub(crate) fn start(live: Weak<LiveA2a>, paths: Vec<PathBuf>, interval: Duration) -> A2aWatch {
    let poll = tokio::spawn(async move {
        let mut ticks = tokio::time::interval(interval.max(MIN_INTERVAL));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            let Some(live) = live.upgrade() else { break };
            if stamps(&paths) == live.seen() {
                continue;
            }
            // Reported, not fatal: a reload that fails changes nothing, and
            // the operator who saved a half-typed file is about to save it
            // again. The error names a file and a field, never a value.
            if let Err(error) = live.reload().await {
                eprintln!(
                    "Warning: A2A configuration changed but was not applied; the agents \
                     configured before stay: {error}"
                );
            }
        }
    });
    A2aWatch { poll }
}

#[cfg(test)]
mod tests;
