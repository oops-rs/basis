//! The one place `!` is read.
//!
//! ADR-0016 keeps the sugar and removes the hazard by reading it exactly once:
//! the string arrives here, a typed [`Spawn`] leaves, and every consumer
//! downstream — the approver, the rule store, the hooks, the audit trail —
//! dispatches on [`Mode`] rather than re-inspecting the text. A second reader
//! is a second opinion, and two consumers that disagree about whether a call
//! was a command is the type confusion the ADR names.
//!
//! ADR-0021 adds one dimension to the same reading: `!@<target> <command>`
//! says *where*. It is the same rule applied to one more fact — a prefix read
//! here and nowhere else, leaving a typed [`Spawn::target`] that basis routes
//! on. It is deliberately not a second schema field, which would be a decision
//! the model had to make on every call, including the calls that have no
//! target to name.
//!
//! Nothing here touches the runtime, which is why it is a module of its own:
//! the whole boundary is testable as a function.

use serde_json::Value;

/// The field the model fills in. One string, because the tool takes one thing.
pub const INPUT_FIELD: &str = "input";

/// What the wire contract calls *here*, and therefore a name no target may
/// take (ADR-0021).
///
/// One constant for two rules that would otherwise drift: the preview writes
/// it when no target was named, and
/// [`RuntimeBuilder::with_command_target`](crate::RuntimeBuilder::with_command_target)
/// refuses it as a registration. Rejecting it *here* as well is what makes the
/// wire spelling unambiguous by construction rather than by convention — a
/// parsed [`Spawn`] can never carry it, so `"target":"local"` in a serialized
/// preview means exactly one thing and a rule written against it cannot be
/// fooled by a model that spelled the reserved word out.
pub(crate) const LOCAL_TARGET: &str = "local";

/// Which of the two acts a call turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `!…` — run this.
    Command,
    /// Anything else — a prompt for a subagent.
    Agent,
}

impl Mode {
    /// The wire spelling, which is what an approver reads and what a pattern
    /// rule globs against. Pinned by a test: changing it silently rewrites
    /// every rule an operator has already stored.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Agent => "agent",
        }
    }
}

/// Every fact the spawn input parser owns: mode, normalized body, and optional
/// command target. The working directory is not model input; the tool context
/// resolves it later when building the authorization preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawn {
    mode: Mode,
    body: String,
    /// Where the command runs, or `None` for *here* (ADR-0021). Always `None`
    /// in [`Mode::Agent`]: a delegation is a subagent on this runtime, and
    /// there is no other place for it to be.
    target: Option<String>,
}

impl Spawn {
    pub const fn mode(&self) -> Mode {
        self.mode
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    /// The registered target this command named, or `None` for the local
    /// executor. Never `Some("local")`: `local` is the reserved wire spelling
    /// for the executor in this process, not a registered target name.
    pub fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }
}

/// Reads a tool call's JSON input into a typed call, or says why it could not.
///
/// The error text reaches the model as that call's result — mentra turns a
/// failed preview into `Tool execution denied: <this>` — so it says what to
/// write instead rather than merely that something was wrong.
pub fn parse(input: &Value) -> Result<Spawn, String> {
    let raw = input
        .get(INPUT_FIELD)
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!(
                "spawn takes one string field, `{INPUT_FIELD}`: a command to run when it starts \
                 with `!`, otherwise a task to delegate"
            )
        })?;

    let mode = classify_spawn_input(input).expect("the same string field was read above");
    read(raw, mode)
}

/// Classifies the lexical spawn mode without validating that the body is
/// executable.
///
/// `None` means the input field is missing or not a string. Empty and
/// whitespace-only strings are still lexically delegations even though
/// [`parse`] subsequently refuses their empty body.
pub fn classify_spawn_input(input: &Value) -> Option<Mode> {
    let raw = input.get(INPUT_FIELD).and_then(Value::as_str)?;
    Some(classify_raw(raw))
}

fn classify_raw(raw: &str) -> Mode {
    match raw.trim().strip_prefix('!') {
        Some(rest) if !rest.starts_with('!') => Mode::Command,
        Some(_) | None => Mode::Agent,
    }
}

/// The prefix rule itself, over a plain string.
///
/// Split from [`parse`] so the JSON shape and the `!` convention are two
/// separately readable rules rather than one nested match.
fn read(raw: &str, mode: Mode) -> Result<Spawn, String> {
    // Trimmed once, here, so that a model's stray leading newline cannot be
    // the difference between a command and a prompt. Every later reader sees
    // the trimmed body, so there is no second normalization to disagree with.
    let trimmed = raw.trim();

    if mode == Mode::Agent {
        // `!!` is the escape, and it consumes exactly one `!`: what remains
        // still begins with the one the prompt meant to have.
        return delegation(trimmed.strip_prefix('!').unwrap_or(trimmed));
    }

    let rest = trimmed
        .strip_prefix('!')
        .expect("command classification requires a leading bang");

    match rest.strip_prefix('@') {
        Some(targeted) => target(targeted),
        None => command(rest, None),
    }
}

/// `!@<name> <command>` — the routing prefix of ADR-0021, read here and
/// nowhere else.
///
/// The name is taken up to the first whitespace and is not trimmed off the
/// front: `!@ mac ls` names nothing, and reading it as `mac` would be this
/// module guessing at a call it cannot see the intent of. Each refusal below
/// spells the working form, because these strings reach the model as
/// `Tool execution denied: …` and a refusal it cannot act on is one it answers
/// by trying the same thing again.
fn target(rest: &str) -> Result<Spawn, String> {
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        return Err(
            "`!@` names where a command runs and needs the target right after it: write \
             `!@<target> <command>`, as `!@mac xcodebuild -list`"
                .to_string(),
        );
    }

    let (name, body) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));

    if !is_target_name(name) {
        return Err(format!(
            "`{name}` is not a target name: a target is letters, digits, `_` or `-`, as \
             `!@mac xcodebuild -list`"
        ));
    }

    if name == LOCAL_TARGET {
        return Err(format!(
            "`{LOCAL_TARGET}` is not a target name: a command with no `@` already runs where \
             basis is running, as `!cargo test`"
        ));
    }

    if body.trim().is_empty() {
        return Err(format!(
            "`!@{name}` names a target but no command; write the command after it, as \
             `!@{name} cargo test`"
        ));
    }

    command(body, Some(name.to_string()))
}

/// The charset a target name may use, and the reason it is this narrow: the
/// name is glob-matched inside a serialized rule pattern and printed into
/// refusals, so a name carrying a quote, a slash or a space would be a name
/// that means one thing to an operator writing a rule and another to the
/// matcher reading it.
pub(crate) fn is_target_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn command(body: &str, target: Option<String>) -> Result<Spawn, String> {
    let body = body.trim();
    if body.is_empty() {
        return Err(
            "`!` on its own runs nothing; write the command after it, as `!cargo test`".to_string(),
        );
    }

    Ok(Spawn {
        mode: Mode::Command,
        body: body.to_string(),
        target,
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
        // A delegation runs on this runtime, so there is nowhere else for it
        // to be: the dimension belongs to commands only (ADR-0021).
        target: None,
    })
}
