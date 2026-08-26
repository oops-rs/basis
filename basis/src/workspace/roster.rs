//! Which tools the model is offered — the workspace's own knob over what used
//! to be two constants and no host input (decision D3).
//!
//! [`ToolRoster::default`] is byte-identical to what every workspace has
//! always done: `spawn`'s replaced doors and the intrinsics basis has never
//! surfaced, hidden, and everything else mentra registers, offered. The two
//! constructors map straight onto mentra's own `ToolProfile`:
//! [`hide`](ToolRoster::hide) extends the denylist, [`only`](ToolRoster::only)
//! replaces it with an allow-list. Neither touches what is *registered* on the
//! runtime — hidden or un-allowed is a roster fact, never a capability fact
//! (see [`crate::workspace::builder`]'s module docs for why that distinction
//! matters to `spawn`).
//!
//! # Composes with two other things, always
//!
//! A roster is the *base* `ToolProfile` an opened workspace carries in its
//! `AgentConfig`. Two things still apply on top of it, regardless of which
//! constructor built it:
//!
//! - **Per-mint foreign-tool hiding.**
//!   [`Workspace::minted_agent`](super::Workspace::minted_agent) adds every
//!   `mcp__*` tool and every declared tool that belongs to a *sibling*
//!   workspace on a shared runtime into `hidden_tools` at mint time, because
//!   that set moves as siblings come and go and a roster fixed at
//!   [`WorkspaceBuilder::open`](super::WorkspaceBuilder::open) cannot know it
//!   yet. mentra's `ToolProfile::allows` checks `hidden_tools` after
//!   `allowed_tools`, so this addition suppresses a name whether the roster is
//!   a [`hide`](ToolRoster::hide) (the name joins an already-populated
//!   denylist) or an [`only`](ToolRoster::only) (the name was already absent
//!   from the allow-list, and the insertion is a harmless no-op — unless a
//!   caller's `only` set named a sibling's tool by coincidence, in which case
//!   it now correctly loses).
//! - **The rendered prompt.** Whatever a workspace's `AGENTS.md`, `CLAUDE.md`
//!   or memory files ([`crate::memory`]) put in the system prompt is a
//!   property of [`WorkspaceContext`](crate::context::WorkspaceContext) and
//!   [`crate::memory::index_block`], assembled entirely independently of the
//!   roster and rendered by `agent_config` regardless of what it decided. A
//!   roster that hides every file tool still ships a prompt — and a memory
//!   index — that may tell the model to go read one; the prompt does not know,
//!   and does not ask, what the roster allows.

use std::collections::BTreeSet;

use mentra::agent::ToolProfile;

/// Which tools the model is offered, for
/// [`WorkspaceBuilder::with_tool_roster`](super::WorkspaceBuilder::with_tool_roster).
///
/// A thin wrapper over mentra's `ToolProfile` rather than a re-export of it:
/// [`ToolRoster::default`] is the one construction this crate has an opinion
/// about (today's exact hidden set), and wrapping it is what lets that default
/// live beside the constant it is built from instead of at every call site
/// that wants "basis's usual roster, plus...".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRoster(ToolProfile);

impl Default for ToolRoster {
    /// What every workspace has offered before this type existed: `spawn`'s
    /// two replaced doors (`REPLACED_TOOLS`) and the intrinsics basis has
    /// never deliberately surfaced (`UNSURFACED_TOOLS`), hidden; everything
    /// else mentra's runtime registers — the file tools, `load_skill`,
    /// `compact`, and whatever a host's own tools or an MCP server add —
    /// offered.
    ///
    /// Pinned by `the_default_roster_is_exactly_this` and its neighbors in
    /// `workspace::builder::tests`: this has to keep producing that exact set
    /// for those tests to mean anything.
    fn default() -> Self {
        Self(ToolProfile::hide(hidden_tools()))
    }
}

impl ToolRoster {
    /// [`ToolRoster::default`]'s set, plus `names`.
    ///
    /// Extends rather than replaces, so narrowing a roster is additive: a
    /// caller hiding one more tool never has to restate `spawn`'s replaced
    /// doors or the unsurfaced intrinsics to keep them hidden too. Call
    /// [`only`](Self::only) for a denylist built from nothing instead of from
    /// basis's own.
    pub fn hide(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        let mut hidden: BTreeSet<String> = hidden_tools().map(str::to_string).collect();
        hidden.extend(names.into_iter().map(Into::into));

        Self(ToolProfile::hide(hidden))
    }

