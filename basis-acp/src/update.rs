//! basis's [`Event`] stream, as ACP `session/update` notifications.
//!
//! This is the reason [`Event`] exists. mentra's `SessionEvent` is normalized
//! once, in `event/mapping.rs`, and every surface downstream — JSONL, ACP,
//! whatever comes next — maps from basis's own shape. Nothing here touches
//! mentra.
//!
//! [`Event`] is `#[non_exhaustive]`, so from this crate the match below must
//! end in a wildcard — but the wildcard does not swallow: an unmapped variant
//! is surfaced as a thought chunk naming its wire tag, the same place every
//! other basis aside goes. The exhaustive match that breaks the build when a
//! variant lands lives in basis itself, beside the mentra mapping.

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, SessionUpdate, TextContent, ToolCall, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use serde_json::Value;

use basis::{
    event::{Event, Mutability},
    tools::SPAWN,
};

/// Maps one basis event to an ACP session update.
///
/// `None` where ACP has no place for it — basis's own bookends, and the
/// housekeeping events (usage, memory, branching) that ACP models differently
/// or not at all. Those still reach a JSONL consumer; they are simply not part
/// of this protocol's vocabulary.
///
/// The events with no ACP kind that a person still needs to see — a retry, a
/// recoverable error, a compaction — become thought chunks. That is a
/// deliberate widening of what "thought" means: it is the one update kind ACP
/// gives an agent for saying something about itself rather than to the user,
/// and the alternative is silence about a pause or a rewritten conversation.
pub fn session_update(event: &Event) -> Option<SessionUpdate> {
    let update = match event {
        // basis's bookends. `session/prompt` returning is what tells an ACP
        // client the turn is over, so repeating it as an update would be
        // noise.
        Event::RunStarted { .. } | Event::RunFinished { .. } => return None,

        // The client sent this; echoing it back would double it in the UI.
        Event::UserMessage { .. } => return None,

        Event::AssistantDelta { text } => SessionUpdate::AgentMessageChunk(chunk(text)),
        Event::AssistantReasoningDelta { text } => SessionUpdate::AgentThoughtChunk(chunk(text)),

        // The deltas already streamed this text. Sending the assembled message
        // too would render it twice.
        Event::AssistantMessage { .. } => return None,

        Event::ToolQueued {
            tool_call_id,
            tool_name,
            summary,
            mutability,
            input,
        } => SessionUpdate::ToolCall(
            ToolCall::new(tool_call_id.clone(), title(summary, tool_name))
                .kind(tool_kind(tool_name, *mutability, input))
                .status(ToolCallStatus::Pending)
                .raw_input(input.clone()),
        ),

        Event::ToolStarted { tool_call_id, .. } => {
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                tool_call_id.clone(),
                ToolCallUpdateFields::new().status(ToolCallStatus::InProgress),
            ))
        }

        // ACP has no progress field, so progress becomes the title: a client
        // showing the call sees it change as the work proceeds.
        Event::ToolProgress {
            tool_call_id,
            progress,
            ..
        } => SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            tool_call_id.clone(),
            ToolCallUpdateFields::new().title(progress.clone()),
        )),

        Event::ToolCompleted {
            tool_call_id,
            summary,
            is_error,
            ..
        } => SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            tool_call_id.clone(),
            ToolCallUpdateFields::new()
                .status(if *is_error {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                })
                .content(vec![text_block(summary).into()]),
        )),

        // The request itself is a `session/request_permission` round trip, not
        // an update — see `acp/approver.rs`. The resolution is reflected by
        // the tool call's own status.
        Event::PermissionRequested { .. } | Event::PermissionResolved { .. } => return None,

        // Concurrent work has no ACP vocabulary yet. Surfacing it as a thought
        // keeps the client informed instead of dropping it silently.
        Event::TaskUpdated {
            title: task_title,
            status,
            ..
        } => SessionUpdate::AgentThoughtChunk(chunk(&format!("[{status:?}] {task_title}"))),

        // Compaction rewrites the conversation the model can see. ACP has no
        // update kind for that, so it goes where every other "the operator
        // should know this" event goes — a thought chunk, the same shape a
        // retry takes below. Dropping it left a client unable to explain why
        // the agent stopped remembering something it was told twenty turns
        // ago, which is the single most confusing thing a long session does.
        Event::CompactionCompleted {
            replaced_items,
            preserved_items,
            ..
        } => SessionUpdate::AgentThoughtChunk(chunk(&format!(
            "context compacted: {replaced_items} earlier items replaced by a summary, \
             {preserved_items} kept"
        ))),

        // Its start is not, and the difference is what each one can tell a
        // client. `CompactionStarted` carries an agent id the notification
        // already names and nothing else, so the only chunk it could produce
        // is "compacting…" — a second line, arriving moments before the one
        // that says what actually happened, in a stream the user is already
        // watching a turn run in.
        Event::CompactionStarted { .. } => return None,

        // Request-only elision changes what the model receives without
        // rewriting the canonical transcript. ACP has no dedicated update for
        // that distinction, so report only the aggregate effect as a thought;
        // the detailed, body-free records remain available on the JSONL stream.
        Event::RequestToolResultsElided {
            canonical_tool_result_content_bytes,
            projected_tool_result_content_bytes,
            results,
            ..
        } => SessionUpdate::AgentThoughtChunk(chunk(&tool_result_elision_line(
            *canonical_tool_result_content_bytes,
            *projected_tool_result_content_bytes,
            results.len(),
        ))),

        // Housekeeping: real, and worth having on the JSONL stream, but not
        // something an ACP client renders. `UsageUpdate` is about the context
        // window rather than per-turn token counts, so basis does not pretend
        // these are the same thing — `Event::Usage` stays dropped here even
        // now that basis can sometimes know the window, because a
        // `UsageUpdate` needs both figures at once and this event carries
        // only the second. `server/turn.rs` sends the real thing separately,
        // once per turn and only when `PreparedRun::context_window` answers.
        Event::MemoryUpdated { .. } | Event::Usage { .. } | Event::Branched { .. } => return None,

        // Anything the operator should see becomes a thought chunk: a retry or
        // a recoverable error explains a pause the user is already watching.
        Event::Notice { message, .. } => SessionUpdate::AgentThoughtChunk(chunk(message)),
        Event::Retry {
            error,
            attempt,
            max_attempts,
            ..
        } => SessionUpdate::AgentThoughtChunk(chunk(&format!(
            "retrying after {error} (attempt {attempt}/{max_attempts})"
        ))),
        Event::Error { message, .. } => {
            SessionUpdate::AgentThoughtChunk(chunk(&format!("error: {message}")))
        }

        // A variant this build has no mapping for yet — the enum is
        // `#[non_exhaustive]`. Said where every other basis aside is said
        // rather than silently dropped; the wire tag is the honest name to
        // surface, and the payload, which could be arbitrarily large, is not.
        unknown => SessionUpdate::AgentThoughtChunk(chunk(&format!(
            "unmapped event: {}",
            unknown.type_tag()
        ))),
    };

    Some(update)
}

