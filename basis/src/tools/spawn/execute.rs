//! Doing the thing, once the answer is in.
//!
//! Both halves reach mentra through nothing but its public tool context, which
//! is the point: `spawn` is a host-registered tool, so whatever it can do here
//! is what ADR-0012's contract already offered. Neither half re-reads the
//! model's string — they are handed a body, and a destination, that
//! [`parse`](super::parse) already decided the meaning of.
//!
//! A delegation here comes in two tempos (ADR-0042). Blocking, the child runs
//! inside the call and the parent records the result before the call returns.
//! In the background, the same [`Delegation`] is opened on the parent, the
//! child runs on a task of its own, and what it settles to is delivered later
//! — by mentra's runner when it arrives on its own, or by [`wait`] when the
//! model asks. One record either way; only who applies it differs.

use std::{future::Future, sync::Arc};

use mentra::{
    ContentBlock, DelegationArtifact, DelegationEdge, DelegationKind, DelegationStatus,
    SpawnedAgentStatus, SpawnedAgentSummary,
    agent::{AgentEventTapGuard, DeferredItem},
    runtime::{CommandOutput, RunOptions},
    tool::{ToolContext, ToolResult},
};

use crate::runtime::agents::{AgentRow, AgentTools};

use super::{
    SpawnTool,
    background::{Children, JoinAtEnd, Kind, Settled},
    child::ChildSpec,
    depth::Entered,
    teammate::Teammate,
};

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

/// Everything one delegation records on its parent before the child runs,
/// and holds while it does.
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
/// and mentra's `RunOptions::child` carries it here. What `child` resets is
/// the round strategy, so the child gets basis's own ([`JoinAtEnd`]) — a
/// grandchild started in the background is joined by the child that started
/// it.
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
///
/// The three guards are held for the child's whole run — inside this value,
/// which the background task owns until the child settles:
///
/// - the ledger row adopting the child, read by both of the ledger's readers.
///   A child of this child asks it the same question this delegation just
///   did, and the honest answer is its grandparent's: the whole tree
///   delegated from one session is one workspace's, and a roster narrowed at
///   any depth must not widen at the next. The child inherits its parent's
///   tool audience, so the workspace's own chain judges its calls — but by
///   the child's own agent id, which nothing else would have put in here.
/// - the depth entry: while it lives, a `spawn` call made *by* the child sees
///   itself one level deeper than this one.
/// - the usage relay: one that ended early would leave the parent's tally
///   short by whatever the child spent afterwards.
pub(super) struct Delegation {
    child_id: String,
    started: SpawnedAgentSummary,
    prompt: String,
    /// The name the parent reaches the child by, when it is a teammate.
    teammate: Option<String>,
    edge: DelegationEdge,
    /// Whether the result arrives later, so its frame names the handle.
    background: bool,
    pub(super) options: RunOptions,
    _adopted: Option<AgentRow>,
    _entered: Entered,
    _usage_relay: AgentEventTapGuard,
}

impl Delegation {
    /// Opens the delegation on the parent: adopts the child's row (a one-off
    /// child; a teammate holds its own), enters the depth ledger, announces
    /// the child on the parent's stream, relays its usage, and records the
    /// request.
    ///
    /// `register_subagent` announces the child on the parent's stream
    /// (`TaskUpdated`, kind `Subagent`, since mentra `bfe952b`), so the
    /// delegation is no longer a silence of unknown length and nothing is
    /// narrated twice here.
    fn open(
        tool: &SpawnTool,
        ctx: &mut ToolContext<'_>,
        child: &mentra::agent::Agent,
        prompt: &str,
        depth: usize,
        teammate: Option<&str>,
        background: bool,
    ) -> Self {
        let child_id = child.id().to_string();
        let agents = tool.agents.as_ref();
        let adopted = match teammate {
            Some(_) => None,
            None => agents.and_then(|agents| agents.adopt(&ctx.agent_id, &child_id)),
        };
        let entered = tool.depth.entered(&child_id, depth + 1);
        let started = ctx.register_subagent(child);
        let usage_relay = ctx.relay_subagent_usage(child);

        let edge = DelegationEdge {
            kind: DelegationKind::Subagent,
            local_agent_id: ctx.agent_id.clone(),
            remote_agent_id: child_id.clone(),
        };
        let frame = match teammate {
            Some(name) => format!(
                "<delegation-request agent=\"{}\" teammate=\"{name}\" model=\"{}\">\n{prompt}\n\
                 </delegation-request>",
                started.name, started.model
            ),
            None => format!(
                "<delegation-request agent=\"{}\" model=\"{}\">\n{prompt}\n</delegation-request>",
                started.name, started.model
            ),
        };
        let requested = ctx.record_delegation_request(
            frame,
            artifact_for(
                &started,
                teammate,
                prompt,
                DelegationStatus::Requested,
                None,
            ),
            Some(edge.clone()),
        );
        note(ctx, requested);

        let mut options = ctx.child_run_options();
        if let Some(agents) = agents {
            options.round_strategy = Some(Arc::new(JoinAtEnd::new(
                Arc::clone(agents),
                child_id.clone(),
                None,
            )));
        }

        Self {
            child_id,
            started,
            prompt: prompt.to_string(),
            teammate: teammate.map(str::to_string),
            edge,
            background,
            options,
            _adopted: adopted,
            _entered: entered,
            _usage_relay: usage_relay,
        }
    }

