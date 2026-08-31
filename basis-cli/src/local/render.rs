//! How lifecycle payloads and events reach a person or a script.
//!
//! The JSON shapes and the exit-code mapping are contract (ADR-0015/0017);
//! `--json` prints payloads verbatim and the prose is derived from the same
//! fields, so the code a script reads cannot depend on the renderer.
//!
//! One event renderer serves both processes that can be showing a run: the
//! executor holding the attach lock, and a `basis watch` tailing the journal
//! that executor writes. They differ in where an event comes from — a typed
//! [`Event`] serialized on its way to `events.jsonl`, or a
//! record read back out of it — and in nothing a person should see, so they
//! meet at [`Live`] on the JSON shape both already have.

use std::{
    io::{self, Write},
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use basis::{Event, RunUsage, event::tool_result_elision_line};
use basis_tasks::{Terminal, TerminalRecord};
use serde::Deserialize;
use serde_json::Value;

use crate::exit::{EXIT_BOUNDED, EXIT_FAILED, EXIT_OK};

use super::error::ClientError;

/// How much of a tool's own words a progress line is worth. Enough to name a
/// failure, short enough to stay one line.
const SUMMARY_BUDGET: usize = 120;

/// The terminal a run is being shown on while it runs.
///
/// Whether anything is shown is a property of the *caller*, not of the run: a
/// shell blocked on this process wants to watch, and `--json` wants the
/// settled object it asked for and nothing in front of it. So the choice is
/// made once where the command is understood and carried down to the sink,
/// which then feeds the journal and the terminal from the same event.
///
/// [`answered`](Self::answered) is the one fact that has to come back up:
/// text that already arrived a delta at a time must not be printed a second
/// time under the settled record.
#[derive(Clone)]
pub(crate) struct Live {
    shown: bool,
    answered: Arc<AtomicBool>,
}

impl Live {
    /// Shown when the caller is a person's terminal rather than a parser.
    pub(crate) fn when(shown: bool) -> Self {
        Self {
            shown,
            answered: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Renders one event on this process's streams.
    ///
    /// Errors are the caller's to ignore, the way journal errors are: a
    /// closed stdout (`basis "…" | head -1`) means nobody is reading, not
    /// that the work should stop — it is durable either way.
    pub(crate) fn show(&self, event: &Value) -> io::Result<()> {
        if !self.shown {
            // Answered here rather than left to `show_to`, so a run nobody is
            // watching does not lock two streams per event to write nothing.
            return Ok(());
        }
        self.show_to(event, &mut io::stdout().lock(), &mut io::stderr().lock())
    }

    fn show_to(&self, event: &Value, out: &mut impl Write, err: &mut impl Write) -> io::Result<()> {
        if !self.shown {
            return Ok(());
        }
        if write_event(event, self.answered(), out, err)? {
            self.answered.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Whether any of the assistant's answer has already reached the terminal.
    pub(crate) fn answered(&self) -> bool {
        self.answered.load(Ordering::Relaxed)
    }

    /// The settled record, for a process that was showing the run as it ran.
    ///
    /// A `succeeded` record's `result` is the text the deltas already spelled
    /// out, so printing it again would say the answer twice. Everything else
    /// still prints: a failure, a cancellation, and the hint were never on the
    /// stream, and neither was a result this process only read off disk.
    pub(crate) fn settled(
        &self,
        record: &TerminalRecord,
        structured: bool,
    ) -> Result<ExitCode, ClientError> {
        if !self.repeats(record, structured) {
            return render_terminal(record, structured);
        }
        print_hint(&record.raw);
        flush_stdout()?;
        Ok(ExitCode::from(terminal_result_code(record)))
    }

    /// Whether rendering `record` would say the answer a second time.
    fn repeats(&self, record: &TerminalRecord, structured: bool) -> bool {
        !structured
            && self.answered()
            && matches!(record.terminal.as_ref(), Some(Terminal::Succeeded { .. }))
    }
}

/// The seam `basis_tasks::Tasks::wait` shows progress through while this
/// process is the one driving a task: `basis-tasks` has no terminal of its
/// own (ADR-0011), so it is handed this rather than deciding how to render.
impl basis_tasks::LiveSink for Live {
    fn on_event(&self, event: &Value) {
        let _ = self.show(event);
    }
}

/// Writes one event the way a terminal wants it: the assistant's answer on
/// `out`, everything about producing it on `err`.
///
/// The split is the whole rule, and it is what makes the shorthand composable
/// — `basis "summarize this" > notes.md` has to leave a file holding the
/// summary, not a transcript of the run that wrote it. Progress is for
/// whoever is waiting, so it goes where waiting is watched.
///
/// `answered` is whether the answer has already begun; the finish line closes
/// a streamed answer rather than opening a blank one under a run that never
/// spoke. Returns whether this event put answer text on `out`.
fn write_event(
    event: &Value,
    answered: bool,
    out: &mut impl Write,
    err: &mut impl Write,
) -> io::Result<bool> {
    let Some(event) = typed(event, err)? else {
        return Ok(false);
    };

    match event {
        Event::AssistantDelta { text } => {
            write!(out, "{text}")?;
            out.flush()?;
            return Ok(!text.is_empty());
        }
        // Two things at the finish line, on the two streams they belong to:
        // the newline that closes a streamed answer, and what the run spent.
        Event::RunFinished { usage, .. } => {
            if answered {
                writeln!(out)?;
            }
            if let Some(spent) = usage.and_then(spent) {
                writeln!(err, "basis: {spent}")?;
            }
        }
        event => {
            if let Some(line) = progress_line(&event) {
                writeln!(err, "{line}")?;
            }
        }
    }
    Ok(false)
}

/// The typed event, or `None` after saying why on the progress stream.
///
/// Typed, so a new `Event` variant is a compile-time question in
/// [`progress_line`] rather than a string that quietly stops matching — and
/// deserialized from the borrowed `Value` directly, because this runs once
/// per streamed delta and a clone per token chunk would be pure overhead. An
/// event this build cannot name — a journal written by a newer basis — is
/// said instead of rendered as nothing.
fn typed(event: &Value, err: &mut impl Write) -> io::Result<Option<Event>> {
    match Event::deserialize(event) {
        Ok(event) => Ok(Some(event)),
        Err(_) => {
            writeln!(
                err,
                "basis: unrecognized event `{}`",
                label(text(event, "type"), "untyped")
            )?;
            Ok(None)
        }
    }
}

/// One progress line for stderr, or `None` for the events deliberately
/// silent at a terminal — reasoning deltas, per-round usage, permission
/// bookkeeping — and, with the enum `#[non_exhaustive]`, the arm a future
/// variant lands in; [`Event::type_tag`] is the exhaustive match that forces
/// this list to be revisited when one does.
fn progress_line(event: &Event) -> Option<String> {
    Some(match event {
        Event::RunStarted {
            model,
            context_files,
            ..
        } => format!(
            "basis: {}, {} context file(s)",
            label(model, "unknown model"),
            context_files.len()
        ),
        // The queue event, not the start: it is the one that carries what the
        // call is *for*, and a person reading progress wants the command, not
        // the second announcement of the same call.
        Event::ToolQueued {
            tool_name, summary, ..
        } => format!(
            "  · {}",
            one_line(label(summary, tool_name), SUMMARY_BUDGET)
        ),
        // Completions separate "the tool is still running" from "the model is
        // thinking again". A failing one is usually why the run went as it
        // did, so it keeps its words; the name is empty only for a result
        // whose call this session never saw, so the id stands in.
        Event::ToolCompleted {
            tool_call_id,
            tool_name,
            summary,
            is_error,
        } => {
            let name = label(tool_name, tool_call_id);
            if *is_error {
                format!("  ! {name}: {}", one_line(summary, SUMMARY_BUDGET))
            } else {
                format!("  ✓ {name}")
            }
        }
        // Both are pauses with a reason, and a terminal that does not name
        // them looks stuck for as long as they last.
        Event::CompactionStarted { .. } => "basis: compacting the conversation".to_string(),
        Event::CompactionCompleted {
            replaced_items,
            preserved_items,
            ..
        } => format!(
            "basis: context compacted: {replaced_items} earlier items replaced by a summary, \
             {preserved_items} kept"
        ),
        Event::RequestToolResultsElided {
            canonical_tool_result_content_bytes,
            projected_tool_result_content_bytes,
            results,
            ..
        } => format!(
            "basis: {}",
            tool_result_elision_line(
                *canonical_tool_result_content_bytes,
                *projected_tool_result_content_bytes,
                results.len(),
            )
        ),
        Event::Retry {
            error,
            attempt,
            max_attempts,
            ..
        } => format!(
            "basis: {} (retry {attempt}/{max_attempts})",
            one_line(error, SUMMARY_BUDGET)
        ),
        Event::Notice { message, .. } | Event::Error { message, .. } => {
            format!("basis: {}", label(message, "task event"))
        }
        _ => return None,
    })
}

/// What the run reported spending, in one line, or `None` when it reported
/// nothing.
///
/// Input and output only — the two `total_tokens` counts and the two a bound
/// is enforced against. Cache reads and writes are priced differently
/// everywhere, so a line that added them in would answer no question exactly;
/// they are on the event for anyone who wants them.
///
/// Nothing reported prints nothing. A provider that says nothing about usage
/// leaves these at zero, and `0 in · 0 out` reads as a measurement of a free
/// run rather than as the absence of a report.
fn spent(usage: RunUsage) -> Option<String> {
    let (input, output) = (usage.input_tokens, usage.output_tokens);
    (input > 0 || output > 0).then(|| {
        format!(
            "{} in · {} out",
            compact_count(input),
            compact_count(output)
        )
    })
}

/// A token count as a person reads it: `980`, `12.3k`, `1.2M`.
///
/// Exact digits past the first few are noise in a progress line — the JSON
/// payload carries the whole number for anything that needs to add it up.
fn compact_count(count: u64) -> String {
    for (unit, scale) in [("M", 1_000_000_u64), ("k", 1_000)] {
        if count >= scale {
            let whole = count / scale;
            let tenth = (count % scale) * 10 / scale;
            return if tenth == 0 {
                format!("{whole}{unit}")
            } else {
                format!("{whole}.{tenth}{unit}")
            };
        }
    }
    count.to_string()
}

/// One string field of an event, empty when it is absent or not a string.
fn text<'a>(event: &'a Value, field: &str) -> &'a str {
    event[field].as_str().unwrap_or_default()
}

/// `value`, or `fallback` when the field it came from was empty.
fn label<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.is_empty() { fallback } else { value }
}

/// One line of at most `budget` characters: a tool's own output is arbitrary
/// text, and a progress line that spans paragraphs stops being progress.
fn one_line(text: &str, budget: usize) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(budget) {
        Some((end, _)) => format!("{}…", &line[..end]),
        None => line.to_string(),
    }
}

/// Decorates a raw terminal record with its handle and the follow-up its
/// state admits.
pub(crate) fn decorate_terminal(task: &str, mut record: TerminalRecord) -> TerminalRecord {
    let object = record
        .raw
        .as_object_mut()
        .expect("terminal payload is an object");
    object.insert("task".to_string(), serde_json::json!(task));
    object.insert(
        "next".to_string(),
        serde_json::json!(next_step(record.terminal.as_ref(), task)),
    );
    record
}

/// The one follow-up a record's state actually admits.
///
/// A hint is a promise, and the state is what decides which promises basis can
/// keep. An agent that holds a terminal record accepts no further messages —
/// `inbox::enqueue` refuses one the moment `terminal.json` exists — so a
/// settled task is never told to continue a conversation it has closed, and a
/// settled *failure* is not told to read an inbox that will be empty when the
/// reason is already on stderr.
fn next_step(terminal: Option<&Terminal>, task: &str) -> String {
    match terminal {
        // Answered. The journal is the one thing this handle still holds that
        // the terminal does not — and the one thing a redirected stdout, or a
        // scrollback that has moved on, did not keep.
        Some(Terminal::Succeeded { .. }) => format!("basis watch {task}"),
        // No answer was produced and none will be: this handle is spent, and
        // the work continues as a new task rather than as a message to a
        // closed one.
        Some(Terminal::Failed { .. } | Terminal::Cancelled) => "basis spawn <PROMPT>".to_string(),
        // A newer terminal state stays available through `raw`; this build
        // offers only the observation paths it can promise are safe.
        None => format!("basis watch {task} or basis inbox {task}"),
    }
}

pub(crate) fn render_terminal(
    record: &TerminalRecord,
    structured: bool,
) -> Result<ExitCode, ClientError> {
    if structured {
        println!("{}", record.raw);
        return Ok(ExitCode::from(terminal_result_code(record)));
    }

    match record.terminal.as_ref() {
        Some(Terminal::Succeeded { result }) => {
            if !result.is_empty() {
                print!("{result}");
                if !result.ends_with('\n') {
                    println!();
                }
            }
        }
        Some(Terminal::Failed { error }) => eprintln!("basis: task failed: {error}"),
        Some(Terminal::Cancelled) => eprintln!("basis: task was cancelled"),
        None => println!("task state: unknown"),
    }
    print_hint(&record.raw);
    flush_stdout()?;
    Ok(ExitCode::from(terminal_result_code(record)))
}

fn terminal_result_code(record: &TerminalRecord) -> u8 {
    // `stopped_by` is a durable string and newer basis versions may add a
    // bound this typed view does not know. Preserve the old exit contract:
    // every present, non-null bound is bounded, even when it cannot be typed.
    if record
        .raw
        .get("stopped_by")
        .is_some_and(|bound| !bound.is_null())
    {
        return EXIT_BOUNDED;
    }
    match record.terminal.as_ref() {
        Some(Terminal::Succeeded { .. }) => EXIT_OK,
        Some(Terminal::Failed { .. } | Terminal::Cancelled) | None => EXIT_FAILED,
    }
}

pub(crate) fn render_result(payload: &Value, structured: bool) -> Result<ExitCode, ClientError> {
    if structured {
        println!("{payload}");
        return Ok(ExitCode::from(result_code(payload)));
    }

    match payload["state"].as_str().unwrap_or("unknown") {
        "running" | "resumable" | "accepted" | "cancel_requested" => {
            if let Some(task) = payload["task"].as_str() {
                let state = payload["state"].as_str().unwrap_or("unknown");
                if state == "accepted"
                    && let Some(message) = payload["message"].as_str()
                {
                    println!("task {task}: accepted message {message}");
                } else {
                    println!("task {task}: {state}");
                }
            }
        }
        "succeeded" => {
            if let Some(result) = payload["result"].as_str()
                && !result.is_empty()
            {
                print!("{result}");
                if !result.ends_with('\n') {
                    println!();
                }
            }
        }
        "failed" => eprintln!(
            "basis: task failed: {}",
            payload["error"].as_str().unwrap_or("unknown failure")
        ),
        "cancelled" => eprintln!("basis: task was cancelled"),
        state => println!("task state: {state}"),
    }
    print_hint(payload);
    flush_stdout()?;
    Ok(ExitCode::from(result_code(payload)))
}

/// stdout is block-buffered whenever it is not a terminal, so a redirected
/// answer is not on disk until this runs.
fn flush_stdout() -> Result<(), ClientError> {
    io::stdout()
        .flush()
        .map_err(|error| ClientError::new(format!("flush task output: {error}")))
}

pub(crate) fn result_code(payload: &Value) -> u8 {
    if !payload["stopped_by"].is_null() {
        return EXIT_BOUNDED;
    }
    match payload["state"].as_str() {
        Some("running" | "resumable" | "accepted" | "cancel_requested" | "succeeded") => EXIT_OK,
        _ => EXIT_FAILED,
    }
}

/// One actionable line after a result, on stderr.
///
/// stderr because stdout is the answer and nothing else: `basis "…" > out.md`
/// has to leave a file holding what was asked for, and a hint addressed to
/// whoever is watching the terminal is not that. The `--json` payload carries
/// the same fact as its `next` field, so a script loses nothing.
pub(crate) fn print_hint(payload: &Value) {
    let _ = write_hint(payload, &mut io::stderr());
}

fn write_hint(payload: &Value, err: &mut impl Write) -> io::Result<()> {
    match payload["next"].as_str() {
        Some(next) => writeln!(err, "next: use `{next}`"),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use basis::{
        Bound, Mutability, RunOutcome,
        event::{
            ContextFile, ElidedToolResult, RequestToolResultElisionPolicy, ToolResultContentKind,
            ToolResultElisionAction,
        },
    };
    use serde_json::json;

    fn value(event: Event) -> Value {
        serde_json::to_value(&event).expect("event serializes")
    }

    fn started(model: &str) -> Value {
        value(Event::RunStarted {
            schema: 1,
            basis: "0.0.0".to_string(),
            session_id: "s".to_string(),
            workspace: "/repo".into(),
            model: model.to_string(),
            provider: "test".to_string(),
            context_files: vec![ContextFile {
                path: "/repo/AGENTS.md".into(),
                scope: "workspace".to_string(),
            }],
            skills_dirs: Vec::new(),
            skills: Vec::new(),
            templates_dirs: Vec::new(),
            templates: Vec::new(),
            mcp_files: Vec::new(),
            mcp_servers: Vec::new(),
        })
    }

    fn tool_queued(summary: &str) -> Value {
        value(Event::ToolQueued {
            tool_call_id: "c1".to_string(),
            tool_name: "shell".to_string(),
            summary: summary.to_string(),
            mutability: Mutability::Unknown,
            input: Value::Null,
        })
    }

    fn tool_completed(summary: &str, is_error: bool) -> Value {
        value(Event::ToolCompleted {
            tool_call_id: "c1".to_string(),
            tool_name: "shell".to_string(),
            summary: summary.to_string(),
            is_error,
        })
    }

    fn delta(text: &str) -> Value {
        value(Event::AssistantDelta {
            text: text.to_string(),
        })
    }

    fn finished(usage: Option<RunUsage>) -> Value {
        value(Event::RunFinished {
            outcome: RunOutcome::Ok,
            stopped_by: None,
            usage,
        })
    }

    fn terminal_record(terminal: Terminal, stopped_by: Option<Bound>) -> TerminalRecord {
        let mut raw = serde_json::to_value(&terminal).expect("terminal serializes");
        if let Some(bound) = stopped_by {
            raw["stopped_by"] = serde_json::json!(bound);
        }
        TerminalRecord {
            raw,
            terminal: Some(terminal),
            stopped_by,
            usage: None,
            result_truncated: false,
        }
    }

    #[test]
    fn terminal_codes_do_not_depend_on_rendering() {
        assert_eq!(result_code(&json!({"state": "resumable"})), EXIT_OK);
        assert_eq!(
            terminal_result_code(&terminal_record(
                Terminal::Failed {
                    error: "failed".to_string(),
                },
                Some(Bound::Deadline),
            )),
            EXIT_BOUNDED
        );
        assert_eq!(
            terminal_result_code(&terminal_record(
                Terminal::Succeeded {
                    result: "done".to_string(),
                },
                None,
            )),
            EXIT_OK
        );
        assert_eq!(
            terminal_result_code(&terminal_record(Terminal::Cancelled, None)),
            EXIT_FAILED
        );
    }

    #[test]
    fn an_unknown_non_null_bound_still_exits_bounded() {
        let mut record = terminal_record(
            Terminal::Failed {
                error: "stopped".to_string(),
            },
            None,
        );
        record.raw["stopped_by"] = json!("newer_bound");

        assert_eq!(record.stopped_by, None, "the typed view stays honest");
        assert_eq!(terminal_result_code(&record), EXIT_BOUNDED);
    }

    #[test]
    fn a_terminal_payload_carries_the_handle_it_settled_under() {
        let record = decorate_terminal(
            "w/t",
            terminal_record(
                Terminal::Succeeded {
                    result: "done".to_string(),
                },
                None,
            ),
        );
        assert_eq!(record.raw["task"], "w/t");
    }

    /// A hint is a promise: the command it names has to work on the task it
    /// names. Two of these were promises basis could not keep — a settled
    /// agent accepts no messages at all (`inbox::enqueue` refuses one the
    /// moment `terminal.json` exists), and pointing a failed run at `watch`
    /// and an empty `inbox` sends someone looking anywhere but at the
    /// failure, which is already on stderr.
    #[test]
    fn each_state_is_told_the_follow_up_that_works_on_it() {
        let hints = [
            (
                Terminal::Succeeded {
                    result: "done".to_string(),
                },
                "basis watch w/t",
            ),
            (
                Terminal::Failed {
                    error: "failed".to_string(),
                },
                "basis spawn <PROMPT>",
            ),
            (Terminal::Cancelled, "basis spawn <PROMPT>"),
        ];

        for (terminal, expected) in hints {
            assert_eq!(
                decorate_terminal("w/t", terminal_record(terminal, None)).raw["next"],
                expected,
                "the follow-up offered for the terminal state"
            );
        }
    }

    /// The rendering rule, in one run's worth of events: stdout carries the
    /// assistant's answer and nothing else, stderr carries the work that
    /// produced it. `basis "…" > answer.md 2> progress.log` is the invocation
    /// that has to leave two files, each holding one of those things.
    #[test]
    fn the_answer_streams_to_stdout_and_the_work_to_stderr() {
        let live = Live::when(true);
        let (mut out, mut err) = (Vec::new(), Vec::new());

        for event in [
            started("test-model"),
            tool_queued("shell: cargo test"),
            value(Event::ToolStarted {
                tool_call_id: "c1".to_string(),
                tool_name: "shell".to_string(),
            }),
            tool_completed("0 failed", false),
            delta("the tests "),
            delta("pass"),
            finished(None),
        ] {
            live.show_to(&event, &mut out, &mut err)
                .expect("writing to a vector");
        }

        let (out, err) = (
            String::from_utf8(out).expect("utf8"),
            String::from_utf8(err).expect("utf8"),
        );
        assert_eq!(
            out, "the tests pass\n",
            "stdout is the answer, closed by the finish line"
        );
        assert!(err.contains("test-model"), "{err}");
        assert!(
            err.contains("shell: cargo test"),
            "a tool call names itself while it runs: {err}"
        );
        assert!(err.contains("shell"), "and reports finishing: {err}");
        assert!(
            !err.contains("the tests"),
            "the answer must never be duplicated onto stderr: {err}"
        );
        assert!(live.answered(), "the answer reached the terminal");
    }

    #[test]
    fn completed_compaction_reports_what_was_replaced_and_kept() {
        let line = progress_line(&Event::CompactionCompleted {
            agent_id: "agent-1".to_string(),
            replaced_items: 42,
            preserved_items: 8,
            transcript_len: 50,
            extracted_facts: 3,
            summary_preview: "summary".to_string(),
        })
        .expect("compaction completion is progress");

        assert_eq!(
            line,
            "basis: context compacted: 42 earlier items replaced by a summary, 8 kept"
        );
    }

    /// What the run cost, once, at the end, on stderr.
    ///
    /// basis ships no price table — prices are the host's and they move — but
    /// the counts are basis's, and a person who never sees them cannot notice
    /// the run that cost ten times the last one. stderr for the same reason
    /// every other progress line is there: `basis "…" > answer.md` has to
    /// leave a file holding the answer, not a receipt stapled to it.
    #[test]
    fn a_finished_run_says_what_it_spent_beside_the_answer_rather_than_in_it() {
        let live = Live::when(true);
        let (mut out, mut err) = (Vec::new(), Vec::new());

        for event in [
            delta("done"),
            finished(Some(RunUsage {
                input_tokens: 12_300,
                output_tokens: 1_200,
                cache_read_tokens: 40,
                cache_creation_tokens: 5,
                ..RunUsage::default()
            })),
        ] {
            live.show_to(&event, &mut out, &mut err)
                .expect("writing to a vector");
        }

        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "done\n",
            "stdout is the answer, and a token count is not part of it"
        );
        assert_eq!(
            String::from_utf8(err).expect("utf8"),
            "basis: 12.3k in · 1.2k out\n"
        );
    }

    /// A run whose provider reported nothing has nothing to say, and a line
    /// reading `0 in · 0 out` would say it anyway — which is how a tally
    /// starts being read as a measurement.
    #[test]
    fn a_run_that_reported_no_usage_prints_no_usage_line() {
        let live = Live::when(true);
        let (mut out, mut err) = (Vec::new(), Vec::new());

        for event in [finished(None), finished(Some(RunUsage::default()))] {
            live.show_to(&event, &mut out, &mut err)
                .expect("writing to a vector");
        }

        assert!(out.is_empty(), "and no answer was streamed to close");
        assert!(err.is_empty(), "{}", String::from_utf8_lossy(&err));
    }

    #[test]
    fn request_tool_result_elision_is_progress_not_answer_text() {
        let live = Live::when(true);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let event = value(Event::RequestToolResultsElided {
            agent_id: "agent-1".to_string(),
            policy: RequestToolResultElisionPolicy::KeepRecent {
                configured_keep_recent_tool_results: 3,
            },
            canonical_tool_result_content_bytes: 8_192,
            projected_tool_result_content_bytes: 512,
            results: vec![ElidedToolResult {
                tool_call_id: "call-1".to_string(),
                tool_name: Some("read".to_string()),
                is_error: false,
                canonical_content_kind: ToolResultContentKind::Text,
                action: ToolResultElisionAction::Marker,
                canonical_content_bytes: 8_192,
                projected_content_bytes: 32,
            }],
        });

        live.show_to(&event, &mut out, &mut err)
            .expect("writing to a vector");

        assert!(out.is_empty(), "projection telemetry is not answer text");
        assert_eq!(
            String::from_utf8(err).expect("utf8"),
            "basis: request tool results reduced: 8192 -> 512 bytes; 1 result changed\n"
        );
        assert!(!live.answered());
    }

    #[test]
    fn counts_are_written_the_way_a_person_reads_them() {
        assert_eq!(compact_count(0), "0");
        assert_eq!(compact_count(980), "980");
        assert_eq!(compact_count(1_200), "1.2k");
        assert_eq!(compact_count(12_000), "12k", "a bare thousand keeps no .0");
        assert_eq!(compact_count(1_250_000), "1.2M");
    }

    /// A failing tool call is the one completion worth its summary: it is
    /// usually why the run went the way it did.
    #[test]
    fn a_failing_tool_call_says_what_it_said() {
        let live = Live::when(true);
        let (mut out, mut err) = (Vec::new(), Vec::new());

        live.show_to(
            &tool_completed("no such file\nand a second line", true),
            &mut out,
            &mut err,
        )
        .expect("writing to a vector");

        let err = String::from_utf8(err).expect("utf8");
        assert_eq!(err, "  ! shell: no such file\n", "{err}");
        assert!(out.is_empty(), "a tool failure is not an answer");
        assert!(!live.answered());
    }

    /// `--json --await` asks for one settled object and nothing else. The
    /// events still reach the journal — the executor writes it either way —
    /// but no renderer stands between them and a parser.
    #[test]
    fn a_run_nobody_is_watching_renders_nothing() {
        let live = Live::when(false);
        let (mut out, mut err) = (Vec::new(), Vec::new());

        for event in [
            delta("an answer"),
            value(Event::ToolStarted {
                tool_call_id: "c1".to_string(),
                tool_name: "shell".to_string(),
            }),
            finished(None),
        ] {
            live.show_to(&event, &mut out, &mut err)
                .expect("writing to a vector");
        }

        assert!(out.is_empty() && err.is_empty());
        assert!(
            !live.answered(),
            "and the settled record is still the only place the answer comes from"
        );
    }

    /// The streamed answer and the settled record are the same text. Printing
    /// the record under text that already arrived would double it.
    #[test]
    fn a_streamed_answer_is_not_printed_again_underneath_itself() {
        let live = Live::when(true);
        let succeeded = terminal_record(
            Terminal::Succeeded {
                result: "done".to_string(),
            },
            None,
        );
        assert!(!live.repeats(&succeeded, false), "nothing streamed yet");

        live.show_to(&delta("done"), &mut Vec::new(), &mut Vec::new())
            .expect("writing to a vector");

        assert!(live.repeats(&succeeded, false));
        assert!(
            !live.repeats(&succeeded, true),
            "`--json` prints the object it was asked for, whatever a terminal saw"
        );
        let failed = terminal_record(
            Terminal::Failed {
                error: "boom".to_string(),
            },
            None,
        );
        assert!(
            !live.repeats(&failed, false),
            "a failure was never on the stream, so it still has to be said"
        );
    }

    /// Journals from before 0.6.0 hold the CLI's synthetic notices with no
    /// `severity`; the message is the part a person needs, and the reader's
    /// `#[serde(default)]` is what keeps it reachable.
    #[test]
    fn a_notice_without_a_severity_still_renders_its_message() {
        let live = Live::when(true);
        let (mut out, mut err) = (Vec::new(), Vec::new());

        live.show_to(
            &json!({"type": "notice", "message": "event omitted because it exceeded 32768 bytes", "seq": 4}),
            &mut out,
            &mut err,
        )
        .expect("writing to a vector");

        assert!(out.is_empty(), "a notice is never the answer");
        assert_eq!(
            String::from_utf8(err).expect("utf8"),
            "basis: event omitted because it exceeded 32768 bytes\n"
        );
    }

    /// A journal written by a newer basis can hold a type this build cannot
    /// name. The old string-matching renderer fell into `_ => {}` and showed
    /// nothing; saying so is the whole point of matching typed variants.
    #[test]
    fn an_event_this_build_cannot_name_is_said_not_swallowed() {
        let live = Live::when(true);
        let (mut out, mut err) = (Vec::new(), Vec::new());

        live.show_to(
            &json!({"type": "from_the_future", "seq": 3}),
            &mut out,
            &mut err,
        )
        .expect("writing to a vector");

        assert!(out.is_empty(), "not the answer stream's business");
        assert_eq!(
            String::from_utf8(err).expect("utf8"),
            "basis: unrecognized event `from_the_future`\n"
        );
        assert!(!live.answered());
    }

    /// stdout is the answer; the hint is not part of it. `basis "…" > out.md`
    /// is the invocation that proves it.
    #[test]
    fn the_hint_goes_to_the_terminal_and_never_into_the_answer() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        write_hint(
            &json!({"state": "succeeded", "next": "basis watch w/t"}),
            &mut err,
        )
        .expect("writing to a vector");
        write_hint(&json!({"state": "succeeded"}), &mut out).expect("writing to a vector");

        assert_eq!(
            String::from_utf8(err).unwrap(),
            "next: use `basis watch w/t`\n"
        );
        assert!(out.is_empty(), "a payload without a next step says nothing");
    }
}
