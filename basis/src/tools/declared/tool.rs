//! The wrapper: one declaration, presented to mentra as an `ExecutableTool`.
//!
//! What the model sees is an ordinary tool — a name, a description, a JSON
//! schema. What happens when it calls one is that basis writes the input object
//! to a program's stdin and reads its stdout back as the result. Nothing is
//! quoted, escaped, or handed to a shell anywhere on that path, which is the
//! whole point of the binding (see [`super`]).

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use mentra::tool::{
    ParallelToolContext, RuntimeToolDescriptor, ToolApprovalCategory, ToolAuthorizationPreview,
    ToolCapability, ToolDefinition, ToolDurability, ToolExecutionCategory, ToolExecutor,
    ToolResult,
};
use serde_json::{Value, json};

use crate::subprocess::{self, Completion};

use super::manifest::DeclaredToolSpec;

/// How much later than the tool's own deadline mentra's is set.
///
/// basis's deadline is the one that matters, because it is the one that kills
/// the process: mentra enforces `execution_timeout` by dropping the future, and
/// a `spawn_blocking` future dropped mid-wait abandons the thread rather than
/// stopping the program it is waiting on. So the descriptor still carries a
/// deadline — a turn must not hang on a saturated blocking pool — and it is set
/// deliberately later, so the message the model reads is the one that names the
/// tool and says it was stopped, not the generic one from outside.
const TIMEOUT_BACKSTOP: Duration = Duration::from_secs(5);

/// One tool a workspace declared, wrapped as the thing mentra can run.
///
/// Cheap to clone in the sense that matters: the declaration sits behind an
/// `Arc`, so the per-call clone `spawn_blocking` needs costs a refcount rather
/// than a copy of the schema.
#[derive(Clone)]
pub struct DeclaredTool {
    spec: Arc<DeclaredToolSpec>,
    /// The workspace root the manifest was discovered from — what a relative
    /// `cwd` and a relative program path are resolved against.
    workspace: PathBuf,
    /// The runtime's fixed command environment, which this program receives on
    /// top of the one it inherits. Empty unless the host said otherwise.
    environment: Arc<Vec<(String, String)>>,
}

/// Hand-written for [`DeclaredToolSpec`]'s reason, now with a second field
/// that needs it: the runtime's command environment can hold whatever the host
/// put there, and a derived impl would put every value of it in every `{:?}`
/// of anything holding one. Names survive, because naming a variable is what
/// makes a misconfiguration fixable and repeats nothing that was read.
impl std::fmt::Debug for DeclaredTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeclaredTool")
            .field("spec", &self.spec)
            .field("workspace", &self.workspace)
            .field(
                "environment",
                &self
                    .environment
                    .iter()
                    .map(|(name, _)| (name, "<redacted>"))
                    .collect::<std::collections::BTreeMap<_, _>>(),
            )
            .finish()
    }
}

impl DeclaredTool {
    pub fn new(spec: DeclaredToolSpec, workspace: impl Into<PathBuf>) -> Self {
        Self {
            spec: Arc::new(spec),
            workspace: workspace.into(),
            environment: Arc::new(Vec::new()),
        }
    }

    /// Adds the runtime's fixed command environment to what this program is
    /// spawned with.
    ///
    /// The gap this closes: a host calls
    /// [`RuntimeBuilder::with_command_environment`](crate::RuntimeBuilder::with_command_environment)
    /// to say where its service lives, and reasonably expects every process the
    /// runtime spawns to be told. Commands through
    /// [`spawn`](crate::tools::spawn) were; a declared tool's program was not,
    /// and failed at the far end complaining about a variable the runtime had
    /// been given.
    ///
    /// Separate from [`new`](Self::new) rather than a third argument to it,
    /// because it is the *runtime's* contribution and `new` takes what the
    /// manifest said. [`crate::Workspace`] applies it at open, from the runtime
    /// the workspace borrows.
    #[must_use]
    pub fn with_command_environment(
        self,
        environment: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self {
            environment: Arc::new(environment.into_iter().collect()),
            ..self
        }
    }

    /// The name the model calls and an operator writes in a rule.
    pub fn name(&self) -> &str {
        &self.spec.name
    }

    /// The declaration this was built from.
    pub fn spec(&self) -> &DeclaredToolSpec {
        &self.spec
    }

    /// The runtime pairs this program is spawned with, before the manifest's
    /// own `env` is layered over them.
    pub fn command_environment(&self) -> &[(String, String)] {
        &self.environment
    }
}