    /// What the parent records once the child has answered, in the order the
    /// blocking path always recorded it: the child's end, then the result.
    /// The guards go with this value.
    fn settle(self, answer: Result<mentra::Message, mentra::error::RuntimeError>) -> Settled {
        // Read once, here, so the transcript's result summary and the string
        // the model gets back are the same words.
        let outcome = answer
            .as_ref()
            .map(|message| said(message.text()))
            .map_err(ToString::to_string);
        let (status, told, summary) = match &outcome {
            Ok(text) => (DelegationStatus::Finished, "finished", text),
            Err(error) => (DelegationStatus::Failed, "failed", error),
        };
        let handle = if self.background {
            format!(" id=\"{}\"", self.child_id)
        } else {
            String::new()
        };
        let frame = match &self.teammate {
            Some(name) => format!(
                "<delegation-result agent=\"{}\" teammate=\"{name}\"{handle} status=\"{told}\">\n\
                 {summary}\n</delegation-result>",
                self.started.name
            ),
            None => format!(
                "<delegation-result agent=\"{}\"{handle} status=\"{told}\">\n{summary}\n\
                 </delegation-result>",
                self.started.name
            ),
        };
        let items = vec![
            DeferredItem::SubagentFinished {
                id: self.child_id.clone(),
                status: match &outcome {
                    Ok(_) => SpawnedAgentStatus::Finished,
                    Err(error) => SpawnedAgentStatus::Failed(error.clone()),
                },
            },
            DeferredItem::DelegationResult {
                content: frame,
                delegation: artifact_for(
                    &self.started,
                    self.teammate.as_deref(),
                    &self.prompt,
                    status,
                    Some(summary.clone()),
                ),
                edge: Some(self.edge.clone()),
            },
        ];
        Settled { outcome, items }
    }
}

/// Applies a settled delegation's record through the parent's tool context:
/// the blocking path, and a wait that collected it. What the runner applies
/// for a result that arrived on its own, applied here instead.
pub(super) fn apply(ctx: &mut ToolContext<'_>, settled: &Settled) {
    for item in &settled.items {
        match item {
            DeferredItem::SubagentFinished { id, status } => {
                ctx.finish_subagent(id, status.clone());
            }
            DeferredItem::DelegationResult {
                content,
                delegation,
                edge,
            } => {
                let written =
                    ctx.record_delegation_result(content.clone(), delegation.clone(), edge.clone());
                note(ctx, written);
            }
            // A plain note is for the runner; a wait returns the words
            // themselves as its result.
            _ => {}
        }
    }
}

/// Hands `prompt` to a disposable subagent and reports what it answered.
pub(super) async fn delegate(
    tool: &SpawnTool,
    ctx: &mut ToolContext<'_>,
    prompt: &str,
    depth: usize,
    spec: ChildSpec,
) -> ToolResult {
    let mut child = child_for(tool, ctx, spec).await?;
    let delegation = Delegation::open(tool, ctx, &child, prompt, depth, None, false);
    let options = delegation.options.clone();
    let answer = Box::pin(child.run(vec![ContentBlock::text(prompt)], options)).await;
    let settled = delegation.settle(answer);
    apply(ctx, &settled);
    refresh_tasks(ctx)?;
    settled
        .outcome
        .map_err(|error| format!("the delegated run failed: {error}"))
}

