# 0037 — An agent is named

> Status: Accepted · 2026-10-14
> Amends [`0029-a-remote-agent-is-a-spawn-target.md`](0029-a-remote-agent-is-a-spawn-target.md)
> §1, §3 and §5 — `@<name>` is resolved rather than looked up in files, a
> name nobody resolves is a local teammate, and a conversation is keyed by the
> agent's id.
> Amends [`0031-remote-agents-are-live.md`](0031-remote-agents-are-live.md)
> — the live set is the configured resolver, one resolver among others.
> Extends [`0016-one-delegation-surface.md`](0016-one-delegation-surface.md)
> — a delegation gains the `reach` dimension; the number of doors stays one.
> Preserves the ruling in `basis/src/workspace/roster.rs` that mentra's team
> tools stay hidden: a teammate arrives through `spawn`, not a second door.

## Context

Four facts, verified in basis 0.17.0 and mentra 0.29.0.

1. **The grammar already has both shapes.** `spawn "task"` is a one-off
   subagent: a new child per call, run to completion, final text back.
   `spawn "@name task"` is a remote agent, with `@name` alone to keep waiting
   and `@name answer` to answer a question (ADR-0029 §1, §5). The only
   difference the model sees is the name.
2. **basis answers `@name` from files only.** `.basis/a2a.json`, the global
   file and `A2aConfig::supplied`, connected at open, with backoff, reload,
   watch and removed-name tracking in `basis/src/a2a` (ADR-0031). A host
   cannot answer the question itself, and the grammar refuses `@ns/name` at
   parse. The first host that needs to is dial: a company registry where
   every agent is `@ns/name`, the registry cannot be enumerated, and a gateway
   mints the credential per request.
3. **The exchange needs two things from the answer.** `spawn::remote::delegate`
   wants a connected `Arc<A2aClient>` and the endpoint it connected to. The
   conversation record, the hop metadata and the submit and settle halves are
   basis's around it (ADR-0029 §5, §6).
4. **A kept child is multi-turn with no mentra change.** `Agent::run(&mut
   self, content, options)` runs one user turn and can be called again on the
   same agent; it is the call a session makes for every turn. A child basis
   keeps instead of dropping is therefore a teammate.

A design was considered that added a `ResolvedAgent` enum with `Local` and
`Remote` arms and local teammates backed by basis-tasks. That is two kinds of
thing, a second dispatch, and a dependency basis cannot take, since basis-tasks
sits above basis. This ADR takes the narrower cut: one lookup, with local as
what remains when the lookup finds nothing.

## Decision

**`@name` is resolved. A workspace open asks its resolver; a name the resolver
knows is a remote agent reached over A2A, and any other name is a local
teammate on this runtime, made on the first `@name task` and continued by every
later one. A `reach` field on `spawn` says which of the two a call allows. A
name is a label; every agent it reaches has a permanent id.**

1. **The seam.** `WorkspaceBuilder::with_agent_resolver(Arc<dyn
   AgentResolver>)`. Per workspace open, because who an open may reach and with
   which credentials is decided per open (ADR-0029 §3). The host's resolver is
   asked before the configured set, in the order `supplied`, workspace file,
   global file already have. `without_discovery` leaves the host's resolver
   alone. A delegated child inherits its parent's, as today.

2. **What crosses the seam.** A name in; a connected client, the agent's id,
   its canonical name and the endpoint out. Connecting, caching, credentials
   and retries are the resolver's. basis holds the client for the whole
   exchange (ADR-0031 §6 unchanged). The trait, behind the default-on `a2a`
   feature in `basis::a2a`:

   ```rust
   #[async_trait]
   pub trait AgentResolver: Send + Sync {
       /// The client `@name` is handed to, or why there is none.
       async fn resolve(&self, name: &str) -> Result<Resolved, Unresolved>;
       /// Where `@name` stands without waiting on anything — the preview's
       /// question. Default `Maybe`: the preview claims `External`, execute
       /// decides.
       fn route(&self, _name: &str) -> Route { Route::Maybe }
       /// What `spawn` teaches about this resolver's names. Default nothing.
       fn taught(&self) -> Taught { Taught::default() }
       /// Given once, at install: what to call after the set changes, so the
       /// descriptions are captured again (ADR-0031 §3).
       fn on_change(&self, _hook: OnChange) {}
   }

   pub enum Route { Known, Unknown, Maybe, Refused(String) }
   pub struct Resolved { client: Arc<A2aClient>, id: String, name: String, endpoint: String }
   pub enum Unresolved { Unknown, Unreachable { reason: String, retry_in: Option<Duration> } }
   pub struct Taught { agents: Vec<TaughtAgent>, note: Option<String> }
   ```

   `Unresolved::Unknown` carries no text because under `auto` it is not a
   refusal; under `reach: remote` basis phrases the refusal from `taught()`,
   in `spawn`'s grammar with the `@@` escape. A resolver never has to know
   how `spawn` talks.

3. **The `reach` field.** The tool's input grows from one string to
   `{ input, reach? }`, with `reach` one of `auto`, `remote`, `local`.
   `auto`, the default, resolves and falls through to local. `remote` refuses
   a name no resolver knows, so a typo is a refusal rather than a new
   teammate. `local` never asks the resolver, so a teammate can share a name
   with a remote agent without reaching it. A host may fix the default and
   the allowed values on the workspace builder; a value the host disallows is
   refused in preview. A plain `spawn "task"` ignores the field. ADR-0016's
   rule is kept where it matters: the string is still parsed once, at the
   edge, and `reach` is a typed field beside the typed mode, never a second
   reader of the string.

