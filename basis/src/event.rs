//! basis's event stream: one schema, many surfaces.
//!
//! Mentra's [`SessionEvent`] broadcast is the source of truth for what happens
//! during a run. This module normalizes it into a wire contract basis owns, so
//! that `basis spawn --json` (with `basis run` retained as an alias), the ACP
//! mapping (P2), and anything downstream all read the same shape — and so a
//! change inside mentra does not silently
//! become a change in basis's output.
//!
//! # Wire format
//!
//! Newline-delimited JSON, one [`EventLine`] per line. The first line is
//! always [`Event::RunStarted`], which carries [`EVENT_SCHEMA_VERSION`]; a
//! consumer reads the version before anything else and can refuse a stream it
//! does not understand. The last line is always [`Event::RunFinished`].
//!
//! ```jsonl
//! {"seq":0,"type":"run_started","schema":1,"basis":"0.1.0","workspace":"/repo",...}
//! {"seq":1,"type":"assistant_delta","text":"Looking at "}
//! {"seq":2,"type":"run_finished","status":"ok"}
//! ```
//!
//! [`SessionEvent`]: mentra::SessionEvent

mod jsonl;
mod mapping;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use jsonl::JsonlWriter;

/// Version of the JSONL wire format. Bumped when a change would break a
/// consumer that reads the current shape.
pub const EVENT_SCHEMA_VERSION: u32 = 1;

/// One line of the stream: a sequence number and the event itself, flattened
/// so a line is a single flat JSON object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventLine {
    pub seq: u64,
    #[serde(flatten)]
    pub event: Event,
}

impl EventLine {
    pub fn new(seq: u64, event: Event) -> Self {
        Self { seq, event }
    }
}

/// Whether a tool call can change anything outside the process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mutability {
    ReadOnly,
    Mutating,
    Unknown,
}

/// How a permission request was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOutcome {
    Allowed,
    Denied,
}

/// How far a remembered permission decision reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleScope {
    Session,
    Project,
    Global,
}

/// Severity of an out-of-band notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeSeverity {
    Info,
    Warning,
}

/// What kind of concurrent work a task event describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Subagent,
    BackgroundTask,
    Teammate,
}

/// Where a task is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Spawned,
    Running,
    Finished,
    Failed,
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunOutcome {
    /// The turn completed and the assistant produced a final message.
    Ok,
    /// The run failed. `message` is the operator-facing reason: the failure's
    /// own words together with whatever its cause chain adds that those words
    /// did not already say (see `chain_message` in `run/prepared.rs`) — so it
    /// can read as more than the identically-worded [`Event::Error`], which
    /// mentra builds from the bare message alone and puts on the stream for
    /// the same failure.
    Error { message: String },
}

/// A skill the run can load by name. Bodies stay out of the stream — they are
/// what `load_skill` is for, and keeping them out is what makes skills cheap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
}

/// A prompt template a client can offer as a command.
///
/// Bodies stay out of the stream for the same reason skill bodies do: the
/// stream says what is available, not what it contains.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateSummary {
    pub name: String,
    pub description: String,
    /// What the template says its arguments look like, when it says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
}

/// A context file that was in effect for the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextFile {
    pub path: PathBuf,
    pub scope: String,
}