/// Hands `prompt` to a disposable subagent and returns its handle at once
/// (ADR-0042). The child runs on a task of its own; what it settles to is
/// delivered to the parent's next round, or collected by [`wait`].
pub(super) async fn delegate_in_background(
    tool: &SpawnTool,
    ctx: &mut ToolContext<'_>,
    prompt: &str,
    depth: usize,
    spec: ChildSpec,
) -> ToolResult {
    let children = children_of(tool, &ctx.agent_id)?;
    let mut child = child_for(tool, ctx, spec).await?;
    let delegation = Delegation::open(tool, ctx, &child, prompt, depth, None, true);
    let id = delegation.child_id.clone();
    let options = delegation.options.clone();
    let prompt = prompt.to_string();
    children.start(
        tool.task_scope.as_ref(),
        id.clone(),
        None,
        Kind::Subagent,
        ctx.deferred(),
        async move {
            let answer = Box::pin(child.run(vec![ContentBlock::text(prompt)], options)).await;
            delegation.settle(answer)
        },
    );
    Ok(started(Kind::Subagent, &id, None, false))
}

/// Mints the child the spec describes and gives it its own copy of this tool.
async fn child_for(
    tool: &SpawnTool,
    ctx: &ToolContext<'_>,
    spec: ChildSpec,
) -> Result<mentra::agent::Agent, String> {
    // Read before the child exists, because it is the *parent's* answer: what
    // this workspace's mint denied its own model, which a roster override must
    // not undo. `None` on a runtime with no basis workspaces on it.
    let inherited = tool
        .agents
        .as_ref()
        .and_then(|agents| agents.of(&ctx.agent_id));
    let mut child = spawn_child(ctx, spec, inherited.as_deref())
        .await
        .map_err(|error| format!("spawn could not start a subagent: {error}"))?;
    teach(tool, &mut child, inherited.as_deref());
    Ok(child)
}

/// The calling agent's background delegations, or why there are none to
/// keep: a host driving mentra directly has no ledger and no rows.
pub(super) fn children_of(tool: &SpawnTool, agent_id: &str) -> Result<Children, String> {
    tool.agents
        .as_ref()
        .and_then(|agents| agents.of(agent_id))
        .map(|tools| tools.background.clone())
        .ok_or_else(|| {
            "spawn keeps no background work on this runtime; leave `background` out and wait \
             for the answer"
                .to_string()
        })
}

/// Mints the child the spec describes: the parent's plain clone on the path
/// this tool has always used, or mentra's disposable template with the
/// policy's overrides applied (D4 — `super::child` has the contract).
///
/// The template is taken from and spawned through *one* `ToolContext`, so
/// mentra's source binding — `RuntimeError::SubagentTemplateMismatch`, the
/// named refusal for a template that crossed to a different agent or runtime
/// — is structurally unreachable from here; if an upstream change ever made
/// it reachable, the error would arrive at the model through the same
/// "spawn could not start a subagent" wrapping as every other spawn failure,
/// mentra's naming intact. A model override naming an unregistered provider
/// arrives the same way (`ProviderNotFound`, at spawn, before anything runs).
///
/// `inherited` is what the delegating agent's own mint denied it, and what a
/// roster override must not undo: see the `with_tool_profile` call below.
async fn spawn_child(
    ctx: &ToolContext<'_>,
    spec: ChildSpec,
    inherited: Option<&AgentTools>,
) -> Result<mentra::agent::Agent, mentra::error::RuntimeError> {
    if spec.is_inherit() {
        // Deliberately not the template path with zero overrides, though
        // mentra documents the two as equivalent: this is the line every
        // policy-free runtime has always run, and "byte-identical default"
        // should be a fact about the code rather than a claim about upstream.
        return ctx.spawn_subagent();
    }

    let ChildSpec {
        roster,
        model,
        system,
    } = spec;
    let mut template = ctx.disposable_subagent_template();
    if let Some(roster) = roster {
        // The same mapping the workspace's own roster resolves through
        // (`ToolRoster::into_profile`), so a policy narrows a child with the
        // exact vocabulary a host narrows a workspace with — plus the one
        // thing that mapping cannot know.
        //
        // mentra's `with_tool_profile` *replaces* the cloned config's profile,
        // and part of what it would replace was never the parent's roster.
        // **A workspace in another directory needs nothing here:** its bridged
        // and declared tools are registered for its own `ToolAudience`, the
        // child inherits its parent's audience with the runtime handle it is
        // spawned from, and mentra's ladder answers a foreign audience's name
        // with `Hidden` however the profile is written.
        //
        // A second live open of *this* directory is the case the ladder cannot
        // express — one directory is one audience — so `Workspace::prepare`
        // hides that sibling's names by hand: its bridged `mcp__*` tools, and
        // the native tools its host supplied it. Replacing the profile away
        // would hand a *narrowed* child the very tools its parent is denied:
        // `mcp__prod-db__query` belonging to the other client's authenticated
        // server, reached through the child a policy wrote to restrict. So the
        // parent's own hidden set goes back in here. It
        // carries the rest of what the parent was minted with too — the doors
        // `spawn` replaced, the workspace's own roster — which is the same
        // rule said once instead of twice: a roster narrows a child, it never
        // widens one.
        //
        // Extending `hidden_tools` covers both roster shapes because
        // `ToolProfile::allows` checks the denylist *after* the allow-list: a
        // `hide` roster simply gains the names, and an `only` roster that
        // happened to name one loses it. Dropping rather than refusing, and
        // silently, is the rule `ToolRoster::only` already documents for the
        // same collision on a workspace's own roster — one composition, one
        // rule, stated in both places.
        let mut profile = roster.into_profile();
        if let Some(inherited) = inherited {
            profile
                .hidden_tools
                .extend(inherited.hidden.iter().cloned());
        }
        template = template.with_tool_profile(profile);
    }
    if let Some(model) = model {
        template = template.with_model(model);
    }
    if let Some(system) = system {
        template = template.with_system(system);
    }

    ctx.spawn_subagent_from(template).await
}

