//! A watch's own wire contract, wrapped around [`run`](crate::run)'s.
//!
//! # Why a separate vocabulary
//!
//! A watch stream is a run stream with scheduling around it. Each iteration
//! that runs emits its own complete run stream — opened by
//! [`Event::RunStarted`], closed by [`Event::RunFinished`], exactly as `lan
//! run --json` produces — so anything that already reads a run stream reads
//! an iteration unchanged.
//!
//! The scheduler's own decisions cannot ride on that enum. `Event` describes
//! one run, and a run has no iteration, no interval, and nothing to skip;
//! putting scheduler state there would push the watch's opinions into every
//! consumer of `lan run --json`, which is Bet 4 leaking in miniature. So the
//! scheduler gets [`WatchEvent`], disjoint from `Event`, and the two are
//! interleaved on one stream.
//!
//! # Why the envelope carries an iteration marker
//!
//! Because it cannot be derived. Segmenting on `run_started` would work only
//! for iterations that ran, and the interesting ones are precisely those that
//! did not: a skip emits no run stream at all, and an iteration whose setup
//! failed emits no run stream either. Those are the lines an operator asking
//! "why has this not run since Tuesday?" needs. So run lines carry
//! `iteration`, scheduler lines carry it in their own payload, and `seq` stays
//! monotonic across the whole watch so a dropped line is still detectable end
//! to end.
//!
//! ```jsonl
//! {"seq":0,"type":"watch_started","schema":1,"every_ms":1800000,...}
//! {"seq":1,"type":"iteration_started","iteration":1,"reason":"first"}
//! {"seq":2,"iteration":1,"type":"run_started","schema":1,...}
//! {"seq":3,"iteration":1,"type":"run_finished","status":"ok"}
//! {"seq":4,"type":"iteration_finished","iteration":1,"status":"ok"}
//! {"seq":5,"type":"iteration_skipped","iteration":2,"fingerprint":"5b1e..."}
//! {"seq":6,"type":"watch_stopped","reason":"completed","ran":1,...}
//! ```

use std::{
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};

use crate::event::Event;

/// Version of the watch envelope. Independent of
/// [`EVENT_SCHEMA_VERSION`](crate::event::EVENT_SCHEMA_VERSION), which each
/// embedded run stream carries for itself on its own `run_started` line.
pub const WATCH_SCHEMA_VERSION: u32 = 1;

/// Why the scheduler decided to run this iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunReason {
    /// Nothing to compare against yet.
    First,
    /// The workspace differs from what the last successful run left behind.
    Changed,
    /// The workspace could not be fingerprinted, so "unchanged" cannot be
    /// claimed. Running is the only answer that cannot silently stop working.
    Unknown,
    /// Change detection is switched off.
    Always,
}

/// How one iteration ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum IterationOutcome {
    Ok,
    /// The turn failed. The iteration still produced a complete run stream —
    /// the same errors-versus-outcomes split `run` makes.
    Error {
        message: String,
    },
    /// The run could not be started at all: no credential, an unreachable
    /// model, a workspace that is not there. No run stream was produced for
    /// this iteration, which is why it is worth its own status.
    SetupFailed {
        message: String,
    },
}

impl IterationOutcome {
    pub const fn succeeded(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// Why the watch stopped. A watch has no natural end, so it always says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The stop signal was tripped.
    Interrupted,
    /// The configured number of iterations was reached.
    Completed,
}

/// Everything the scheduler itself puts on the stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WatchEvent {
    /// Always the first line. Carries the envelope's schema version.
    WatchStarted {
        schema: u32,
        lan: String,
        workspace: PathBuf,
        every_ms: u64,
        /// `false` when change detection is off: nothing is fingerprinted and
        /// every iteration runs.
        change_detection: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_iterations: Option<u64>,
    },

    IterationStarted {
        iteration: u64,
        reason: RunReason,
    },

    /// The workspace matches what the last successful run left behind, so
    /// nothing was asked of a model.
    IterationSkipped {
        iteration: u64,
        fingerprint: String,
    },

    IterationFinished {
        iteration: u64,
        #[serde(flatten)]
        outcome: IterationOutcome,
    },

    /// Always the last line.
    WatchStopped {
        reason: StopReason,
        /// Iterations attempted, which is `ran + skipped`.
        iterations: u64,
        /// Iterations that performed a run, successful or not.
        ran: u64,
        skipped: u64,
        /// Iterations that ran and did not succeed. Always a subset of `ran`.
        failed: u64,
    },
}

/// Where a watch's stream goes.
///
/// Two methods rather than one because the two vocabularies are genuinely
/// different: scheduler lines belong to the watch, run lines belong to an
/// iteration and carry its number.
///
/// Object-safe on purpose — the scheduler holds one sink behind a lock for the
/// whole watch, and hands each iteration a view of it.
pub trait WatchSink: Send + 'static {
    fn watch_event(&mut self, event: WatchEvent) -> std::io::Result<()>;

    fn run_event(&mut self, iteration: u64, event: Event) -> std::io::Result<()>;
}

