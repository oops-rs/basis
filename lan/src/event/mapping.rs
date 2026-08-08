//! Normalization from mentra's [`SessionEvent`] to lan's [`Event`].
//!
//! The match is deliberately exhaustive with no wildcard arm. When mentra
//! grows an event, lan fails to compile rather than dropping it silently — a
//! new kind of thing happening during a run is exactly what a harness must not
//! hide from its clients.
//!
//! [`SessionEvent`]: mentra::SessionEvent

use mentra::{
    SessionEvent,
    session::{
        NoticeSeverity as MentraNoticeSeverity, PermissionOutcome as MentraPermissionOutcome,
        PermissionRuleScope, TaskKind as MentraTaskKind, TaskLifecycleStatus, ToolMutability,
    },
};
use serde_json::Value;

use super::{
    Event, Mutability, NoticeSeverity, PermissionOutcome, RuleScope, TaskKind, TaskStatus,
};

/// Maps one session event, or `None` when lan's stream already carries the
/// same information.
pub(super) fn from_session_event(event: &SessionEvent) -> Option<Event> {
    let mapped = match event {
        // The stream header already names the session, and it is emitted
        // before the subscription starts, so this would be a duplicate.
        SessionEvent::SessionStarted { .. } => return None,

        SessionEvent::UserMessage { text } => Event::UserMessage { text: text.clone() },
        SessionEvent::AssistantTokenDelta { delta, .. } => Event::AssistantDelta {
            text: delta.clone(),
        },
        SessionEvent::AssistantReasoningDelta { delta, .. } => Event::AssistantReasoningDelta {
            text: delta.clone(),
        },
        SessionEvent::AssistantMessageCompleted { text } => {
            Event::AssistantMessage { text: text.clone() }
        }

        SessionEvent::ToolQueued {
            tool_call_id,
            tool_name,
            summary,
            mutability,
            input_json,
        } => Event::ToolQueued {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            summary: summary.clone(),
            mutability: mutability_of(*mutability),
            input: json_or_string(input_json),
        },
        SessionEvent::ToolStarted {
            tool_call_id,
            tool_name,
        } => Event::ToolStarted {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
        },
        SessionEvent::ToolProgress {
            tool_call_id,
            tool_name,
            progress,
        } => Event::ToolProgress {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            progress: progress.clone(),
        },
        SessionEvent::ToolCompleted {
            tool_call_id,
            tool_name,
            summary,
            is_error,
        } => Event::ToolCompleted {
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            summary: summary.clone(),
            is_error: *is_error,
        },

        SessionEvent::PermissionRequested {
            request_id,
            tool_call_id,
            tool_name,
            description,
            preview,
        } => Event::PermissionRequested {
            request_id: request_id.clone(),
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            description: description.clone(),
            preview: json_or_string(preview),
        },
        SessionEvent::PermissionResolved {
            request_id,
            tool_call_id,
            tool_name,
            outcome,
            rule_scope,
        } => Event::PermissionResolved {
            request_id: request_id.clone(),
            tool_call_id: tool_call_id.clone(),
            tool_name: tool_name.clone(),
            outcome: outcome_of(*outcome),
            rule_scope: rule_scope.map(scope_of),
        },

        SessionEvent::TaskUpdated {
            task_id,
            kind,
            status,
            title,
            detail,
        } => Event::TaskUpdated {
            task_id: task_id.clone(),
            kind: task_kind_of(*kind),
            status: task_status_of(*status),
            title: title.clone(),
            detail: detail.clone(),
        },

        SessionEvent::CompactionStarted { agent_id } => Event::CompactionStarted {
            agent_id: agent_id.clone(),
        },
        SessionEvent::CompactionCompleted {
            agent_id,
            replaced_items,
            preserved_items,
            resulting_transcript_len,
            extracted_facts_count,
            summary_preview,
        } => Event::CompactionCompleted {
            agent_id: agent_id.clone(),
            replaced_items: *replaced_items,
            preserved_items: *preserved_items,
            transcript_len: *resulting_transcript_len,
            extracted_facts: *extracted_facts_count,
            summary_preview: summary_preview.clone(),
        },
        SessionEvent::MemoryUpdated {
            agent_id,
            stored_records,
        } => Event::MemoryUpdated {
            agent_id: agent_id.clone(),
            stored_records: *stored_records,
        },

        SessionEvent::UsageReport {
            agent_id,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
        } => Event::Usage {
            agent_id: agent_id.clone(),
            input_tokens: *input_tokens,
            output_tokens: *output_tokens,
            cache_read_tokens: *cache_read_tokens,
            cache_creation_tokens: *cache_creation_tokens,
        },
        SessionEvent::Notice { severity, message } => Event::Notice {
            severity: severity_of(*severity),
            message: message.clone(),
        },
        SessionEvent::RetryAttempt {
            agent_id,
            error_message,
            attempt,
            max_attempts,
            next_delay_ms,
        } => Event::Retry {
            agent_id: agent_id.clone(),
            error: error_message.clone(),
            attempt: *attempt,
            max_attempts: *max_attempts,
            next_delay_ms: *next_delay_ms,
        },
        SessionEvent::Error {
            message,
            recoverable,
        } => Event::Error {
            message: message.clone(),
            recoverable: *recoverable,
        },
    };

    Some(mapped)
}

