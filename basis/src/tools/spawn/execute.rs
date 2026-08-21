//! Doing the thing, once the answer is in.
//!
//! Both halves reach mentra through nothing but its public tool context, which
//! is the point: `spawn` is a host-registered tool, so whatever it can do here
//! is what ADR-0012's contract already offered. Neither half re-reads the
//! model's string — they are handed a body, and a destination, that
//! [`parse`](super::parse) already decided the meaning of.

use mentra::{
    ContentBlock, SpawnedAgentStatus,
    runtime::CommandOutput,
    tool::{ToolContext, ToolResult},
};

use super::depth::Depth;

/// Runs a command on mentra's own execution path, at the place it named.
///
/// Authorization has already happened — the orchestrator put this call's
/// preview to the authorizer before calling the executor — and mentra's policy
/// check happens *inside* this call: `RuntimeHandle::execute_shell_command_on`
/// asks `RuntimePolicy::authorize_command_execution` before it builds a
/// request, which is how `--no-shell` (`allow_shell_commands(false)`) refuses
/// with a reason the model can read rather than by the command quietly not
/// happening. Naming a target changes none of that ordering: `target` is
/// execution data on the request, read only by the installed executor, so
/// routing a command elsewhere is never a route around the policy that guards
/// running it here (ADR-0021).
///
/// The `cwd` is resolved here as it always was and is **advisory** for a
/// target: it is a path in this process's filesystem, and what it means on the
/// far side is the host executor's to decide. basis sends it because an
/// approver cannot judge a command without knowing where it was meant to run,
/// and translates nothing.
pub(super) async fn command(
    ctx: &ToolContext<'_>,
    command: &str,
    target: Option<&str>,
) -> ToolResult {
    let cwd = ctx.resolve_working_directory(None)?;
    // No justification field: the tool takes one string, and inventing a
    // second one for mentra's optional argument would be a field the model has
    // to decide about on every call.
    let output = ctx
        .execute_shell_command_on(
            target.map(str::to_string),
            command.to_string(),
            None,
            None,
            cwd,
        )
        .await?;

    narrate(ctx, &output);
    read_back(output)
}

/// Streams the command's output as tool progress, which is how a client shows a
/// build scrolling rather than a spinner. `shell` did this and `spawn` replaces
/// `shell`, so dropping it would be a regression in what a person sees.
fn narrate(ctx: &ToolContext<'_>, output: &CommandOutput) {
    for line in output.stdout.lines() {
        ctx.emit_progress(format!("stdout: {line}"));
    }
    for line in output.stderr.lines() {
        ctx.emit_progress(format!("stderr: {line}"));
    }
}

/// Turns a finished command into the result the model reads, in the same shape
/// mentra's `shell` produced: a failure is an error rather than prose about
/// one, so the model does not have to notice a non-zero exit in a wall of text.
fn read_back(output: CommandOutput) -> ToolResult {
    if output.success() {
        return Ok(if output.stdout.is_empty() {
            output.stderr
        } else {
            output.stdout
        });
    }

    Err(if !output.stderr.trim().is_empty() {
        output.stderr
    } else if !output.stdout.trim().is_empty() {
        output.stdout
    } else if output.timed_out {
        "Command timed out after the configured limit".to_string()
    } else {
        format!(
            "Command exited with status {}",
            output
                .status_code
                .map_or_else(|| "unknown".to_string(), |code| code.to_string())
        )
    })
}

/// Hands `prompt` to a disposable subagent and reports what it answered.
///
/// The child runs on [`ToolContext::child_run_options`] — the parent's
/// cancellation, stop, deadline and *shared* token counter. Driving it on
/// `RunOptions::default()` instead would give delegated work a fresh, unbounded
/// allowance, which is the difference between a bound and a suggestion.
///
/// A parent whose budget was already crossed gets a child that ends at its
/// first round boundary with nothing said, which mentra reports as
/// `EmptyAssistantResponse` — a failed tool call the model reads, rather than
/// an empty answer it might believe.
///
/// Those options also carry the runtime's provider retry schedule and its
/// budget, which is what keeps a delegated run as patient as the run that
/// delegated it: a subagent that reset to mentra's default would give up after
/// twelve and a half seconds against the same rate limit its parent was told
/// to wait a minute for. basis sets that schedule once, on the runtime
/// ([`RuntimeBuilder::with_provider_retry`](crate::RuntimeBuilder::with_provider_retry)),
/// and mentra's `RunOptions::child` carries it here.
pub(super) async fn delegate(
    ledger: &Depth,
    ctx: &mut ToolContext<'_>,
    prompt: &str,
    depth: usize,
) -> ToolResult {
    let mut child = ctx
        .spawn_subagent()
        .map_err(|error| format!("spawn could not start a subagent: {error}"))?;
    let child_id = child.id().to_string();

    // Held for the child's whole run: while this lives, a `spawn` call made
    // *by* the child sees itself one level deeper than this one.
    let _entered = ledger.entered(&child_id, depth + 1);

    let started = ctx.register_subagent(&child);
    // A subagent has its own event bus and basis reads the parent's, so without
    // this the delegation is a silence of unknown length. Progress is the one
    // channel a host-registered tool has into the parent's stream.
    ctx.emit_progress(format!(
        "delegating to {} ({})",
        started.name, started.model
    ));

    let options = ctx.child_run_options();
    let answer = Box::pin(child.run(vec![ContentBlock::text(prompt)], options)).await;

    let status = match &answer {
        Ok(_) => SpawnedAgentStatus::Finished,
        Err(error) => SpawnedAgentStatus::Failed(error.to_string()),
    };
    ctx.finish_subagent(&child_id, status);
    // The child may have written the shared task list; the parent reads a stale
    // copy until it is told, exactly as mentra's own `task` intrinsic refreshes.
    ctx.refresh_tasks().map_err(|error| {
        format!("the delegated run finished but its tasks did not load: {error}")
    })?;

    let message = answer.map_err(|error| format!("the delegated run failed: {error}"))?;
    let text = message.text();

    Ok(if text.trim().is_empty() {
        "The subagent finished without saying anything.".to_string()
    } else {
        text
    })
}