/// The sink shared between the scheduler and the iteration currently running.
///
/// A lock rather than passing ownership around, because `lan::run` moves an
/// [`EventSink`](crate::run::EventSink) into a task and consumes it, while a
/// watch outlives every iteration and must keep writing to one stream — and
/// because a run that fails on the way in would otherwise take the sink with
/// it. Contention is nil: iterations never overlap.
pub(crate) type SharedSink = Arc<Mutex<dyn WatchSink>>;

/// Locks the shared sink.
///
/// A poisoned lock means some earlier emission panicked. The stream is still
/// the caller's and the watch is still running, so recovery is to keep
/// writing rather than to take the whole loop down.
pub(crate) fn lock(sink: &SharedSink) -> std::sync::MutexGuard<'_, dyn WatchSink> {
    sink.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Writes a watch stream as JSONL on one output.
///
/// Sequence numbers are assigned across the whole watch, not per iteration, so
/// a consumer can tell a dropped line from an iteration boundary. Each line is
/// flushed as it is written, for the same reason
/// [`JsonlWriter`](crate::event::JsonlWriter) does it: a delta should reach a
/// reader when it happens, not when a buffer fills.
#[derive(Debug)]
pub struct WatchJsonlWriter<W: Write> {
    writer: W,
    next_seq: u64,
}

impl<W: Write> WatchJsonlWriter<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            next_seq: 0,
        }
    }

    /// The sequence number the next line will use.
    pub const fn next_seq(&self) -> u64 {
        self.next_seq
    }

    pub fn into_inner(self) -> W {
        self.writer
    }

    fn write_line<L: Serialize>(&mut self, line: &L) -> std::io::Result<()> {
        let encoded = serde_json::to_string(line).map_err(std::io::Error::other)?;

        writeln!(self.writer, "{encoded}")?;
        self.writer.flush()?;

        self.next_seq += 1;
        Ok(())
    }
}

/// A scheduler line: the envelope plus the event, flattened.
#[derive(Serialize)]
struct WatchLine<'a> {
    seq: u64,
    #[serde(flatten)]
    event: &'a WatchEvent,
}

/// A run line: the same shape `lan run --json` writes, plus the iteration it
/// belongs to.
#[derive(Serialize)]
struct RunLine<'a> {
    seq: u64,
    iteration: u64,
    #[serde(flatten)]
    event: &'a Event,
}

impl<W: Write + Send + 'static> WatchSink for WatchJsonlWriter<W> {
    fn watch_event(&mut self, event: WatchEvent) -> std::io::Result<()> {
        let seq = self.next_seq;
        self.write_line(&WatchLine { seq, event: &event })
    }

    fn run_event(&mut self, iteration: u64, event: Event) -> std::io::Result<()> {
        let seq = self.next_seq;
        self.write_line(&RunLine {
            seq,
            iteration,
            event: &event,
        })
    }
}

/// One line of a watch stream, kept in memory.
#[derive(Debug, Clone, PartialEq)]
pub enum WatchRecord {
    Watch(WatchEvent),
    Run { iteration: u64, event: Event },
}

/// Keeps every line in memory, shared with whoever else holds a clone.
///
/// Cloneable and shared because a watch never hands its sink back — there is
/// no moment at which the loop is finished with it — so observing what was
/// written means holding a second view of the same buffer.
#[derive(Debug, Clone, Default)]
pub struct CollectingWatchSink {
    records: Arc<Mutex<Vec<WatchRecord>>>,
}

impl CollectingWatchSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything written so far, oldest first.
    pub fn records(&self) -> Vec<WatchRecord> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Just the scheduler's own lines.
    pub fn watch_events(&self) -> Vec<WatchEvent> {
        self.records()
            .into_iter()
            .filter_map(|record| match record {
                WatchRecord::Watch(event) => Some(event),
                WatchRecord::Run { .. } => None,
            })
            .collect()
    }

    /// The run events belonging to `iteration`, in order.
    pub fn run_events(&self, iteration: u64) -> Vec<Event> {
        self.records()
            .into_iter()
            .filter_map(|record| match record {
                WatchRecord::Run {
                    iteration: which,
                    event,
                } if which == iteration => Some(event),
                _ => None,
            })
            .collect()
    }

    fn push(&self, record: WatchRecord) {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(record);
    }
}

impl WatchSink for CollectingWatchSink {
    fn watch_event(&mut self, event: WatchEvent) -> std::io::Result<()> {
        self.push(WatchRecord::Watch(event));
        Ok(())
    }

    fn run_event(&mut self, iteration: u64, event: Event) -> std::io::Result<()> {
        self.push(WatchRecord::Run { iteration, event });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::RunOutcome;

    fn lines(buffer: &[u8]) -> Vec<serde_json::Value> {
        String::from_utf8(buffer.to_vec())
            .expect("utf-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is json"))
            .collect()
    }