/// Gives a child its own copy of this tool: the one that knows whose it is,
/// so it keeps and lists the child's own teammates (ADR-0037 §7), and — when
/// the parent's row holds remote agents — bound to that set, so the child may
/// reach them and is told so (ADR-0029 §4). Its own conversation with each,
/// because a child is a conversation of its own — which is also what lets
/// two children started in the background each hand the same remote agent a
/// task: a conversation, and its one outstanding task, is per caller.
fn teach(tool: &SpawnTool, child: &mut mentra::agent::Agent, inherited: Option<&AgentTools>) {
    let Some(refresh) = tool.refresh.clone() else {
        // The runtime's own instance belongs to nobody and refreshes nothing;
        // a host driving mentra directly gets the plain tool its child
        // inherits.
        return;
    };
    let copy = tool.for_agent(child.id(), refresh);
    let copy = match inherited.and_then(|row| row.remote.as_ref()) {
        Some(remote) => copy.for_remote_agents(remote),
        None => copy,
    };
    child.register_tool(copy);
}

/// One teammate turn as `spawn` decided it: whom, how deep, whether the
/// teammate exists yet (and if not, what the child policy said it is), and
/// whether the turn runs in the background.
pub(super) struct TeammateCall {
    pub(super) name: String,
    pub(super) depth: usize,
    pub(super) existing: Option<Arc<Teammate>>,
    pub(super) spec: ChildSpec,
    pub(super) background: bool,
}

