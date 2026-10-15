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
//! A command may also say *where*: `!@<target> <command>` names an executor to
//! run it on (ADR-0021) — though nothing currently registers one; see
//! `docs/targets.md`. One more fact on the same act rather than a second
//! tool, because a second tool would be a second name at the gate and a
//! second rule namespace — the two doors ADR-0016 closed, rebuilt by hand.
//! The prefix is read by the same parser, in the same pass, and the target it
//! leaves behind is a typed field like the mode beside it.
//!
//! # What the approver sees
//!
//! [`ToolExecutor::authorization_preview`] is overridden rather than left at
//! its default, which merely restates the static descriptor. A command reports
//! [`ToolSideEffectLevel::Process`] and a delegation reports
//! [`ToolSideEffectLevel::LocalState`]; both are consequential, so
//! [`is_consequential`](crate::approval::is_consequential) never waves either
//! through under the reads-are-never-asked rule. The preview's
//! parser owns `{mode, body, target}`; after the tool context resolves the
//! working directory, `structured_input` carries `{mode, body, cwd, target}` — the thing an
//! approver renders, and the thing mentra globs a remembered rule's pattern against
//! (`RuleStore::matching_rule`). A `RuleKey { tool_name: "spawn", pattern }` is
//! therefore a command allowlist expressible as data. A delegation whose child
//! policy ([`ChildSpec`], D4) overrode something adds a `child` key describing
//! what the child will be — roster, model, whether the system prompt is
//! replaced — and only then, so the four-key shape every existing rule was
//! written against is untouched on a runtime with no policy.
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

mod child;
mod depth;
mod describe;
mod execute;
mod files;
mod parse;
mod reach;
mod remote;
pub(crate) mod teammate;

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use mentra::tool::{
    ParallelToolContext, RuntimeToolDescriptor, ToolApprovalCategory, ToolAuthorizationPreview,
    ToolCapability, ToolContext, ToolDefinition, ToolDurability, ToolExecutionCategory,
    ToolExecutor, ToolResult, ToolSideEffectLevel,
};
use serde_json::{Value, json};

use describe::{Names, Remote, Taught, description, input_schema, listed};
use parse::{Mode, Spawn, parse};
use teammate::Teammate;

// A guard that has to know whether a `spawn` call is a command must not
// become a second reader of the `!` prefix — the module docs above make the
// rule: it is read exactly once, here. Exported so a host's interceptor asks
// this parser rather than writing its own.
pub use parse::{
    INPUT_FIELD as SPAWN_INPUT_FIELD, Mode as SpawnMode, REACH_FIELD as SPAWN_REACH_FIELD,
    Spawn as SpawnInput, classify_spawn_input, parse as parse_spawn_input,
};
pub(crate) use parse::{LOCAL_TARGET, is_namespaced, is_target_name};
pub use reach::{Reach, ReachPolicy};

pub use child::{ChildContext, ChildSpec};
pub use depth::DEFAULT_DELEGATION_DEPTH;

pub(crate) use child::ChildPolicy;

/// The name the model calls, an operator writes in a rule, and a
/// `.basis/hooks.json` entry matches on.
///
/// Public because it is all three of those: a host writing a hook or a
/// remembered rule needs the string, and a literal in their config that drifts
/// from basis's fails by never matching rather than by erroring.
pub const SPAWN: &str = "spawn";

