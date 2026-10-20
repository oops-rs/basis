# 0042 — A delegation may run in the background

> Status: Accepted · 2026-10-20
> Extends [`0016-one-delegation-surface.md`](0016-one-delegation-surface.md)
> (one door, two more typed facts on it) and
> [`0017-structured-agent-concurrency.md`](0017-structured-agent-concurrency.md)
> (the in-process half its §2 described and nothing built); amends
> [`0020-spawn-routing-is-decided-by-the-environment.md`](0020-spawn-routing-is-decided-by-the-environment.md)
> (a parent model turn now has the non-blocking route that ADR named it as
> needing). Needs mentra 0.32.0 (`ToolContext::deferred`).

## Context

The model's `spawn` blocked. A task for a subagent, a teammate or a remote
agent ran inside the call and returned its answer, and mentra runs every
delegation in its exclusive lane, so three tasks written in one response ran
one after another. The CLI had had the other shape since ADR-0017 — `basis
spawn` returns a handle in a task, `basis wait` collects it — and that ADR's
§2 says in as many words that *inside another task* is the case where a
handle must come back at once, because blocking a parent's turn is how its
wait-for cycles start. ADR-0020 then made the shell the one place that
blocks, since a shell has nothing better to do. The model's tool was written
before both and reconciled with neither.

Two hosts built the missing half themselves. iBot wrote a `fan_out` tool that
mints children from the workspace on a `JoinSet` and hides itself from them;
nous wrote a facet orchestrator. Both re-derived the same rules — the parent's
cancellation, deadline and token counter, the delegation depth, a cap on how
many run at once — and both got one of them wrong, because a child minted
from `prepare` cannot adopt its parent's token counter. iBot also found that
two children sharing their parent's remote conversation refuse each other,
since a remote agent takes one task at a time (ADR-0029 §5).

Three designs were weighed.

1. **Lazy handles.** A background start registers the task and runs nothing;
   `wait` polls what it names, so a wait on several is a `join!`. This is
   Rust's future: nothing until polled, free to drop. It gives no overlap with
   the model's own work, makes a wait for *any* a cancellation of the rest,
   and does not describe a remote agent, whose task runs whether or not
   anyone here waits.
2. **Eager handles, dropped at the end.** A `tokio::spawn`, and a turn that
   ends with children still running aborts them, as dropping a `JoinSet`
   does. Simple, and it throws away tokens the model paid for and an answer
   it asked for.
3. **Eager handles, joined at the end.** A `tokio::spawn`, and the turn is the
   scope: it ends only after its children have, the way a task group does,
   and what they answered reaches the model before its answer is final.

## Decision

**A task may be started in the background through `spawn`, its handle is
the id it already has, its answer arrives on its own, and the turn that
started it does not end before it does.** Design 3.

1. **Two typed fields, one door.** `background: true` beside `input` starts
   the task and returns its handle at once. `wait: [handles]` in a call with
   no `input` collects one or more, all of them or — with `any: true` — the
   first to finish. `@<handle>` alone collects one, the spelling a remote
   wait already had. Neither is a prefix in the string: like `reach`,
   `metadata` and `files`, each is a fact the approver sees under its own
   key, and ADR-0016's one parse at the edge reads them. A command cannot run
   in the background; it runs here, to completion.
2. **The handle is the id the thing already has.** A subagent's handle is its
   agent id; a teammate's is its id, and its name reaches it too (ADR-0037
   §6); a remote agent's is its name, since one task per remote agent at a
   time (ADR-0029 §5) makes the name the conversation. No new id namespace.
3. **The start is eager; the scope is structured.** A background child runs
   on a task of its own from the moment the call returns, on the parent's
   `RunOptions::child` — its cancellation, stop, deadline and shared token
   counter — so a cancelled parent cancels it and a deadline ends them
   together. The turn owns its children: when the model ends its turn with
   some still running, basis's own round strategy (`JoinAtEnd`, composed in
   front of whatever the host set) waits for them, and mentra runs one more
   round so the model reads what they answered. An unawaited handle is never
   silently dropped and never silently discarded.
4. **The answer arrives on its own.** A child that finishes while the model
   is doing something else leaves its result with mentra's `Deferred`
   handle, and the runner puts it in front of the model before the next
   request, carrying the delegation's own record — the result entry with its
   artifact and edge, and the child's end on the parent's snapshot and
   stream — exactly as the blocking path records it. A wait that claims a
   handle records the same things through the tool context instead; the two
   paths are serialized under one lock, so nothing is delivered twice.
5. **Depth and width.** A background child counts toward the delegation depth
   like any other, and the guard is checked before the child policy, in the
   preview. Beside depth there is now width: how many of one agent's
   delegations may run in the background at once,
   `RuntimeBuilder::with_delegation_width`, four by default. Children share
   their parent's token counter and each round is checked against the bound
   only after it has spent, so N running at once can overshoot by N rounds
   where a blocking child overshoots by one; width is that N. Checked in the
   preview too, so no approver's answer and no remembered rule lifts it.
6. **A child's remote conversations are its own.** A delegated child's copy
   of `spawn` keeps its conversations in memory under the child's own id, as
   it always has, so two children started in the background may each hand
   the same remote agent a task, and the parent a third. A host that keys a
   run's conversations (ADR-0035) keys the run's; children are never bound
   to the parent's key. This is the rule iBot needed and the rule that was
   already there.
7. **mentra owns the primitive.** `ToolContext::deferred` and `Runtime::
   deferred` hand out a `Deferred` that reaches an agent after the call has
   returned, with a `Context`, a `DelegationResult` or a `SubagentFinished`
   applied at the next round boundary; an item that lands after the model's
   last message runs one more round. This is `background_run`'s notification
   path, generalized for anything a tool starts and does not wait for, and it
   is mentra's because nothing in it is basis's convention (Bet 6).

## Consequences

- A fan-out is `spawn` three times with `background: true` and, if the model
  wants them sooner than they come, one `wait`. iBot's `fan_out` and nous's
  facet orchestrator are host code that this replaces for the in-process
  case; the host keeps its approver, its tools and its policy, because the
  children are minted exactly as blocking children are.
- `SpawnMode` gains `Wait`, so a host matching it exhaustively needs an arm;
  the ACP adapter reads a wait as a delegation. The preview gains
  `background` and `wait`/`any` keys on the calls that said them, so every
  rule written against the four keys still matches.
- `spawn`'s schema requires `input` only on a runtime with no agent ledger —
  a host driving mentra directly, which keeps no background work and is
  offered none of this. Everywhere else `input` or `wait` is required, and
  the description says which goes with which.
- A background child's results are delivered as `<delegation-result
  agent="…" id="…" status="…">`, the blocking frame with the handle added,
  so the model can tell three answers apart. A remote agent's arrives as a
  plain note with the same words the blocking call would have returned,
  since a remote task writes no delegation entry on either path; its
  returned files are written into the workspace inbox directly, the path
  being basis's own.
- Width is a knob a host sets with the token budget it sets: a wider fan-out
  overshoots by more.
- Not done: a per-wait timeout (the parent's deadline bounds every wait); a
  background *command* (a task may run one); delivering a child's progress
  to the parent while it runs. Each is a proposal when a host asks.
