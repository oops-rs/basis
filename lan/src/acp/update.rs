//! lan's [`Event`] stream, as ACP `session/update` notifications.
//!
//! This is the reason [`Event`] exists. mentra's `SessionEvent` is normalized
//! once, in `event/mapping.rs`, and every surface downstream — JSONL, ACP,
//! whatever comes next — maps from lan's own shape. Nothing here touches
//! mentra.
//!
//! The match is exhaustive with no wildcard, for the same reason the mentra
//! mapping is: a new [`Event`] variant should break this build rather than
//! quietly stop reaching ACP clients.

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, SessionUpdate, TextContent, ToolCall, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};

use crate::event::{Event, Mutability};

/// Maps one lan event to an ACP session update.
///
/// `None` where ACP has no place for it — lan's own bookends, and the
/// housekeeping events (usage, compaction, retries) that ACP models
/// differently or not at all. Those still reach a JSONL consumer; they are
/// simply not part of this protocol's vocabulary.
pub fn session_update(event: &Event) -> Option<SessionUpdate> {
    let update = match event {
        // lan's bookends. `session/prompt` returning is what tells an ACP
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
                .kind(tool_kind(tool_name, *mutability))
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

        // Housekeeping: real, and worth having on the JSONL stream, but not
        // something an ACP client renders. `UsageUpdate` is about the context
        // window rather than per-turn token counts, so lan does not pretend
        // these are the same thing.
        Event::CompactionStarted { .. }
        | Event::CompactionCompleted { .. }
        | Event::MemoryUpdated { .. }
        | Event::Usage { .. }
        | Event::Branched { .. } => return None,

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
    };

    Some(update)
}

fn chunk(text: &str) -> ContentChunk {
    ContentChunk::new(text_block(text))
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text(TextContent::new(text.to_string()))
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
/// Name-based, because that is all the information there is — mentra reports a
/// name and a mutability, not a category. Unknown names fall back to
/// mutability, and then to `Other`: a wrong icon is worse than no icon, and a
/// tool lan has never heard of is exactly the case where guessing is unwise.
fn tool_kind(tool_name: &str, mutability: Mutability) -> ToolKind {
    match tool_name {
        "shell" | "bash" | "command" | "background_command" => ToolKind::Execute,
        "files" | "read" | "read_file" => match mutability {
            Mutability::ReadOnly => ToolKind::Read,
            _ => ToolKind::Edit,
        },
        "write" | "write_file" | "edit" | "edit_file" | "apply_patch" => ToolKind::Edit,
        "delete" | "remove" => ToolKind::Delete,
        "move" | "rename" => ToolKind::Move,
        "search" | "grep" | "glob" | "find" => ToolKind::Search,
        "fetch" | "web_fetch" | "http" => ToolKind::Fetch,
        "think" | "load_skill" => ToolKind::Think,
        _ => match mutability {
            Mutability::ReadOnly => ToolKind::Read,
            Mutability::Mutating => ToolKind::Edit,
            Mutability::Unknown => ToolKind::Other,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{NoticeSeverity, RunOutcome};
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
                outcome: RunOutcome::Ok
            }),
            None
        );
        assert_eq!(
            session_update(&Event::UserMessage {
                text: "hi".to_string()
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
    fn tool_kinds_follow_the_name_then_the_mutability() {
        assert_eq!(tool_kind("shell", Mutability::Mutating), ToolKind::Execute);
        assert_eq!(tool_kind("files", Mutability::ReadOnly), ToolKind::Read);
        assert_eq!(tool_kind("files", Mutability::Mutating), ToolKind::Edit);
        assert_eq!(tool_kind("grep", Mutability::ReadOnly), ToolKind::Search);

        // An unknown tool falls back to what mentra says it does, and admits
        // ignorance when mentra does not know either.
        assert_eq!(
            tool_kind("something_new", Mutability::ReadOnly),
            ToolKind::Read
        );
        assert_eq!(
            tool_kind("something_new", Mutability::Unknown),
            ToolKind::Other
        );
    }
}