    /// Offers exactly `names`, mapped straight to mentra's own
    /// `ToolProfile::only` — an allow-list rather than [`hide`](Self::hide)'s
    /// denylist, and basis states no opinion of its own on top of it.
    ///
    /// Three honest caveats, each pinned by a test beside this one:
    ///
    /// - **Cannot un-register the file tools.** mentra 0.20 has no
    ///   `FileToolProfile::None`: whatever this workspace's runtime was built
    ///   with — basis's own default of `Split`, or a host's `Batched` — stays
    ///   on the runtime's registry no matter what `names` says. What `only`
    ///   *can* do, and does, is stop *offering* them: a set that names none of
    ///   `read`/`ls`/`grep`/`glob`/`write`/`edit` makes `ToolProfile::allows`
    ///   refuse every one of them for this workspace's agents, exactly as it
    ///   refuses any other name left out — see
    ///   `only_stops_offering_the_file_tools_but_cannot_unregister_them`. What
    ///   it does not buy is a runtime that never carried the capability: a
    ///   sibling workspace sharing this runtime, or mentra's own APIs reached
    ///   through [`Runtime::mentra_runtime`](crate::Runtime::mentra_runtime),
    ///   still find the tool sitting on the registry.
    /// - **Does not imply `spawn`.** A set that omits
    ///   [`crate::tools::SPAWN`] is a legitimate roster — a knowledge agent
    ///   with no delegation and no command door is a real shape — but it is
    ///   also a model with no route to a subagent or a shell command at all.
    ///   Name it if the agent needs either.
    /// - **Does not imply `load_skill`.** Skills are basis's own on-demand
    ///   convention; a set that omits it is a model that cannot load one, no
    ///   matter how many are registered.
    pub fn only(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self(ToolProfile::only(
            names.into_iter().map(Into::into).collect::<Vec<_>>(),
        ))
    }

    /// The mentra profile this roster resolved to, for
    /// [`WorkspaceBuilder::open`](super::WorkspaceBuilder::open) to place on
    /// the `AgentConfig` it builds.
    pub(crate) fn into_profile(self) -> ToolProfile {
        self.0
    }

    /// The same profile, borrowed — what a child policy's preview reads to
    /// describe an overriding roster without consuming the spec that carries
    /// it (`crate::tools::spawn::child`).
    pub(crate) fn as_profile(&self) -> &ToolProfile {
        &self.0
    }
}

/// Every name basis takes off the model's roster by default: what `spawn`
/// replaced, and what basis has never surfaced.
///
/// Two constants rather than one list because the two carry different
/// arguments and a reader deserves to know which applies to a given name.
fn hidden_tools() -> impl Iterator<Item = &'static str> {
    REPLACED_TOOLS.into_iter().chain(UNSURFACED_TOOLS)
}

/// The tools `spawn` replaces, by the names mentra registers them under.
const REPLACED_TOOLS: [&str; 3] = ["shell", "background_run", "task"];

/// What mentra registers that basis has never deliberately offered.
///
/// Registration is mentra's default posture — `register_tools` walks every
/// intrinsic variant it has — so a name reaching the model here is the absence
/// of a decision rather than one. Each of these fails a different way, and none
/// of the failures is visible to the person running the agent:
///
/// - **`team_spawn` and its six siblings are delegation by another name.** A
///   second door for *hand work to something else, read back a summary* is
///   exactly what ADR-0016 removed `task` for: two names arriving at one
///   approval gate, and two namespaces of remembered rules, for a question an
///   operator asks once. Nothing in basis mints a team, reads a teammate inbox,
///   or renders a `team_request`, so the door does not even lead where its
///   description says. `docs/REDESIGN.md` has recorded these as awaiting a
///   concrete use case since Phase D; reachable-by-accident is not the
///   deliberate surfacing that row is waiting for.
/// - **`idle` is that surface's exit.** Its whole effect is
///   `Agent::request_idle`, which mentra's orchestrator reads as
///   `should_end_turn` — a yield *back to the teammate loop* basis never
///   starts. On a basis run the model calling it ends its own turn mid-task
///   and the caller reads a short answer with no error in it.
/// - **`task_create` and the other four write a board nothing reads.** basis
///   surfaces no task board — not on the event stream, not over ACP, not in
///   the CLI — so a model that files, claims and updates work items gets
///   plausible success back from every call and nothing observable happens.
///   Confident bookkeeping into a void is worse than no bookkeeping, because
///   it reads to the model as coordination.
/// - **`check_background` reports on a tool that is hidden.** The only thing it
///   can report on is `background_run`, which left the roster with ADR-0016's
///   two other doors, so it can answer nothing but "no such task".
/// - **`memory_pin`, `memory_forget` and `memory_search` reach a store basis
///   has decided against (D2, wave 1).** basis's memory is a file convention
///   (`crate::memory`); mentra's engine — recall injection included, switched
///   off in `agent_config` beside this list — is not it. A model pinning facts
///   into a store nothing surfaces is the task-board failure again: plausible
///   success, and nothing the person running the agent can see.
///
/// Deliberately still offered, and each for a reason: `load_skill`, because
/// on-demand skills are basis's own convention and that tool is how a skill is
/// loaded; and `compact`, because a model that can see its context filling
/// should be able to act on it (that the *user* has no matching control is a
/// separate gap, and hiding this would not close it).
const UNSURFACED_TOOLS: [&str; 17] = [
    "check_background",
    "idle",
    "task_create",
    "task_claim",
    "task_update",
    "task_list",
    "task_get",
    "team_spawn",
    "team_send",
    "team_read_inbox",
    "team_broadcast",
    "team_request",
    "team_respond",
    "team_list_requests",
    "memory_pin",
    "memory_forget",
    "memory_search",
];

#[cfg(test)]
mod tests;
