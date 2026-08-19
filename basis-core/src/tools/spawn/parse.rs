//! The one place `!` is read.
//!
//! ADR-0016 keeps the sugar and removes the hazard by reading it exactly once:
//! the string arrives here, a typed [`Spawn`] leaves, and every consumer
//! downstream — the approver, the rule store, the hooks, the audit trail —
//! dispatches on [`Mode`] rather than re-inspecting the text. A second reader
//! is a second opinion, and two consumers that disagree about whether a call
//! was a command is the type confusion the ADR names.
//!
//! Nothing here touches the runtime, which is why it is a module of its own:
//! the whole boundary is testable as a function.

use serde_json::Value;

/// The field the model fills in. One string, because the tool takes one thing.
pub(crate) const INPUT_FIELD: &str = "input";

/// Which of the two acts a call turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// `!…` — run this.
    Command,
    /// Anything else — a prompt for a subagent.
    Agent,
}

impl Mode {
    /// The wire spelling, which is what an approver reads and what a pattern
    /// rule globs against. Pinned by a test: changing it silently rewrites
    /// every rule an operator has already stored.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Agent => "agent",
        }
    }
}

/// A call, after the only reading of it that ever happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Spawn {
    mode: Mode,
    body: String,
}

impl Spawn {
    pub(crate) const fn mode(&self) -> Mode {
        self.mode
    }

    pub(crate) fn body(&self) -> &str {
        &self.body
    }
}

/// Reads a tool call's JSON input into a typed call, or says why it could not.
///
/// The error text reaches the model as that call's result — mentra turns a
/// failed preview into `Tool execution denied: <this>` — so it says what to
/// write instead rather than merely that something was wrong.
pub(crate) fn parse(input: &Value) -> Result<Spawn, String> {
    let raw = input
        .get(INPUT_FIELD)
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!(
                "spawn takes one string field, `{INPUT_FIELD}`: a command to run when it starts \
                 with `!`, otherwise a task to delegate"
            )
        })?;

    read(raw)
}

/// The prefix rule itself, over a plain string.
///
/// Split from [`parse`] so the JSON shape and the `!` convention are two
/// separately readable rules rather than one nested match.
fn read(raw: &str) -> Result<Spawn, String> {
    // Trimmed once, here, so that a model's stray leading newline cannot be
    // the difference between a command and a prompt. Every later reader sees
    // the trimmed body, so there is no second normalization to disagree with.
    let trimmed = raw.trim();

    let Some(rest) = trimmed.strip_prefix('!') else {
        return delegation(trimmed);
    };

    // `!!` is the escape, and it consumes exactly one `!`: what remains still
    // begins with the one the prompt meant to have. This is the only way to
    // delegate a task whose own text starts with `!`.
    if rest.starts_with('!') {
        return delegation(rest);
    }

    command(rest)
}

fn command(body: &str) -> Result<Spawn, String> {
    let body = body.trim();
    if body.is_empty() {
        return Err(
            "`!` on its own runs nothing; write the command after it, as `!cargo test`".to_string(),
        );
    }

    Ok(Spawn {
        mode: Mode::Command,
        body: body.to_string(),
    })
}

fn delegation(body: &str) -> Result<Spawn, String> {
    let body = body.trim();
    if body.is_empty() {
        return Err(
            "spawn needs something to do: a command after `!`, or a task to delegate".to_string(),
        );
    }

    Ok(Spawn {
        mode: Mode::Agent,
        body: body.to_string(),
    })
}