/// Mentra carries tool input and permission previews as JSON-encoded strings.
/// A client should not have to parse a string out of a JSON document to get at
/// JSON, so parse here; a value that is not JSON passes through as a string.
fn json_or_string(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string()))
}

fn mutability_of(value: ToolMutability) -> Mutability {
    match value {
        ToolMutability::ReadOnly => Mutability::ReadOnly,
        ToolMutability::Mutating => Mutability::Mutating,
        ToolMutability::Unknown => Mutability::Unknown,
    }
}

fn outcome_of(value: MentraPermissionOutcome) -> PermissionOutcome {
    match value {
        MentraPermissionOutcome::Allowed => PermissionOutcome::Allowed,
        MentraPermissionOutcome::Denied => PermissionOutcome::Denied,
    }
}

fn scope_of(value: PermissionRuleScope) -> RuleScope {
    match value {
        PermissionRuleScope::Session => RuleScope::Session,
        PermissionRuleScope::Project => RuleScope::Project,
        PermissionRuleScope::Global => RuleScope::Global,
    }
}

fn task_kind_of(value: MentraTaskKind) -> TaskKind {
    match value {
        MentraTaskKind::Subagent => TaskKind::Subagent,
        MentraTaskKind::BackgroundTask => TaskKind::BackgroundTask,
        MentraTaskKind::Teammate => TaskKind::Teammate,
    }
}

fn task_status_of(value: TaskLifecycleStatus) -> TaskStatus {
    match value {
        TaskLifecycleStatus::Spawned => TaskStatus::Spawned,
        TaskLifecycleStatus::Running => TaskStatus::Running,
        TaskLifecycleStatus::Finished => TaskStatus::Finished,
        TaskLifecycleStatus::Failed => TaskStatus::Failed,
    }
}

fn severity_of(value: MentraNoticeSeverity) -> NoticeSeverity {
    match value {
        MentraNoticeSeverity::Info => NoticeSeverity::Info,
        MentraNoticeSeverity::Warning => NoticeSeverity::Warning,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_started_is_already_covered_by_the_header() {
        let event = SessionEvent::SessionStarted {
            session_id: mentra::SessionId::new(),
        };

        assert_eq!(from_session_event(&event), None);
    }

    #[test]
    fn token_deltas_carry_only_the_delta() {
        // `full_text` is the accumulated message; re-sending it on every token
        // would make the stream quadratic in the response length.
        let event = SessionEvent::AssistantTokenDelta {
            delta: "lo".to_string(),
            full_text: "hello".to_string(),
        };

        assert_eq!(
            from_session_event(&event),
            Some(Event::AssistantDelta {
                text: "lo".to_string()
            })
        );
    }

    #[test]
    fn tool_input_is_parsed_into_real_json() {
        let event = SessionEvent::ToolQueued {
            tool_call_id: "c1".to_string(),
            tool_name: "shell".to_string(),
            summary: "run ls".to_string(),
            mutability: ToolMutability::ReadOnly,
            input_json: r#"{"command":"ls"}"#.to_string(),
        };

        let Some(Event::ToolQueued { input, .. }) = from_session_event(&event) else {
            panic!("expected a tool_queued event");
        };
        assert_eq!(input["command"], "ls");
    }

    #[test]
    fn unparseable_tool_input_survives_as_a_string() {
        let event = SessionEvent::ToolQueued {
            tool_call_id: "c1".to_string(),
            tool_name: "shell".to_string(),
            summary: String::new(),
            mutability: ToolMutability::Unknown,
            input_json: "not json {".to_string(),
        };

        let Some(Event::ToolQueued { input, .. }) = from_session_event(&event) else {
            panic!("expected a tool_queued event");
        };
        assert_eq!(input, Value::String("not json {".to_string()));
    }

    #[test]
    fn permission_resolution_keeps_the_remembered_scope() {
        let event = SessionEvent::PermissionResolved {
            request_id: "r1".to_string(),
            tool_call_id: "c1".to_string(),
            tool_name: "shell".to_string(),
            outcome: MentraPermissionOutcome::Allowed,
            rule_scope: Some(PermissionRuleScope::Project),
        };

        let Some(Event::PermissionResolved {
            outcome,
            rule_scope,
            ..
        }) = from_session_event(&event)
        else {
            panic!("expected a permission_resolved event");
        };
        assert_eq!(outcome, PermissionOutcome::Allowed);
        assert_eq!(rule_scope, Some(RuleScope::Project));
    }

    #[test]
    fn errors_keep_their_recoverability() {
        let event = SessionEvent::Error {
            message: "rate limited".to_string(),
            recoverable: true,
        };

        assert_eq!(
            from_session_event(&event),
            Some(Event::Error {
                message: "rate limited".to_string(),
                recoverable: true,
            })
        );
    }
}
