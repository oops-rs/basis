//! Newline-delimited JSON rendering of the event stream.

use std::io::Write;

use super::{Event, EventLine};

/// Writes [`Event`]s as JSONL, assigning sequence numbers.
///
/// Each line is flushed as it is written: a subprocess consumer reading the
/// stream live should see a token delta when it happens, not when a buffer
/// happens to fill.
///
/// The writer does not police stream structure — emitting
/// [`Event::RunStarted`] first and [`Event::RunFinished`] last is the caller's
/// contract.
#[derive(Debug)]
pub struct JsonlWriter<W: Write> {
    writer: W,
    next_seq: u64,
}

impl<W: Write> JsonlWriter<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            next_seq: 0,
        }
    }

    /// Writes one event and returns the sequence number it was given.
    pub fn write(&mut self, event: Event) -> std::io::Result<u64> {
        let seq = self.next_seq;
        self.write_line(EventLine::new(seq, event))?;
        Ok(seq)
    }

    /// Writes one already-numbered line and returns the encoded byte count,
    /// including its newline.
    pub fn write_line(&mut self, line: EventLine) -> std::io::Result<usize> {
        let seq = line.seq;
        let encoded = serde_json::to_string(&line).map_err(std::io::Error::other)?;
        let written = encoded.len().saturating_add(1);

        writeln!(self.writer, "{encoded}")?;
        self.writer.flush()?;

        self.next_seq = self.next_seq.max(seq.saturating_add(1));
        Ok(written)
    }

    /// The sequence number the next write will use.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Returns the underlying writer.
    pub fn into_inner(self) -> W {
        self.writer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::RunOutcome;

    fn lines(buffer: &[u8]) -> Vec<serde_json::Value> {
        String::from_utf8(buffer.to_vec())
            .expect("utf-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is json"))
            .collect()
    }

    #[test]
    fn every_event_gets_the_next_sequence_number() {
        let mut writer = JsonlWriter::new(Vec::new());

        writer
            .write(Event::AssistantDelta {
                text: "a".to_string(),
            })
            .expect("writes");
        writer
            .write(Event::AssistantDelta {
                text: "b".to_string(),
            })
            .expect("writes");

        let written = lines(&writer.into_inner());
        assert_eq!(written[0]["seq"], 0);
        assert_eq!(written[1]["seq"], 1);
    }

    #[test]
    fn each_event_is_exactly_one_line() {
        let mut writer = JsonlWriter::new(Vec::new());

        // Text with newlines in it must not break the framing.
        writer
            .write(Event::AssistantMessage {
                text: "one\ntwo\nthree".to_string(),
            })
            .expect("writes");
        writer
            .write(Event::RunFinished {
                outcome: RunOutcome::Ok,
                stopped_by: None,
                usage: None,
            })
            .expect("writes");

        let buffer = writer.into_inner();
        let text = String::from_utf8(buffer.clone()).expect("utf-8");
        assert_eq!(text.lines().count(), 2);

        let written = lines(&buffer);
        assert_eq!(written[0]["text"], "one\ntwo\nthree");
    }

    #[test]
    fn next_seq_reports_what_the_next_write_will_use() {
        let mut writer = JsonlWriter::new(Vec::new());
        assert_eq!(writer.next_seq(), 0);

        writer
            .write(Event::CompactionStarted {
                agent_id: "a1".to_string(),
            })
            .expect("writes");

        assert_eq!(writer.next_seq(), 1);
    }

    #[test]
    fn a_broken_pipe_surfaces_rather_than_being_swallowed() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut writer = JsonlWriter::new(Broken);
        let error = writer
            .write(Event::AssistantDelta {
                text: "x".to_string(),
            })
            .expect_err("write fails");

        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn writing_a_presequenced_line_advances_the_next_sequence() {
        let mut writer = JsonlWriter::new(Vec::new());

        let first_bytes = writer
            .write_line(EventLine::new(
                7,
                Event::AssistantDelta {
                    text: "x".to_string(),
                },
            ))
            .expect("writes");

        assert_eq!(writer.next_seq(), 8);
        let lower_bytes = writer
            .write_line(EventLine::new(
                3,
                Event::AssistantDelta {
                    text: "older".to_string(),
                },
            ))
            .expect("a lower explicit sequence still writes");
        assert_eq!(
            writer.next_seq(),
            8,
            "an older line cannot rewind the writer"
        );
        let assigned = writer
            .write(Event::AssistantDelta {
                text: "next".to_string(),
            })
            .expect("the automatic sequence remains monotonic");
        assert_eq!(assigned, 8);

        let buffer = writer.into_inner();
        assert!(first_bytes + lower_bytes < buffer.len());
        let written = lines(&buffer);
        assert_eq!(written[0]["seq"], 7);
        assert_eq!(written[1]["seq"], 3);
        assert_eq!(written[2]["seq"], 8);
    }
}
