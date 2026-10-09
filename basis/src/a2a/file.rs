//! The `a2a.json` format.
//!
//! One object, `agents`, keyed by name — the name is the map key rather than
//! a field, the one place this format and mentra's [`A2aAgentConfig`]
//! disagree. Each entry is a `url` and, optionally, `headers`; nothing else.

use std::{collections::BTreeMap, path::Path};

use mentra::a2a::{A2aAgentConfig, A2aConfigError};
use serde::Deserialize;

use crate::expand::expand;

use super::{A2aAgent, A2aError};

/// The whole file.
///
/// `deny_unknown_fields`, unlike `.mcp.json`'s reader and like
/// `.basis/tools.json`'s: nobody but basis writes this file, so a key basis
/// does not know is a typo rather than another agent's business.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct A2aFile {
    /// Optional so that its absence can be reported. A missing `agents` is
    /// almost always a misspelled one, and defaulting to empty would turn
    /// that into a workspace whose agents quietly never connect.
    agents: Option<BTreeMap<String, RawAgent>>,
}

/// One entry.
///
/// Every field optional so a missing `url` is reported by name: serde's own
/// "missing field" message would be dropped by [`A2aError::Parse`], which
/// keeps only a location.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAgent {
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

/// Reads `text` as the file at `path`.
pub(super) fn parse(path: &Path, text: &str) -> Result<Vec<A2aAgent>, A2aError> {
    parse_with(path, text, &|name| std::env::var(name).ok())
}

/// The same, against an explicit environment, so the rules are testable
/// without mutating the process's own.
fn parse_with(
    path: &Path,
    text: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<A2aAgent>, A2aError> {
    let file: A2aFile = serde_json::from_str(text).map_err(|source| A2aError::Parse {
        path: path.to_path_buf(),
        // serde's own message quotes the value it choked on, which in this
        // file is as likely as not a credential. Location and kind only.
        problem: match source.classify() {
            serde_json::error::Category::Syntax => "a syntax error",
            serde_json::error::Category::Data => "an unknown key or a value of the wrong type",
            serde_json::error::Category::Eof => "an unexpected end of input",
            serde_json::error::Category::Io => "a read error",
        },
        line: source.line(),
        column: source.column(),
    })?;

    let Some(entries) = file.agents else {
        return Err(A2aError::NoAgents {
            path: path.to_path_buf(),
        });
    };

    let origin = path.display().to_string();

    entries
        .into_iter()
        .map(|(name, raw)| raw.into_agent(&origin, name, lookup))
        .collect()
}

/// mentra's refusal, restated where it would echo a value.
///
/// `PlaintextCredentials` embeds the whole URL, whose query string may hold
/// an expanded credential, and suggests `allow_plaintext_credentials` — a
/// knob this file deliberately does not offer. Every other variant already
/// names a field, a header's name or a fixed reason, and never a value.
pub(super) fn refused(error: &A2aConfigError) -> String {
    match error {
        A2aConfigError::PlaintextCredentials { .. } => "has headers that would travel over \
             plaintext `http` to a non-loopback host; use `https` or a loopback address"
            .to_string(),
        other => format!("is not a valid A2A agent configuration: {other}"),
    }
}

impl RawAgent {
    fn into_agent(
        self,
        origin: &str,
        name: String,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<A2aAgent, A2aError> {
        let invalid = |reason: String| A2aError::Invalid {
            origin: origin.to_string(),
            name: name.clone(),
            reason,
        };

        // Expansion failures name the field rather than quoting it: `headers`
        // is where credentials live, so a header's *name* is the most an
        // error may say about it.
        let expanded = |field: &str, raw: &str| -> Result<String, A2aError> {
            expand(raw, lookup)
                .map_err(|reason| invalid(format!("has a `{field}` value that {reason}")))
        };

        let url = self
            .url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
            .ok_or_else(|| invalid("has no `url` to reach".to_string()))?;
        let url = expanded("url", url)?;

        let config = self.headers.iter().try_fold(
            A2aAgentConfig::new(name.clone(), url),
            |config, (key, value)| {
                Ok::<_, A2aError>(
                    config.with_header(key, expanded(&format!("headers.{key}"), value)?),
                )
            },
        )?;

        // mentra's whole check — the name rule, the URL, every header, and the
        // plaintext refusal — here, where a refused entry fails the workspace
        // open loudly and names this file, rather than at connect, where a
        // failure is a warning an ACP client never sees.
        config
            .validate()
            .map_err(|error| invalid(refused(&error)))?;

        Ok(A2aAgent(config))
    }
}

#[cfg(test)]
mod tests;
