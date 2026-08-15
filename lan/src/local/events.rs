//! The append-only per-agent event journal, `events.jsonl`.
//!
//! One line per [`EventRecord`]: `{"seq":N,"event":{...}}`. The executor is
//! the only writer (it holds `attach.lock`); watchers tail the file
//! concurrently, which Rust's std file sharing permits on every platform.
//! Replay-from-start is therefore the default watch behavior.

use std::{
    fs::{File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::PathBuf,
};

use serde_json::Value;

use super::{
    data_dir::{AgentPaths, restrict_file},
    state::{EventRecord, MAX_EVENT_BYTES, MAX_EVENTS_BYTES},
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
                "message": format!("event omitted because it exceeded {MAX_EVENT_BYTES} bytes"),
            })
        };
        self.write_line(event)?;
        if self.written >= MAX_EVENTS_BYTES {
            self.capped = true;
            self.write_line(serde_json::json!({
                "type": "notice",
                "message": format!(
                    "event journal reached {MAX_EVENTS_BYTES} bytes; further events are not recorded"
                ),
            }))?;
        }
        Ok(())
    }

    fn write_line(&mut self, event: Value) -> io::Result<()> {
        let record = EventRecord {
            seq: self.next_seq,
            event,
        };
        let mut line = serde_json::to_vec(&record)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        line.push(b'\n');
        self.file.write_all(&line)?;
        self.file.flush()?;
        self.next_seq = self.next_seq.saturating_add(1);
        self.written = self.written.saturating_add(line.len() as u64);
        Ok(())
    }
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
        if let Ok(record) = serde_json::from_str::<EventRecord>(&line) {
            last = Some(record.seq);
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

    pub(crate) fn poll(&mut self) -> io::Result<Vec<EventRecord>> {
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
            if let Ok(record) = serde_json::from_slice::<EventRecord>(line)
                && record.seq > self.since
            {
                self.since = record.seq;
                records.push(record);
            }
        }
        self.offset += consumed as u64;
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::data_dir::DataDir;

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
            records.iter().map(|record| record.seq).collect::<Vec<_>>(),
            [1, 2]
        );
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
        assert_eq!(records[0].event["type"], "notice");
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
        assert_eq!(first[0].seq, 2);

        log.append(serde_json::json!({"n": 3})).unwrap();
        let second = tail.poll().unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].seq, 3);
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
