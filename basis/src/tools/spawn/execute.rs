//! Doing the thing, once the answer is in.
//!
//! Both halves reach mentra through nothing but its public tool context, which
//! is the point: `spawn` is a host-registered tool, so whatever it can do here
//! is what ADR-0012's contract already offered. Neither half re-reads the
//! model's string — they are handed a body, and a destination, that
//! [`parse`](super::parse) already decided the meaning of.

use mentra::{
    ContentBlock, DelegationArtifact, DelegationEdge, DelegationKind, DelegationStatus,
    SpawnedAgentStatus, SpawnedAgentSummary,
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
///
/// Two things follow the child's run out to the parent, and both exist because
/// a subagent has an event bus and a transcript of its own while basis reads
/// the parent's:
///
/// - its **usage reports**, relayed onto the parent's bus. The delegated spend
///   already counts against the parent's `token_budget` — that is what the
///   shared counter is for — so without the relay a run could stop on a total
///   its own [`RunUsage`](crate::RunUsage) never saw, which is exactly what
///   that type's doc promises cannot happen. Usage and nothing else: relaying
///   the child's tool calls and text would put a second run's work on the
///   parent's stream, where a host renders it as the parent's own.
/// - the **delegation entries**, written into the parent's transcript. Without
///   them the answer appears in the transcript with nothing saying where it
///   came from, and a reader following delegation edges to reconstruct who
///   asked whom sees `spawn`'s delegations as work the parent did itself. The
///   shape is mentra's `task` intrinsic's, because `spawn` replaces that door
///   (ADR-0016) and a reader should not have to know which one was used.
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
    // Held for the same span and for the same kind of reason: the relay ends
    // when this guard drops, and one that ended early would leave the parent's
    // tally short by whatever the child spent afterwards.
    let _usage_relay = ctx.relay_subagent_usage(&child);

    // `register_subagent` above announced the child on the parent's stream
    // (`TaskUpdated`, kind `Subagent`, since mentra `bfe952b`), so the
    // delegation is no longer a silence of unknown length and nothing is
    // narrated twice here.

    let edge = DelegationEdge {
        kind: DelegationKind::Subagent,
        local_agent_id: ctx.agent_id.clone(),
        remote_agent_id: child_id.clone(),
    };
    let requested = ctx.record_delegation_request(
        format!(
            "<delegation-request agent=\"{}\" model=\"{}\">\n{prompt}\n</delegation-request>",
            started.name, started.model
        ),
        artifact(&started, prompt, DelegationStatus::Requested, None),
        Some(edge.clone()),
    );
    note(ctx, requested);

    let options = ctx.child_run_options();
    let answer = Box::pin(child.run(vec![ContentBlock::text(prompt)], options)).await;

    // Read once, here, so the transcript's result summary and the string the
    // model gets back are the same words.
    let outcome = answer
        .as_ref()
        .map(|message| said(message.text()))
        .map_err(ToString::to_string);

    ctx.finish_subagent(
        &child_id,
        match &outcome {
            Ok(_) => SpawnedAgentStatus::Finished,
            Err(error) => SpawnedAgentStatus::Failed(error.clone()),
        },
    );

    let (status, told, summary) = match &outcome {
        Ok(text) => (DelegationStatus::Finished, "finished", text),
        Err(error) => (DelegationStatus::Failed, "failed", error),
    };
    let returned = ctx.record_delegation_result(
        format!(
            "<delegation-result agent=\"{}\" status=\"{told}\">\n{summary}\n</delegation-result>",
            started.name
        ),
        artifact(&started, prompt, status, Some(summary.clone())),
        Some(edge),
    );
    note(ctx, returned);

    // The child may have written the shared task list; the parent reads a stale
    // copy until it is told, exactly as mentra's own `task` intrinsic refreshes.
    ctx.refresh_tasks().map_err(|error| {
        format!("the delegated run finished but its tasks did not load: {error}")
    })?;

    outcome.map_err(|error| format!("the delegated run failed: {error}"))
}

/// What the model reads back, with a name for the case where a subagent
/// answered with nothing: an empty result reads as a tool that did nothing,
/// which is a different thing from one that finished quietly.
fn said(text: String) -> String {
    if text.trim().is_empty() {
        return "The subagent finished without saying anything.".to_string();
    }

    text
}

/// The delegation entry both halves of the record carry, differing only in what
/// has happened to it by then.
fn artifact(
    child: &SpawnedAgentSummary,
    prompt: &str,
    status: DelegationStatus,
    result_summary: Option<String>,
) -> DelegationArtifact {
    DelegationArtifact {
        kind: DelegationKind::Subagent,
        agent_id: child.id.clone(),
        agent_name: child.name.clone(),
        role: Some("subagent".to_string()),
        status,
        task_summary: prompt.to_string(),
        result_summary,
        artifacts: Vec::new(),
    }
}

/// Says that half a delegation's record did not get written, and lets the
/// delegation stand.
///
/// A transcript entry that could not be written is a bookkeeping failure, and
/// turning it into a failed tool call would hand the model an error about work
/// that actually happened — which it would answer by doing the work again.
/// mentra's own `task` intrinsic drops the same error entirely; this one says
/// so on the progress channel, because a transcript with half a delegation in
/// it is worth knowing about and there is nowhere else for basis to say it.
fn note(ctx: &ToolContext<'_>, written: Result<(), mentra::error::RuntimeError>) {
    if let Err(error) = written {
        ctx.emit_progress(format!("the delegation was not recorded: {error}"));
    }
}
