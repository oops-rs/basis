# 0022 — The task layer is a crate

> Status: Accepted · 2026-08-25
> Extends [`0017-structured-agent-concurrency.md`](0017-structured-agent-concurrency.md)
> (the rules) and [`0019-the-filesystem-is-the-coordination-surface.md`](0019-the-filesystem-is-the-coordination-surface.md)
> (the substrate they run over); applies
> [`0011-layered-crates.md`](0011-layered-crates.md) one crate further.

## Context

ADR-0017 decided what a durable task is: a handle that outlives the process
that minted it, a bounded inbox with correlated replies, downward-only
cancellation, repeatable terminal observation, a wait-edge policy that admits
no cycle. ADR-0019 decided what it runs on: one global data directory, one
`fs2` attach lock per task, a checkpoint at every turn boundary, an atomically
written terminal record whose existence is the whole completion signal, no
resident process of any kind. Both are real decisions, fully implemented,
fully tested — and reachable by exactly one thing: the `basis` binary. Every
type and every function that carries them out was `pub(crate)` inside
`basis-cli/src/local`.

That placement was never examined on its own terms; it was where ADR-0019's
daemon retirement happened to land, because the daemon it replaced lived in
the binary too. But nothing about ADR-0017's rules is a CLI concern. They name
no protocol, no transport, no terminal — the exact three things ADR-0011
draws basis's own layering on. `basis` (the SDK) already carries the
run-lifecycle half of an agent harness with no opinion about how a host
reaches it; the task-lifecycle half carried an opinion nobody had stated:
*reach it through the binary, or not at all.*

That opinion has a cost with a name. ADR-0011 exists because two independent
implementations of one protocol drift, silently, until a client notices; it
is why `basis-acp` exists rather than a second ACP server bolted onto
`basis-cli`. A Rust host that wanted ADR-0017's durable lifecycle — spawn,
resume, correlated messaging, bounded waits, downward cancellation — for its
own agents, without a person at a terminal, had exactly the trap ADR-0011
was written to close for ACP: reimplement the state machine, or shell out to
`basis` and parse `stdout`. No host has hit this yet only because no second
host has asked; that is not evidence the trap is not there, it is evidence
nobody has fallen into it yet.

## Decision

**The durable task layer is its own crate, `basis-tasks`, over `basis` alone.
`basis-cli` is its first client, not its implementation.**

1. **The split point is CLI opinion, not file boundary.** Everything ADR-0017
   and ADR-0019 built that carries no such opinion — data-directory
   resolution, the `meta.json`/`inbox.json`/`events.jsonl`/`terminal.json`
   formats, the attach protocol, the event journal, the wait-edge policy, and
   the workspace scan a listing and a continuation both resolve against —
   moves to `basis-tasks`. What stays in `basis-cli` is exactly the CLI's:
   the `next:`/exit-code mapping ADR-0015 owns, plain-text rendering, and the
   grammar itself.

2. **The public surface is `Tasks`.** Opened per workspace — or at an
   explicit data-directory root, for a host that manages its own location
   rather than `BASIS_DATA_DIR` — with `spawn` (mints and returns
   immediately; nothing durable ever blocks on a model), `send`/`ask`,
   `wait`/`wait_message`, `cancel`, `watch` (a pull-based cursor over the
   event journal, never a resident loop the crate runs for you), `list`, and
   the small read-only accessors (`terminal`, `is_attached`, `inbox`,
   `workspace_of`) a caller composes its own reporting from. `TaskHandle`
   replaces the bare `String` the CLI passed around, validated once at the
   boundary; `RunSpec` is the durable spawn request, built the way
   `basis::RunSpec` already is (`new` plus `with_*`, returning new values).

3. **Two seams the CLI never had to name become the crate's own.** A library
   has no terminal to ask at — the reasoning ADR-0011 already applies to
   `TerminalApprover` staying in the binary — so `Approve::Prompt` is
   answered through a `PromptHost` a caller supplies, not a direct
   dependency on stdin. And showing a task's progress live is a property of
   who asked, not of the task: `LiveSink` is a trait a caller plugs in per
   call, in place of the CLI's `Live` renderer being reachable from inside
   the executor at all.

4. **ADR-0011's layering grows a fourth crate, applied rather than
   revisited.** `basis-tasks` depends on `basis` alone — no protocol, no
   transport, no TTY, the same three exclusions `basis` itself holds to —
   and sits between `basis` and `basis-cli` by dependency weight, the same
   axis that already orders `basis-acp`. One version across all four, still
   (the workspace comment this ADR's implementation updates says so
   directly).

5. **Every cap and every rule carries over unchanged.** 16 messages, 4 KiB
   bounded summaries, the finite default deadline every unattended task
   gets, downward-only cancellation, the wait-edge policy admitting a
   descendant or an independent root and refusing an ancestor or a peer.
   This is an extraction, not a redesign — nothing ADR-0017 or ADR-0019
   decided is reopened here.

6. **The `BASIS_TASK_ID`/`BASIS_DATA_DIR`/`BASIS_PARENT_TASK_ID` environment
   convention is published, not merely relied upon.** It was always the
   channel a task's own tool calls used to find their way back to the task
   that spawned them; it was never written down as a contract anyone else
   could build against. It is now declared in `basis-tasks`'s crate root,
   with `current_task()` reading the first variable back for any host that
   wants to know whether it is itself running as a task's tool call.

## Consequences

- A Rust host can depend on `basis-tasks` directly for a durable, resumable,
  multi-turn agent lifecycle without spawning `basis` and parsing its
  `stdout` — the trap this ADR's Context names, closed for this surface the
  way `basis-acp` already closed it for ACP.
- `basis-cli` carries one more crate in its dependency graph and loses direct
  dependencies it no longer needs at that layer (`fs2`, `uuid` as
  `basis-cli`-owned normal dependencies; `fs2` remains a dev-dependency for
  a test that locks a task file directly from outside the executor).
- `basis-tasks`'s error type carries no exit code or hint of its own —
  ADR-0015's mapping stays exactly where it was — except one fact promoted
  out of it: whether an error is over a reference that could never have
  resolved (a malformed handle, one from another workspace) versus an
  ordinary operational failure. A host mapping errors onto its own
  vocabulary reads that fact rather than parsing message text; `basis-cli`'s
  usage-vs-failed exit code split is the first caller of it.
- Nothing about the on-disk format changes, and nothing about ADR-0019's
  daemon-retirement precedent is revisited: no resident process, one attach
  lock, files as the coordination surface, all unchanged one crate up.
- `basis-tasks/tests/lifecycle.rs` is the acceptance case this ADR's own
  claim is checked against: spawn, ask, wait, and list a task from Rust,
  against a scripted provider endpoint, with no `basis` binary anywhere in
  the process tree.