/// One line for the client to show, in the same place every other "the
/// operator should know this" line goes.
///
/// The server has one thing to say that no [`Event`] carries — that a
/// `/compact` found nothing to compact — and it says it through this rather
/// than assembling a `SessionUpdate` of its own, so there is still exactly one
/// answer to what a basis aside looks like on the wire.
pub(crate) fn thought(text: &str) -> SessionUpdate {
    SessionUpdate::AgentThoughtChunk(chunk(text))
}

fn chunk(text: &str) -> ContentChunk {
    ContentChunk::new(text_block(text))
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent::new(text.to_string()))
}

fn tool_result_elision_line(
    canonical_bytes: usize,
    projected_bytes: usize,
    changed: usize,
) -> String {
    let result = if changed == 1 { "result" } else { "results" };
    format!(
        "request tool results reduced: {canonical_bytes} -> {projected_bytes} bytes; \
         {changed} {result} changed"
    )
}

/// What to call the tool call in a client's UI.
///
/// mentra's summary is written for a person ("Run 'cargo test'"), so it is the
/// better title when there is one; the tool's name is the fallback.
fn title(summary: &str, tool_name: &str) -> String {
    if summary.trim().is_empty() {
        tool_name.to_string()
    } else {
        summary.to_string()
    }
}