impl ToolDefinition for DeclaredTool {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        RuntimeToolDescriptor::builder(&self.spec.name)
            .description(&self.spec.description)
            .input_schema(self.spec.input_schema.clone())
            // `ProcessExec` and nothing else: basis knows a program will run and
            // knows nothing about what it touches, and a capability list that
            // guessed would be worse than one that is merely coarse.
            .capabilities(vec![ToolCapability::ProcessExec])
            .side_effect_level(self.spec.side_effect.level())
            .durability(ToolDurability::Ephemeral)
            // Never batched with anything. basis cannot know what somebody's
            // program writes, so it never lets one run beside another call.
            .execution_category(ToolExecutionCategory::ExclusiveLocalMutation)
            .approval_category(ToolApprovalCategory::Process)
            .execution_timeout(self.spec.timeout() + TIMEOUT_BACKSTOP)
            .build()
    }
}

#[async_trait]
impl ToolExecutor for DeclaredTool {
    /// What the approver sees, and it is deliberately not the default.
    ///
    /// The default preview restates the static descriptor and passes the raw
    /// input through as the structured input, which for this binding would show
    /// an approver the arguments and leave out the only thing they actually
    /// need: *which program is about to run*. A declared tool's name is chosen
    /// by the same file that chooses its command, so the name is not evidence.
    ///
    /// So `structured_input` carries `{tool, command, cwd, input}` — what an
    /// approver renders, what mentra globs a remembered rule's pattern against
    /// (`RuleStore::matching_rule`), and what the audit trail keeps. **`env` is
    /// not in it**, and that is the one asymmetry worth stating: the command
    /// and its arguments are how a spawn is understood, while the environment
    /// is where the credential is, and a preview travels further than a glance.
    fn authorization_preview(
        &self,
        _ctx: &ParallelToolContext,
        input: &Value,
    ) -> Result<ToolAuthorizationPreview, String> {
        preview(&self.spec, &self.workspace, &self.descriptor(), input)
    }

    async fn execute(&self, _ctx: ParallelToolContext, input: Value) -> ToolResult {
        run(
            Arc::clone(&self.spec),
            self.workspace.clone(),
            &self.environment,
            input,
        )
        .await
    }
}

/// Assembles the per-call preview.
///
/// A free function rather than the method's body so the shape an approver is
/// shown can be asserted without a runtime to build a context from — the
/// context is unused here anyway, because everything this presents comes from
/// the declaration rather than from the call's surroundings.
fn preview(
    spec: &DeclaredToolSpec,
    workspace: &Path,
    descriptor: &RuntimeToolDescriptor,
    input: &Value,
) -> Result<ToolAuthorizationPreview, String> {
    // Refused here, ahead of the approver: a call that cannot run is not worth
    // a person's attention, and asking about one teaches them that approving is
    // what makes errors go away.
    check_input(spec, input)?;

    let cwd = spec.working_directory(workspace);

    Ok(ToolAuthorizationPreview {
        capabilities: descriptor.capabilities.clone(),
        side_effect_level: descriptor.side_effect_level,
        durability: descriptor.durability,
        execution_category: descriptor.execution_category,
        approval_category: descriptor.approval_category,
        raw_input: input.clone(),
        structured_input: json!({
            "tool": spec.name,
            "command": spec.command,
            "cwd": cwd,
            "input": input,
        }),
        working_directory: cwd,
    })
}

/// Runs the program and turns what it did into the call's result.
async fn run(
    spec: Arc<DeclaredToolSpec>,
    workspace: PathBuf,
    runtime_environment: &[(String, String)],
    input: Value,
) -> ToolResult {
    // Asked again rather than trusted from the preview: the preview is only
    // reached when an authorizer is installed, and a check that a missing
    // authorizer removes is not a check.
    check_input(&spec, &input)?;

    let payload = serde_json::to_string(&input).map_err(|error| {
        format!(
            "{} was called with input basis could not serialize: {error}",
            spec.name
        )
    })?;

    let running = Arc::clone(&spec);
    let environment = environment(runtime_environment, &spec.env);

    // Spawning a process and waiting for it is genuinely blocking work, so it
    // goes to a thread meant for it rather than onto a runtime worker — which
    // holds on every runtime flavor, including the `current_thread` an embedder
    // inside an editor is likely to have.
    let completion = tokio::task::spawn_blocking(move || {
        subprocess::execute(
            &running.command,
            &running.working_directory(&workspace),
            &environment,
            &payload,
            running.timeout(),
        )
    })
    .await
    .map_err(|error| format!("{} could not be run: {error}", spec.name))?;

    answer(&spec, completion)
}

/// What one declared program is spawned with, over the environment it
/// inherits.
///
/// **The manifest wins.** The runtime's pairs are the host's statement about
/// every process this runtime spawns; the manifest's are this one tool's own,
/// and between two statements about the same name the more specific one holds —
/// the same direction the workspace's `tools.json` already beats the global
/// one. A host that wants the opposite is asking for a value a repository
/// cannot override, which is a different feature and not this one.
///
/// Neither set clears the inherited environment, and that is deliberate:
/// `PATH`, `HOME` and the rest come from there, and a program that lost them
/// would be a manifest that used to work and now does not.
fn environment(
    runtime: &[(String, String)],
    manifest: &[(String, String)],
) -> Vec<(String, String)> {
    runtime
        .iter()
        .filter(|(name, _)| !manifest.iter().any(|(declared, _)| declared == name))
        .chain(manifest.iter())
        .cloned()
        .collect()
}

