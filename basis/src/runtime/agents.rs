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
//! - **[`ForeignMcpGuard`]**, in every workspace's own interception chain,
//!   answers *may this call run*. It reads [`AgentTools::mcp_servers`] — what
//!   the calling agent's workspace actually configured — on every call, so it
//!   is right however long ago the session was minted, whichever open of the
//!   directory came first, and whether or not a sibling's bridge had finished
//!   when the mint computed its roster.
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
        Arc, Mutex, RwLock,
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
    /// [`ForeignMcpGuard`]'s whole input, and the reason it cannot go stale: a
    /// workspace's server list is settled at its open, before any tool of any
    /// sibling is bridged, and it does not change while the workspace lives.
    #[cfg(feature = "mcp")]
    pub(crate) mcp_servers: Vec<String>,
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
/// self-correcting: **a handle that is no longer the row's owner must not erase
/// it on its way out.** A workspace that resumed a sibling's conversation once
/// still has the id in its `recorded` set long after the sibling took it back,
/// and an absent row is not a safe default — [`ForeignMcpGuard`] reads it as
/// "an agent basis did not make" and allows the call, `spawn` reads it as "no
/// inherited hides" — so the erasure would reopen both holes for exactly as
/// long as the victim's session stayed live.
///
/// Monotonic and never reused, so a stamp names one handle for the life of the
/// runtime; a pointer identity would have to outlive the handle to stay unique,
/// and a slot index would be reused by the next open.
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
    pub(crate) fn adopt(self: &Arc<Self>, parent: &str, child: &str) -> Option<AdoptedChild> {
        let tools = self.of(parent)?;
        let owner = self.new_owner();
        self.agents
            .write()
            .expect("agent registry poisoned")
            .insert(child.to_string(), Entry { owner, tools });

        Some(AdoptedChild {
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
    /// This handle's stamp: what its writes carry and what its drop releases
    /// against. Per *handle* rather than per root, because two opens of one
    /// root are exactly the pair this has to tell apart.
    owner: AgentOwner,
    /// Every agent id this workspace recorded. A set rather than a list
    /// because resuming one conversation twice records it twice.
    ///
    /// A superset of what this workspace still owns, deliberately: an id a
    /// sibling open has since taken over stays in here, and the release is
    /// what declines to act on it.
    recorded: Mutex<BTreeSet<String>>,
}

impl WorkspaceAgents {
    pub(crate) fn new(registry: Arc<AgentRegistry>) -> Self {
        let owner = registry.new_owner();
        Self {
            registry,
            owner,
            recorded: Mutex::new(BTreeSet::new()),
        }
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
    pub(crate) fn record(&self, agent_id: &str, tools: AgentTools) {
        self.registry.record(self.owner, agent_id, Arc::new(tools));
        self.recorded
            .lock()
            .expect("recorded agent set poisoned")
            .insert(agent_id.to_string());
    }
}

impl Drop for WorkspaceAgents {
    fn drop(&mut self) {
        for agent_id in self
            .recorded
            .lock()
            .expect("recorded agent set poisoned")
            .iter()
        {
            self.registry.forget_if_owned(agent_id, self.owner);
        }
    }
}

/// A delegated child's entry, forgotten when the delegation returns.
///
/// Scoped to the delegation rather than to the workspace because that is
/// exactly how long the child exists: `spawn` owns the `Agent` from
/// `spawn_subagent` to the answer it reads back, and mentra disposes of it
/// after.
#[derive(Debug)]
pub(crate) struct AdoptedChild {
    registry: Arc<AgentRegistry>,
    /// Stamped and released like a workspace's, for the same reason and with
    /// no reliance on mentra minting a fresh id per delegation: whoever wrote
    /// the row last is whoever may take it away.
    owner: AgentOwner,
    agent_id: String,
}

impl Drop for AdoptedChild {
    fn drop(&mut self) {
        self.registry.forget_if_owned(&self.agent_id, self.owner);
    }
}

/// Refuses a call to a bridged tool whose server the calling agent's workspace
/// never configured.
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
/// **Duplicate registrations are harmless, and that matters here.** Two opens
/// of one directory join a single chain
/// ([`Runtime::register_hook_chain`](super::Runtime::register_hook_chain)), so
/// only the first open's guard is ever live — and it still answers correctly
/// for the second open's sessions, because it decides from the ledger rather
/// than from the workspace that happened to build it.
#[cfg(feature = "mcp")]
#[derive(Debug)]
pub(crate) struct ForeignMcpGuard {
    agents: Arc<AgentRegistry>,
}

#[cfg(feature = "mcp")]
impl ForeignMcpGuard {
    pub(crate) fn new(agents: Arc<AgentRegistry>) -> Self {
        Self { agents }
    }
}

#[cfg(feature = "mcp")]
#[async_trait::async_trait]
impl crate::hooks::Interceptor for ForeignMcpGuard {
    fn name(&self) -> &str {
        "basis mcp ownership"
    }

    async fn intercept(
        &self,
        call: &crate::hooks::HookRequest,
    ) -> Result<crate::hooks::HookOutcome, crate::hooks::InterceptorError> {
        // mentra's own parser, so a name a suffixed claim assembled
        // (`claim_mcp_server` resolves a collision with `-<hash>`, which holds
        // no `__`) splits here exactly as it was put together there.
        let Some((server, _)) = mentra::mcp::parse_mcp_tool_name(&call.tool_name) else {
            return Ok(crate::hooks::HookOutcome::Allow);
        };
        let Some(tools) = self.agents.of(&call.agent_id) else {
            return Ok(crate::hooks::HookOutcome::Allow);
        };
        if tools.mcp_servers.iter().any(|own| own == server) {
            return Ok(crate::hooks::HookOutcome::Allow);
        }

        Ok(crate::hooks::HookOutcome::Deny(format!(
            "'{}' belongs to the MCP server '{server}', which this workspace did not configure",
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
        }
    }

    /// Every question the guard asks, and the one it deliberately does not.
    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn the_guard_judges_a_bridged_name_by_the_callers_own_servers() {
        use crate::hooks::{HookEvent, HookOutcome, HookRequest, Interceptor};

        let registry = Arc::new(AgentRegistry::default());
        let workspace = WorkspaceAgents::new(Arc::clone(&registry));
        workspace.record(
            "owner",
            AgentTools {
                hidden: BTreeSet::new(),
                mcp_servers: vec!["prod-db".to_string()],
            },
        );
        workspace.record("stranger", tools(&[]));

        let guard = ForeignMcpGuard::new(registry);
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

    #[test]
    fn a_workspaces_agents_leave_the_registry_with_it() {
        let registry = Arc::new(AgentRegistry::default());
        let workspace = WorkspaceAgents::new(Arc::clone(&registry));

        workspace.record("agent-1", tools(&["mcp__prod-db__query"]));
        workspace.record("agent-2", tools(&["mcp__prod-db__query"]));
        assert!(registry.of("agent-1").is_some());

        drop(workspace);

        assert!(
            registry.of("agent-1").is_none() && registry.of("agent-2").is_none(),
            "an entry outliving its workspace would speak for a registry it no longer knows"
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
    /// write. Dropping is where a handle that lost the row can still do damage:
    /// it holds the id in `recorded` forever, and an absent row is *allowed* by
    /// both readers of this ledger.
    #[test]
    fn a_workspace_only_releases_the_agents_it_still_owns() {
        let registry = Arc::new(AgentRegistry::default());
        let a = WorkspaceAgents::new(Arc::clone(&registry));
        let b = WorkspaceAgents::new(Arc::clone(&registry));

        a.record("moved", tools(&["mcp__a-only__query"]));
        a.record("a's own", tools(&["mcp__a-only__query"]));
        b.record("moved", tools(&["mcp__b-only__query"]));
        // `a` resumes the conversation back, which re-records it. From here
        // only `a` can hold its lease, so only `a`'s row may answer for it.
        a.record("moved", tools(&["mcp__a-only__query"]));

        drop(b);

        assert_eq!(
            hides(&registry, "moved"),
            ["mcp__a-only__query"],
            "the sibling that no longer owns this agent must not erase the row the live \
             one re-recorded: a missing row is a guard that allows and a child that \
             inherits no hides"
        );

        drop(a);

        assert!(
            registry.of("moved").is_none() && registry.of("a's own").is_none(),
            "the owner's own drop still releases everything standing in its name"
        );
    }

    /// The same rule read from the other side: what a sibling took over stays
    /// when the workspace that minted it goes, and the rest still leaves.
    #[test]
    fn an_agent_a_sibling_took_over_outlives_the_workspace_that_minted_it() {
        let registry = Arc::new(AgentRegistry::default());
        let a = WorkspaceAgents::new(Arc::clone(&registry));
        let b = WorkspaceAgents::new(Arc::clone(&registry));

        a.record("moved", tools(&["mcp__a-only__query"]));
        a.record("stayed", tools(&["mcp__a-only__query"]));
        b.record("moved", tools(&["mcp__b-only__query"]));

        drop(a);

        assert_eq!(
            hides(&registry, "moved"),
            ["mcp__b-only__query"],
            "the row belongs to whoever wrote it last, and that is who is running it"
        );
        assert!(
            registry.of("stayed").is_none(),
            "declining to release one agent must not hold back the others"
        );

        drop(b);

        assert!(
            registry.of("moved").is_none(),
            "and the row still leaves with the handle that does own it"
        );
    }

    #[test]
    fn a_delegated_child_answers_for_its_parents_workspace_until_it_returns() {
        let registry = Arc::new(AgentRegistry::default());
        let workspace = WorkspaceAgents::new(Arc::clone(&registry));
        workspace.record("parent", tools(&["mcp__prod-db__query"]));

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