/// Classifies a mentra tool for ACP's icon vocabulary.
///
/// Name-based, because for every tool but one that is all the information
/// there is — mentra reports a name and a mutability, not a category. Unknown
/// names fall back to mutability, and then to `Other`: a wrong icon is worse
/// than no icon, and a tool basis has never heard of is exactly the case where
/// guessing is unwise.
///
/// [`SPAWN`] is the exception, and the reason this takes the call's input at
/// all. Since ADR-0016 one name carries two acts — a command and a delegation —
/// so a name-keyed answer is necessarily wrong for one of them.
///
/// **The mutability fallback is only ever a last resort, and `files` is the
/// only builtin that still reaches it.** mentra reports `Unknown` on *every*
/// queued call (`session/mapping.rs`, `ToolUseReady`), and a queued call is the
/// one this function classifies — so for a name the map answers by mutability,
/// the mutability it reads is always `Unknown`. `files` keeps that arm because
/// one name genuinely carries both acts there and `Edit` is the conservative
/// render of a batch that may write. A tool that only ever reads must be
/// answered by name, or it renders as an edit for the whole time it is
/// pending: mentra's split `read` is exactly such a tool, and that is why it
/// sits on its own arm below rather than beside `files`.
fn tool_kind(tool_name: &str, mutability: Mutability, input: &Value) -> ToolKind {
    if tool_name == SPAWN {
        return spawn_kind(input);
    }

    match tool_name {
        "shell" | "bash" | "command" | "background_command" => ToolKind::Execute,
        // The batched profile's one tool, which is a read or a write depending
        // on the ops inside it. Nothing but mutability can answer, and at queue
        // time mutability says `Unknown` — so a pending `files` call renders as
        // an edit, which is the safe half to be wrong about.
        "files" => match mutability {
            Mutability::ReadOnly => ToolKind::Read,
            _ => ToolKind::Edit,
        },
        "read" | "read_file" => ToolKind::Read,
        "write" | "write_file" | "edit" | "edit_file" | "apply_patch" => ToolKind::Edit,
        "delete" | "remove" => ToolKind::Delete,
        "move" | "rename" => ToolKind::Move,
        // `ls` sits with `glob` and `find` rather than with `read`: what all
        // three return is *which paths exist*, never a file's contents, and
        // ACP's `Search` is the kind for locating things. `Read` would promise
        // a client that a file was opened.
        "search" | "grep" | "glob" | "find" | "ls" | "list_directory" => ToolKind::Search,
        "fetch" | "web_fetch" | "http" => ToolKind::Fetch,
        "think" | "load_skill" => ToolKind::Think,
        _ => match mutability {
            Mutability::ReadOnly => ToolKind::Read,
            Mutability::Mutating => ToolKind::Edit,
            Mutability::Unknown => ToolKind::Other,
        },
    }
}

/// The field `spawn` takes its one string in.
///
/// A literal because `basis` keeps the name private; see [`spawn_kind`] for
/// what that costs and what it does not.
const SPAWN_INPUT: &str = "input";

/// What ACP calls handing work to a subagent, which is nothing.
///
/// `ToolKind` in schema v1 offers `Read`, `Edit`, `Delete`, `Move`, `Search`,
/// `Execute`, `Think`, `Fetch`, `SwitchMode` and `Other`, and none of them
/// means delegation. `Think` is the nearest name and the wrong one: it promises
/// internal reasoning with nothing outside the process changed, while a
/// delegation is consequential enough that basis puts it to the approver, and the
/// subagent on the other side of it holds `spawn` in its own turn. Rendering
/// that as a thought would contradict the permission prompt the client has just
/// been asked to answer.
///
/// So `Other` — the schema's own default, and an honest "no category" — until
/// something can say `delegate`. ACP v2's `ToolKind::Unknown(String)` reserves
/// leading-underscore values for exactly this kind of extension; v1, which this
/// crate speaks, has no such escape.
const DELEGATION: ToolKind = ToolKind::Other;

