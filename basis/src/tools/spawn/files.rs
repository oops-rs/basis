//! The files a delegation carries (ADR-0041): read from the workspace to send,
//! and saved to its inbox when an agent sends some back.
//!
//! Both directions go through mentra's file policy — the read and write roots
//! the file tools answer to — so `spawn` can send nothing the agent could not
//! `read`, and save nothing where it could not `write`.

use std::path::{Path, PathBuf};

use crate::agents::Attachment;

/// The most a delegation sends, all files together: 6 MiB, which A2A's
/// base64 makes 8 MiB on the wire — mentra's default reply bound and dial's
/// request bound.
pub(crate) const MAX_SENT_BYTES: usize = 6 * 1024 * 1024;

/// Where an agent's files are saved, relative to the calling agent's working
/// directory: one folder per task under it.
pub(crate) const INBOX: &str = ".basis/inbox";

/// One file read to send: where it was, and what goes.
#[derive(Debug)]
pub(super) struct Sent {
    pub(super) path: PathBuf,
    pub(super) file: Attachment,
}

/// Reads `paths` with `read_bytes` — the tool context's, which answers to
/// the file tools' read policy — refusing the lot past [`MAX_SENT_BYTES`]
/// together.
pub(super) fn read(
    paths: &[String],
    read_bytes: impl Fn(&str) -> Result<(PathBuf, Vec<u8>), String>,
) -> Result<Vec<Sent>, String> {
    let mut total = 0usize;
    let mut sent = Vec::with_capacity(paths.len());
    for raw in paths {
        let (path, data) =
            read_bytes(raw).map_err(|error| format!("`{raw}` cannot be sent: {error}"))?;
        if path.is_dir() {
            return Err(format!(
                "`{raw}` is a directory; send the files in it by name"
            ));
        }
        total = total.saturating_add(data.len());
        if total > MAX_SENT_BYTES {
            return Err(format!(
                "the files come to more than the {} MiB a task can carry; send fewer, or \
                 smaller ones",
                MAX_SENT_BYTES / (1024 * 1024)
            ));
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        let media_type = mentra::media::media_type(name.as_deref().unwrap_or_default(), &data);
        sent.push(Sent {
            path,
            file: Attachment::new(name, media_type, data),
        });
    }
    Ok(sent)
}

/// The line a task for an agent here carries instead of the files: it shares
/// the workspace, so it is told where they are.
pub(super) fn named(body: &str, sent: &[Sent]) -> String {
    if sent.is_empty() {
        return body.to_string();
    }
    let paths: Vec<String> = sent
        .iter()
        .map(|sent| format!("- {}", sent.path.display()))
        .collect();
    format!("{body}\n\nFiles for this task:\n{}", paths.join("\n"))
}

/// Saves `files` under `<inbox>/<folder>/` in `root`, the calling agent's
/// working directory, with `write_bytes` — the tool context's, which answers
/// to the file tools' write policy — and says what was saved where, or for a
/// file that could not be, why, as lines for the model. Empty when there
/// were none.
pub(super) fn save(
    root: &Path,
    folder: &str,
    files: Vec<Attachment>,
    write_bytes: impl Fn(&str, &[u8]) -> Result<PathBuf, String>,
) -> String {
    if files.is_empty() {
        return String::new();
    }
    let folder = format!(
        "{INBOX}/{}",
        mentra::media::safe_file_name(folder).unwrap_or_else(|| "task".to_string())
    );
    let mut lines = Vec::with_capacity(files.len());
    for (index, file) in files.into_iter().enumerate() {
        let name = file_name(file.name.as_deref(), &file.media_type, index + 1);
        let relative = unique(root, &folder, &name);
        let about = format!("{}, {} bytes", file.media_type, file.data.len());
        lines.push(match write_bytes(&relative, &file.data) {
            Ok(_) => format!("- {relative} ({about})"),
            Err(error) => format!("- {name} ({about}) could not be saved: {error}"),
        });
    }
    format!(
        "Files it returned, saved in the workspace (`read` shows an image):\n{}",
        lines.join("\n")
    )
}

/// The name a file is saved under: the one it came with, reduced to a single
/// path component, or `file-<n>` with its type's extension.
fn file_name(given: Option<&str>, media_type: &str, n: usize) -> String {
    given
        .and_then(mentra::media::safe_file_name)
        .unwrap_or_else(|| match mentra::media::extension_for(media_type) {
            Some(extension) => format!("file-{n}.{extension}"),
            None => format!("file-{n}"),
        })
}

/// `folder/name`, or `folder/stem-2.ext` and onward when that is taken.
fn unique(root: &Path, folder: &str, name: &str) -> String {
    let taken = |relative: &str| root.join(relative).exists();
    let first = format!("{folder}/{name}");
    if !taken(&first) {
        return first;
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
        _ => (name, String::new()),
    };
    (2..)
        .map(|n| format!("{folder}/{stem}-{n}{extension}"))
        .find(|candidate| !taken(candidate))
        .expect("an unbounded range finds a free name")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_without_a_name_is_numbered_with_its_types_extension() {
        assert_eq!(file_name(None, "image/png", 2), "file-2.png");
        assert_eq!(file_name(Some(".."), "application/x-thing", 1), "file-1");
        assert_eq!(file_name(Some("shot.png"), "image/png", 1), "shot.png");
    }

    #[test]
    fn a_taken_name_gets_the_next_number() {
        let root = std::env::temp_dir().join(format!("basis-inbox-{}", std::process::id()));
        let folder = format!("{INBOX}/t1");
        std::fs::create_dir_all(root.join(&folder)).unwrap();
        std::fs::write(root.join(&folder).join("shot.png"), b"x").unwrap();
        std::fs::write(root.join(&folder).join("shot-2.png"), b"x").unwrap();
        assert_eq!(
            unique(&root, &folder, "shot.png"),
            format!("{folder}/shot-3.png")
        );
        assert_eq!(
            unique(&root, &folder, "new.png"),
            format!("{folder}/new.png")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_task_here_is_told_where_its_files_are() {
        let sent = vec![Sent {
            path: PathBuf::from("/work/screens/login.png"),
            file: Attachment::new(Some("login.png".into()), "image/png", vec![1]),
        }];
        assert_eq!(
            named("why is it red?", &sent),
            "why is it red?\n\nFiles for this task:\n- /work/screens/login.png"
        );
        assert_eq!(named("plain", &[]), "plain");
    }
}
