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
    /// This record's sequence number, monotonic within one task.
    pub seq: u64,
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
        Ok(records
            .into_iter()
            .map(|raw| {
                let seq = raw.get("seq").and_then(Value::as_u64).unwrap_or_default();
                let event = basis::Event::deserialize(&raw).ok().map(Box::new);
                WatchRecord { seq, raw, event }
            })
            .collect())
    }
}
