//! Which workspace each live agent on this runtime answers for.
//!
//! The fourth ledger of [`super`], and the only one keyed by *agent* rather
//! than by name, root or audience. It exists because the other three cannot
//! answer the question a shared registry keeps asking: **two live opens of one
//! directory are one tool audience** (`SessionScope::audience`), and that is
//! the shape `basis-host` produces on purpose — one workspace per set of
//! client-supplied `mcpServers`. Everything mentra scopes, it scopes by
//! audience, so nothing mentra offers can tell those two apart. An agent id
//! can.
//!
//! Two readers, and they guard two different things:
//!
//! - **`spawn`** (`crate::tools::spawn::execute`) reads
//!   [`AgentTools::hidden`] to answer *what may this delegated child be told
//!   about*. mentra's `DisposableSubagentTemplate::with_tool_profile` replaces
//!   a child's cloned profile outright, so a [`ChildSpec`](crate::ChildSpec)
//!   roster would otherwise drop every hide the parent was minted with —
//!   including the `mcp__*` names belonging to a sibling open of the parent's
//!   own directory, which is the one hide no audience can restate. mentra
//!   exposes no reader for an agent's or a template's effective profile (the
//!   neighbour of upstream `mentra#55`), so basis carries the set itself.
//! - **[`ForeignToolGuard`]**, in every workspace's own interception chain,
//!   answers *may this call run*. It reads [`AgentTools::mcp_servers`] and
//!   [`AgentTools::host_tools`] — what the calling agent's workspace actually
//!   configured and was given — on every call, so it is right however long ago
//!   the session was minted, whichever open of the directory came first, and
//!   whether or not a sibling's bridge or native tool had arrived when the
//!   mint computed its roster.
//!
//! Neither substitutes for the other: `hidden_tools` decides what the model is
//! *told exists*, the guard decides what *executes*. A name the model was
//! never offered is still a name it can guess, and a roster is a snapshot
//! while a call is not.
//!
//! # Who is in here
//!
//! Every agent basis mints ([`Workspace::prepare`](crate::Workspace::prepare)),
//! resumes ([`Workspace::resume`](crate::Workspace::resume)) or delegates to
//! (`spawn`, the only door — mentra's `task` intrinsic is hidden from every
//! workspace roster, see `workspace::roster`). An agent basis did not make is
//! deliberately absent: a session a host drives through
//! [`Runtime::mentra_runtime`](super::Runtime::mentra_runtime) has no tool
//! audience at all and none of a workspace's guards, which is the posture
//! [`Workspace`](crate::Workspace)'s own docs describe. Absent means
//! unjudged here, which is the same answer that posture already gives.
//!
//! Entries leave with whatever *last* put them there: a workspace's on its
//! drop, a delegated child's when the delegation returns. "Last" is the load
//! bearing word and [`AgentOwner`] is how it is known — one agent id can be
//! written by two different opens of one directory, and only one of them may
//! take it away again.

use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

/// What one mint settled about the tools an agent may see and use.
///
/// A value rather than two maps because both facts are settled at the same
/// instant, by the same code, about the same agent — and a child inherits them
/// as one thing.
#[derive(Debug, Default)]
pub(crate) struct AgentTools {
    /// Every name this agent's config hides from its model: the workspace's
    /// own roster, the doors `spawn` replaced, and — the part no roster can
    /// restate — the `mcp__*` names belonging to a sibling open of this same
    /// directory.
    ///
    /// Read by `spawn` when a [`ChildSpec`](crate::ChildSpec) roster override
    /// replaces the child's profile, so that a narrowed child is narrowed
    /// rather than widened.
    pub(crate) hidden: BTreeSet<String>,
    /// The effective MCP server names this agent's workspace claimed — what
    /// [`Workspace::mcp_servers`](crate::Workspace::mcp_servers) reports.
    ///
    /// [`ForeignToolGuard`]'s bridged-name input, and the reason it cannot go
    /// stale: a
    /// workspace's server list is settled at its open, before any tool of any
    /// sibling is bridged, and it does not change while the workspace lives.
    #[cfg(feature = "mcp")]
    pub(crate) mcp_servers: Vec<String>,
    /// The native tools this agent's workspace supplied
    /// ([`WorkspaceBuilder::with_tool`](crate::WorkspaceBuilder::with_tool)) —
    /// what [`Workspace::host_tools`](crate::Workspace::host_tools) reports.
    ///
    /// [`ForeignToolGuard`]'s native-name input, and unlike the servers beside it
    /// this one is *not* about a name a workspace configured elsewhere: a
    /// native tool is registered for the audience two same-root opens share,
    /// so the open that supplied none can otherwise resolve — and run — the
    /// closure the other one supplied. Settled at the open and unchanging, for
    /// the same reason the server list is.
    pub(crate) host_tools: Vec<String>,
}