/// Everything that can appear on the stream.
///
/// `RunStarted` and `RunFinished` are basis's own bookends; the rest are
/// normalized from mentra's [`SessionEvent`](mentra::SessionEvent).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Always the first line. Carries the schema version.
    RunStarted {
        schema: u32,
        basis: String,
        session_id: String,
        workspace: PathBuf,
        model: String,
        provider: String,
        /// Context files discovered for this run, weakest precedence first.
        context_files: Vec<ContextFile>,
        /// Skills directories in effect, most specific first. Omitted rather
        /// than empty so a stream without skills stays quiet about them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        skills_dirs: Vec<PathBuf>,
        /// The skills those directories produced, after layering — what the
        /// model can actually load by name.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        skills: Vec<SkillSummary>,
        /// Template directories in effect, most specific first.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        templates_dirs: Vec<PathBuf>,
        /// The templates those directories produced, after layering — what a
        /// client can offer as commands.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        templates: Vec<TemplateSummary>,
        /// MCP configuration files in effect, weakest precedence first.
        ///
        /// Named for the same reason context files are, and more urgently: an
        /// `.mcp.json` says which programs to spawn and carries the
        /// credentials to spawn them with, so it is the last thing that should
        /// take effect without appearing anywhere.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mcp_files: Vec<ContextFile>,
        /// The servers those files produced, after layering. Names only —
        /// commands, arguments, and environment stay out of the stream, which
        /// is the same no-echo rule `McpError` follows.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        mcp_servers: Vec<String>,
    },

    UserMessage {
        text: String,
        /// How many images the turn carried. A turn can be images alone, and
        /// `text` is then empty — a consumer rendering only `text` would show
        /// a blank user message. Absent from the line when zero, so a stream
        /// written before the field existed reads the same as one after.
        #[serde(default, skip_serializing_if = "is_zero")]
        image_count: usize,
    },
    AssistantDelta {
        text: String,
    },
    AssistantReasoningDelta {
        text: String,
    },
    AssistantMessage {
        text: String,
    },

    ToolQueued {
        tool_call_id: String,
        tool_name: String,
        summary: String,
        mutability: Mutability,
        /// Parsed tool input when it is valid JSON, else the raw string.
        input: Value,
    },
    ToolStarted {
        tool_call_id: String,
        tool_name: String,
    },
    ToolProgress {
        tool_call_id: String,
        tool_name: String,
        progress: String,
    },
    ToolCompleted {
        tool_call_id: String,
        tool_name: String,
        summary: String,
        is_error: bool,
    },

    PermissionRequested {
        request_id: String,
        tool_call_id: String,
        tool_name: String,
        description: String,
        /// Parsed preview when it is valid JSON, else the raw string.
        preview: Value,
    },
    PermissionResolved {
        request_id: String,
        tool_call_id: String,
        tool_name: String,
        outcome: PermissionOutcome,
        #[serde(skip_serializing_if = "Option::is_none")]
        rule_scope: Option<RuleScope>,
    },

    TaskUpdated {
        task_id: String,
        kind: TaskKind,
        status: TaskStatus,
        title: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },

    CompactionStarted {
        agent_id: String,
    },
    CompactionCompleted {
        agent_id: String,
        replaced_items: usize,
        preserved_items: usize,
        transcript_len: usize,
        extracted_facts: usize,
        summary_preview: String,
    },
    MemoryUpdated {
        agent_id: String,
        stored_records: usize,
    },

    Usage {
        agent_id: String,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
        /// Reasoning counted *inside* `output_tokens` (the Responses wire).
        #[serde(default)]
        reasoning_tokens: u64,
        /// Thinking counted *outside* `output_tokens` (Gemini). Kept apart
        /// from `reasoning_tokens` for the reason [`RunUsage`](crate::RunUsage)
        /// gives: a sum would be wrong for one of the two wires.
        #[serde(default)]
        thoughts_tokens: u64,
    },
    Notice {
        severity: NoticeSeverity,
        message: String,
    },
    Retry {
        agent_id: String,
        error: String,
        attempt: u32,
        max_attempts: u32,
        next_delay_ms: u64,
    },
    Error {
        message: String,
        recoverable: bool,
    },

    /// The session returned to an earlier entry; later turns continue from
    /// there along a different path.
    Branched {
        entry_id: String,
        /// How many entries left the active path. They stay in the transcript
        /// and remain reachable.
        abandoned_entries: usize,
    },

    /// Always the last line.
    RunFinished {
        #[serde(flatten)]
        outcome: RunOutcome,
        /// The bound that ended the run, when one did — the same fact the
        /// CLI's exit `3` carries, for a consumer reading the stream instead
        /// of the exit code. Absent, not null, on an unbounded finish, so a
        /// schema-1 consumer that never heard of it reads the line unchanged.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        stopped_by: Option<crate::run::Bound>,
        /// What the run reported spending, summed over its rounds — the same
        /// figure [`RunReport::usage`](crate::RunReport) carries in-process,
        /// for the consumers that only ever see the stream.
        ///
        /// It rides the finish line because a total is only a total once the
        /// run is over; the per-round [`Event::Usage`] reports are still there
        /// for anyone metering as it goes. basis ships no price table — that
        /// is the host's, and prices change — so this is the last basis-side
        /// fact between a run and a bill.
        ///
        /// Absent, not zero, when the producer stated nothing: an old stream
        /// and a run that cost nothing are different claims, and only one of
        /// them is worth acting on. Optional and additive, so
        /// [`EVENT_SCHEMA_VERSION`] does not move for it.
        #[serde(skip_serializing_if = "Option::is_none", default)]
        usage: Option<crate::run::RunUsage>,
    },
}