/// basis's own tool, and the first tool basis registers on a runtime it builds.
///
/// One instance serves a whole runtime: mentra's registry holds it behind an
/// `Arc` and every subagent shares its parent's runtime handle, which is what
/// lets the delegation depth ledger inside see the whole tree — and what makes
/// the child policy apply at every depth, including to a child's own children.
///
/// Every agent a workspace mints or resumes is given a *copy* on mentra's
/// exact-agent layer, bound to that open's live set of remote agents
/// (ADR-0029 §4, ADR-0031 §3). Cloning shares the ledgers and the policy, so a
/// copy is the same tool told about different targets, never a second depth
/// count.
#[derive(Default, Clone)]
pub struct SpawnTool {
    depth: depth::Depth,
    /// The command targets this runtime can route to, sorted and deduplicated
    /// so the description a model reads is the same string on every build.
    /// Empty is the ordinary case, and the one in which the model is never
    /// told the routing prefix exists.
    targets: Vec<String>,
    /// Who a delegated child is (D4, [`child`]'s module docs). `None` — the
    /// ordinary case — is inherit-everything on the exact code path this tool
    /// has always used.
    policy: Option<ChildPolicy>,
    /// The runtime's agent ledger
    /// ([`crate::runtime::agents`]), read for two things a delegation cannot
    /// ask mentra: what the delegating agent's own roster hides — mentra
    /// exposes no reader for an agent's or a template's effective
    /// `ToolProfile`, and `with_tool_profile` replaces it wholesale — and
    /// where to record the child, so that the guard judging the parent judges
    /// it too.
    ///
    /// `None` for a host that built this tool itself against mentra, which has
    /// no basis workspaces to keep apart.
    agents: Option<Arc<crate::runtime::agents::AgentRegistry>>,
    /// The live set of remote agents this copy teaches (ADR-0031 §3), read
    /// every time mentra captures the descriptor — at registration, and again
    /// whenever the set says it changed. `None` on the runtime's own instance.
    /// An empty set teaches nothing, which is what keeps an agent with no
    /// remote agents from being taught a prefix that leads nowhere.
    ///
    /// Description only. *Routing* reads the calling agent's ledger row, which
    /// is the one place that knows which open's agents — and credentials — a
    /// call may use.
    remote: Option<Arc<crate::agents::remote::RemoteAgents>>,
    /// Where the agent this copy belongs to keeps its remote conversations
    /// (ADR-0029 §5): on disk by agent for the copy a workspace registers for
    /// an agent it minted or resumed, under a host's key for one bound to it
    /// (ADR-0035), and in memory for a delegated child's, which is not
    /// resumed and so has nothing to pick back up.
    remembers: crate::agents::remote::Remembered,
    /// The agent this copy belongs to, so it can list that agent's teammates
    /// (ADR-0037 §7). `None` on the runtime's own instance, which belongs to
    /// nobody and lists none.
    owner: Option<String>,
    /// What to call when this copy changes what `spawn` teaches — a teammate
    /// made — so the next model request is told. `None` on the runtime's own
    /// instance.
    refresh: Option<crate::runtime::Refresh>,
    /// What `@<name>` may reach (ADR-0037 §3), the runtime's say.
    reach: ReachPolicy,
}

/// The per-call facts both authorization preview and execution must decide.
#[derive(Debug)]
enum SpawnDecision {
    Command,
    Agent {
        depth: usize,
        spec: Box<ChildSpec>,
        /// Resolved only when a child policy needed it.
        workspace: Option<std::path::PathBuf>,
    },
    /// A turn for a teammate of the calling agent (ADR-0037 §5): one it has,
    /// or one to make under `name`.
    Teammate {
        depth: usize,
        name: String,
        existing: Option<Arc<Teammate>>,
        /// Consulted only when the teammate is made.
        spec: Box<ChildSpec>,
        workspace: Option<std::path::PathBuf>,
    },
    /// A task for a remote agent the calling agent's workspace connected.
    Remote {
        agents: Arc<crate::agents::remote::RemoteAgents>,
        name: String,
    },
}

/// What a named delegation adds to the preview (ADR-0037 §4): which kind of
/// agent the name was decided to reach, and the agent's id when it is
/// already known.
struct Named {
    reach: Reach,
    agent_id: Option<String>,
}

