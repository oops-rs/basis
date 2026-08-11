//! Where a run's events go.
//!
//! `lan run --json` writes JSONL to stdout; an in-process host wants a
//! callback or a buffer; P2's ACP server will want a protocol notification.
//! All of them are the same stream, so they are all the same trait.

use std::io::Write;

use crate::event::{Event, JsonlWriter};

/// A destination for run events.
///
/// Emission happens on a background task, so a sink must be `Send`. Returning
/// an error stops emission for the rest of the run — a client that has gone
/// away should not cost the run a full transcript of failed writes.
///
/// The run itself carries on. That task is also the one answering approval
/// requests, and mentra blocks the turn until one is answered, so giving up on
/// it would turn a broken pipe into a hung agent.
pub trait EventSink: Send + 'static {
    fn emit(&mut self, event: Event) -> std::io::Result<()>;
}

impl<W: Write + Send + 'static> EventSink for JsonlWriter<W> {
    fn emit(&mut self, event: Event) -> std::io::Result<()> {
        self.write(event).map(|_| ())
    }
}

/// Keeps every event in memory. The natural sink for tests, and for a host
/// that wants the whole run before deciding what to do with it.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CollectingSink {
    events: Vec<Event>,
}

impl CollectingSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn into_events(self) -> Vec<Event> {
        self.events
    }
}

impl EventSink for CollectingSink {
    fn emit(&mut self, event: Event) -> std::io::Result<()> {
        self.events.push(event);
        Ok(())
    }
}

/// Discards every event, for a host that only wants the final message.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&mut self, _event: Event) -> std::io::Result<()> {
        Ok(())
    }
}

/// Calls a closure per event.
pub struct FnSink<F>(F)
where
    F: FnMut(Event) -> std::io::Result<()> + Send + 'static;

impl<F> FnSink<F>
where
    F: FnMut(Event) -> std::io::Result<()> + Send + 'static,
{
    pub fn new(callback: F) -> Self {
        Self(callback)
    }
}

impl<F> EventSink for FnSink<F>
where
    F: FnMut(Event) -> std::io::Result<()> + Send + 'static,
{
    fn emit(&mut self, event: Event) -> std::io::Result<()> {
        (self.0)(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::RunOutcome;

    fn delta(text: &str) -> Event {
        Event::AssistantDelta {
            text: text.to_string(),
        }
    }

    #[test]
    fn collecting_sink_keeps_order() {
        let mut sink = CollectingSink::new();
        sink.emit(delta("a")).expect("emits");
        sink.emit(delta("b")).expect("emits");

        assert_eq!(sink.into_events(), vec![delta("a"), delta("b")]);
    }

    #[test]
    fn null_sink_accepts_everything() {
        let mut sink = NullSink;

        assert!(
            sink.emit(Event::RunFinished {
                outcome: RunOutcome::Ok
            })
            .is_ok()
        );
    }

    #[test]
    fn fn_sink_forwards_to_the_closure() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut sink = FnSink::new(move |event| {
            tx.send(event).expect("receiver alive");
            Ok(())
        });

        sink.emit(delta("x")).expect("emits");

        assert_eq!(rx.recv().expect("an event"), delta("x"));
    }

    #[test]
    fn a_jsonl_writer_is_a_sink() {
        let mut sink = JsonlWriter::new(Vec::new());
        sink.emit(delta("hi")).expect("emits");

        let written = String::from_utf8(sink.into_inner()).expect("utf-8");
        assert!(written.contains("\"type\":\"assistant_delta\""));
    }
}
