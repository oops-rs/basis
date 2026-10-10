//! Hops and deadlines across the wire (ADR-0030 §9; proposal 0005 §2):
//! the server half of `docs/a2a/hops-v1.md`.
//!
//! Two agents can call each other. Within one data directory `basis-tasks`'
//! wait-edge policy keeps that from cycling; across the wire nothing did, so
//! a loop cost a full deadline at every hop. A message now says how many
//! hops from a top-level caller it is and when its caller gives up, and a
//! served task is bounded by both: refused past this server's hop cap, and
//! never given longer than its caller will wait. Absent metadata is hop 0
//! with no deadline of the caller's, so agents that do not speak the
//! extension interoperate unchanged.

use std::time::Duration;

use a2a::{A2AError, Message};
use chrono::{DateTime, Utc};
use serde_json::Value;

/// The hops extension's URI: the card declares it, and a message's metadata
/// carries `{"hop", "deadline"}` under it.
pub(crate) const HOPS_EXTENSION: &str =
    "https://github.com/oops-rs/basis/blob/main/docs/a2a/hops-v1.md";

/// The deepest hop served when the operator names no other: deep enough for
/// any honest chain of delegation, shallow enough that a loop ends long
/// before its deadlines would end it.
pub const DEFAULT_HOP_CAP: u32 = 8;

/// Where a message says it stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Inbound {
    pub(crate) hop: u32,
    /// When the caller stops waiting, in milliseconds since the epoch.
    pub(crate) deadline_ms: Option<u64>,
}

/// Reads `message`'s hop and deadline. A message without the extension is
/// hop 0 with no deadline of the caller's; one that carries it malformed is
/// refused rather than guessed at.
pub(crate) fn inbound(message: &Message) -> Result<Inbound, A2AError> {
    let Some(value) = message
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get(HOPS_EXTENSION))
    else {
        return Ok(Inbound {
            hop: 0,
            deadline_ms: None,
        });
    };
    let malformed = |what: &str| {
        A2AError::invalid_params(format!(
            "the hops extension's metadata needs {what}; see {HOPS_EXTENSION}"
        ))
    };
    let hop = value
        .get("hop")
        .and_then(Value::as_u64)
        .and_then(|hop| u32::try_from(hop).ok())
        .ok_or_else(|| malformed("a non-negative integer `hop`"))?;
    let deadline_ms = match value.get("deadline") {
        None | Some(Value::Null) => None,
        Some(deadline) => {
            let deadline = deadline
                .as_str()
                .and_then(|deadline| DateTime::parse_from_rfc3339(deadline).ok())
                .ok_or_else(|| malformed("an RFC 3339 `deadline`"))?;
            Some(u64::try_from(deadline.with_timezone(&Utc).timestamp_millis()).unwrap_or(0))
        }
    };
    Ok(Inbound { hop, deadline_ms })
}

/// Refuses a message past `cap`.
pub(crate) fn check_cap(inbound: Inbound, cap: u32) -> Result<(), A2AError> {
    if inbound.hop > cap {
        return Err(A2AError::invalid_params(format!(
            "this message is {} hops from its top-level caller, past this agent's cap of {cap}; \
             the chain of delegation is too deep, or loops",
            inbound.hop
        )));
    }
    Ok(())
}

/// The deadline a task is given: its own, clamped to what its caller has
/// left at `now_ms`. A deadline only ever shrinks across a hop; one already
/// past is refused, since no work could finish inside it.
pub(crate) fn effective_deadline(
    own: Duration,
    inbound: Inbound,
    now_ms: u64,
) -> Result<Duration, A2AError> {
    let Some(caller_ms) = inbound.deadline_ms else {
        return Ok(own);
    };
    let left = caller_ms
        .checked_sub(now_ms)
        .filter(|left| *left > 0)
        .ok_or_else(|| A2AError::invalid_params("the caller's deadline has already passed"))?;
    Ok(own.min(Duration::from_millis(left)))
}

#[cfg(test)]
mod tests {
    use a2a::{Part, Role, error_code};
    use serde_json::json;

    use super::*;

    fn message(metadata: Option<Value>) -> Message {
        Message {
            metadata: metadata
                .map(|value| [(HOPS_EXTENSION.to_string(), value)].into_iter().collect()),
            ..Message::new(Role::User, vec![Part::text("hi")])
        }
    }

    #[test]
    fn absent_metadata_is_hop_zero_with_no_deadline() {
        assert_eq!(
            inbound(&message(None)).unwrap(),
            Inbound {
                hop: 0,
                deadline_ms: None
            }
        );
    }

    #[test]
    fn a_hop_and_deadline_are_read_and_malformed_ones_refused() {
        let read = inbound(&message(Some(json!({
            "hop": 2, "deadline": "2026-10-10T12:00:00Z",
        }))))
        .unwrap();
        assert_eq!(read.hop, 2);
        assert_eq!(read.deadline_ms, Some(1_791_633_600_000));
        for bad in [
            json!({"hop": -1}),
            json!({"hop": "two"}),
            json!({}),
            json!({"hop": 1, "deadline": "tomorrow"}),
        ] {
            assert_eq!(
                inbound(&message(Some(bad.clone()))).unwrap_err().code,
                error_code::INVALID_PARAMS,
                "{bad}"
            );
        }
    }

    #[test]
    fn a_hop_past_the_cap_is_refused() {
        let at = |hop| Inbound {
            hop,
            deadline_ms: None,
        };
        assert!(check_cap(at(8), 8).is_ok());
        let error = check_cap(at(9), 8).unwrap_err();
        assert!(error.message.contains("cap of 8"), "{}", error.message);
    }

    #[test]
    fn a_deadline_only_shrinks() {
        let own = Duration::from_secs(600);
        let caller = |deadline_ms| Inbound {
            hop: 1,
            deadline_ms: Some(deadline_ms),
        };
        assert_eq!(
            effective_deadline(own, caller(1_000 + 60_000), 1_000).unwrap(),
            Duration::from_secs(60)
        );
        assert_eq!(
            effective_deadline(own, caller(1_000 + 6_000_000), 1_000).unwrap(),
            own
        );
        assert!(effective_deadline(own, caller(1_000), 1_000).is_err());
        assert!(effective_deadline(own, caller(10), 1_000).is_err());
    }
}
