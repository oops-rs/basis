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
//! ADR-0029 gives a delegation the same dimension: `@<agent> <task>` hands the
//! task to a remote agent, `@<agent>` alone waits again on what that agent is
//! still working on, and `@@` escapes a task that begins with a literal `@`.
//!
//! Nothing here touches the runtime, which is why it is a module of its own:
//! the whole boundary is testable as a function.

use serde_json::Value;

use super::reach::Reach;

/// The field the model fills in. One string, because the tool takes one thing.
pub const INPUT_FIELD: &str = "input";

/// The one optional field beside it: which agents `@<name>` may reach
/// (ADR-0037 §3). Typed, never read out of the string.
pub const REACH_FIELD: &str = "reach";

/// Parameters for a remote agent, beside the task: a JSON object sent with
/// it — over A2A, as the request's `metadata` (ADR-0040). Only with
/// `@<agent> <task>`.
pub const METADATA_FIELD: &str = "metadata";

/// Files sent with a task: workspace paths, read when the task is handed
/// over (ADR-0041). With a task for any agent — never a command or a wait.
pub const FILES_FIELD: &str = "files";

/// What the wire contract calls *here*, and therefore a name no target may
/// take (ADR-0021).
///
/// One constant for two rules that would otherwise drift: the preview writes
/// it when no target was named, and target-name validation refuses it as a
/// registration. Rejecting it *here* as well is what makes the wire spelling
/// unambiguous by construction rather than by convention — a parsed
/// [`Spawn`] can never carry it, so `"target":"local"` in a serialized
/// preview means exactly one thing and a rule written against it cannot be
/// fooled by a model that spelled the reserved word out.
pub(crate) const LOCAL_TARGET: &str = "local";

/// Which of the two acts a call turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `!…` — run this.
    Command,
    /// Anything else — a task for a subagent here, or for a remote agent when
    /// it begins `@<agent>`.
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
/// target — where a command runs, or which remote agent a task is for. The working directory is not model input; the tool context
/// resolves it later when building the authorization preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spawn {
    mode: Mode,
    body: String,
    /// Where a command runs (ADR-0021), or the agent a delegation names
    /// (ADR-0029, ADR-0037); `None` for *here* — this process for a command, a
    /// one-off subagent on this runtime for a delegation.
    target: Option<String>,
    /// What the model said `@<name>` may reach, or `None` when it said
    /// nothing and the runtime's policy decides.
    reach: Option<Reach>,
    /// Parameters for the remote agent a task is handed to (ADR-0040).
    metadata: Option<serde_json::Map<String, Value>>,
    /// Workspace paths of the files sent with a task (ADR-0041), as written.
    files: Vec<String>,
}

impl Spawn {
    pub const fn mode(&self) -> Mode {
        self.mode
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    /// What the model said `@<name>` may reach (ADR-0037 §3), when it said.
    pub const fn reach(&self) -> Option<Reach> {
        self.reach
    }

    /// The parameters sent beside a task for a remote agent (ADR-0040).
    pub fn metadata(&self) -> Option<&serde_json::Map<String, Value>> {
        self.metadata.as_ref()
    }

    /// The workspace paths of the files sent with a task (ADR-0041), as
    /// written; empty when it sends none.
    pub fn files(&self) -> &[String] {
        &self.files
    }

    /// The target this call named — a command's executor, or a delegation's
    /// agent — or `None` for here. Never `Some("local")`: `local` is the
    /// reserved wire spelling for this process, not a name anything can take.
    pub fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }

    /// Whether this is a delegation to a named agent that hands it nothing
    /// new: `@<agent>` alone, which waits again on a remote agent's
    /// outstanding task (ADR-0029 §5). The one call whose body may be empty.
    pub fn is_remote_wait(&self) -> bool {
        self.mode == Mode::Agent && self.target.is_some() && self.body.is_empty()
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
    let reach = match input.get(REACH_FIELD) {
        None | Some(Value::Null) => None,
        Some(Value::String(said)) => Some(Reach::parse(said).ok_or_else(|| {
            format!(
                "`{REACH_FIELD}` is one of `auto`, `remote` or `local`, not `{said}`; leave it \
                 out to let a name reach a remote agent or a teammate here, whichever has it"
            )
        })?),
        Some(_) => {
            return Err(format!(
                "`{REACH_FIELD}` is a string, one of `auto`, `remote` or `local`"
            ));
        }
    };
    let mut spawn = read(raw, mode)?;
    spawn.reach = reach;
    spawn.metadata = match input.get(METADATA_FIELD) {
        None | Some(Value::Null) => None,
        Some(Value::Object(map)) if map.is_empty() => None,
        Some(Value::Object(map)) => {
            // Parameters travel with a task to an agent elsewhere, and with
            // nothing else: a command, a local task and a wait have nowhere
            // to put them, and dropping them would be a call quietly unlike
            // the one written.
            if spawn.mode != Mode::Agent || spawn.target.is_none() || spawn.body.is_empty() {
                return Err(format!(
                    "`{METADATA_FIELD}` goes with a task for a remote agent, `@<agent> <task>`: \
                     it is sent beside the task as the agent's parameters. `@<agent>` alone \
                     keeps waiting on a task that already has its parameters, so leave \
                     `{METADATA_FIELD}` out there"
                ));
            }
            Some(map.clone())
        }
        Some(_) => {
            return Err(format!(
                "`{METADATA_FIELD}` is a JSON object of parameters for the agent, as \
                 {{\"project\": \"lama\"}}"
            ));
        }
    };
    spawn.files = files(input.get(FILES_FIELD), &spawn)?;
    Ok(spawn)
}

/// The paths in `files`, when the call is one that can carry them: files go
/// with a task, for an agent here or elsewhere, and a command or a wait has
/// nowhere to put them.
fn files(field: Option<&Value>, spawn: &Spawn) -> Result<Vec<String>, String> {
    let shape =
        || format!("`{FILES_FIELD}` is a list of workspace paths, as [\"screens/login.png\"]");
    let paths = match field {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(paths)) => paths
            .iter()
            .map(|path| match path {
                Value::String(path) if !path.trim().is_empty() => Ok(path.clone()),
                _ => Err(shape()),
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(shape()),
    };
    if !paths.is_empty() && (spawn.mode != Mode::Agent || spawn.body.is_empty()) {
        return Err(format!(
            "`{FILES_FIELD}` goes with a task: they are sent to the agent the task is handed \
             to. A command has nowhere to put them, and `@<agent>` alone keeps waiting on a \
             task that already has its files, so leave `{FILES_FIELD}` out there"
        ));
    }
    Ok(paths)
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
        // still begins with the one the prompt meant to have. It is read
        // before `@`, so `!!@x` is a task that begins `!@x`.
        if let Some(escaped) = trimmed.strip_prefix('!') {
            return delegation(escaped);
        }
        // `@@` is the same escape for `@`, consuming exactly one of them.
        if let Some(escaped) = trimmed.strip_prefix("@@") {
            return delegation(&format!("@{escaped}"));
        }
        return match trimmed.strip_prefix('@') {
            Some(addressed) => remote(addressed),
            None => delegation(trimmed),
        };
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
            "`{name}` is not a target name: a target is letters, digits, `_` or `-`, with `/` \
             between parts, as `!@mac xcodebuild -list`"
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
/// refusals, so a name carrying a quote or a space would be a name that
/// means one thing to an operator writing a rule and another to the matcher
/// reading it.
///
/// `/` joins segments — `team/reviewer`, a registry's namespaced handle
/// (ADR-0037 §8) — and nothing else: never first, never last, never doubled.
/// It is the one character the glob treats specially (`*` does not cross
/// it), which is a fact the ADR records for rule writers rather than a
/// reason to keep it out.
pub(crate) fn is_target_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.contains("//")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '/')
}

/// Whether a name is namespaced — `team/reviewer` — which always means a
/// resolver's agent, never a teammate here (ADR-0037 §8).
pub(crate) fn is_namespaced(name: &str) -> bool {
    name.contains('/')
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
        reach: None,
        metadata: None,
        files: Vec::new(),
    })
}

/// `@<agent> <task>` — a delegation to a named agent (ADR-0029, ADR-0037:
/// a remote agent a resolver knows, or a teammate here), read here and
/// nowhere else.
///
/// Read like `!@`'s target: the name runs to the first whitespace, a space
/// straight after `@` names nothing, and every refusal spells the working form
/// and the `@@` escape, because a task that merely happened to begin with `@`
/// should be one rewrite away from going through.
fn remote(rest: &str) -> Result<Spawn, String> {
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        return Err(
            "`@` names an agent and needs its name right after it: write \
             `@<agent> <task>`, as `@reviewer look at this diff`, or `@@` to begin a task with \
             a literal `@`"
                .to_string(),
        );
    }

    let (name, body) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));

    if !is_target_name(name) {
        return Err(format!(
            "`{name}` is not an agent name: a name is letters, digits, `_` or `-`, with `/` \
             between parts, as `@reviewer look at this diff` or `@team/reviewer …`; write `@@` \
             to begin a task with a literal `@`"
        ));
    }

    if name == LOCAL_TARGET {
        return Err(format!(
            "`{LOCAL_TARGET}` is not an agent name: a task with no `@` already goes to a \
             subagent here"
        ));
    }

    Ok(Spawn {
        mode: Mode::Agent,
        // Empty is allowed here and only here: `@<agent>` alone waits on that
        // agent's outstanding task rather than sending a new one.
        body: body.trim().to_string(),
        target: Some(name.to_string()),
        reach: None,
        metadata: None,
        files: Vec::new(),
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
        // A delegation with no `@` is a subagent on this runtime.
        target: None,
        reach: None,
        metadata: None,
        files: Vec::new(),
    })
}