/// Which handle a registry row belongs to *now*.
///
/// The registry is keyed by agent id alone, and one agent id can be written by
/// two different opens of one directory:
/// [`Workspace::resume`](crate::Workspace::resume) checks the conversation's
/// *root*, and two same-root opens have the identical root identifier by
/// construction, so either may pick up an id the other minted. Last writer
/// wins, which is the right answer for the *value* — mentra refuses a second
/// live session on one agent id (`RuntimeError::LeaseUnavailable`), so the open
/// running the live session and the open that recorded last are always the
/// same one.
///
/// This stamp exists for the other end of that, where last-writer-wins is not
/// self-correcting: **a hold that is no longer the row's owner must not erase
/// it on its way out.** A run that resumed a conversation once still holds its
/// row long after a sibling open took the conversation back,
/// and an absent row is not a safe default — [`ForeignToolGuard`] reads it as
/// "an agent basis did not make" and allows the call, `spawn` reads it as "no
/// inherited hides" — so the erasure would reopen both holes for exactly as
/// long as the victim's session stayed live.
///
/// Monotonic and never reused, so a stamp names one *write* for the life of the
/// runtime — not one workspace, because a workspace can write one agent id
/// twice and only the standing write may retract it. A pointer identity would
/// have to outlive the hold to stay unique, and a slot index would be reused by
/// the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AgentOwner(u64);

/// One agent's row: what it was recorded with, and by which handle.
#[derive(Debug)]
struct Entry {
    owner: AgentOwner,
    tools: Arc<AgentTools>,
}

/// Which workspace each live agent on this runtime answers for.
#[derive(Debug, Default)]
pub(crate) struct AgentRegistry {
    agents: RwLock<HashMap<String, Entry>>,
    /// Hands out [`AgentOwner`]s, and the only thing outside the map's lock.
    next_owner: AtomicU64,
}

impl AgentRegistry {
    /// What the agent called `agent_id` was minted with, or `None` for an
    /// agent basis did not make.
    pub(crate) fn of(&self, agent_id: &str) -> Option<Arc<AgentTools>> {
        self.agents
            .read()
            .expect("agent registry poisoned")
            .get(agent_id)
            .map(|entry| Arc::clone(&entry.tools))
    }

    /// A stamp no other handle on this runtime will be given.
    ///
    /// `Relaxed` because uniqueness is all that is asked of it: the counter
    /// orders nothing else, and every comparison of a stamp happens under the
    /// map's own lock.
    fn new_owner(&self) -> AgentOwner {
        AgentOwner(self.next_owner.fetch_add(1, Ordering::Relaxed))
    }

