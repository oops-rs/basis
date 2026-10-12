# 0029 — A remote agent is a `spawn` target

> Status: Accepted · 2026-10-09
> Amends [`0028-a2a-is-the-agent-to-agent-protocol.md`](0028-a2a-is-the-agent-to-agent-protocol.md)
> §1 — basis no longer registers mentra's `a2a__{agent}` tool; the client, the
> convention and the server are unchanged.
> Extends [`0016-one-delegation-surface.md`](0016-one-delegation-surface.md)
> and [`0021-a-command-names-where-it-runs.md`](0021-a-command-names-where-it-runs.md)
> — a delegation gains the target dimension a command already has; the number
> of doors stays one.
> Applies [`0005-mentra-coevolution-discipline.md`](0005-mentra-coevolution-discipline.md)
> ([mentra#61](https://github.com/oops-rs/mentra/issues/61)).
> Amended by [`0031-remote-agents-are-live.md`](0031-remote-agents-are-live.md):
> the set an open connected is live — reloaded, watched and retried (§3, §4).
> Extended by [`0035-a-host-may-key-remote-conversations.md`](0035-a-host-may-key-remote-conversations.md):
> a host may key a run's remote conversations instead of the calling agent (§5).

## Context

basis 0.13.0 put every remote agent a workspace named in `.basis/a2a.json` into
the roster as a tool of its own, `a2a__{agent}`. It worked — a live run proved
a caller reaching a served agent and reading its answer — and it reopened the
problem ADR-0016 closed.

1. **Handing work to something else is one act, with one name.** ADR-0016
   folded mentra's `shell` and `task` into `spawn` because two names were two
   questions at the approval gate and two rule namespaces for what an operator
   thinks of as one. `a2a__{agent}` is a third name for a delegation, and then
   a fourth, one per agent: "always let this repository's agent ask the
   reviewer" was a rule about a tool name, not about a delegation.
2. **The roster grew with the agent list.** Every configured agent was a tool
   description in every request, and the point of A2A is that the list grows —
   agents other people build, added by a line in a file.
3. **A runtime-wide registry made a workspace's agent a shared name.** The tool
   registry belongs to the runtime; an agent belongs to one open of one
   workspace, with that open's credential. Bridging one as a tool therefore
   needed what MCP needs: a name claim, a suffix when two workspaces named the
   same agent, sibling hiding for a second open of one directory, and a guard
   refusing a guessed foreign name. None of it is about A2A.
4. **basis already knows, per agent, what that agent's workspace configured.**
   The agent ledger (`runtime::agents`) is keyed by agent id, written at every
   mint and resume, and inherited by a delegated child. It is where `spawn`
   already reads what a delegating agent may pass on.
5. **`spawn` already has a target.** ADR-0021 gave a command `!@<target>`: a
   prefix read once by the parser, a typed field after it, and a `target` key
   in the preview every rule globs against.
6. **A conversation's ids are bookkeeping.** The `a2a__{agent}` tool asked the
   model to carry `task_id` and `context_id` between calls. A harness can keep
   them, and a model that never sees them cannot mistype them.
7. **One tool, one description, is the registry's contract.** A tool's
   description is what the model learns its targets from, and two agents on one
   runtime can reach different remote agents. mentra's registry resolves a name
   exact-agent → audience → global, but only its own `read_tool_result` could
   reach the exact-agent layer.

## Decision

**A remote agent is a target of a delegation. `spawn "@reviewer look at this
diff"` hands the task to the remote agent `reviewer` over A2A, and every
delegation — local or remote — is the one tool, `spawn`.**

1. **The grammar.** A delegation that begins `@<agent>` names a remote agent:
   `@<agent> <task>` hands it the task. `@<agent>` alone, with no task, waits
   again on the agent's outstanding task (§5). `@@` escapes a task that begins
   with a literal `@`, as `!!` escapes one that begins with `!`. The prefix is
   read by the same parser, once, in the same pass as `!` and `!@`; after it,
   [`Spawn::target`] is the agent's name and nothing re-reads the string. Agent
   names share the target charset, and `local` is reserved for both.

2. **What the approver sees.** The preview is ADR-0016's
   `{mode, body, cwd, target}`, unchanged in shape: `mode` is `"agent"`,
   `target` is the agent's name rather than `"local"`. A remote delegation
   declares [`External`] side effects — it hands work to a system this process
   does not control — where a local one declares `LocalState`; both are
   consequential, so neither is ever waved through. A rule written for `spawn`
   with `target` globbed to an agent's name is "always let this workspace ask
   that agent", expressed as data.

3. **Who can reach whom is decided per agent.** A workspace open reads its A2A
   files and connects its agents as before — card fetched once, an unreachable
   agent a warning — and records them on every agent it mints or resumes, in
   the agent ledger beside the rest of what that mint settled. A delegated
   child inherits its parent's. `spawn` routes `@<agent>` by reading the
   calling agent's row, so a sibling open of the same directory reaches only
   its own agents with its own credentials, by construction. Nothing is
   registered per remote agent, so the claim, the suffix, the sibling hiding,
   the foreign-tool guard and the reserved `a2a__` prefix are retired for A2A;
   [`Workspace::a2a_agents`] reports the configured names verbatim.

4. **What the model is told is decided per agent too.** An agent that can reach
   remote agents gets its own registration of `spawn`, on mentra's
   exact-agent layer ([mentra#61](https://github.com/oops-rs/mentra/issues/61)),
   whose description adds one paragraph: the grammar, and each agent's name
   with the summary its card gives. It is the same tool — same name, same
   depth ledger, same child policy, same preview — told about different
   targets. An agent with none sees the `spawn` it always did, and is never
   taught a prefix that leads nowhere, which is ADR-0021's rule for commands.

5. **basis keeps the conversation.** Within one session, successive
   delegations to one remote agent continue one A2A conversation (one
   `contextId`). A task that is still working when the wait bound passes, or
   that is waiting for input, is the agent's *outstanding* task: `@<agent>`
   alone waits on it again, `@<agent> <text>` answers a task waiting for input,
   and a new task while one is still working is refused with that instruction
   rather than sent, because a remote agent's conversation runs one task at a
   time.

   **The record outlives the process**, because a basis task does: a session
   resumed after a restart that had forgotten its outstanding task would hand
   the same work off twice. For every agent a workspace mints or resumes, the
   record — one `contextId` and at most one outstanding task id per remote
   agent — is written beside the store that holds the session
   (`a2a-conversations/<agent id>.json`), replaced whole on every change, and
   read back when a resumed session first delegates. The task id is written
   the moment the remote agent returns it, *before* the wait begins, so a
   process that dies mid-wait comes back knowing the task exists; mentra's
   exchange is exposed as `submit` and `settle` for exactly that gap.
   Forgetting a conversation removes its record. A delegated child, which is
   never resumed, and every agent on a runtime that keeps no history, keep the
   record in memory only.

6. **mentra keeps the protocol.** The client, the send-and-wait exchange —
   submit with `returnImmediately`, poll to a settled state within the bound,
   cancel when the run is cancelled, whole or as its `submit` and `settle`
   halves — and the framing that marks a remote
   agent's words as its own are mentra's, exposed as data (mentra#61) so basis
   writes only `spawn`'s guidance around them. mentra's `A2aAgentTool` stays,
   built on the same exchange, for harnesses that want a tool per agent.

7. **A remote delegation is not a local level.** It mints no child on this
   runtime, so it neither enters nor is refused by the delegation depth ledger.
   Depth and budget that travel *between* agents remain
   [proposal 0005](../proposals/0005-a2a-beyond-the-first-cut.md)'s.

## Consequences

- One door again: an operator approves, remembers and audits every delegation
  under one name, and the roster no longer grows with the agent list.
- **Breaking, so 0.14.0.** The `a2a__{agent}` tool names are gone, and a rule or
  hook written against one is a `spawn` rule with a `target` now. The file, the
  feature, `Workspace::a2a_agents`, the `RunStarted` fields and the server are
  unchanged; a name that 0.13.0 suffixed for a second workspace is no longer
  suffixed.
- A prompt that begins with `@` to mean something else is now a target name or
  a refusal that names the `@@` escape — the same trade ADR-0016 made for `!`.
- The model never sees a task or context id; basis carries them, and carries
  them across a restart, so a resumed task picks up remote work it had
  handed off — `@<agent>` alone waits on it — rather than sending it again.
- Each agent that can reach remote agents costs one exact-agent registration,
  dropped with the agent.

[`Spawn::target`]: ../../basis/src/tools/spawn/parse.rs
[`External`]: https://docs.rs/mentra/latest/mentra/tool/enum.ToolSideEffectLevel.html
[`Workspace::a2a_agents`]: ../../basis/src/workspace.rs