4. **What the approver sees.** The preview keeps `{mode, body, cwd, target}`
   and adds `reach` with the value *decided*, `remote` or `local`, never
   `auto`, and `agent_id` once it is known. A remote delegation declares
   `External`; a local one declares `LocalState`, as today. Under `auto`, a
   name no resolver can classify without the network (`Route::Maybe`) is
   previewed as `External`, the stronger claim. Existing rules on `target`
   keep matching.

5. **Local teammates.** A teammate is the one-off child basis already mints,
   kept instead of dropped.
   - Minted through the child policy: `ChildContext` gains `name()`, so a
     host's existing policy gives `@triage` a cheap model or a narrow roster
     with no new seam. A one-off child sees `None`.
   - Kept per calling agent, in the ledger row that already holds the
     caller's remote set. A child's teammates are its own. The row's drop
     ends them.
   - Each call is one turn, run under `child_run_options`, so spend lands on
     the parent's counter and the deadline and cancellation are the parent's.
     The reply is the final text. A question is a reply that asks; `@name
     answer` continues it. No outstanding state, no inbox.
   - One turn at a time: a second call to a teammate still working is refused
     with the wording the remote case already teaches.
   - `@name` alone is refused for a teammate: nothing is outstanding.
   - A teammate's turn enters the depth ledger like a one-off child and leaves
     it after; the delegation request and result entries are written as
     today, under the teammate's agent id.
   - Lifetime is the process. A session resumed after a restart has no
     teammates; the next `@name task` makes a fresh one, and the reply shows a
     new id. A teammate that must outlive the process is a task the host
     serves over basis-a2a (ADR-0033, ADR-0034) and resolves by name, which is
     then a remote agent to basis.

6. **A name is a label; an id is the identity.** Names collide: two callers
   each make a `reviewer`; a restart makes a second `reviewer`; a reload adds
   a remote `reviewer` beside a local one; a registry frees a label and hands
   it to another agent. The id is what stays true through all four.
   - A local teammate's id is the mentra agent id it was minted with, already
     unique on the runtime and already the id in every delegation edge. A
     remote agent's id is what its resolver returns, the canonical name for a
     resolver that has nothing more permanent, such as a2a.json.
   - Records key on the id: a remote conversation is kept under the
     resolver's id, so a renamed registry entry continues its conversation
     and a reused label does not inherit one. A teammate is found by id; its
     name is an index onto it.
   - The model sees both: the description lists each teammate as `reviewer
     (id …)`, the preview carries `agent_id`, and every reply opens with the
     name and id.
   - `@<id>` is always unambiguous: lookup is by id first, then by name — the
     caller's own teammate ids, then the resolvers. An id parses under the
     same charset as a name.
   - A bound name stays bound: once a caller has a teammate called
     `reviewer`, `@reviewer` under `auto` keeps reaching it even after a
     reload adds a remote `reviewer`; a conversation in progress never moves
     silently. `reach: remote`, or the remote agent's id, reaches the other
     one, and the description says the name is shadowed.

7. **Teaching follows both.** Each agent's `spawn` copy lists the remote names
   its resolvers can enumerate, the note for a namespace that cannot be, and
   the caller's own teammates by name and id. A new teammate refreshes the
   descriptor, as a reload does (ADR-0031 §3).

8. **Grammar.** A target name admits `/` between segments: `@ns/name`, and
   `!@ns/box cargo test` for the same reason, one charset for one `target`
   field. A name cannot begin or end with `/` or hold two in a row; `local`
   stays reserved. A teammate's name is flat: a name with `/` under `reach:
   local` is refused, so a namespaced name always means a resolver.

9. **What today's a2a module becomes.** `ConfiguredAgents`, the resolver
   built from `A2aConfig`: the three sources, connect at open, backoff retry,
   reload, watch, removed-name tracking. Same code, same tests, under the
   trait. `Workspace::reload_a2a` and `watch_a2a` act on it; `a2a_agents`
   reports every taught remote name, host resolver first.

## Consequences

- One grammar, one door. The model learns `@name` once and never learns
  whether a teammate is a process or a server. An operator approves, remembers
  and audits every delegation under `spawn`, with `reach` and `agent_id` in the
  preview telling them apart.
- A host answers `@name` however it likes. dial installs one resolver and
  needs nothing else from basis.
- Local teammates arrive with no new tool, no basis-tasks dependency and no
  mentra change. mentra's team tools stay hidden.
- a2a.json, reload, watch and backoff are unchanged for everyone using them,
  and are now one implementation among others. The supplied-list bug (a host
  could not change `A2aConfig::supplied` after open) closes by construction: a
  host that owns its set owns its resolver.
- **Breaking in behavior, so 0.18.0.** Today `@unknown task` is refused; after,
  under the default `auto`, it makes a teammate. A host that wants the old
  behavior fixes `reach` to `remote` on the builder. A typo under `auto` makes
  a teammate rather than reaching the intended agent; the preview shows
  `reach: local`, the reply shows a fresh id, and `reach: remote` is one field
  away. This is the trade for a grammar with no second prefix.
- **A remembered rule that globbed `target` with `*` stops matching a
  namespaced name**: mentra's `glob-match` does not cross `/`. An operator who
  means every agent writes `**`; one who means a team writes `team/*`. The
  `cwd` key already behaves this way for paths.
- A teammate holds its context in memory for the process. Long-running hosts
  (ACP, the A2A server, the interactive CLI) get the most from it; a one-shot
  run gets what it got today.
- What is given up: basis cannot list a registry's agents, so the model is
  taught the shape of a name and must know, or be told, which exist.