/// Hand-written for the policy field: a `dyn Fn` has no `Debug`, and presence
/// is all it can honestly print.
impl std::fmt::Debug for SpawnTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpawnTool")
            .field("depth", &self.depth)
            .field("targets", &self.targets)
            .field("policy", &self.policy.as_ref().map(|_| "<child policy>"))
            .field("agents", &self.agents.as_ref().map(|_| "<agent ledger>"))
            .field("owner", &self.owner)
            .field("reach", &self.reach)
            .field("remote", &self.taught().remote)
            .finish()
    }
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
    /// [`DEFAULT_DELEGATION_DEPTH`]'s floor; call
    /// [`with_targets_and_depth`](Self::with_targets_and_depth) for a
    /// different one.
    ///
    /// Called by [`RuntimeBuilder`](crate::RuntimeBuilder) with the set it
    /// collected; a host driving mentra directly calls it with whatever it
    /// registered on its own executor. Names it does not recognise are refused
    /// at the boundary, so passing a name here that no executor answers to
    /// turns a working call into a refusal rather than into a silent local
    /// run — which is the direction this has to fail in.
    pub fn with_targets(targets: impl IntoIterator<Item = String>) -> Self {
        Self::with_targets_and_depth(targets, depth::DEFAULT_DELEGATION_DEPTH)
    }

    /// [`with_targets`](Self::with_targets), with the delegation floor stated
    /// explicitly (decision D9) instead of defaulted — what
    /// [`RuntimeBuilder::with_delegation_depth`](crate::RuntimeBuilder::with_delegation_depth)
    /// threads through at registration. The guard's shape does not change:
    /// still basis's own ledger, still refusing *in the preview*, so a
    /// remembered allow-rule cannot lift it whatever the floor is set to.
    pub fn with_targets_and_depth(
        targets: impl IntoIterator<Item = String>,
        max_depth: usize,
    ) -> Self {
        Self {
            depth: depth::Depth::new(max_depth),
            targets: targets
                .into_iter()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            policy: None,
            agents: None,
            remote: None,
            remembers: crate::agents::remote::Remembered::Not,
            owner: None,
            refresh: None,
            reach: ReachPolicy::default(),
        }
    }

    /// Says what `@<name>` may reach (ADR-0037 §3). basis's own
    /// [`RuntimeBuilder::with_reach_policy`](crate::RuntimeBuilder::with_reach_policy)
    /// calls this; a host driving mentra directly calls it itself.
    #[must_use]
    pub fn with_reach_policy(self, reach: ReachPolicy) -> Self {
        Self { reach, ..self }
    }

    /// Gives this tool's delegations a child policy (D4): consulted per
    /// delegation with what `spawn` knows ([`ChildContext`]), answering which
    /// of the child's inherited facts to override ([`ChildSpec`]).
    ///
    /// [`ChildSpec`]'s module docs carry the contract — inherit is the
    /// default and byte-identical to no policy, bounds are not the spec's to
    /// carry, the depth floor is checked before the policy runs, and the
    /// policy is consulted both for the approver's preview and at execution,
    /// so it should be a pure function of its context. A host driving mentra
    /// directly calls this where basis's own builder calls it for
    /// [`RuntimeBuilder::with_child_policy`](crate::RuntimeBuilder::with_child_policy).
    #[must_use]
    pub fn with_child_policy<F>(self, policy: F) -> Self
    where
        F: Fn(&ChildContext<'_>) -> ChildSpec + Send + Sync + 'static,
    {
        self.with_child_policy_arc(Arc::new(policy))
    }

    /// [`with_child_policy`](Self::with_child_policy) for a caller that
    /// already holds the policy behind an `Arc` — basis's own
    /// [`RuntimeBuilder`](crate::RuntimeBuilder), which stores one so its
    /// `Debug` can say whether a policy is set. Wrapping that `Arc` in a
    /// closure and a second `Arc` would be one more indirection per
    /// delegation for nothing.
    #[must_use]
    pub(crate) fn with_child_policy_arc(self, policy: ChildPolicy) -> Self {
        Self {
            policy: Some(policy),
            ..self
        }
    }

    /// Lets this tool read what the delegating agent is denied, and record the
    /// child it spawns under the same answer (see the
    /// [`agents`](Self::agents) field). basis's own
    /// [`RuntimeBuilder`](crate::RuntimeBuilder) always says this; a host
    /// registering `SpawnTool` on its own mentra runtime has no basis
    /// workspaces and says nothing.
    #[must_use]
    pub(crate) fn with_agents(self, agents: Arc<crate::runtime::agents::AgentRegistry>) -> Self {
        Self {
            agents: Some(agents),
            ..self
        }
    }

    /// A copy of this tool for the agent `owner`: the one that lists that
    /// agent's teammates (ADR-0037 §7) and calls `refresh` when it makes one,
    /// so the next model request is told.
    pub(crate) fn for_agent(&self, owner: &str, refresh: crate::runtime::Refresh) -> Self {
        Self {
            owner: Some(owner.to_string()),
            refresh: Some(refresh),
            ..self.clone()
        }
    }

    /// A copy of this tool for one agent whose workspace holds `remote`,
    /// teaching it that live set (ADR-0029 §4, ADR-0031 §3).
    ///
    /// Made whether or not the set is empty: an agent added later must reach
    /// sessions already running, and the copy's description is computed from
    /// the set each time mentra captures it, so an empty set reads as the
    /// `spawn` every other agent sees, word for word.
    pub(crate) fn for_remote_agents(
        &self,
        remote: &Arc<crate::agents::remote::RemoteAgents>,
    ) -> Self {
        Self {
            remote: Some(Arc::clone(remote)),
            remembers: crate::agents::remote::Remembered::Not,
            ..self.clone()
        }
    }

    /// Everything this copy teaches about named agents, read each time
    /// mentra captures the descriptor: the remote agents its resolvers teach
    /// now (ADR-0037 §7) — each with what its card says, or marked
    /// unreachable when its card could not be fetched, and a resolver's note
    /// about the names it cannot list — the owner's teammates by name and id,
    /// and which `reach` values the model may choose between.
    fn taught(&self) -> Names {
        let teammates = self.owner.as_ref().and_then(|owner| {
            // Teammates are kept on the ledger, so a copy on a runtime with
            // none — a host's own `SpawnTool` — keeps none and teaches none.
            Some(self.agents.as_ref()?.of(owner)?.teammates.listed())
        });
        Names {
            remote: self.taught_remote(),
            teammates,
            reach: if self.reach.offers_a_choice() {
                self.reach.allowed().collect()
            } else {
                Vec::new()
            },
        }
    }

    fn taught_remote(&self) -> Remote {
        let Some(remote) = &self.remote else {
            return Remote::default();
        };
        let taught = remote.taught();
        Remote {
            agents: taught
                .agents
                .into_iter()
                .map(|agent| Taught {
                    name: agent.name,
                    summary: agent.summary,
                })
                .collect(),
            note: taught.note,
        }
    }

    /// The same copy, for an agent whose remote conversations are kept on disk
    /// — one a workspace minted or resumed.
    #[must_use]
    pub(crate) fn remembering(self) -> Self {
        Self {
            remembers: crate::agents::remote::Remembered::ByAgent,
            ..self
        }
    }

    /// The same copy, keeping its remote conversations under a host's `key`
    /// rather than its agent's id (ADR-0035).
    #[must_use]
    pub(crate) fn remembering_under(self, key: String) -> Self {
        Self {
            remembers: crate::agents::remote::Remembered::ByKey(key),
            ..self
        }
    }

    /// The live set of remote agents the calling agent's workspace holds,
    /// read from its ledger row — `None` for an agent basis did not mint.
    fn remote_agents_of(&self, agent_id: &str) -> Option<Arc<crate::agents::remote::RemoteAgents>> {
        self.agents.as_ref()?.of(agent_id)?.remote.clone()
    }

    /// Where `@<name>` stands with the calling agent's remote agents, without
    /// waiting on anything: the decision to hand the task over, the refusal
    /// to read instead, or `Ok(None)` when no resolver knows the name.
    ///
    /// A refusal here is a fact, not a judgement, so it is refused in the
    /// preview like an unregistered command target: no approver is asked
    /// about an agent tried too recently to be worth trying again.
    fn remote_route(&self, agent_id: &str, name: &str) -> Result<Option<SpawnDecision>, String> {
        let Some(agents) = self.remote_agents_of(agent_id) else {
            return Ok(None);
        };
        if agents.route(name) == crate::agents::Route::Unknown {
            return Ok(None);
        }
        remote::authorize(&agents, name)?;
        Ok(Some(SpawnDecision::Remote {
            agents,
            name: name.to_string(),
        }))
    }

    /// The refusal for `@<name>` under `reach: remote` when no resolver knows
    /// the name: the configured names when there are any, else that there is
    /// nowhere to send it.
    fn no_remote_agent(&self, agent_id: &str, name: &str) -> String {
        if let Some(agents) = self.remote_agents_of(agent_id) {
            return remote::unknown(&agents, name);
        }
        no_remote_agents(name)
    }

    /// Decides a `@<name>` delegation (ADR-0037 §3, §6): the calling agent's
    /// own teammate when it has one by that id or name and `reach` allows
    /// local, else a remote agent when a resolver knows the name and `reach`
    /// allows remote, else a new teammate when `reach` allows local, else
    /// the refusal that says so.
    fn decide_named(
        &self,
        spawn: &Spawn,
        agent_id: &str,
        name: &str,
        resolve_workspace: impl FnOnce() -> Result<std::path::PathBuf, String>,
    ) -> Result<SpawnDecision, String> {
        let reach = self.reach.decide(spawn.reach())?;
        let row = self.agents.as_ref().and_then(|agents| agents.of(agent_id));
        let mine = row.as_ref().and_then(|row| row.teammates.find(name));

        if reach != Reach::Remote
            && let Some(existing) = mine
        {
            // A bound name stays bound: a conversation in progress never
            // moves to a remote agent a reload added under the same name.
            return self.teammate_turn(spawn, agent_id, name, Some(existing), resolve_workspace);
        }
        if reach != Reach::Local
            && let Some(remote) = self.remote_route(agent_id, name)?
        {
            return Ok(remote);
        }
        if reach == Reach::Remote {
            return Err(self.no_remote_agent(agent_id, name));
        }
        if is_namespaced(name) {
            // A teammate's name is flat: `team/reviewer` is a resolver's
            // handle, and one nobody resolved is a refusal, not a teammate
            // called that (ADR-0037 §8).
            return Err(format!(
                "`{name}` is a namespaced name, which reaches a remote agent, and no resolver \
                 here knows it; a teammate's name has no `/`, as `@{}`",
                name.rsplit('/').next().unwrap_or(name)
            ));
        }
        if row.is_none() {
            return Err(format!(
                "spawn keeps no teammates on this runtime, so there is nobody to hand `@{name}` \
                 to; write the task with no `@` to hand it to a subagent here, or `@@` to begin \
                 it with a literal `@`"
            ));
        }
        self.teammate_turn(spawn, agent_id, name, None, resolve_workspace)
    }

    /// One turn for a teammate: the one the caller has, or one to make.
    fn teammate_turn(
        &self,
        spawn: &Spawn,
        agent_id: &str,
        name: &str,
        existing: Option<Arc<Teammate>>,
        resolve_workspace: impl FnOnce() -> Result<std::path::PathBuf, String>,
    ) -> Result<SpawnDecision, String> {
        if spawn.body().is_empty() {
            return Err(match &existing {
                Some(_) => format!(
                    "`@{name}` is a teammate here and has nothing outstanding to wait on; write \
                     `@{name} <task>` to hand it one"
                ),
                None => format!(
                    "`@{name}` names nobody yet; write `@{name} <task>` to make a teammate called \
                     `{name}` and hand it that task"
                ),
            });
        }
        let depth = self.depth.authorize_delegation(agent_id)?;
        let (spec, workspace) = match (&existing, self.policy.as_ref()) {
            // The policy shapes a teammate when it is made, not on every turn.
            (None, Some(policy)) => {
                let workspace = resolve_workspace()?;
                let spec =
                    policy(&ChildContext::new(spawn.body(), agent_id, &workspace).named(name));
                (spec, Some(workspace))
            }
            _ => (ChildSpec::inherit(), None),
        };
        Ok(SpawnDecision::Teammate {
            depth,
            name: name.to_string(),
            existing,
            spec: Box::new(spec),
            workspace,
        })
    }

    /// What the policy says this delegation's child is — asked identically on
    /// the two paths that need it, the preview and the execution, so the
    /// approver reads the child that will actually be spawned.
    /// [`ChildSpec::inherit`] whenever there is no policy to ask.
    fn child_spec(
        &self,
        prompt: &str,
        parent_agent_id: &str,
        workspace_dir: &std::path::Path,
    ) -> ChildSpec {
        match &self.policy {
            Some(policy) => policy(&ChildContext::new(prompt, parent_agent_id, workspace_dir)),
            None => ChildSpec::inherit(),
        }
    }

    /// `Ok` when this call names a target this runtime can reach, and the
    /// refusal the model should read instead when it does not.
    ///
    /// Asked only of a command: a delegation's target is a remote agent
    /// (ADR-0029), which is the calling agent's workspace's to answer, not
    /// this runtime's command targets.
    fn authorize_target(&self, spawn: &Spawn) -> Result<(), String> {
        let (Mode::Command, Some(target)) = (spawn.mode(), spawn.target()) else {
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

    /// Decides one parsed call. Both consumers invoke this independently:
    /// preview so the approver sees the real decision, execution again so a
    /// missing authorizer cannot remove a floor.
    fn decide(
        &self,
        spawn: &Spawn,
        agent_id: &str,
        resolve_workspace: impl FnOnce() -> Result<std::path::PathBuf, String>,
    ) -> Result<SpawnDecision, String> {
        self.authorize_target(spawn)?;
        match spawn.mode() {
            Mode::Command => Ok(SpawnDecision::Command),
            // Not an `if let` guard: those are newer than `rust-version`.
            Mode::Agent => {
                if let Some(name) = spawn.target() {
                    return self.decide_named(spawn, agent_id, name, resolve_workspace);
                }
                let depth = self.depth.authorize_delegation(agent_id)?;
                let (spec, workspace) = match self.policy.as_ref() {
                    Some(_) => {
                        let workspace = resolve_workspace()?;
                        let spec = self.child_spec(spawn.body(), agent_id, &workspace);
                        (spec, Some(workspace))
                    }
                    None => (ChildSpec::inherit(), None),
                };
                Ok(SpawnDecision::Agent {
                    depth,
                    spec: Box::new(spec),
                    workspace,
                })
            }
        }
    }
}

/// The refusal for `@<name>` from an agent that can reach no remote agent.
fn no_remote_agents(name: &str) -> String {
    format!(
        "spawn has no remote agents to hand `@{name}` to; write the task with no `@` to hand it \
         to a subagent here, or `@@` to begin it with a literal `@`"
    )
}

impl ToolDefinition for SpawnTool {
    /// The static descriptor, which is what a reader that has no call in hand
    /// sees. Its side-effect level is the *stronger* of the two modes on
    /// purpose: a tool that can run commands should never describe itself as
    /// something milder when asked in the abstract. Per-call precision is
    /// [`ToolExecutor::authorization_preview`]'s job.
    fn descriptor(&self) -> RuntimeToolDescriptor {
        let names = self.taught();
        RuntimeToolDescriptor::builder(SPAWN)
            .description(description(&self.targets, &names))
            .input_schema(input_schema(&self.targets, &names))
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
        // person (ADR-0021). Both checks run before the child policy is
        // consulted, so no host policy runs for a call the floor refuses.
        let decision = self.decide(&spawn, &ctx.agent_id, || {
            ctx.resolve_working_directory(None)
        })?;
        parameters_reach_a_remote_agent(&spawn, &decision)?;
        let (cwd, child, named) = match decision {
            SpawnDecision::Command => (ctx.resolve_working_directory(None)?, None, None),
            SpawnDecision::Remote { .. } => (
                ctx.resolve_working_directory(None)?,
                None,
                Some(Named {
                    reach: Reach::Remote,
                    agent_id: None,
                }),
            ),
            SpawnDecision::Agent {
                spec, workspace, ..
            } => {
                let cwd = match workspace {
                    Some(workspace) => workspace,
                    None => ctx.resolve_working_directory(None)?,
                };
                (cwd, spec.preview_value(), None)
            }
            SpawnDecision::Teammate {
                spec,
                workspace,
                existing,
                ..
            } => {
                let cwd = match workspace {
                    Some(workspace) => workspace,
                    None => ctx.resolve_working_directory(None)?,
                };
                // What the child will be is said when it is made; after that
                // it is who it is, and the id says so.
                let child = existing.is_none().then(|| spec.preview_value()).flatten();
                (
                    cwd,
                    child,
                    Some(Named {
                        reach: Reach::Local,
                        agent_id: existing.map(|teammate| teammate.id().to_string()),
                    }),
                )
            }
        };

        Ok(preview(
            &spawn,
            cwd,
            &self.descriptor(),
            input,
            child,
            named,
        ))
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
        // Deliberately decided again: preview is skipped when no authorizer is
        // installed, and that must not remove target or depth floors.
        let decision = self.decide(&spawn, &ctx.agent_id, || {
            ctx.resolve_working_directory(None)
        })?;
        parameters_reach_a_remote_agent(&spawn, &decision)?;
        // Read before anything is handed over, so a file that cannot be sent
        // stops the call rather than leaving a task without it (ADR-0041).
        let sent = files::read(spawn.files(), |path| ctx.read_file_bytes(path))?;

        match decision {
            SpawnDecision::Command => execute::command(&ctx, spawn.body(), spawn.target()).await,
            SpawnDecision::Remote { agents, name } => {
                let task = remote::Task {
                    body: spawn.body(),
                    metadata: spawn.metadata(),
                    files: sent.into_iter().map(|sent| sent.file).collect(),
                };
                remote::delegate(ctx, &agents, &name, task, &self.remembers).await
            }
            SpawnDecision::Agent { depth, spec, .. } => {
                let body = files::named(spawn.body(), &sent);
                execute::delegate(self, &mut ctx, &body, depth, *spec).await
            }
            SpawnDecision::Teammate {
                depth,
                name,
                existing,
                spec,
                ..
            } => {
                let body = files::named(spawn.body(), &sent);
                execute::teammate(self, &mut ctx, &name, &body, depth, existing, *spec).await
            }
        }
    }
}

/// A delegation's parameters go to a remote agent and nowhere else
/// (ADR-0040): a name that turned out to be a teammate here has nowhere to
/// put them, and dropping them would be a call quietly unlike the one
/// written.
fn parameters_reach_a_remote_agent(spawn: &Spawn, decision: &SpawnDecision) -> Result<(), String> {
    match (spawn.metadata(), decision) {
        (Some(_), SpawnDecision::Remote { .. }) | (None, _) => Ok(()),
        (Some(_), _) => Err(format!(
            "`{}` goes to a remote agent, and `@{}` is not one here; leave the parameters out, \
             or name a remote agent",
            parse::METADATA_FIELD,
            spawn.target().unwrap_or_default()
        )),
    }
}

/// Assembles the per-call preview: the parsed call, and the descriptor fields
/// that this mode — rather than the tool in the abstract — earns.
fn preview(
    spawn: &Spawn,
    cwd: std::path::PathBuf,
    descriptor: &RuntimeToolDescriptor,
    raw_input: &Value,
    child: Option<Value>,
    named: Option<Named>,
) -> ToolAuthorizationPreview {
    // The wire contract of ADR-0016, widened by ADR-0021 to
    // `{mode, body, cwd, target}`, and still never the string the model wrote.
    // What an approver renders, what a pattern rule globs against, and what the
    // audit trail keeps.
    //
    // `target` is additive in the strict sense: the three older keys keep
    // their spellings and their values, and it is written last here, so a
    // rule an operator already wrote still matches exactly what it matched
    // before. It reads `"local"` rather than `null` when no target was named,
    // because *here* is a value an operator will want to write a rule about
    // and a glob against a JSON null is a spelling nobody notices missing.
    //
    // Where in the serialization a key lands is not something to reason from,
    // and an earlier version of this comment did: `serde_json` orders a map
    // by insertion when `preserve_order` is on and alphabetically when it is
    // not, and the shipped binary has it on — `agent-client-protocol` enables
    // it, which is a transitive dependency's choice basis does not control
    // and an embedder may not share. So `child` (D4) is additive on the one
    // ground that holds either way: **it is absent unless a child policy
    // actually overrode something.** Every delegation on a policy-free
    // runtime, and every inherit answer on a policied one, serializes the
    // same four keys it always did, whatever order this build puts them in.
    // When the key is there it is the point: a remembered rule about
    // delegation can match on what the child will be —
    // `ChildSpec::preview_value` has the shape.
    let mut structured_input = json!({
        "mode": spawn.mode().as_str(),
        "body": spawn.body(),
        "cwd": cwd,
        "target": spawn.target().unwrap_or(LOCAL_TARGET),
    });
    if let Some(child) = child {
        structured_input
            .as_object_mut()
            .expect("built as an object two lines up")
            .insert("child".to_string(), child);
    }
    // ADR-0037 §4: a named delegation says which kind of agent the name was
    // decided to reach — never `auto` — and, once it is known, which agent.
    // Additive on the same ground as `child`: absent from every call that
    // names nobody, so a rule written against the four keys still matches.
    if let Some(named) = &named {
        let object = structured_input
            .as_object_mut()
            .expect("built as an object above");
        object.insert("reach".to_string(), json!(named.reach.as_str()));
        if let Some(agent_id) = &named.agent_id {
            object.insert("agent_id".to_string(), json!(agent_id));
        }
    }
    // ADR-0040: what goes beside the task is shown to whoever approves it.
    // Additive on the same ground: absent unless the call gave parameters.
    if let Some(metadata) = spawn.metadata() {
        structured_input
            .as_object_mut()
            .expect("built as an object above")
            .insert("metadata".to_string(), Value::Object(metadata.clone()));
    }
    // ADR-0041: the files that leave with the task, as written — shown on
    // the same additive ground.
    if !spawn.files().is_empty() {
        structured_input
            .as_object_mut()
            .expect("built as an object above")
            .insert(parse::FILES_FIELD.to_string(), json!(spawn.files()));
    }

    ToolAuthorizationPreview {
        working_directory: cwd,
        capabilities: capabilities(spawn.mode()),
        side_effect_level: side_effect_level(spawn, named.as_ref()),
        durability: descriptor.durability,
        execution_category: execution_category(spawn.mode()),
        approval_category: approval_category(spawn.mode()),
        raw_input: raw_input.clone(),
        structured_input,
    }
}

/// `Process` for a command, `LocalState` for a delegation here — the levels
/// `shell` and `task` declared, now decided per call instead of per name — and
/// `External` for a delegation to a remote agent, which hands work to a system
/// this process does not control (ADR-0029 §2). A teammate is a delegation
/// here (ADR-0037 §4). None is `None`, so no call can present as read-only.
fn side_effect_level(spawn: &Spawn, named: Option<&Named>) -> ToolSideEffectLevel {
    match (spawn.mode(), named.map(|named| named.reach)) {
        (Mode::Command, _) => ToolSideEffectLevel::Process,
        (Mode::Agent, Some(Reach::Remote | Reach::Auto)) => ToolSideEffectLevel::External,
        (Mode::Agent, Some(Reach::Local) | None) => ToolSideEffectLevel::LocalState,
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