/// Runs one turn of the teammate `name` (ADR-0037 §5): the one the caller
/// has, or — on the first `@<name> <task>` — one made now through the child
/// policy and kept on the caller's row. In the background, the turn runs on
/// a task of its own and the handle is the teammate's name as much as its id
/// (ADR-0042).
///
/// Everything a one-off delegation does per run, this does per turn: the
/// parent's bounds, the depth entry, the announcement on the parent's
/// stream, the usage relay, the delegation entries in the transcript. The
/// one thing it does not do is drop the child.
pub(super) async fn teammate(
    tool: &SpawnTool,
    ctx: &mut ToolContext<'_>,
    prompt: &str,
    call: TeammateCall,
) -> ToolResult {
    let TeammateCall {
        name,
        depth,
        existing,
        spec,
        background,
    } = call;
    let name = name.as_str();
    let children = if background {
        Some(children_of(tool, &ctx.agent_id)?)
    } else {
        None
    };
    let agents = tool.agents.as_ref();
    let inherited = agents.and_then(|agents| agents.of(&ctx.agent_id));
    let (teammate, made) = match existing {
        Some(teammate) => (teammate, false),
        None => {
            let mut child = spawn_child(ctx, spec, inherited.as_deref())
                .await
                .map_err(|error| format!("spawn could not start a teammate: {error}"))?;
            teach(tool, &mut child, inherited.as_deref());
            // The row lives as long as the teammate: judged by the parent's
            // guard throughout, and able to keep teammates of its own.
            let row = agents.and_then(|agents| agents.adopt(&ctx.agent_id, child.id()));
            let teammate = Arc::new(Teammate::new(name, child, row));
            if let Some(row) = &inherited {
                row.teammates.keep(Arc::clone(&teammate));
            }
            // Told from the next model request on, like an agent a reload
            // added (ADR-0031 §3).
            if let Some(refresh) = &tool.refresh {
                refresh();
            }
            (teammate, true)
        }
    };
    // Called by id or by name, it is its name the reply and the record say.
    let name = teammate.name().to_string();
    let Some(mut child) = teammate.take_owned() else {
        return Err(format!(
            "`{name}` is still working on the last task you handed it, and takes one task at a \
             time; wait for that answer before handing it another"
        ));
    };
    let child_id = teammate.id().to_string();
    let delegation = Delegation::open(tool, ctx, &child, prompt, depth, Some(&name), background);
    let options = delegation.options.clone();

    if let Some(children) = children {
        let prompt = prompt.to_string();
        children.start(
            tool.task_scope.as_ref(),
            child_id.clone(),
            Some(name.clone()),
            Kind::Teammate,
            ctx.deferred(),
            async move {
                let answer = Box::pin(child.run(vec![ContentBlock::text(prompt)], options)).await;
                drop(child);
                delegation.settle(answer)
            },
        );
        return Ok(started(Kind::Teammate, &child_id, Some(&name), made));
    }

    let answer = Box::pin(child.run(vec![ContentBlock::text(prompt)], options)).await;
    drop(child);
    let settled = delegation.settle(answer);
    apply(ctx, &settled);
    refresh_tasks(ctx)?;

    // The name and the id open every reply (ADR-0037 §6), so a fresh
    // teammate after a restart reads as a different agent.
    let headline = if made {
        format!("Made teammate `{name}` (id `{child_id}`). It replied:")
    } else {
        format!("Teammate `{name}` (id `{child_id}`) replied:")
    };
    match settled.outcome {
        Ok(text) => Ok(format!("{headline}\n\n{text}")),
        Err(error) => Err(format!("the teammate `{name}`'s turn failed: {error}")),
    }
}

/// Collects one or more background delegations (ADR-0042): all of `ids`, or
/// the first to finish when `any`. Each collected result is recorded on the
/// parent here, exactly as the blocking path records one.
pub(super) async fn wait(
    tool: &SpawnTool,
    ctx: &mut ToolContext<'_>,
    ids: &[String],
    any: bool,
) -> ToolResult {
    let children = children_of(tool, &ctx.agent_id)?;
    let mut claimed = Vec::with_capacity(ids.len());
    for id in ids {
        match children.claim(id) {
            Ok(handle) => claimed.push(handle),
            Err(why) => {
                // Hand back what was claimed so far; one that finished
                // meanwhile is collected now rather than lost.
                let mut collected = Vec::new();
                for handle in claimed {
                    if let Some(done) = children.release(handle) {
                        collected.push(done.settled().await);
                    }
                }
                let report = record(ctx, &children, collected)?;
                return Err(if report.is_empty() {
                    why
                } else {
                    format!("{why}\n\nMeanwhile:\n\n{report}")
                });
            }
        }
    }

    let mut collected = Vec::new();
    if any && claimed.len() > 1 {
        let (index, first) = first_finished(&mut claimed).await;
        let winner = claimed.swap_remove(index);
        collected.push(winner.finished(first));
        for loser in claimed {
            if let Some(done) = children.release(loser) {
                collected.push(done.settled().await);
            }
        }
    } else {
        for handle in claimed {
            collected.push(handle.settled().await);
        }
    }

    let single = collected.len() == 1 && !any;
    let failed = collected
        .iter()
        .filter(|(_, _, _, settled)| settled.outcome.is_err())
        .count();
    let mut report = record(ctx, &children, collected)?;
    let running = children.listed();
    if !running.is_empty() {
        report.push_str("\n\nStill running: ");
        report.push_str(&super::describe::listed(
            &running
                .into_iter()
                .map(|(id, name, _)| name.unwrap_or(id))
                .collect::<Vec<_>>(),
        ));
    }
    if single && failed == 1 {
        return Err(report);
    }
    Ok(report)
}

