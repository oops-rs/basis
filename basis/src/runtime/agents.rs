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
//! Entries leave with what put them there: a workspace's on its drop, a
//! delegated child's when the delegation returns.

use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex, RwLock},
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

/// Which workspace each live agent on this runtime answers for.
#[derive(Debug, Default)]
pub(crate) struct AgentRegistry {
    agents: RwLock<HashMap<String, Arc<AgentTools>>>,
}

impl AgentRegistry {
    /// What the agent called `agent_id` was minted with, or `None` for an
    /// agent basis did not make.
    pub(crate) fn of(&self, agent_id: &str) -> Option<Arc<AgentTools>> {
        self.agents
            .read()
            .expect("agent registry poisoned")
            .get(agent_id)
            .map(Arc::clone)
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
        self.agents
            .write()
            .expect("agent registry poisoned")
            .insert(child.to_string(), tools);

        Some(AdoptedChild {
            registry: Arc::clone(self),
            agent_id: child.to_string(),
        })
    }

    fn record(&self, agent_id: &str, tools: Arc<AgentTools>) {
        self.agents
            .write()
            .expect("agent registry poisoned")
            .insert(agent_id.to_string(), tools);
    }

    fn forget(&self, agent_id: &str) {
        self.agents
            .write()
            .expect("agent registry poisoned")
            .remove(agent_id);
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
    /// Every agent id this workspace recorded. A set rather than a list
    /// because resuming one conversation twice records it twice.
    recorded: Mutex<BTreeSet<String>>,
}

impl WorkspaceAgents {
    pub(crate) fn new(registry: Arc<AgentRegistry>) -> Self {
        Self {
            registry,
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
    pub(crate) fn record(&self, agent_id: &str, tools: AgentTools) {
        self.registry.record(agent_id, Arc::new(tools));
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
            self.registry.forget(agent_id);
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
    agent_id: String,
}

impl Drop for AdoptedChild {
    fn drop(&mut self) {
        self.registry.forget(&self.agent_id);
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
