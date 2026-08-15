//! `spawn` — the model's only route to a command and to a subagent.
//!
//! ADR-0016. Two doors existed for *do something I cannot do by thinking* —
//! mentra's `shell` builtin and its `task` intrinsic — and they were governed
//! as if they were unrelated: two side-effect levels, two names at lan's
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
//! # What the approver sees
//!
//! [`ToolExecutor::authorization_preview`] is overridden rather than left at
//! its default, which merely restates the static descriptor. A command reports
//! [`ToolSideEffectLevel::Process`] and a delegation reports
//! [`ToolSideEffectLevel::LocalState`]; both are consequential, so
//! [`is_consequential`](crate::approval::is_consequential) never waves either
//! through under the reads-are-never-asked rule. The preview's
//! `structured_input` is the parsed `{mode, body, cwd}` — the thing an approver
//! renders, and the thing mentra globs a remembered rule's pattern against
//! (`RuleStore::matching_rule`). A `RuleKey { tool_name: "spawn", pattern }` is
//! therefore a command allowlist expressible as data.
//!
//! Order matters and is mentra's, not lan's: the orchestrator builds this
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

use parse::{INPUT_FIELD, Mode, Spawn, parse};

// The runtime's hook dispatcher must know whether a `spawn` call is a command
// before denying it for a shell-off workspace, and the module docs above make
// the rule: the `!` prefix is read exactly once, here. Re-exported crate-wide
// so the dispatcher asks this parser rather than becoming a second reader.
pub(crate) use parse::{Mode as SpawnMode, parse as parse_spawn};

pub use depth::MAX_DEPTH;

/// The name the model calls, an operator writes in a rule, and a
/// `.lan/hooks.json` entry matches on.
///
/// Public because it is all three of those: a host writing a hook or a
/// remembered rule needs the string, and a literal in their config that drifts
/// from lan's fails by never matching rather than by erroring.
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

/// lan's own tool, and the first tool lan registers on a runtime it builds.
///
/// One instance serves a whole runtime: mentra's registry holds it behind an
/// `Arc` and every subagent shares its parent's runtime handle, which is what
/// lets the delegation depth ledger inside see the whole tree.
#[derive(Debug, Default)]
pub struct SpawnTool {
    depth: depth::Depth,
}

impl SpawnTool {
    pub fn new() -> Self {
        Self::default()
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
            .description(DESCRIPTION)
            .input_schema(input_schema())
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

/// One required string, and no second field to decide about on every call.
fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            INPUT_FIELD: {
                "type": "string",
                "description": "A shell command when it starts with `!`, otherwise a task to \
                                delegate. Write `!!` to begin a task with a literal `!`.",
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
        // the floor cannot be lifted by an answer or by a remembered rule.
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
            Mode::Command => execute::command(&ctx, spawn.body()).await,
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
    // The wire contract of ADR-0016: `{mode, body, cwd}`, and never the string
    // the model wrote. What an approver renders, what a pattern rule globs
    // against, and what the audit trail keeps.
    let structured_input = json!({
        "mode": spawn.mode().as_str(),
        "body": spawn.body(),
        "cwd": cwd,
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
