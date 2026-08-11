//! lan's event stream: one schema, many surfaces.
//!
//! Mentra's [`SessionEvent`] broadcast is the source of truth for what happens
//! during a run. This module normalizes it into a wire contract lan owns, so
//! that `lan run --json`, the ACP mapping (P2), and anything downstream all
//! read the same shape — and so a change inside mentra does not silently
//! become a change in lan's output.
//!
//! # Wire format
//!
//! Newline-delimited JSON, one [`EventLine`] per line. The first line is
//! always [`Event::RunStarted`], which carries [`EVENT_SCHEMA_VERSION`]; a
//! consumer reads the version before anything else and can refuse a stream it
//! does not understand. The last line is always [`Event::RunFinished`].
//!
//! ```jsonl
//! {"seq":0,"type":"run_started","schema":1,"lan":"0.1.0","workspace":"/repo",...}
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
    /// The run failed. `message` is the operator-facing reason.
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
/// `RunStarted` and `RunFinished` are lan's own bookends; the rest are
/// normalized from mentra's [`SessionEvent`](mentra::SessionEvent).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Always the first line. Carries the schema version.
    RunStarted {
        schema: u32,
        lan: String,
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
    },
}

impl Event {
    /// Normalizes a mentra session event, or `None` when lan's stream already
    /// carries the information some other way.
    pub fn from_session_event(event: &mentra::SessionEvent) -> Option<Self> {
        mapping::from_session_event(event)
    }
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
                lan: "0.1.0".to_string(),
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
                lan: "0.1.0".to_string(),
                session_id: "s1".to_string(),
                workspace: PathBuf::from("/repo"),
                model: "gpt-5".to_string(),
                provider: "openai".to_string(),
                context_files: Vec::new(),
                skills_dirs: vec![PathBuf::from("/repo/.lan/skills")],
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

        assert_eq!(json["skills_dirs"][0], "/repo/.lan/skills");
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
            },
        ))
        .expect("serializes");
        assert_eq!(ok["type"], "run_finished");
        assert_eq!(ok["status"], "ok");

        let failed = serde_json::to_value(EventLine::new(
            3,
            Event::RunFinished {
                outcome: RunOutcome::Error {
                    message: "boom".to_string(),
                },
            },
        ))
        .expect("serializes");
        assert_eq!(failed["status"], "error");
        assert_eq!(failed["message"], "boom");
    }

    #[test]
    fn the_header_names_mcp_files_and_servers_but_never_their_configuration() {
        let line = EventLine::new(
            0,
            Event::RunStarted {
                schema: EVENT_SCHEMA_VERSION,
                lan: "0.1.0".to_string(),
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
