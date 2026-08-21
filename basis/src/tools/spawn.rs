//! `spawn` — the model's only route to a command and to a subagent.
//!
//! ADR-0016. Two doors existed for *do something I cannot do by thinking* —
//! mentra's `shell` builtin and its `task` intrinsic — and they were governed
//! as if they were unrelated: two side-effect levels, two names at basis's
//! [`ApprovalGate`](crate::approval::ApprovalGate), and therefore two rule
//! namespaces for what an operator thinks of as one question. This is that one
//! question, with one name on it.
//!
//! # The shape the model sees
//!
//! One string. A leading `!` means *run this*; anything else is a task for a
//! subagent. A prompt that genuinely starts with `!` is written `!!`. The
//! prefix is surface: it is read exactly once, at the boundary, and everything
//! after that dispatches on the typed mode — which is why the sugar costs no
//! type confusion between the approver, the rule store and the audit trail.
//!
//! A command may also say *where*: `!@<target> <command>` runs it on an
//! executor the host registered by that name
//! ([`RuntimeBuilder::with_command_target`](crate::RuntimeBuilder::with_command_target),
//! ADR-0021). One more fact on the same act rather than a second tool, because
//! a second tool would be a second name at the gate and a second rule
//! namespace — the two doors ADR-0016 closed, rebuilt by hand. The prefix is
//! read by the same parser, in the same pass, and the target it leaves behind
//! is a typed field like the mode beside it.
//!
//! # What the approver sees
//!
//! [`ToolExecutor::authorization_preview`] is overridden rather than left at
//! its default, which merely restates the static descriptor. A command reports
//! [`ToolSideEffectLevel::Process`] and a delegation reports
//! [`ToolSideEffectLevel::LocalState`]; both are consequential, so
//! [`is_consequential`](crate::approval::is_consequential) never waves either
//! through under the reads-are-never-asked rule. The preview's
//! `structured_input` is the parsed `{mode, body, cwd, target}` — the thing an
//! approver renders, and the thing mentra globs a remembered rule's pattern against
//! (`RuleStore::matching_rule`). A `RuleKey { tool_name: "spawn", pattern }` is
//! therefore a command allowlist expressible as data.
//!
//! Order matters and is mentra's, not basis's: the orchestrator builds this
//! preview and puts it to the authorizer *before* it calls `execute_mut`
//! (`ToolRuntime::execute_registered_tool`), so nothing here runs ahead of the
//! answer. A preview that returns `Err` is a refusal in its own right, reaching
//! the model as `Tool execution denied: …` without the approver — or the rule
//! store — ever being consulted. That is where the depth guard lives, because
//! a structural floor is not a thing an allow-rule should be able to lift.
//!
//! # What this is not
//!
//! Approval is policy, not confinement (ADR-0013). An approved command runs
//! with the full authority of this process, and `--no-shell` remains the only
//! thing that shuts commands off entirely: it sets
//! `RuntimePolicy::allow_shell_commands(false)`, and mentra's policy refuses on
//! the same path `spawn` calls — after this tool has been authorized, before
//! anything executes.

mod depth;
mod execute;
mod parse;

use async_trait::async_trait;
use mentra::tool::{
    ParallelToolContext, RuntimeToolDescriptor, ToolApprovalCategory, ToolAuthorizationPreview,
    ToolCapability, ToolContext, ToolDefinition, ToolDurability, ToolExecutionCategory,
    ToolExecutor, ToolResult, ToolSideEffectLevel,
};
use serde_json::{Value, json};

use parse::{INPUT_FIELD, LOCAL_TARGET, Mode, Spawn, parse};

use std::collections::BTreeSet;

// The runtime's hook dispatcher must know whether a `spawn` call is a command
// before denying it for a shell-off workspace, and the module docs above make
// the rule: the `!` prefix is read exactly once, here. Re-exported crate-wide
// so the dispatcher asks this parser rather than becoming a second reader.
pub(crate) use parse::{Mode as SpawnMode, parse as parse_spawn};

pub use depth::MAX_DEPTH;

/// The name the model calls, an operator writes in a rule, and a
/// `.basis/hooks.json` entry matches on.
///
/// Public because it is all three of those: a host writing a hook or a
/// remembered rule needs the string, and a literal in their config that drifts
/// from basis's fails by never matching rather than by erroring.
pub const SPAWN: &str = "spawn";

/// What the model reads to learn the tool. The `!` convention is only
/// discoverable here, so it is spelled out with an example of each mode.
const DESCRIPTION: &str = "\
Hand work to something else and read the result back.

Takes one string. If it starts with `!`, the rest runs as a shell command in \
this workspace and you get its output — `!cargo test -q`. Anything else is a \
task handed to a subagent, which works on it and returns a final answer — \
`find every TODO under src/ and summarise them`.

To delegate a task whose own text starts with `!`, double it: `!!important, …`.

Commands are put to the operator before they run, so ask for one command that \
does the job rather than several that each need answering.";

