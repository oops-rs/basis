//! Tailing a task's event journal from outside its execution.
//!
//! [`EventCursor`] is a pull-based iterator over `events.jsonl`: replay from
//! the start by default, one `poll` per batch of whole lines appended since
//! the last one. It does not attach, drive, or wait — a watcher only ever
//! observes (ADR-0019) — so a host composes its own cadence and its own
//! terminal check around it, exactly as `basis watch` does.

use serde::Deserialize;
use serde_json::Value;

use crate::{Error, events::EventTail};

/// One journal record: the flat `EventLine` shape exactly as written —
/// `{"seq":N,"type":...}`, whichever vintage of journal it came from
/// (`events::EventTail` already normalizes the pre-0.6 nested wrapper) — with
/// basis's typed [`basis::Event`] alongside it, when this build recognizes
/// the `type` it names.
///
/// `raw` is never re-derived from `event`: `basis::Event`'s own fields do not
/// include `seq` (the journal writer splices it in), so reserializing `event`
/// would drop it. `raw` is the wire contract (ADR-0015) — what `--json`
/// output reproduces verbatim — and `event` is read-side convenience for a
/// host that would rather match on a type.
#[derive(Debug, Clone)]
pub struct WatchRecord {
    /// This record's sequence number, monotonic within one task — `None`
    /// only for a line whose `seq` is missing or not a number, which no
    /// writer this crate ever produces but a hand-edited or foreign-written
    /// journal could. Never silently `0`: that is a real sequence number
    /// (the journal's first line), and reporting it for a line that carried
    /// none would be indistinguishable from that line.
    pub seq: Option<u64>,
    /// The exact JSON on disk.
    pub raw: Value,
    /// `raw`, typed — `None` for a record a newer basis wrote with a `type`
    /// this build does not know. The enum is `#[non_exhaustive]` for exactly
    /// this: a host can still show or forward `raw` for a record it cannot
    /// fully type.
    pub event: Option<Box<basis::Event>>,
}

/// A cursor over one task's event journal.
pub struct EventCursor {
    tail: EventTail,
}

impl EventCursor {
    pub(crate) fn new(tail: EventTail) -> Self {
        Self { tail }
    }

    /// Every whole record appended since the last call, oldest first.
    ///
    /// Empty rather than blocking when nothing is new — a host polls this at
    /// its own pace, sleeping between calls exactly as `basis watch` does.
    pub fn poll(&mut self) -> Result<Vec<WatchRecord>, Error> {
        let records = self
            .tail
            .poll()
            .map_err(|error| Error::new(format!("read task events: {error}")))?;
        Ok(records.into_iter().map(build_record).collect())
    }
}

/// A raw journal line becomes a [`WatchRecord`]. `events::EventTail` already
/// refuses to yield a line whose `seq` does not parse — this crate's own
/// `WatchRecord::seq` still reads it back independently rather than trusting
/// that filter to hold forever, because a public type's contract should not
/// depend on an internal module's current strictness.
fn build_record(raw: Value) -> WatchRecord {
    let seq = raw.get("seq").and_then(Value::as_u64);
    let event = basis::Event::deserialize(&raw).ok().map(Box::new);
    WatchRecord { seq, raw, event }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_with_a_numeric_seq_carries_it() {
        let record = build_record(serde_json::json!({"seq": 3, "type": "notice", "message": "hi"}));
        assert_eq!(record.seq, Some(3));
    }

    /// Never silently `0`: that is a real sequence number, the journal's
    /// first line, and reporting it for a line that carried none would be
    /// indistinguishable from that line.
    #[test]
    fn a_record_with_no_seq_is_none_not_zero() {
        let record = build_record(serde_json::json!({"type": "notice", "message": "hi"}));
        assert_eq!(record.seq, None);

        let not_a_number = build_record(serde_json::json!({"seq": "not-a-number"}));
        assert_eq!(not_a_number.seq, None);
    }
}