/// `spawn`'s kind, which its *mode* decides rather than its name (ADR-0016).
///
/// The mode is re-derived from the raw string here, and that is a knowing
/// second reading of the convention `basis::tools::spawn` parses exactly
/// once. It is not free: if the two ever disagree — a new escape, a different
/// trim — a client renders the wrong icon and nothing says so. What keeps it
/// tolerable is that this path decides nothing. The typed
/// `{mode, body, cwd, target}` is what reaches the approver, the rule store,
/// the hooks and the audit trail;
/// what reaches ACP is a `ToolQueued` event carrying the string the model
/// wrote, and basis exports no reader for it. The fix is that reader,
/// exported from the crate that owns the convention — not a second copy that
/// grows.
fn spawn_kind(input: &Value) -> ToolKind {
    let Some(body) = input.get(SPAWN_INPUT).and_then(Value::as_str) else {
        // Nothing to read, and spawn's own preview will refuse this call before
        // it runs. Reported as the stronger of the two modes for the reason
        // spawn's static descriptor is `Process`: a tool that can run commands
        // should not describe itself as something milder when there is nothing
        // per-call to go on.
        return ToolKind::Execute;
    };

    match body.trim().strip_prefix('!') {
        // `!!` is the escape a task whose own text starts with `!` is written
        // with, so it is a delegation and not a command.
        Some(rest) if rest.starts_with('!') => DELEGATION,
        // Everything else after a single `!` is a command, `!@<target> …`
        // included: ADR-0021 made *where* a dimension of a command rather than
        // a third mode, so there is no third kind for a client to render.
        Some(_) => ToolKind::Execute,
        None => DELEGATION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use basis::event::{
        ElidedToolResult, NoticeSeverity, RequestToolResultElisionPolicy, RunOutcome,
        ToolResultContentKind, ToolResultElisionAction,
    };
    use serde_json::json;

    fn text_of(chunk: &ContentChunk) -> String {
        match &chunk.content {
            ContentBlock::Text(text) => text.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn assistant_deltas_become_message_chunks() {
        let update = session_update(&Event::AssistantDelta {
            text: "hello".to_string(),
        })
        .expect("mapped");

        let SessionUpdate::AgentMessageChunk(chunk) = update else {
            panic!("expected an agent message chunk");
        };
        assert_eq!(text_of(&chunk), "hello");
    }

    #[test]
    fn reasoning_is_a_thought_not_a_message() {
        // Rendering private reasoning as the answer would be a real leak of
        // one into the other.
        let update = session_update(&Event::AssistantReasoningDelta {
            text: "considering".to_string(),
        })
        .expect("mapped");

        assert!(matches!(update, SessionUpdate::AgentThoughtChunk(_)));
    }

    #[test]
    fn the_assembled_message_is_not_sent_after_its_own_deltas() {
        assert_eq!(
            session_update(&Event::AssistantMessage {
                text: "hello".to_string()
            }),
            None,
            "the deltas already carried this text; sending it again renders it twice"
        );
    }

    #[test]
    fn lan_bookends_are_not_acp_updates() {
        assert_eq!(
            session_update(&Event::RunFinished {
                outcome: RunOutcome::Ok,
                stopped_by: None,
                usage: None
            }),
            None
        );
        assert_eq!(
            session_update(&Event::UserMessage {
                text: "hi".to_string(),
                image_count: 0,
            }),
            None,
            "the client sent this; echoing it doubles it"
        );
    }

    #[test]
    fn a_queued_tool_call_carries_its_title_kind_and_input() {
        let update = session_update(&Event::ToolQueued {
            tool_call_id: "c1".to_string(),
            tool_name: "shell".to_string(),
            summary: "Run 'cargo test'".to_string(),
            mutability: Mutability::Mutating,
            input: json!({"command": "cargo test"}),
        })
        .expect("mapped");

        let SessionUpdate::ToolCall(call) = update else {
            panic!("expected a tool call");
        };
        assert_eq!(&*call.tool_call_id.0, "c1");
        assert_eq!(call.title, "Run 'cargo test'");
        assert_eq!(call.kind, ToolKind::Execute);
        assert_eq!(call.status, ToolCallStatus::Pending);
        assert_eq!(
            call.raw_input,
            Some(json!({"command": "cargo test"})),
            "a client showing what a call would do needs its real input"
        );
    }

    #[test]
    fn a_call_with_no_summary_falls_back_to_its_name() {
        let update = session_update(&Event::ToolQueued {
            tool_call_id: "c1".to_string(),
            tool_name: "files".to_string(),
            summary: "   ".to_string(),
            mutability: Mutability::ReadOnly,
            input: json!({}),
        })
        .expect("mapped");

        let SessionUpdate::ToolCall(call) = update else {
            panic!("expected a tool call");
        };
        assert_eq!(call.title, "files", "a blank title tells a client nothing");
    }

    #[test]
    fn a_completed_call_reports_success_or_failure() {
        for (is_error, expected) in [
            (false, ToolCallStatus::Completed),
            (true, ToolCallStatus::Failed),
        ] {
            let update = session_update(&Event::ToolCompleted {
                tool_call_id: "c1".to_string(),
                tool_name: "shell".to_string(),
                summary: "output".to_string(),
                is_error,
            })
            .expect("mapped");

            let SessionUpdate::ToolCallUpdate(call) = update else {
                panic!("expected a tool call update");
            };
            assert_eq!(call.fields.status, Some(expected));
        }
    }

    #[test]
    fn a_started_call_goes_in_progress() {
        let update = session_update(&Event::ToolStarted {
            tool_call_id: "c1".to_string(),
            tool_name: "shell".to_string(),
        })
        .expect("mapped");

        let SessionUpdate::ToolCallUpdate(call) = update else {
            panic!("expected a tool call update");
        };
        assert_eq!(call.fields.status, Some(ToolCallStatus::InProgress));
    }

    #[test]
    fn permission_events_are_a_round_trip_not_an_update() {
        assert_eq!(
            session_update(&Event::PermissionRequested {
                request_id: "r1".to_string(),
                tool_call_id: "c1".to_string(),
                tool_name: "shell".to_string(),
                description: "wants to run".to_string(),
                preview: json!({}),
            }),
            None,
            "a permission request is session/request_permission, not session/update"
        );
    }

    #[test]
    fn an_operator_facing_notice_reaches_the_client() {
        let update = session_update(&Event::Notice {
            severity: NoticeSeverity::Warning,
            message: "context is nearly full".to_string(),
        })
        .expect("mapped");

        let SessionUpdate::AgentThoughtChunk(chunk) = update else {
            panic!("expected a thought chunk");
        };
        assert!(text_of(&chunk).contains("context is nearly full"));
    }

    #[test]
    fn a_completed_compaction_tells_the_client_what_it_lost() {
        // The conversation the model can see has just been rewritten. A client
        // that heard nothing has no way to explain why the agent stopped
        // remembering what it was told earlier in the same session.
        let update = session_update(&Event::CompactionCompleted {
            agent_id: "agent-1".to_string(),
            replaced_items: 42,
            preserved_items: 8,
            transcript_len: 50,
            extracted_facts: 3,
            summary_preview: "the user is refactoring the parser".to_string(),
        })
        .expect("mapped");

        let SessionUpdate::AgentThoughtChunk(chunk) = update else {
            panic!("expected a thought chunk");
        };
        let text = text_of(&chunk);
        assert!(
            text.contains("42") && text.contains('8'),
            "both counts are what makes this more than a rumor: {text}"
        );
    }

    #[test]
    fn a_started_compaction_says_nothing_the_completion_will_not() {
        // It carries an agent id the notification already names, so the only
        // chunk it could produce is "compacting…" — noise arriving moments
        // before the line that says what happened.
        assert_eq!(
            session_update(&Event::CompactionStarted {
                agent_id: "agent-1".to_string()
            }),
            None
        );
    }

    #[test]
    fn request_tool_result_elision_is_a_concise_thought() {
        let update = session_update(&Event::RequestToolResultsElided {
            agent_id: "agent-1".to_string(),
            policy: RequestToolResultElisionPolicy::ByteBudget {
                configured_max_bytes: 4_096,
                configured_prioritize_recent_results: 2,
                configured_max_preview_bytes: 512,
            },
            canonical_tool_result_content_bytes: 8_192,
            projected_tool_result_content_bytes: 4_096,
            results: vec![ElidedToolResult {
                tool_call_id: "call-1".to_string(),
                tool_name: Some("grep".to_string()),
                is_error: false,
                canonical_content_kind: ToolResultContentKind::Text,
                action: ToolResultElisionAction::Preview,
                canonical_content_bytes: 8_192,
                projected_content_bytes: 4_096,
            }],
        })
        .expect("mapped");

        let SessionUpdate::AgentThoughtChunk(chunk) = update else {
            panic!("expected a thought chunk");
        };
        assert_eq!(
            text_of(&chunk),
            "request tool results reduced: 8192 -> 4096 bytes; 1 result changed"
        );
    }

    #[test]
    fn tool_kinds_follow_the_name_then_the_mutability() {
        let no_input = json!({});

        assert_eq!(
            tool_kind("shell", Mutability::Mutating, &no_input),
            ToolKind::Execute
        );
        assert_eq!(
            tool_kind("files", Mutability::ReadOnly, &no_input),
            ToolKind::Read
        );
        assert_eq!(
            tool_kind("files", Mutability::Mutating, &no_input),
            ToolKind::Edit
        );
        assert_eq!(
            tool_kind("grep", Mutability::ReadOnly, &no_input),
            ToolKind::Search
        );

        // An unknown tool falls back to what mentra says it does, and admits
        // ignorance when mentra does not know either.
        //
        // (The split-file-tool cases live in their own test below, because the
        // property that matters for them is that they *never* consult the
        // fallback.)
        assert_eq!(
            tool_kind("something_new", Mutability::ReadOnly, &no_input),
            ToolKind::Read
        );
        assert_eq!(
            tool_kind("something_new", Mutability::Unknown, &no_input),
            ToolKind::Other
        );
    }

    #[test]
    fn the_split_file_tools_are_classified_by_name_at_every_mutability() {
        // The default roster since `RuntimeBuilder::with_file_tools`, and the
        // reason each has to be answered by name: `ToolQueued` carries
        // `Mutability::Unknown` always, so anything reaching the fallback
        // renders as `Other` — and anything on the old `files` arm renders as
        // an *edit*, which is what a pending `read` used to show as. Every
        // mutability is asserted because the right answer here does not depend
        // on one.
        let no_input = json!({});

        for mutability in [
            Mutability::ReadOnly,
            Mutability::Mutating,
            Mutability::Unknown,
        ] {
            for (name, expected) in [
                ("read", ToolKind::Read),
                ("ls", ToolKind::Search),
                ("grep", ToolKind::Search),
                ("glob", ToolKind::Search),
                ("write", ToolKind::Edit),
                ("edit", ToolKind::Edit),
            ] {
                assert_eq!(
                    tool_kind(name, mutability, &no_input),
                    expected,
                    "{name} at {mutability:?}"
                );
            }
        }
    }

    #[test]
    fn spawn_is_classified_by_its_mode_rather_than_by_its_name() {
        // The one tool whose name cannot answer for it. mentra reports
        // `Unknown` mutability on every queued call, so before ADR-0016's map
        // both of these rendered as `Other`.
        assert_eq!(
            tool_kind(
                SPAWN,
                Mutability::Unknown,
                &json!({"input": "!cargo test -q"})
            ),
            ToolKind::Execute,
            "a command is what `shell` always was"
        );
        assert_eq!(
            tool_kind(
                SPAWN,
                Mutability::Unknown,
                &json!({"input": "find every TODO under src/"})
            ),
            ToolKind::Other,
            "ACP v1 has no kind meaning delegation, and `Think` would understate it"
        );
        assert_eq!(
            tool_kind(
                SPAWN,
                Mutability::Unknown,
                &json!({"input": "  !!urgent: rewrite the README"})
            ),
            ToolKind::Other,
            "`!!` escapes a task whose own text starts with `!`; it is not a command"
        );
        assert_eq!(
            tool_kind(
                SPAWN,
                Mutability::Unknown,
                &json!({"input": "!@mac xcodebuild -list"})
            ),
            ToolKind::Execute,
            "ADR-0021 made *where* a dimension of a command, not a third mode: \
             a routed command still renders as an execution"
        );
    }

    #[test]
    fn an_unreadable_spawn_call_reports_the_stronger_mode() {
        // Nothing per-call to go on, so this answers as spawn's static
        // descriptor does: `Process`, never the milder of the two.
        for input in [json!({}), json!({"input": 7}), json!("!cargo test")] {
            assert_eq!(
                tool_kind(SPAWN, Mutability::Unknown, &input),
                ToolKind::Execute,
                "{input}"
            );
        }
    }

    #[test]
    fn a_queued_spawn_command_reaches_the_client_as_an_execution() {
        // The mode lives in the input, so this pins the wiring as well as the
        // classifier: a call site that forgot to pass the input would still
        // satisfy the tests above.
        let update = session_update(&Event::ToolQueued {
            tool_call_id: "c1".to_string(),
            tool_name: SPAWN.to_string(),
            summary: "Run 'cargo test'".to_string(),
            mutability: Mutability::Unknown,
            input: json!({"input": "!cargo test"}),
        })
        .expect("mapped");

        let SessionUpdate::ToolCall(call) = update else {
            panic!("expected a tool call");
        };
        assert_eq!(call.kind, ToolKind::Execute);
    }
}