    /// Records `child` as answering for whatever workspace `parent` does, for
    /// as long as the returned hold lives.
    ///
    /// A delegated child inherits its parent's runtime handle and therefore
    /// its parent's tool audience, so every guard that judges the parent
    /// already judges the child — but by the child's own agent id, which
    /// nothing else would have put in here. `None` when the parent is not a
    /// basis-minted agent, which is the same answer a reader would have got
    /// for the parent anyway.
    pub(crate) fn adopt(self: &Arc<Self>, parent: &str, child: &str) -> Option<AgentRow> {
        let tools = self.of(parent)?;
        let owner = self.new_owner();
        self.agents
            .write()
            .expect("agent registry poisoned")
            .insert(child.to_string(), Entry { owner, tools });

        Some(AgentRow {
            registry: Arc::clone(self),
            owner,
            agent_id: child.to_string(),
        })
    }

    fn record(&self, owner: AgentOwner, agent_id: &str, tools: Arc<AgentTools>) {
        self.agents
            .write()
            .expect("agent registry poisoned")
            .insert(agent_id.to_string(), Entry { owner, tools });
    }

    /// Takes `agent_id` off the registry, but only while `owner` is still the
    /// handle whose write is standing there.
    ///
    /// The condition is the whole of it: a handle that recorded an id and was
    /// then overwritten by another open of the same directory has nothing to
    /// release, and removing the row anyway would take the *current* owner's
    /// answer away from a session that is still running on it. See
    /// [`AgentOwner`] for why an absent row is worse than a stale one.
    fn forget_if_owned(&self, agent_id: &str, owner: AgentOwner) {
        let mut agents = self.agents.write().expect("agent registry poisoned");
        if agents
            .get(agent_id)
            .is_some_and(|entry| entry.owner == owner)
        {
            agents.remove(agent_id);
        }
    }
}

/// One workspace's share of the registry, released on drop.
///
/// The sibling of [`SkillRoots`](crate::skills::SkillRoots) and the hook
/// chain's hold, and held by the [`Workspace`](crate::Workspace) for their
/// reason: what a workspace put on a runtime it may not own has to come off
/// when the workspace goes.
#[derive(Debug)]
pub(crate) struct WorkspaceAgents {
    registry: Arc<AgentRegistry>,
}

impl WorkspaceAgents {
    pub(crate) fn new(registry: Arc<AgentRegistry>) -> Self {
        Self { registry }
    }

    /// States what this workspace's `agent_id` was minted with, replacing
    /// whatever an earlier mint or resume of the same conversation said.
    ///
    /// Replacing rather than merging is the point of restating it on resume:
    /// mentra's [`SessionResumeOptions`](mentra::runtime::SessionResumeOptions)
    /// carries no tool profile, so a resumed session's *config* keeps the
    /// roster its first mint froze — and this is the one thing about it that
    /// can be brought up to date.
    ///
    /// Replacing includes taking the row back off a *sibling* open of this
    /// same directory, which is allowed and is how a conversation moves
    /// between two live opens of one root. This workspace becomes its owner
    /// again by saying so.
    pub(crate) fn record(&self, agent_id: &str, tools: AgentTools) -> AgentRow {
        // A stamp per *write*, not per workspace, and `adopt` has always done
        // it this way. One workspace can write one agent id twice — a
        // conversation resumed after an earlier run of it ended — and if both
        // holds carried the workspace's stamp, releasing the stale one would
        // retract the row the live one is standing on. Whoever wrote the row
        // last is whoever may take it away, and only a per-write stamp says
        // exactly that.
        let owner = self.registry.new_owner();
        // Recorded on the registry *before* the map insert below, and the order
        // is load bearing: the insert may drop an earlier write's hold, whose
        // `Drop` compares its stamp against whatever entry is standing. Doing
        // the insert first would let that comparison meet the *old* entry,
        // which the old stamp does match — and retract a row this call is in
        // the middle of establishing.
        self.registry.record(owner, agent_id, Arc::new(tools));
        AgentRow {
            registry: Arc::clone(&self.registry),
            owner,
            agent_id: agent_id.to_string(),
        }
    }
}