impl Event {
    /// Normalizes a mentra session event, or `None` when basis's stream already
    /// carries the information some other way.
    pub fn from_session_event(event: &mentra::SessionEvent) -> Option<Self> {
        mapping::from_session_event(event)
    }
}

/// `skip_serializing_if` for a count that is only news when it is not zero.
fn is_zero(count: &usize) -> bool {
    *count == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_one_flat_object() {
        let line = EventLine::new(
            7,
            Event::AssistantDelta {
                text: "hi".to_string(),
            },
        );
        let json = serde_json::to_value(&line).expect("serializes");

        assert_eq!(json["seq"], 7);
        assert_eq!(json["type"], "assistant_delta");
        assert_eq!(json["text"], "hi");
        assert!(json.get("event").is_none(), "envelope must stay flat");
    }

    #[test]
    fn the_header_carries_the_schema_version() {
        let line = EventLine::new(
            0,
            Event::RunStarted {
                schema: EVENT_SCHEMA_VERSION,
                basis: "0.1.0".to_string(),
                session_id: "s1".to_string(),
                workspace: PathBuf::from("/repo"),
                model: "gpt-5".to_string(),
                provider: "openai".to_string(),
                context_files: vec![ContextFile {
                    path: PathBuf::from("/repo/AGENTS.md"),
                    scope: "workspace".to_string(),
                }],
                skills_dirs: Vec::new(),
                skills: Vec::new(),
                templates_dirs: Vec::new(),
                templates: Vec::new(),
                mcp_files: Vec::new(),
                mcp_servers: Vec::new(),
            },
        );
        let json = serde_json::to_value(&line).expect("serializes");

        assert_eq!(json["type"], "run_started");
        assert_eq!(json["schema"], EVENT_SCHEMA_VERSION);
        assert_eq!(json["context_files"][0]["scope"], "workspace");
        assert!(
            json.get("skills_dirs").is_none() && json.get("skills").is_none(),
            "a run without skills must not mention them"
        );
    }

    #[test]
    fn skills_are_reported_when_there_are_any() {
        let line = EventLine::new(
            0,
            Event::RunStarted {
                schema: EVENT_SCHEMA_VERSION,
                basis: "0.1.0".to_string(),
                session_id: "s1".to_string(),
                workspace: PathBuf::from("/repo"),
                model: "gpt-5".to_string(),
                provider: "openai".to_string(),
                context_files: Vec::new(),
                skills_dirs: vec![PathBuf::from("/repo/.basis/skills")],
                skills: vec![SkillSummary {
                    name: "review".to_string(),
                    description: "house review style".to_string(),
                }],
                templates_dirs: Vec::new(),
                templates: Vec::new(),
                mcp_files: Vec::new(),
                mcp_servers: Vec::new(),
            },
        );
        let json = serde_json::to_value(&line).expect("serializes");

        assert_eq!(json["skills_dirs"][0], "/repo/.basis/skills");
        assert_eq!(json["skills"][0]["name"], "review");
        assert!(
            !json["skills"][0]
                .as_object()
                .expect("an object")
                .contains_key("path"),
            "the stream carries what the model can load, not where it lives on this machine"
        );
    }

    #[test]
    fn run_outcome_flattens_into_the_finish_line() {
        let ok = serde_json::to_value(EventLine::new(
            3,
            Event::RunFinished {
                outcome: RunOutcome::Ok,
                stopped_by: None,
                usage: None,
            },
        ))
        .expect("serializes");
        assert_eq!(ok["type"], "run_finished");
        assert_eq!(ok["status"], "ok");
        assert!(
            !ok.as_object()
                .expect("an object")
                .contains_key("stopped_by"),
            "an unbounded finish is byte-identical to what a schema-1 consumer already reads"
        );

        let failed = serde_json::to_value(EventLine::new(
            3,
            Event::RunFinished {
                outcome: RunOutcome::Error {
                    message: "boom".to_string(),
                },
                stopped_by: None,
                usage: None,
            },
        ))
        .expect("serializes");
        assert_eq!(failed["status"], "error");
        assert_eq!(failed["message"], "boom");
    }

    /// The counts a consumer needs to price a run, on the line that says the
    /// run is over. basis ships no price table, and that argument only holds
    /// if the numbers arrive: until now the total existed in-process
    /// (`RunReport::usage`) and nowhere on the wire.
    ///
    /// Optional and additive, which is why [`EVENT_SCHEMA_VERSION`] does not
    /// move: an absent `usage` means a producer that reported none, and a
    /// schema-1 reader that ignores the key reads the line exactly as before.
    #[test]
    fn a_finish_line_reports_what_the_run_spent() {
        let line = serde_json::to_value(EventLine::new(
            4,
            Event::RunFinished {
                outcome: RunOutcome::Ok,
                stopped_by: None,
                usage: Some(crate::RunUsage {
                    input_tokens: 12_300,
                    output_tokens: 1_200,
                    cache_read_tokens: 40,
                    cache_creation_tokens: 5,
                    reasoning_tokens: 300,
                    thoughts_tokens: 0,
                }),
            },
        ))
        .expect("serializes");

        assert_eq!(line["usage"]["input_tokens"], 12_300);
        assert_eq!(line["usage"]["output_tokens"], 1_200);
        assert_eq!(line["usage"]["cache_read_tokens"], 40);
        assert_eq!(line["usage"]["cache_creation_tokens"], 5);

        let unreported = serde_json::to_value(EventLine::new(
            4,
            Event::RunFinished {
                outcome: RunOutcome::Ok,
                stopped_by: None,
                usage: None,
            },
        ))
        .expect("serializes");
        assert!(
            !unreported
                .as_object()
                .expect("an object")
                .contains_key("usage"),
            "a producer that reported nothing says nothing, and the line keeps its schema-1 shape"
        );

        let read_back: EventLine =
            serde_json::from_value(unreported).expect("a line without usage still parses");
        assert!(matches!(
            read_back.event,
            Event::RunFinished { usage: None, .. }
        ));
    }

    #[test]
    fn a_bounded_finish_names_its_bound_on_the_stream() {
        // The exit code says `3`; this is the same fact for a consumer reading
        // the stream instead. It rides `run_finished` rather than a new event
        // because a bound is a property of how the run ended, and it can
        // accompany either status — a token budget can end a run that answered.
        let line = serde_json::to_value(EventLine::new(
            2,
            Event::RunFinished {
                outcome: RunOutcome::Ok,
                stopped_by: Some(crate::run::Bound::TokenBudget),
                usage: None,
            },
        ))
        .expect("serializes");

        assert_eq!(line["type"], "run_finished");
        assert_eq!(line["status"], "ok");
        assert_eq!(line["stopped_by"], "token_budget");
    }

    #[test]
    fn the_header_names_mcp_files_and_servers_but_never_their_configuration() {
        let line = EventLine::new(
            0,
            Event::RunStarted {
                schema: EVENT_SCHEMA_VERSION,
                basis: "0.1.0".to_string(),
                session_id: "s1".to_string(),
                workspace: PathBuf::from("/repo"),
                model: "gpt-5".to_string(),
                provider: "openai".to_string(),
                context_files: Vec::new(),
                skills_dirs: Vec::new(),
                skills: Vec::new(),
                templates_dirs: Vec::new(),
                templates: Vec::new(),
                mcp_files: vec![ContextFile {
                    path: PathBuf::from("/repo/.mcp.json"),
                    scope: "workspace".to_string(),
                }],
                mcp_servers: vec!["github".to_string()],
            },
        );
        let text = serde_json::to_string(&line).expect("serializes");

        assert!(text.contains("/repo/.mcp.json"), "the file must be named");
        assert!(text.contains("github"), "so must the server");

        // A server list is names, never configuration. The type makes this
        // true — `mcp_servers` is `Vec<String>` — and the test says why, so a
        // later change to a richer summary has to argue with it first: an
        // `.mcp.json` holds the credentials its servers are spawned with, and
        // this line travels into logs and client error panes.
        for leak in ["command", "args", "env", "npx", "token"] {
            assert!(
                !text.contains(leak),
                "the header must not carry MCP configuration, found {leak}: {text}"
            );
        }
    }

    #[test]
    fn absent_optionals_are_omitted_not_null() {
        let json = serde_json::to_value(EventLine::new(
            1,
            Event::TaskUpdated {
                task_id: "t1".to_string(),
                kind: TaskKind::Subagent,
                status: TaskStatus::Running,
                title: "work".to_string(),
                detail: None,
            },
        ))
        .expect("serializes");

        assert!(json.get("detail").is_none());
    }

    #[test]
    fn lines_round_trip() {
        let line = EventLine::new(
            2,
            Event::ToolCompleted {
                tool_call_id: "c1".to_string(),
                tool_name: "shell".to_string(),
                summary: "ok".to_string(),
                is_error: false,
            },
        );
        let text = serde_json::to_string(&line).expect("serializes");
        let back: EventLine = serde_json::from_str(&text).expect("deserializes");

        assert_eq!(line, back);
    }
}
