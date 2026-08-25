//! The append-only per-agent event journal, `events.jsonl`.
//!
//! One line per event, in the same flat shape `basis --json` streams —
//! basis's `EventLine`, the `seq` spliced into the event object:
//! `{"seq":N,"type":...}`. One schema on disk and on stdout, so a consumer
//! written against either reads both. The executor is the only writer (it
//! holds `attach.lock`); watchers tail the file concurrently, which Rust's
//! std file sharing permits on every platform. Replay-from-start is
//! therefore the default watch behavior.
//!
//! The reader also accepts the nested `{"seq":N,"event":{...}}` wrapper this
//! file wrote before 0.6.0 — a task directory outlives the binary that
//! minted it, so durable state on disk is a compatibility surface the same
//! way E2 ruled for the rest of the agent directory. Old journals are
//! normalized to the flat shape on the way out; only the writer changed.

use std::{
    fs::{File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::PathBuf,
};

use serde_json::Value;

use super::{
    data_dir::{AgentPaths, restrict_file},
    state::{MAX_EVENT_BYTES, MAX_EVENTS_BYTES},
};

/// The single writer's append handle. Sequence numbers continue from whatever
/// the file already holds, so a resumed agent's journal stays monotonic.
pub(crate) struct EventLog {
    file: File,
    next_seq: u64,
    written: u64,
    capped: bool,
}

impl EventLog {
    pub(crate) fn open(paths: &AgentPaths) -> io::Result<Self> {
        let path = paths.events();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        restrict_file(&path)?;
        let written = file.seek(SeekFrom::End(0))?;
        let next_seq = last_seq(&path)?.map_or(1, |seq| seq.saturating_add(1));
        Ok(Self {
            file,
            next_seq,
            written,
            capped: written >= MAX_EVENTS_BYTES,
        })
    }

    /// Appends one event, substituting a small notice for an oversized one and
    /// stopping (with one final notice) at the byte cap. Errors are the
    /// caller's to ignore: a run is never failed for observability.
    pub(crate) fn append(&mut self, event: Value) -> io::Result<()> {
        if self.capped {
            return Ok(());
        }
        let event = if serde_json::to_vec(&event).is_ok_and(|bytes| bytes.len() <= MAX_EVENT_BYTES)
        {
            event
        } else {
            serde_json::json!({
                "type": "notice",
                "severity": "warning",
                "message": format!("event omitted because it exceeded {MAX_EVENT_BYTES} bytes"),
            })
        };
        self.write_line(event)?;
        if self.written >= MAX_EVENTS_BYTES {
            self.capped = true;
            self.write_line(serde_json::json!({
                "type": "notice",
                "severity": "warning",
                "message": format!(
                    "event journal reached {MAX_EVENTS_BYTES} bytes; further events are not recorded"
                ),
            }))?;
        }
        Ok(())
    }

    fn write_line(&mut self, event: Value) -> io::Result<()> {
        // The flat `EventLine` shape: the seq keyed into the event object
        // itself. An event is always a JSON object; anything else would be a
        // caller bug, and wrapping it keeps the journal parseable instead of
        // losing the line.
        let mut object = match event {
            Value::Object(object) => object,
            other => {
                let mut object = serde_json::Map::new();
                object.insert("event".to_string(), other);
                object
            }
        };
        object.insert("seq".to_string(), Value::from(self.next_seq));

        let mut line = serde_json::to_vec(&Value::Object(object))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.flush()?;
        self.next_seq = self.next_seq.saturating_add(1);
        self.written = self.written.saturating_add(line.len() as u64);
        Ok(())
    }
}

/// One journal line as the flat `EventLine` shape, whichever vintage wrote it.
///
/// Journals written before 0.6.0 hold `{"seq":N,"event":{...}}`; the wrapper
/// is unfolded here so every consumer — the renderer, `watch --json` — sees
/// exactly one schema. A flat line is told apart by carrying its own `type`
/// tag at the top; the nested wrapper never does.
fn normalized(line: &[u8]) -> Option<(u64, Value)> {
    let value: Value = serde_json::from_slice(line).ok()?;
    let seq = value.get("seq")?.as_u64()?;
    if value.get("type").is_none()
        && let Some(nested) = value.get("event").and_then(Value::as_object)
    {
        // The pre-0.6 nested wrapper, unfolded to the flat shape.
        let mut object = nested.clone();
        object.insert("seq".to_string(), Value::from(seq));
        return Some((seq, Value::Object(object)));
    }
    // Flat lines — and the wrapped non-object `write_line` kept rather than
    // lost. That one passes through as-is, seq intact, so the tailer still
    // sees it, `last_seq` still counts it, and a reopened log never reuses
    // its number; the renderer degrades to an `unrecognized event` line,
    // which is the honest rendering of a line nothing can type.
    Some((seq, value))
}

fn last_seq(path: &PathBuf) -> io::Result<Option<u64>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut last = None;
    for line in BufReader::new(file).lines() {
        let line = line?;
        if let Some((seq, _)) = normalized(line.as_bytes()) {
            last = Some(seq);
        }
    }
    Ok(last)
}

/// An incremental reader for `watch`: each poll parses only the bytes appended
/// since the last one, skipping a final line the writer has not finished.
pub(crate) struct EventTail {
    path: PathBuf,
    offset: u64,
    since: u64,
}

impl EventTail {
    pub(crate) fn new(paths: &AgentPaths, since: u64) -> Self {
        Self {
            path: paths.events(),
            offset: 0,
            since,
        }
    }

    /// Every whole line appended since the last poll, as flat `EventLine`
    /// values (`{"seq":N,"type":...}`) whatever shape is on disk.
    pub(crate) fn poll(&mut self) -> io::Result<Vec<Value>> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;