/// One agent's entry, kept for exactly as long as the thing that needs it.
///
/// **The row's lifetime is the *agent's*, not the workspace's**, and that
/// distinction is the whole of what this type is for. A run outlives the
/// workspace that minted it — `Workspace::prepare` hands back a `PreparedRun`
/// and does not attach the workspace to it, so a host may drop the workspace
/// and go on running — while the guard that judges that run's calls reads this
/// ledger on every one of them. A row released with the workspace would leave a
/// live session unattributable, which is a denial the guard cannot make and an
/// allowance it should not: see `docs/proposals/0004`.
///
/// Two things hold one. A [`PreparedRun`](crate::PreparedRun) holds the row for
/// its session, minted or resumed. `spawn` holds one for a delegated child,
/// which lives from `spawn_subagent` to the answer it reads back and no longer.
/// Both release the same way, and neither can release a row a sibling open has
/// since taken over — whoever wrote the row last is whoever may take it away.
/// **One holder, and it is the run.** A [`PreparedRun`](crate::PreparedRun) is
/// the unique owner of a basis-minted session — nothing on it yields an owned
/// `Session`, only borrows that cannot outlive it — so tying the row to the run
/// states the invariant exactly: a row outlives every live session minted or
/// resumed against it, because the run *is* how long that session lives.
///
/// A workspace-side hold was tried and dropped. It states a coarser thing —
/// every id this open ever recorded, until the open goes — which is a superset
/// today and would be only a *partial* cover if a session-escape API ever
/// returned: it holds while the workspace lives and not otherwise, so it would
/// satisfy the obvious regression test and leave open the very ordering that
/// `PreparedRun::into_session` was withdrawn for. An exact invariant that fails
/// loudly beats a coarse one that fails quietly. `docs/proposals/0004` records
/// what a reintroduced escape hatch has to solve.
#[derive(Debug)]
#[must_use = "dropping the hold takes the agent's row off the ledger"]
pub(crate) struct AgentRow {
    registry: Arc<AgentRegistry>,
    /// Stamped per *write*, so last-writer-wins stays unambiguous: whoever
    /// wrote the row last is whoever may take it away.
    owner: AgentOwner,
    agent_id: String,
}

impl Drop for AgentRow {
    fn drop(&mut self) {
        self.registry.forget_if_owned(&self.agent_id, self.owner);
    }
}

/// Refuses a call to a tool that belongs to another open of the calling
/// agent's own directory: a bridged tool whose server this workspace never
/// configured, or a native tool another open of this directory supplied.
///
/// Registered in **every** workspace's own interception chain
/// ([`WorkspaceBuilder::open`](crate::WorkspaceBuilder::open)), which is what
/// makes it live: the chain is consulted per call, and what it reads is the
/// calling agent's own server list rather than a set some past mint computed.
/// Three holes close at once, and none of them could be closed by hiding names
/// at mint:
///
/// - **A sibling that opened later.** `Workspace::prepare` hides the `mcp__*`
///   names a sibling open of this directory had bridged *by then*; a sibling
///   that opens afterwards is not in that set, and mentra resolves its tools
///   live on every round, for the audience both opens share.
/// - **A session that was resumed.** mentra persists the tool profile and
///   `SessionResumeOptions` restates none of it, so a resumed conversation
///   carries the roster its *first* mint froze — from a process that may have
///   ended long ago.
/// - **The window inside a sibling's own open.** `claim_mcp_server` reserves a
///   name before the connection is attempted and `record_bridged_tools` says
///   what came back after, so between the two there is a claim with no tool
///   names under it and a mint that finds nothing to hide. This asks the
///   *caller's* own server list instead, which is settled at its open and
///   never briefly empty by accident.
///
/// The native half is the same three holes with one word changed, and it needs
/// the guard for a reason the bridged half does not have: a
/// [`WorkspaceBuilder::with_tool`](crate::WorkspaceBuilder::with_tool)
/// registration is refused to a *second* open that asks for the same name, so
/// the ledger tells the two apart — but the open that asks for nothing is
/// refused nothing, and would otherwise both be offered and be able to run the
/// closure its sibling supplied. What the name is judged against is the
/// caller's own supplied list, which is settled at its open.
///
/// **Duplicate registrations are harmless, and that matters here.** Two opens
/// of one directory join a single chain
/// ([`Runtime::register_hook_chain`](super::Runtime::register_hook_chain)), so
/// only the first open's guard is ever live — and it still answers correctly
/// for the second open's sessions, because it decides from the ledger rather
/// than from the workspace that happened to build it.
#[derive(Debug)]
pub(crate) struct ForeignToolGuard {
    agents: Arc<AgentRegistry>,
    claims: crate::runtime::ToolClaims,
}