/// Records each collected delegation on the parent and removes its slot,
/// then words the lot for the model.
fn record(
    ctx: &mut ToolContext<'_>,
    children: &Children,
    collected: Vec<(String, Option<String>, Kind, Settled)>,
) -> Result<String, String> {
    let mut lines = Vec::with_capacity(collected.len());
    for (id, name, kind, settled) in &collected {
        apply(ctx, settled);
        children.finish(id);
        let who = match name {
            Some(name) => format!("{} `{name}` (id `{id}`)", kind.noun()),
            None => format!("{} `{id}`", kind.noun()),
        };
        lines.push(match &settled.outcome {
            Ok(text) => format!("{who} finished:\n\n{text}"),
            Err(error) => format!("{who} failed: {error}"),
        });
    }
    if !collected.is_empty() {
        refresh_tasks(ctx)?;
    }
    Ok(lines.join("\n\n"))
}

impl super::background::Claimed {
    /// The claimed handle whose task a wait for *any* already saw finish:
    /// its result is in hand, and the handle must not be awaited again.
    fn finished(
        self,
        result: Result<Settled, tokio::task::JoinError>,
    ) -> (String, Option<String>, Kind, Settled) {
        let settled = result.unwrap_or_else(|error| Settled {
            outcome: Err(format!("the delegated run stopped unexpectedly: {error}")),
            items: Vec::new(),
        });
        (self.id, self.name, self.kind, settled)
    }
}

/// Polls every claimed task until one has finished: a wait for *any*. The
/// handles are `Unpin`, and one that has finished is not polled again.
async fn first_finished(
    claimed: &mut [super::background::Claimed],
) -> (usize, Result<Settled, tokio::task::JoinError>) {
    std::future::poll_fn(|cx| {
        for (index, handle) in claimed.iter_mut().enumerate() {
            if let std::task::Poll::Ready(result) = std::pin::Pin::new(&mut handle.task).poll(cx) {
                return std::task::Poll::Ready((index, result));
            }
        }
        std::task::Poll::Pending
    })
    .await
}

/// What the model reads when a delegation starts in the background: the
/// handle, and the three ways its answer comes back.
pub(super) fn started(kind: Kind, id: &str, name: Option<&str>, made: bool) -> String {
    let (who, handle) = match (kind, name) {
        (Kind::Teammate, Some(name)) if made => (
            format!("Made teammate `{name}` (id `{id}`); it"),
            format!("`@{name}` alone, or `wait: [\"{name}\"]`"),
        ),
        (Kind::Teammate, Some(name)) => (
            format!("Teammate `{name}` (id `{id}`)"),
            format!("`@{name}` alone, or `wait: [\"{name}\"]`"),
        ),
        (Kind::Remote, Some(name)) => (
            format!("Remote agent `{name}`"),
            format!("`@{name}` alone, or `wait: [\"{name}\"]`"),
        ),
        _ => (
            format!("Subagent `{id}`"),
            format!("`wait: [\"{id}\"]`, or `@{id}` alone"),
        ),
    };
    format!(
        "{who} is working on it in the background. Its answer arrives on its own when it is \
         done, as a delegation result; {handle} collects it sooner. Anything still running when \
         you finish is waited for before your answer is final."
    )
}

/// The child may have written the shared task list; the parent reads a stale
/// copy until it is told, exactly as mentra's own `task` intrinsic refreshes.
fn refresh_tasks(ctx: &mut ToolContext<'_>) -> Result<(), String> {
    ctx.refresh_tasks()
        .map_err(|error| format!("the delegated run finished but its tasks did not load: {error}"))
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

/// The delegation entry both halves of the record carry, differing only in
/// what has happened to it by then — with the teammate's name as its role
/// when the child is one.
fn artifact_for(
    child: &SpawnedAgentSummary,
    teammate: Option<&str>,
    prompt: &str,
    status: DelegationStatus,
    result_summary: Option<String>,
) -> DelegationArtifact {
    DelegationArtifact {
        kind: DelegationKind::Subagent,
        agent_id: child.id.clone(),
        agent_name: child.name.clone(),
        role: Some(match teammate {
            Some(name) => format!("teammate:{name}"),
            None => "subagent".to_string(),
        }),
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