/// The `!@<target>` paragraph, added to the description and to the schema only
/// when this runtime has somewhere to route to (ADR-0021).
///
/// A model must not be taught a door that does not exist. The best case for
/// mentioning an unregistered prefix is a wasted call; the worse case is a
/// model reading an unexplained refusal as an invitation to guess names.
fn targets_paragraph(targets: &[String]) -> String {
    format!(
        "A command can also say where it runs: `!@<target> <command>` runs it on that target \
         rather than here — `!@{first} <command>`. Registered targets: {names}. A command with \
         no `@` runs where basis itself is running, which is what you want unless the work \
         needs one of those targets.",
        first = targets[0],
        names = listed(targets),
    )
}

/// Target names as prose, in one spelling, so the description, the schema and
/// every refusal name the same set the same way.
fn listed(targets: &[String]) -> String {
    targets
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// basis's own tool, and the first tool basis registers on a runtime it builds.
///
/// One instance serves a whole runtime: mentra's registry holds it behind an
/// `Arc` and every subagent shares its parent's runtime handle, which is what
/// lets the delegation depth ledger inside see the whole tree.
#[derive(Debug, Default)]
pub struct SpawnTool {
    depth: depth::Depth,
    /// The command targets this runtime can route to, sorted and deduplicated
    /// so the description a model reads is the same string on every build.
    /// Empty is the ordinary case, and the one in which the model is never
    /// told the routing prefix exists.
    targets: Vec<String>,
}

impl SpawnTool {
    /// The tool as every runtime had it before ADR-0021: commands run where
    /// basis runs, and the model is told nothing about targets.
    pub fn new() -> Self {
        Self::default()
    }

    /// The same tool, told which command targets its runtime registered.
    ///
    /// The names are all this needs: `spawn` routes by putting the parsed
    /// target on the request, and *which executor* that name resolves to is
    /// the runtime's business, not the tool's (ADR-0018). What the names buy
    /// is the two things only the tool can do with them — teach the prefix in
    /// the description, and refuse a name nothing registered before the
    /// approver is asked about it.
    ///
    /// Called by [`RuntimeBuilder`](crate::RuntimeBuilder) with the set it
    /// collected; a host driving mentra directly calls it with whatever it
    /// registered on its own executor. Names it does not recognise are refused
    /// at the boundary, so passing a name here that no executor answers to
    /// turns a working call into a refusal rather than into a silent local
    /// run — which is the direction this has to fail in.
    pub fn with_targets(targets: impl IntoIterator<Item = String>) -> Self {
        Self {
            depth: depth::Depth::default(),
            targets: targets
                .into_iter()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        }
    }

    /// `Ok` when this call names a target this runtime can reach, and the
    /// refusal the model should read instead when it does not.
    ///
    /// Asked only of a command: [`Mode::Agent`] never carries a target, by
    /// construction in the parser.
    fn authorize_target(&self, spawn: &Spawn) -> Result<(), String> {
        let Some(target) = spawn.target() else {
            return Ok(());
        };

        if self.targets.iter().any(|name| name == target) {
            return Ok(());
        }

        Err(if self.targets.is_empty() {
            format!(
                "spawn has no command targets registered, so `!@{target}` has nowhere to run; \
                 write the command with no `@` to run it where basis is running"
            )
        } else {
            format!(
                "spawn has no command target named `{target}`; the registered targets are {}, \
                 and a command with no `@` runs where basis is running",
                listed(&self.targets)
            )
        })
    }
}

impl ToolDefinition for SpawnTool {
    /// The static descriptor, which is what a reader that has no call in hand
    /// sees. Its side-effect level is the *stronger* of the two modes on
    /// purpose: a tool that can run commands should never describe itself as
    /// something milder when asked in the abstract. Per-call precision is
    /// [`ToolExecutor::authorization_preview`]'s job.
    fn descriptor(&self) -> RuntimeToolDescriptor {
        RuntimeToolDescriptor::builder(SPAWN)
            .description(description(&self.targets))
            .input_schema(input_schema(&self.targets))
            .capabilities(vec![
                ToolCapability::ProcessExec,
                ToolCapability::FilesystemWrite,
                ToolCapability::Delegation,
            ])
            .side_effect_level(ToolSideEffectLevel::Process)
            .durability(ToolDurability::Ephemeral)
            .execution_category(ToolExecutionCategory::ExclusiveLocalMutation)
            .approval_category(ToolApprovalCategory::Process)
            .build()
    }
}

/// What the model reads, with the routing prefix included only when this
/// runtime has somewhere to route to.
fn description(targets: &[String]) -> String {
    if targets.is_empty() {
        return DESCRIPTION.to_string();
    }

    format!("{DESCRIPTION}\n\n{}", targets_paragraph(targets))
}

/// One required string, and no second field to decide about on every call —
/// which is why the target rides in the same string rather than beside it.
fn input_schema(targets: &[String]) -> Value {
    let mut description = "A shell command when it starts with `!`, otherwise a task to \
                           delegate. Write `!!` to begin a task with a literal `!`."
        .to_string();
    if !targets.is_empty() {
        description.push_str(&format!(
            " Write `!@<target> <command>` to run a command on one of this runtime's targets \
             ({}).",
            listed(targets)
        ));
    }

    json!({
        "type": "object",
        "properties": {
            INPUT_FIELD: {
                "type": "string",
                "description": description,
            }
        },
        "required": [INPUT_FIELD],
    })
}

#[async_trait]
impl ToolExecutor for SpawnTool {
    fn authorization_preview(
        &self,
        ctx: &ParallelToolContext,
        input: &Value,
    ) -> Result<ToolAuthorizationPreview, String> {
        let spawn = parse(input)?;
        // Refused here, ahead of the approver and ahead of the rule store, so
        // the floor cannot be lifted by an answer or by a remembered rule. A
        // routing destination nothing registered is the same kind of fact as
        // the depth floor: not a judgement call, so not a question for a
        // person (ADR-0021).
        self.authorize_target(&spawn)?;
        if spawn.mode() == Mode::Agent {
            self.depth.authorize_delegation(&ctx.agent_id)?;
        }
        let cwd = ctx.resolve_working_directory(None)?;

        Ok(preview(&spawn, cwd, &self.descriptor(), input))
    }

    /// Both modes run in the exclusive lane — a command mutates the workspace
    /// and a delegation borrows the agent — so this only ever refines *which*
    /// exclusive category a call is, never whether it can be batched.
    fn execution_category(&self, input: &Value) -> ToolExecutionCategory {
        parse(input).map_or(ToolExecutionCategory::ExclusiveLocalMutation, |spawn| {
            execution_category(spawn.mode())
        })
    }

    async fn execute_mut(&self, mut ctx: ToolContext<'_>, input: Value) -> ToolResult {
        let spawn = parse(&input)?;

        match spawn.mode() {
            Mode::Command => {
                // Asked again rather than trusted from the preview, for the
                // reason the depth floor is: the preview is only reached when
                // an authorizer is installed, and a guard that a missing
                // authorizer removes is not a guard.
                self.authorize_target(&spawn)?;
                execute::command(&ctx, spawn.body()).await
            }
            Mode::Agent => {
                // Asked again rather than trusted from the preview: the preview
                // is only reached when an authorizer is installed, and a floor
                // that a missing authorizer removes is not a floor.
                let depth = self.depth.authorize_delegation(&ctx.agent_id)?;
                execute::delegate(&self.depth, &mut ctx, spawn.body(), depth).await
            }
        }
    }
}

/// Assembles the per-call preview: the parsed call, and the descriptor fields
/// that this mode — rather than the tool in the abstract — earns.
fn preview(
    spawn: &Spawn,
    cwd: std::path::PathBuf,
    descriptor: &RuntimeToolDescriptor,
    raw_input: &Value,
) -> ToolAuthorizationPreview {
    // The wire contract of ADR-0016, widened by ADR-0021 to
    // `{mode, body, cwd, target}`, and still never the string the model wrote.
    // What an approver renders, what a pattern rule globs against, and what the
    // audit trail keeps.
    //
    // `target` is additive in the strict sense: the three older keys keep their
    // spellings and their values, and serde_json serializes a map in key order,
    // so `"target"` sorts after all three and a rule an operator already wrote
    // still matches exactly what it matched before. It reads `"local"` rather
    // than `null` when no target was named, because *here* is a value an
    // operator will want to write a rule about and a glob against a JSON null
    // is a spelling nobody notices missing.
    let structured_input = json!({
        "mode": spawn.mode().as_str(),
        "body": spawn.body(),
        "cwd": cwd,
        "target": spawn.target().unwrap_or(LOCAL_TARGET),
    });

    ToolAuthorizationPreview {
        working_directory: cwd,
        capabilities: capabilities(spawn.mode()),
        side_effect_level: side_effect_level(spawn.mode()),
        durability: descriptor.durability,
        execution_category: execution_category(spawn.mode()),
        approval_category: approval_category(spawn.mode()),
        raw_input: raw_input.clone(),
        structured_input,
    }
}

/// `Process` for a command, `LocalState` for a delegation — the levels `shell`
/// and `task` declared, now decided per call instead of per name. Neither is
/// `None`, so command mode can never present as read-only.
const fn side_effect_level(mode: Mode) -> ToolSideEffectLevel {
    match mode {
        Mode::Command => ToolSideEffectLevel::Process,
        Mode::Agent => ToolSideEffectLevel::LocalState,
    }
}

fn capabilities(mode: Mode) -> Vec<ToolCapability> {
    match mode {
        Mode::Command => vec![ToolCapability::ProcessExec, ToolCapability::FilesystemWrite],
        Mode::Agent => vec![ToolCapability::Delegation],
    }
}

const fn approval_category(mode: Mode) -> ToolApprovalCategory {
    match mode {
        Mode::Command => ToolApprovalCategory::Process,
        Mode::Agent => ToolApprovalCategory::Delegation,
    }
}

const fn execution_category(mode: Mode) -> ToolExecutionCategory {
    match mode {
        Mode::Command => ToolExecutionCategory::ExclusiveLocalMutation,
        Mode::Agent => ToolExecutionCategory::Delegation,
    }
}

#[cfg(test)]
mod tests;
