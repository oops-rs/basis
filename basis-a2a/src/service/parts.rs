//! A message's parts, as the one prompt a basis task is spawned with
//! (ADR-0030 §6).
//!
//! Three kinds are accepted, and each reaches the model as text it can tell
//! apart from the caller's own words:
//!
//! - **text**, as written;
//! - **data**, as pretty-printed JSON under a label saying it is structured
//!   data from the caller;
//! - **an inline file whose media type is textual** — `text/*`, JSON, XML,
//!   YAML, or a `+json`/`+xml`/`+yaml` suffix — decoded as UTF-8 and fenced
//!   under its name and media type.
//!
//! Everything else is refused with `ContentTypeNotSupportedError`, never
//! dropped: a caller who sent a file and got an answer that ignored it has no
//! way to know. A file *by URL* is refused whatever its type, because this
//! server never fetches a URL a caller names — that would make it a proxy
//! into whatever network it runs on. Binary bytes are refused because no
//! model behind basis is handed them.
//!
//! The whole prompt is bounded by `basis-tasks`' own prompt limit, checked
//! here so that an oversized message is the caller's error rather than a
//! failure of the server's.

use a2a::{A2AError, Message, Part, PartContent, Role};
use basis_tasks::MAX_PROMPT;

/// The media types the card lists as input modes: exactly what [`render`]
/// accepts. Wildcards stand for the families [`is_textual`] admits.
pub(crate) const INPUT_MODES: [&str; 9] = [
    "text/plain",
    "text/*",
    "application/json",
    "application/*+json",
    "application/xml",
    "application/*+xml",
    "application/yaml",
    "application/x-yaml",
    "application/*+yaml",
];

/// What a message says, as one prompt, and whether any part was more than
/// text — in which case the parts themselves are worth keeping for the
/// task's history.
#[derive(Debug, PartialEq)]
pub(crate) struct Rendered {
    pub(crate) prompt: String,
    pub(crate) only_text: bool,
}

/// The prompt `message` carries.
pub(crate) fn render(message: &Message) -> Result<Rendered, A2AError> {
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
    for part in &message.parts {
        match &part.content {
            PartContent::Text(text) => sections.push(text.clone()),
            PartContent::Data(value) => {
                only_text = false;
                sections.push(data(part, value)?);
            }
            PartContent::Raw(bytes) => {
                only_text = false;
                sections.push(file(part, bytes)?);
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
    Ok(Rendered { prompt, only_text })
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

fn file(part: &Part, bytes: &[u8]) -> Result<String, A2AError> {
    let media_type = part
        .media_type
        .as_deref()
        .filter(|media_type| is_textual(media_type))
        .ok_or_else(A2AError::content_type_not_supported)?;
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