/// Turns how the program ended into what the model reads.
///
/// Every failure is an `Err`, which reaches the model as that call's result and
/// is the only thing telling it what to do next — so each one says what
/// happened rather than that something did. The program's own stderr is quoted
/// because it is the program's own explanation; the manifest's `env` is not,
/// anywhere.
fn answer(spec: &DeclaredToolSpec, completion: std::io::Result<Completion>) -> ToolResult {
    let completion = completion.map_err(|error| {
        // The io error names the failure, never the command: a `${VAR}` in a
        // program path was resolved before it got here.
        format!("{} could not be started: {error}", spec.name)
    })?;

    let (code, stdout, stderr) = match completion {
        Completion::TimedOut => {
            return Err(format!(
                "{} did not finish within {} seconds and was stopped",
                spec.name,
                spec.timeout().as_secs()
            ));
        }
        Completion::Exited {
            code,
            stdout,
            stderr,
        } => (code, stdout, stderr),
    };

    match code {
        Some(0) => Ok(succeeded(spec, stdout)),
        Some(code) => Err(failed(spec, code, &stdout, &stderr)),
        None => Err(format!(
            "{} was killed by a signal before it answered",
            spec.name
        )),
    }
}

/// stdout, verbatim but for the trailing newline every program prints.
///
/// How much of it survives is mentra's `ToolOutputLimiter`, which bounds and
/// spills every tool result on the runtime; a second cap here would only make
/// the two disagree.
fn succeeded(spec: &DeclaredToolSpec, stdout: String) -> String {
    if stdout.trim().is_empty() {
        // A result that is empty or only whitespace reads to a model as a tool
        // that did nothing, which is a different thing from one that succeeded
        // quietly.
        return format!("{} finished and printed nothing", spec.name);
    }

    // Only the trailing newline, and only from the end: what a program printed
    // in between is the answer, indentation and blank lines included.
    stdout.trim_end_matches('\n').to_string()
}

/// Names the tool, the exit code, and whatever the program said about itself.
///
/// stderr first because that is where a program explains a failure, and stdout
/// as the fallback because plenty of them do not — a failure that quotes
/// neither leaves the model with "it failed" and nothing to act on.
fn failed(spec: &DeclaredToolSpec, code: i32, stdout: &str, stderr: &str) -> String {
    let explanation = if stderr.trim().is_empty() {
        subprocess::truncated_output(stdout)
    } else {
        stderr.trim().to_string()
    };

    if explanation.is_empty() {
        return format!("{} exited {code} and said nothing", spec.name);
    }

    format!("{} exited {code}: {explanation}", spec.name)
}

/// The complement of mentra's root-shape rule, and nothing else.
///
/// mentra reads a call against the `input_schema` its tool published before it
/// authorizes anything, so `required`, scalar types, `enum` and a misspelled
/// property are answered upstream — with the field named, which is what a
/// model can act on. Since 0.21 (`5e16092`, moved upstream by this binding's
/// own report) that includes a root that is not an object:
/// `validate_tool_input`'s `root_shape_error` refuses one for a schema
/// declaring `properties` or `required` without saying `type`
/// (`mentra/src/tool/schema.rs`), and a schema that *does* say `type` is
/// refused by the ordinary type check beside it.
///
/// What that rule deliberately leaves open is a schema describing nothing at
/// all — `{}`, or `{"description": "…"}` — on the stated reasoning that a
/// schema describing nothing is not describing an object either. Correct for
/// mentra, whose tools may take anything; wrong here, because
/// [`check_schema`](super::manifest) accepts exactly such a manifest and this
/// binding writes whatever arrives to a program's **stdin**, where a bare
/// string is not the JSON object the program was written to read.
///
/// So this fires exactly where upstream's rule does not — no `type`, no
/// `properties`, no `required` — and names the tool, which is what makes the
/// refusal something the model can act on. Anything upstream can see stays
/// upstream's to word.
fn check_input(spec: &DeclaredToolSpec, input: &Value) -> Result<(), String> {
    if input.is_object() {
        return Ok(());
    }

    // A schema that says any of these three is one upstream's validator reads
    // for itself, so a non-object call under it never reaches this binding.
    let described = spec.input_schema.as_object().is_some_and(|schema| {
        schema.contains_key("type")
            || schema.contains_key("properties")
            || schema.contains_key("required")
    });
    if described {
        return Ok(());
    }

    Err(format!(
        "{} takes a JSON object matching its input schema",
        spec.name
    ))
}

#[cfg(test)]
mod tests;