    fn started() -> WatchEvent {
        WatchEvent::WatchStarted {
            schema: WATCH_SCHEMA_VERSION,
            lan: "0.1.0".to_string(),
            workspace: PathBuf::from("/repo"),
            every_ms: 1_800_000,
            change_detection: true,
            max_iterations: None,
        }
    }

    #[test]
    fn the_first_line_carries_the_envelope_version() {
        let mut sink = WatchJsonlWriter::new(Vec::new());
        sink.watch_event(started()).expect("writes");

        let written = lines(&sink.into_inner());
        assert_eq!(written[0]["type"], "watch_started");
        assert_eq!(written[0]["schema"], WATCH_SCHEMA_VERSION);
        assert_eq!(written[0]["every_ms"], 1_800_000);
        assert!(
            written[0].get("max_iterations").is_none(),
            "an unbounded watch must not mention a bound"
        );
    }

    #[test]
    fn run_lines_carry_the_iteration_and_scheduler_lines_do_not_need_to() {
        let mut sink = WatchJsonlWriter::new(Vec::new());
        sink.watch_event(WatchEvent::IterationStarted {
            iteration: 2,
            reason: RunReason::Changed,
        })
        .expect("writes");
        sink.run_event(
            2,
            Event::AssistantDelta {
                text: "hi".to_string(),
            },
        )
        .expect("writes");

        let written = lines(&sink.into_inner());
        assert_eq!(written[0]["type"], "iteration_started");
        assert_eq!(written[0]["iteration"], 2);
        assert_eq!(written[0]["reason"], "changed");

        assert_eq!(written[1]["type"], "assistant_delta");
        assert_eq!(written[1]["iteration"], 2);
        assert_eq!(written[1]["text"], "hi");
    }

    #[test]
    fn sequence_numbers_run_across_the_whole_watch() {
        let mut sink = WatchJsonlWriter::new(Vec::new());
        sink.watch_event(started()).expect("writes");
        sink.run_event(
            1,
            Event::RunFinished {
                outcome: RunOutcome::Ok,
            },
        )
        .expect("writes");
        sink.watch_event(WatchEvent::IterationFinished {
            iteration: 1,
            outcome: IterationOutcome::Ok,
        })
        .expect("writes");

        let written = lines(&sink.into_inner());
        let seqs: Vec<u64> = written
            .iter()
            .map(|line| line["seq"].as_u64().expect("a sequence number"))
            .collect();

        assert_eq!(seqs, vec![0, 1, 2], "one counter, not one per iteration");
    }

    #[test]
    fn an_iteration_outcome_flattens_into_its_line() {
        let mut sink = WatchJsonlWriter::new(Vec::new());
        sink.watch_event(WatchEvent::IterationFinished {
            iteration: 3,
            outcome: IterationOutcome::SetupFailed {
                message: "no credential".to_string(),
            },
        })
        .expect("writes");

        let written = lines(&sink.into_inner());
        assert_eq!(written[0]["status"], "setup_failed");
        assert_eq!(written[0]["message"], "no credential");
    }

    #[test]
    fn a_skip_says_what_it_compared() {
        let mut sink = WatchJsonlWriter::new(Vec::new());
        sink.watch_event(WatchEvent::IterationSkipped {
            iteration: 4,
            fingerprint: "00ff".to_string(),
        })
        .expect("writes");

        let written = lines(&sink.into_inner());
        assert_eq!(written[0]["type"], "iteration_skipped");
        assert_eq!(written[0]["fingerprint"], "00ff");
    }

    #[test]
    fn watch_events_round_trip() {
        let event = WatchEvent::WatchStopped {
            reason: StopReason::Interrupted,
            iterations: 5,
            ran: 3,
            skipped: 2,
            failed: 1,
        };
        let text = serde_json::to_string(&event).expect("serializes");

        assert_eq!(
            serde_json::from_str::<WatchEvent>(&text).expect("deserializes"),
            event
        );
    }

    #[test]
    fn a_collecting_sink_separates_the_two_vocabularies() {
        let sink = CollectingWatchSink::new();
        let mut writing = sink.clone();

        writing.watch_event(started()).expect("collects");
        writing
            .run_event(
                1,
                Event::AssistantDelta {
                    text: "a".to_string(),
                },
            )
            .expect("collects");
        writing
            .run_event(
                2,
                Event::AssistantDelta {
                    text: "b".to_string(),
                },
            )
            .expect("collects");

        // The clone observes what the sink handed to the loop wrote.
        assert_eq!(sink.watch_events(), vec![started()]);
        assert_eq!(
            sink.run_events(2),
            vec![Event::AssistantDelta {
                text: "b".to_string()
            }]
        );
        assert_eq!(sink.records().len(), 3);
    }

    #[test]
    fn only_ok_counts_as_success() {
        assert!(IterationOutcome::Ok.succeeded());
        assert!(
            !IterationOutcome::Error {
                message: "boom".to_string()
            }
            .succeeded()
        );
        assert!(
            !IterationOutcome::SetupFailed {
                message: "boom".to_string()
            }
            .succeeded()
        );
    }
}
