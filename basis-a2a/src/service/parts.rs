//! A message's parts, as the one prompt a basis task is spawned with
//! (ADR-0030 §6, revised by ADR-0041 §6).
//!
//! Each kind reaches the model as text it can tell apart from the caller's
//! own words:
//!
//! - **text**, as written;
//! - **data**, as pretty-printed JSON under a label saying it is structured
//!   data from the caller;
//! - **an inline file whose media type is textual** — `text/*`, JSON, XML,
//!   YAML, or a `+json`/`+xml`/`+yaml` suffix — decoded as UTF-8 and fenced
//!   under its name and media type;
//! - **any other inline file** — a screenshot, a PDF — named by its name, type
//!   and size. A served workspace saves it under `.basis/inbox/<id>/` and the
//!   prompt says where, so the model can `read` it (an image is shown); a
//!   host is handed the parts and keeps them its own way.
//!
//! A file *by URL* is refused with `ContentTypeNotSupportedError`, never
//! dropped, because this server never fetches a URL a caller names — that
//! would make it a proxy into whatever network it runs on.
//!
//! The prompt text is bounded by `basis-tasks`' own prompt limit, checked
//! here so that an oversized message is the caller's error rather than a
//! failure of the server's. Files saved rather than inlined count against the
//! request body bound instead.

use std::collections::BTreeSet;

use a2a::{A2AError, Message, Part, PartContent, Role};
use basis_tasks::MAX_PROMPT;

/// The media types the card lists as input modes: any. A textual file is
/// read into the prompt, any other saved or handed over; only a file by URL
/// is refused, and that is not a media type.
pub(crate) const INPUT_MODES: [&str; 1] = ["*/*"];

/// Where a served workspace saves the files a caller sends, relative to it:
/// one folder per message under it (ADR-0041 §6).
pub(crate) const INBOX: &str = ".basis/inbox";

/// What a message says, as one prompt; whether any part was more than text —
/// in which case the parts themselves are worth keeping for the task's
/// history; and the files to save before the task runs.
#[derive(Debug, PartialEq)]
pub(crate) struct Rendered {
    pub(crate) prompt: String,
    pub(crate) only_text: bool,
    pub(crate) files: Vec<Received>,
}

/// A file the caller sent that the prompt names rather than contains, and the
/// workspace-relative path the prompt says it is at.
#[derive(Debug, PartialEq)]
pub(crate) struct Received {
    pub(crate) path: String,
    pub(crate) data: Vec<u8>,
}

/// The prompt `message` carries. `inbox` is the workspace-relative folder a
/// file that is not text is saved in, or `None` when the parts go to a host
/// as they are.
pub(crate) fn render(message: &Message, inbox: Option<&str>) -> Result<Rendered, A2AError> {
    if message.role != Role::User {
        return Err(A2AError::invalid_params(
            "a message to an agent has role ROLE_USER",
        ));
    }
    if message.message_id.trim().is_empty() {
        return Err(A2AError::invalid_params("messageId is required"));
    }
    let mut sections = Vec::with_capacity(message.parts.len());
    let mut only_text = true;
    let mut files = Vec::new();
    let mut names = BTreeSet::new();
    for part in &message.parts {
        match &part.content {
            PartContent::Text(text) => sections.push(text.clone()),
            PartContent::Data(value) => {
                only_text = false;
                sections.push(data(part, value)?);
            }
            PartContent::Raw(bytes) => {
                only_text = false;
                let media_type = media_type_of(part, bytes);
                if is_textual(&media_type) {
                    sections.push(file(part, &media_type, bytes)?);
                    continue;
                }
                let name = unique(&mut names, part, &media_type, files.len() + 1);
                let about = format!("{media_type}, {} bytes", bytes.len());
                match inbox {
                    Some(inbox) => {
                        let path = format!("{inbox}/{name}");
                        sections.push(format!(
                            "File `{name}` from the caller ({about}), saved at `{path}`; \
                             `read` it to look at it."
                        ));
                        files.push(Received {
                            path,
                            data: bytes.clone(),
                        });
                    }
                    None => sections.push(format!("File `{name}` from the caller ({about}).")),
                }
            }
            PartContent::Url(_) => return Err(A2AError::content_type_not_supported()),
        }
    }
    let prompt = sections.join("\n\n");
    if prompt.trim().is_empty() {
        return Err(A2AError::invalid_params("the message has no content"));
    }
    if prompt.len() > MAX_PROMPT {
        return Err(A2AError::invalid_params(format!(
            "the message is {} bytes as a prompt; the limit is {MAX_PROMPT}",
            prompt.len()
        )));
    }
    Ok(Rendered {
        prompt,
        only_text,
        files,
    })
}

