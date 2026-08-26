//! Normalization from mentra's [`SessionEvent`] to basis's [`Event`].
//!
//! The match is deliberately exhaustive with no wildcard arm. When mentra
//! grows an event, basis fails to compile rather than dropping it silently — a
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

/// Maps one session event, or `None` when basis's stream already carries the
/// same information.
pub(super) fn from_session_event(event: &SessionEvent) -> Option<Event> {
    let mapped = match event {
        // The stream header already names the session, and it is emitted
        // before the subscription starts, so this would be a duplicate.
        SessionEvent::SessionStarted { .. } => return None,

        SessionEvent::UserMessage { text, image_count } => Event::UserMessage {
            text: text.clone(),
            image_count: *image_count,
        },
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
            // Deliberately not carried onto basis's Event: adding it is a
            // wire-format decision, deferred until a consumer needs it.
            classification: _,
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
            reasoning_tokens,
            thoughts_tokens,
        } => Event::Usage {
            agent_id: agent_id.clone(),
            input_tokens: *input_tokens,
            output_tokens: *output_tokens,
            cache_read_tokens: *cache_read_tokens,
            cache_creation_tokens: *cache_creation_tokens,
            reasoning_tokens: *reasoning_tokens,
            thoughts_tokens: *thoughts_tokens,
        },
        SessionEvent::Notice { severity, message }
            if is_refused_memory_write(severity, message) =>
        {
            // The one notice basis drops by decision rather than maps, and
            // dropping it is the decision D2 already made: the file store
            // refuses a long-term-memory write (mentra
            // `runtime/file_store/delegated.rs`), and mentra reports the
            // refusal after every compaction ingests its summary. basis
            // switched mentra's memory engine off — nothing recalls from that
            // store and no tool reaches it — so under SQLite the same write
            // "succeeded" into a table nothing ever read, invisibly. A
            // warning that the unused write now fails is a fact about a
            // decision, not about the run, and its advice (enable a mentra
            // cargo feature) is addressed to a mentra embedder, which a basis
            // operator is not.
            return None;
        }
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
        SessionEvent::Branched {
            entry_id,
            abandoned_entries,
        } => Event::Branched {
            entry_id: entry_id.clone(),
            abandoned_entries: *abandoned_entries,
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

/// Whether this notice is the file store refusing the memory write basis
/// already decided not to use — the one notice the mapping drops.
///
/// Deliberately narrow on both axes, because a mapping that swallows an event
/// is the one place a harness can hide something from its clients.
///
/// **Severity**: `Warning` only. mentra composes this notice in exactly one
/// place (`SessionHookBridge`, on a failed `MemoryIngestFinished`) and sends
/// it at `Warning`, so anything arriving at another severity did not come
/// from there and is not this. `Info` is the only other severity mentra has;
/// if a future one carries this text, it reaches the stream.
///
/// **Text**: the whole of what upstream composes around the store error —
/// `RuntimeError::Store`'s own `runtime store error: ` prefix followed by the
/// file store's sentence — rather than a loose phrase from the middle of it.
/// A message merely *mentioning* long-term memory, or reporting a different
/// failure that happens to quote this one, keeps its place on the stream.
/// Matched as text because mentra gives the notice no structure to match on
/// (upstream candidate); if the wording moves this fails open and the warning
/// reappears, which is visible rather than dangerous.
fn is_refused_memory_write(severity: &MentraNoticeSeverity, message: &str) -> bool {
    const REFUSAL: &str = "runtime store error: FileRuntimeStore does not persist long-term memory";

    matches!(severity, MentraNoticeSeverity::Warning) && message.contains(REFUSAL)
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
    fn the_refused_memory_write_is_a_decision_not_a_run_fact() {
        // The file store refuses long-term-memory writes and mentra reports
        // it after every compaction's summary ingest. basis switched that
        // engine off (D2), so the warning describes a write nothing would
        // ever have read — dropped by decision, see the mapping arm.
        let refused = SessionEvent::Notice {
            severity: MentraNoticeSeverity::Warning,
            message: refused_memory_write(),
        };

        assert_eq!(from_session_event(&refused), None);
    }

    /// Exactly what mentra composes for this notice: `SessionHookBridge`
    /// wraps the agent id around the failed ingest's error, and that error is
    /// `RuntimeError::Store`'s Display around the file store's own sentence.
    fn refused_memory_write() -> String {
        "agent 'a-1': runtime store error: FileRuntimeStore does not persist long-term memory; \
         enable mentra's `store-sqlite` feature and use SqliteRuntimeStore or HybridRuntimeStore \
         for durable memory"
            .to_string()
    }

    #[test]
    fn only_a_warning_carrying_that_exact_refusal_is_dropped() {
        // A mapping that swallows an event is the one place a harness can
        // hide something from its clients, so the drop is narrow on both axes
        // and each is pinned here. `Info` stands in for "any severity but the
        // one upstream sends this at" — mentra's enum has no third.
        let elsewhere = SessionEvent::Notice {
            severity: MentraNoticeSeverity::Info,
            message: refused_memory_write(),
        };
        assert_eq!(
            from_session_event(&elsewhere),
            Some(Event::Notice {
                severity: NoticeSeverity::Info,
                message: refused_memory_write(),
            }),
            "the same text at a severity upstream never sends it at did not come from there"
        );

        // A different memory failure that merely mentions the same subject —
        // and any other notice at all — keeps its place on the stream.
        for message in [
            "agent 'a-1': runtime store error: could not save the memory cursor",
            "agent 'a-1': long-term memory is unavailable here",
            "something else worth hearing",
        ] {
            let ordinary = SessionEvent::Notice {
                severity: MentraNoticeSeverity::Warning,
                message: message.to_string(),
            };
            assert_eq!(
                from_session_event(&ordinary),
                Some(Event::Notice {
                    severity: NoticeSeverity::Warning,
                    message: message.to_string(),
                }),
                "{message}"
            );
        }
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
