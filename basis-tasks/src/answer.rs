//! What a task answering a caller hands back beside its text (ADR-0041 §7).
//!
//! A task's result is the text of its last turn, and a model cannot put a
//! file in text. So a task spawned to answer someone —
//! [`RunSpec::answering`](crate::RunSpec::answering) — is offered one tool,
//! [`ATTACH_FILE`], that adds a workspace file to its answer. The file is
//! copied into the task's own directory at the moment of the call: the answer
//! is what the model chose then, not whatever the workspace file becomes by
//! the time the caller reads it.
//!
//! The copies sit under `files/`, indexed by `files.json` — written
//! atomically after each copy, so a reader sees every attached file or the
//! index from before it, never a half-written entry.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use basis::{
    async_trait, media,
    tools::{
        RuntimeToolDescriptor, ToolContext, ToolDefinition, ToolDurability, ToolExecutor,
        ToolResult, ToolSideEffectLevel,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::data_dir::write_private_atomic;

/// The tool a task answering a caller attaches a file with.
pub const ATTACH_FILE: &str = "attach_file";

/// The most one task's answer carries in files: 6 MiB, which A2A's base64
/// makes 8 MiB — mentra's default bound on a reply a caller reads.
pub const MAX_ANSWER_FILES_BYTES: usize = 6 * 1024 * 1024;

/// One file a task's answer carries.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AnswerFile {
    /// The name it goes by: the workspace file's own.
    pub name: String,
    pub media_type: String,
    pub data: Vec<u8>,
}

/// One entry of `files.json`: what was attached, and the copy holding it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    name: String,
    media_type: String,
    /// The copy's file name under `files/`.
    stored: String,
    bytes: usize,
}

fn index(dir: &Path) -> PathBuf {
    dir.join("files.json")
}

fn load(dir: &Path) -> io::Result<Vec<Entry>> {
    match fs::read(index(dir)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

/// The files the task in `dir` attached to its answer, in the order it
/// attached them. Empty for a task that attached none.
pub(crate) fn read(dir: &Path) -> io::Result<Vec<AnswerFile>> {
    load(dir)?
        .into_iter()
        .map(|entry| {
            Ok(AnswerFile {
                data: fs::read(dir.join("files").join(&entry.stored))?,
                name: entry.name,
                media_type: entry.media_type,
            })
        })
        .collect()
}

/// [`ATTACH_FILE`] for the task whose directory is `dir`.
pub(crate) struct AttachFile {
    dir: PathBuf,
}

impl AttachFile {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Copies `data` in as `name` and records it, refusing past the bound.
    fn attach(&self, name: &str, data: &[u8]) -> Result<Entry, String> {
        let mut entries = load(&self.dir).map_err(|error| format!("read files.json: {error}"))?;
        let total: usize = entries.iter().map(|entry| entry.bytes).sum();
        if total.saturating_add(data.len()) > MAX_ANSWER_FILES_BYTES {
            return Err(format!(
                "an answer carries at most {} MiB of files, and this one would take it past \
                 that; attach fewer, or smaller ones",
                MAX_ANSWER_FILES_BYTES / (1024 * 1024)
            ));
        }
        let stored = format!("{}-{name}", entries.len() + 1);
        let files = self.dir.join("files");
        fs::create_dir_all(&files).map_err(|error| format!("create files/: {error}"))?;
        write_private_atomic(&files.join(&stored), data)
            .map_err(|error| format!("copy {name}: {error}"))?;
        let entry = Entry {
            name: name.to_string(),
            media_type: media::media_type(name, data).to_string(),
            stored,
            bytes: data.len(),
        };
        entries.push(entry.clone());
        let json = serde_json::to_vec_pretty(&entries).map_err(|error| error.to_string())?;
        write_private_atomic(&index(&self.dir), &json)
            .map_err(|error| format!("write files.json: {error}"))?;
        Ok(entry)
    }
}

impl ToolDefinition for AttachFile {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        RuntimeToolDescriptor::builder(ATTACH_FILE)
            .description(
                "Attach a workspace file to your answer, so whoever asked receives the file \
                 itself as well as your words — a screenshot, a report, a patch. The file is \
                 copied as it is now. Up to 6 MiB of files in all.",
            )
            .input_schema(json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The workspace path of the file to attach.",
                    },
                },
                "required": ["path"],
            }))
            // Reads like `read` does and writes only the task's own record,
            // so it is never a question for whoever approves this task's
            // consequential calls — a task told to refuse them can still
            // answer with a file.
            .side_effect_level(ToolSideEffectLevel::None)
            .durability(ToolDurability::ReplaySafe)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for AttachFile {
    async fn execute_mut(&self, ctx: ToolContext<'_>, input: Value) -> ToolResult {
        let raw = input
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| !path.trim().is_empty())
            .ok_or("`path` is the workspace path of the file to attach")?;
        let (path, data) = ctx.read_file_bytes(raw)?;
        if path.is_dir() {
            return Err(format!(
                "`{raw}` is a directory; attach the files in it by name"
            ));
        }
        let name = path
            .file_name()
            .and_then(|name| media::safe_file_name(&name.to_string_lossy()))
            .unwrap_or_else(|| "file".to_string());
        let entry = self.attach(&name, &data)?;
        Ok(format!(
            "attached {} ({}, {} bytes); it goes back with your answer",
            entry.name, entry.media_type, entry.bytes
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attached_files_are_read_back_in_order_with_their_types() {
        let dir = tempfile::tempdir().unwrap();
        let tool = AttachFile::new(dir.path().to_path_buf());
        tool.attach("shot.png", b"\x89PNG\r\n\x1a\nrest").unwrap();
        tool.attach("notes.md", b"# done").unwrap();

        let files = read(dir.path()).unwrap();
        assert_eq!(
            files,
            vec![
                AnswerFile {
                    name: "shot.png".into(),
                    media_type: "image/png".into(),
                    data: b"\x89PNG\r\n\x1a\nrest".to_vec(),
                },
                AnswerFile {
                    name: "notes.md".into(),
                    media_type: "text/markdown".into(),
                    data: b"# done".to_vec(),
                },
            ]
        );
    }

    #[test]
    fn a_task_that_attached_nothing_has_no_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn the_answer_is_bounded_in_total() {
        let dir = tempfile::tempdir().unwrap();
        let tool = AttachFile::new(dir.path().to_path_buf());
        tool.attach("a.bin", &vec![0; MAX_ANSWER_FILES_BYTES - 1])
            .unwrap();
        let refused = tool.attach("b.bin", &[0, 0]).unwrap_err();
        assert!(refused.contains("at most 6 MiB"), "{refused}");
        assert_eq!(
            read(dir.path()).unwrap().len(),
            1,
            "the refused one is not recorded"
        );
    }
}