fn data(part: &Part, value: &serde_json::Value) -> Result<String, A2AError> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|error| A2AError::invalid_params(format!("a data part: {error}")))?;
    let label = match &part.filename {
        Some(name) => format!("Structured data from the caller, `{name}` (JSON):"),
        None => "Structured data from the caller (JSON):".to_string(),
    };
    Ok(format!("{label}\n{}", fenced(&json, "json")))
}

/// The media type a file part declared, or else what its bytes and name say.
fn media_type_of(part: &Part, bytes: &[u8]) -> String {
    part.media_type
        .clone()
        .filter(|declared| !declared.trim().is_empty())
        .unwrap_or_else(|| {
            mentra::media::media_type(part.filename.as_deref().unwrap_or_default(), bytes)
                .to_string()
        })
}

fn file(part: &Part, media_type: &str, bytes: &[u8]) -> Result<String, A2AError> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        A2AError::invalid_params(format!(
            "a file part declared {media_type} but is not UTF-8 text"
        ))
    })?;
    let name = part.filename.as_deref().unwrap_or("untitled");
    Ok(format!(
        "File `{name}` from the caller ({media_type}, {} bytes):\n{}",
        bytes.len(),
        fenced(text, "")
    ))
}

/// The name a saved file takes: the caller's, made safe, or `file-<n>` with
/// its type's extension — and unique among this message's, so two parts both
/// called `shot.png` do not overwrite each other.
fn unique(taken: &mut BTreeSet<String>, part: &Part, media_type: &str, n: usize) -> String {
    let name = part
        .filename
        .as_deref()
        .and_then(mentra::media::safe_file_name)
        .unwrap_or_else(|| match mentra::media::extension_for(media_type) {
            Some(extension) => format!("file-{n}.{extension}"),
            None => format!("file-{n}"),
        });
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem.to_string(), format!(".{extension}")),
        _ => (name.clone(), String::new()),
    };
    let name = std::iter::once(name)
        .chain((2..).map(|k| format!("{stem}-{k}{extension}")))
        .find(|candidate| !taken.contains(candidate))
        .expect("an unbounded sequence finds a free name");
    taken.insert(name.clone());
    name
}

/// `body` in a code fence no line of it can close: one backtick longer than
/// the longest run of backticks it contains, and never shorter than three.
fn fenced(body: &str, language: &str) -> String {
    let longest = body
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}{language}\n{body}\n{fence}")
}

/// Whether `media_type` names text a model can read as it is: `text/*`, or
/// JSON, XML or YAML by name or by structured-syntax suffix. Parameters
/// (`; charset=utf-8`) are ignored; the bytes must be UTF-8 regardless.
pub(crate) fn is_textual(media_type: &str) -> bool {
    let essence = media_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let Some((kind, subtype)) = essence.split_once('/') else {
        return false;
    };
    if subtype.is_empty() {
        return false;
    }
    match kind {
        "text" => true,
        "application" => {
            matches!(subtype, "json" | "xml" | "yaml" | "x-yaml")
                || ["+json", "+xml", "+yaml"]
                    .iter()
                    .any(|suffix| subtype.len() > suffix.len() && subtype.ends_with(suffix))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests;