impl ForeignToolGuard {
    pub(crate) fn new(agents: Arc<AgentRegistry>, claims: crate::runtime::ToolClaims) -> Self {
        Self { agents, claims }
    }
}

#[async_trait::async_trait]
impl crate::hooks::Interceptor for ForeignToolGuard {
    fn name(&self) -> &str {
        "basis tool ownership"
    }

    async fn intercept(
        &self,
        call: &crate::hooks::HookRequest,
    ) -> Result<crate::hooks::HookOutcome, crate::hooks::InterceptorError> {
        let tools = self.agents.of(&call.agent_id);

        // mentra's own parser, so a name a suffixed claim assembled
        // (`claim_mcp_server` resolves a collision with `-<hash>`, which holds
        // no `__`) splits here exactly as it was put together there.
        //
        // A missing row allows: an agent basis did not make is unjudged, which
        // is the posture `Workspace`'s own docs describe for a session driven
        // straight through `Runtime::mentra_runtime`. A host driving mentra
        // itself can legitimately own a bridged server, so there is a real
        // caller behind that default.
        #[cfg(feature = "mcp")]
        if let Some((server, _)) = mentra::mcp::parse_mcp_tool_name(&call.tool_name) {
            let Some(tools) = tools.as_ref() else {
                return Ok(crate::hooks::HookOutcome::Allow);
            };
            return Ok(if tools.mcp_servers.iter().any(|own| own == server) {
                crate::hooks::HookOutcome::Allow
            } else {
                crate::hooks::HookOutcome::Deny(format!(
                    "'{}' belongs to the MCP server '{server}', which this workspace did not \
                     configure",
                    call.tool_name
                ))
            });
        }

        // **The native arm defaults the other way, and has to.** A ledger row
        // lives for its workspace, while a run does not: `Workspace::prepare`
        // does not attach the workspace to the run it returns, so
        // `let run = ws.prepare(..)?; drop(ws); run.execute(..)` is a
        // supported shape — and it takes the row away underneath a live
        // session. Allowing an unrowed caller here would hand that session
        // exactly what this guard exists to refuse, in exactly the ordering no
        // hide can cover.
        //
        // Defaulting to deny costs nothing, because unlike a bridged name
        // there is no legitimate unjudged caller for this one: a name is in
        // the ledger as native only because a live basis workspace put it
        // there, and a session with no audience cannot resolve it at all.
        //
        // Asked of the ledger rather than of any snapshot, so a sibling that
        // opened after this session was minted is judged too. A name no live
        // open claimed natively is not this guard's business: it is a global,
        // a declaration, `spawn`, or a mentra builtin.
        let owns = tools
            .as_ref()
            .is_some_and(|tools| tools.host_tools.contains(&call.tool_name));
        if owns || !self.claims.holds_native(&call.tool_name) {
            return Ok(crate::hooks::HookOutcome::Allow);
        }

        Ok(crate::hooks::HookOutcome::Deny(format!(
            "'{}' is a native tool another open of this workspace supplied, and this one did not",
            call.tool_name
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools(hidden: &[&str]) -> AgentTools {
        AgentTools {
            hidden: hidden.iter().map(|name| (*name).to_string()).collect(),
            #[cfg(feature = "mcp")]
            mcp_servers: Vec::new(),
            host_tools: Vec::new(),
        }
    }

    /// Every question the guard asks, and the one it deliberately does not.
    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn the_guard_judges_a_bridged_name_by_the_callers_own_servers() {
        use crate::hooks::{HookEvent, HookOutcome, HookRequest, Interceptor};

        let registry = Arc::new(AgentRegistry::default());
        let workspace = WorkspaceAgents::new(Arc::clone(&registry));
        let _owner = workspace.record(
            "owner",
            AgentTools {
                hidden: BTreeSet::new(),
                mcp_servers: vec!["prod-db".to_string()],
                host_tools: Vec::new(),
            },
        );
        let _stranger = workspace.record("stranger", tools(&[]));

        let guard = ForeignToolGuard::new(registry, crate::runtime::ToolClaims::default());
        let call = |agent: &str, tool: &str| HookRequest {
            hook_schema: 1,
            event: HookEvent::PreToolUse,
            workspace: std::path::PathBuf::from("/repo"),
            agent_id: agent.to_string(),
            tool_call_id: "call-0".to_string(),
            tool_name: tool.to_string(),
            input: serde_json::json!({}),
            output: None,
            is_error: None,
        };

        assert!(matches!(
            guard
                .intercept(&call("owner", "mcp__prod-db__query"))
                .await
                .expect("decides"),
            HookOutcome::Allow
        ));
        assert!(
            matches!(
                guard
                    .intercept(&call("stranger", "mcp__prod-db__query"))
                    .await
                    .expect("decides"),
                HookOutcome::Deny(_)
            ),
            "the open that configured no servers must not reach the other's"
        );
        assert!(
            matches!(
                guard
                    .intercept(&call("stranger", "read"))
                    .await
                    .expect("decides"),
                HookOutcome::Allow
            ),
            "a name that is not a bridged tool's is none of this guard's business"
        );
        assert!(
            matches!(
                guard
                    .intercept(&call("host-session", "mcp__prod-db__query"))
                    .await
                    .expect("decides"),
                HookOutcome::Allow
            ),
            "a session basis never minted carries no workspace's guards at all"
        );
    }

    /// The row's lifetime is the *agent's*, and this is where that is decided.
    ///
    /// It used to be the workspace's, and that was the defect
    /// (`docs/proposals/0004`): a run outlives the workspace that minted it —
    /// nothing attaches the two — while the guard reads this ledger on every
    /// call that run makes. A row released with the workspace left a live
    /// session unattributable, which for a bridged name means allowed.
    /// A row's lifetime is its run's, because a run is how long the session
    /// lives: nothing on a `PreparedRun` yields an owned `Session`.
    #[test]
    fn a_row_outlives_its_workspace_and_leaves_with_its_run() {
        let registry = Arc::new(AgentRegistry::default());
        let workspace = WorkspaceAgents::new(Arc::clone(&registry));

        let one = workspace.record("agent-1", tools(&["mcp__prod-db__query"]));
        let two = workspace.record("agent-2", tools(&["mcp__prod-db__query"]));

        // The run half: a run outlives the workspace that minted it, and a row
        // released with the workspace would vanish under a live session.
        drop(workspace);
        assert!(
            registry.of("agent-1").is_some() && registry.of("agent-2").is_some(),
            "a session whose workspace has gone is still a session, and still has to be \
             judged by what its own open configured"
        );

        drop(one);
        assert!(
            registry.of("agent-1").is_none() && registry.of("agent-2").is_some(),
            "each row leaves with the last hold on it and takes no other with it"
        );
        drop(two);
        assert!(
            registry.of("agent-2").is_none(),
            "and nothing outlives the thing that needed it"
        );
    }

    /// Every hide standing against `agent_id` right now.
    fn hides(registry: &AgentRegistry, agent_id: &str) -> Vec<String> {
        registry
            .of(agent_id)
            .expect("a recorded agent")
            .hidden
            .iter()
            .cloned()
            .collect()
    }

    /// **The erase half of last-writer-wins**, which is the half that does not
    /// correct itself.
    ///
    /// Two opens of one directory, and a conversation that moves between them:
    /// [`Workspace::resume`](crate::Workspace::resume) checks the persisted
    /// agent's *root*, and two same-root opens have the identical root
    /// identifier by construction, so either may pick up an id the other
    /// minted. Overwriting is harmless — mentra refuses a second live session
    /// on one agent id, so nothing can be running under a row it did not just
    /// write. Dropping is where a hold that lost the row can still do damage:
    /// it outlives the takeover, and an absent row is *allowed* by both readers
    /// of this ledger.
    #[test]
    fn a_workspace_only_releases_the_agents_it_still_owns() {
        let registry = Arc::new(AgentRegistry::default());
        let a = WorkspaceAgents::new(Arc::clone(&registry));
        let b = WorkspaceAgents::new(Arc::clone(&registry));

        let a_moved = a.record("moved", tools(&["mcp__a-only__query"]));
        let a_own = a.record("a's own", tools(&["mcp__a-only__query"]));
        let b_moved = b.record("moved", tools(&["mcp__b-only__query"]));
        // `a` resumes the conversation back, which re-records it. From here
        // only `a` can hold its lease, so only `a`'s row may answer for it.
        let a_moved_again = a.record("moved", tools(&["mcp__a-only__query"]));

        drop(b_moved);
        drop(a_moved);

        assert_eq!(
            hides(&registry, "moved"),
            ["mcp__a-only__query"],
            "neither the sibling that lost this agent nor the stale hold from before it \
             came back may erase the row the live run re-recorded: a missing row is a \
             guard that allows and a child that inherits no hides"
        );

        drop(a_moved_again);
        drop(a_own);

        assert!(
            registry.of("moved").is_none() && registry.of("a's own").is_none(),
            "and the holds that do own their rows still release them"
        );
    }

    /// The same rule read from the other side: what a sibling took over stays
    /// when the workspace that minted it goes, and the rest still leaves.
    #[test]
    fn an_agent_a_sibling_took_over_outlives_the_workspace_that_minted_it() {
        let registry = Arc::new(AgentRegistry::default());
        let a = WorkspaceAgents::new(Arc::clone(&registry));
        let b = WorkspaceAgents::new(Arc::clone(&registry));

        let a_moved = a.record("moved", tools(&["mcp__a-only__query"]));
        let a_stayed = a.record("stayed", tools(&["mcp__a-only__query"]));
        let b_moved = b.record("moved", tools(&["mcp__b-only__query"]));

        drop(a_moved);
        drop(a_stayed);

        assert_eq!(
            hides(&registry, "moved"),
            ["mcp__b-only__query"],
            "the row belongs to whoever wrote it last, and that is who is running it"
        );
        assert!(
            registry.of("stayed").is_none(),
            "declining to release one agent must not hold back the others"
        );

        drop(b_moved);

        assert!(
            registry.of("moved").is_none(),
            "and the row still leaves with the hold that does own it"
        );
    }

    #[test]
    fn a_delegated_child_answers_for_its_parents_workspace_until_it_returns() {
        let registry = Arc::new(AgentRegistry::default());
        let workspace = WorkspaceAgents::new(Arc::clone(&registry));
        let _parent = workspace.record("parent", tools(&["mcp__prod-db__query"]));

        let adopted = registry
            .adopt("parent", "child")
            .expect("the parent is known");
        assert_eq!(
            registry
                .of("child")
                .expect("the child inherits")
                .hidden
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["mcp__prod-db__query"],
            "a child inherits its parent's audience, so it inherits its parent's denials"
        );

        drop(adopted);

        assert!(registry.of("child").is_none());
        assert!(
            registry.of("parent").is_some(),
            "the parent is still running"
        );
        assert!(
            registry.adopt("stranger", "grandchild").is_none(),
            "an agent basis did not make has nothing to hand down"
        );
    }
}