        let mut records = Vec::new();
        let mut consumed = 0_usize;
        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
            if line.last() != Some(&b'\n') {
                break;
            }
            consumed += line.len();
            if let Some((seq, event)) = normalized(line)
                && seq > self.since
            {
                self.since = seq;
                records.push(event);
            }
        }
        self.offset += consumed as u64;
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_dir::DataDir;

    fn agent(dir: &tempfile::TempDir) -> AgentPaths {
        let data = DataDir::from_path(dir.path()).unwrap();
        let paths = data
            .agent_dir("0123456789abcdef/0123456789abcdef0123456789abcdef")
            .unwrap();
        std::fs::create_dir_all(paths.dir()).unwrap();
        paths
    }

    #[test]
    fn sequence_numbers_survive_reopening_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let paths = agent(&dir);
        {
            let mut log = EventLog::open(&paths).unwrap();
            log.append(serde_json::json!({"type": "notice", "message": "one"}))
                .unwrap();
        }
        let mut log = EventLog::open(&paths).unwrap();
        log.append(serde_json::json!({"type": "notice", "message": "two"}))
            .unwrap();

        let mut tail = EventTail::new(&paths, 0);
        let records = tail.poll().unwrap();
        assert_eq!(
            records
                .iter()
                .filter_map(|record| record["seq"].as_u64())
                .collect::<Vec<_>>(),
            [1, 2]
        );
    }

    /// The journal line is the flat `EventLine` shape — one schema on disk
    /// and on stdout — and a nested pre-0.6 line still reads, normalized to
    /// the same shape, because a task directory outlives the binary that
    /// wrote it.
    #[test]
    fn both_journal_vintages_read_as_one_flat_shape() {
        let dir = tempfile::tempdir().unwrap();
        let paths = agent(&dir);
        std::fs::write(
            paths.events(),
            "{\"seq\":1,\"event\":{\"type\":\"notice\",\"message\":\"old\"}}\n",
        )
        .unwrap();

        let mut log = EventLog::open(&paths).unwrap();
        log.append(serde_json::json!({"type": "notice", "message": "new"}))
            .unwrap();

        let written = std::fs::read_to_string(paths.events()).unwrap();
        let last = written.lines().last().unwrap();
        let parsed: Value = serde_json::from_str(last).unwrap();
        assert_eq!(parsed["seq"], 2, "the writer continues the old numbering");
        assert_eq!(parsed["type"], "notice", "and writes only the flat shape");
        assert!(parsed.get("event").is_none());

        let records = EventTail::new(&paths, 0).poll().unwrap();
        assert_eq!(records.len(), 2);
        for record in &records {
            assert_eq!(record["type"], "notice", "one shape out, whatever went in");
            assert!(record["seq"].is_u64());
        }
        assert_eq!(records[0]["message"], "old");
        assert_eq!(records[1]["message"], "new");
    }

    #[test]
    fn oversized_events_become_small_explicit_notices() {
        let dir = tempfile::tempdir().unwrap();
        let paths = agent(&dir);
        let mut log = EventLog::open(&paths).unwrap();
        log.append(serde_json::json!({"text": "x".repeat(MAX_EVENT_BYTES)}))
            .unwrap();

        let records = EventTail::new(&paths, 0).poll().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["type"], "notice");
        assert_eq!(
            records[0]["severity"], "warning",
            "a synthetic notice states its severity — `Event::Notice` requires one"
        );
    }

    /// The defensive wrap in `write_line` must stay visible to the reader: a
    /// kept line holds a seq the next writer must not reuse, and a tailer
    /// that skipped it would hide that something was appended at all.
    #[test]
    fn a_wrapped_non_object_event_keeps_its_seq_and_reaches_the_tailer() {
        let dir = tempfile::tempdir().unwrap();
        let paths = agent(&dir);
        {
            let mut log = EventLog::open(&paths).unwrap();
            log.append(serde_json::json!(42)).unwrap();
        }
        let mut log = EventLog::open(&paths).unwrap();
        log.append(serde_json::json!({"type": "notice", "severity": "info", "message": "next"}))
            .unwrap();

        let records = EventTail::new(&paths, 0).poll().unwrap();
        assert_eq!(records.len(), 2, "the kept line is not invisible");
        assert_eq!(records[0]["seq"], 1);
        assert_eq!(records[0]["event"], 42);
        assert_eq!(
            records[1]["seq"], 2,
            "a reopened log continues past the wrapped line's number"
        );
    }

    #[test]
    fn the_tail_reads_incrementally_and_respects_since() {
        let dir = tempfile::tempdir().unwrap();
        let paths = agent(&dir);
        let mut log = EventLog::open(&paths).unwrap();
        log.append(serde_json::json!({"n": 1})).unwrap();
        log.append(serde_json::json!({"n": 2})).unwrap();

        let mut tail = EventTail::new(&paths, 1);
        let first = tail.poll().unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0]["seq"], 2);

        log.append(serde_json::json!({"n": 3})).unwrap();
        let second = tail.poll().unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0]["seq"], 3);
        assert!(tail.poll().unwrap().is_empty());
    }

    /// Pins the share-mode assumption that resolves the spec's open question:
    /// a tail can read while the single writer holds the file open.
    #[test]
    fn a_tail_reads_while_the_writer_holds_the_file_open() {
        let dir = tempfile::tempdir().unwrap();
        let paths = agent(&dir);
        let mut log = EventLog::open(&paths).unwrap();
        let mut tail = EventTail::new(&paths, 0);
        log.append(serde_json::json!({"n": 1})).unwrap();
        assert_eq!(tail.poll().unwrap().len(), 1);
        log.append(serde_json::json!({"n": 2})).unwrap();
        assert_eq!(tail.poll().unwrap().len(), 1);
    }
}
