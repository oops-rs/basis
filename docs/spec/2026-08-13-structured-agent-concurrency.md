# Structured Agent Concurrency

Status: approved  
Owner: basis  
Date: 2026-08-13

## Problem

basis needs one simple command surface for starting work, communicating with an
agent, and waiting for a result. The current disposable delegation path is
bounded, but it does not provide a durable handle, an attached process model,
or a protocol for agent-to-agent communication.

The dangerous failure mode is a logical wait cycle: an agent waits for a child
while that child waits for the parent, or a completion signal is blocked behind
the event stream it is supposed to close. Threads, processes, and async syntax
do not remove that risk; ownership, cancellation, and wait ordering must be
explicit.

## Target users

- Humans and scripts starting one-shot agent work from a repository.
- Agent runtimes delegating bounded work to child agents.
- Hosts embedding `basis` and needing observable, cancellable child runs.

## Objectives

- Make `basis <PROMPT>` the canonical shorthand for `basis spawn <PROMPT>`.
- Make protocol serving explicit through `basis serve --acp` or
  `basis serve --bridge`.
- Make human-readable CLI results and usage errors end with one concise
  `next:` hint; structured lifecycle results will carry the same action as
  metadata rather than requiring documentation lookup.
- Give every attached child a structured owner, handle, deadline, and
  cancellation path.
- Keep communication one-way by default, with one durable correlated reply for
  each accepted message, and make terminal results independent from
  progress/event delivery.
- Keep inboxes, replies, waits, and event history bounded so a durable task
  cannot grow without limit.
- Keep an attached parent scope open until its children settle, while making
  detached roots explicitly independent.
- Preserve a small CLI and a generic core with no task-specific vocabulary.

## Non-goals

- Unrestricted peer-to-peer synchronous protocols or messages after a task has
  reached its terminal state. The first service supports one correlated reply
  for each message accepted while a task is running.
- A new wire protocol in `basis`; transports remain in adapters or the
  binary.
- A TUI, process sandbox, or replacement for Mentra's agent loop.
- Making detached work implicitly inherit a parent's lifetime.

## Constraints

- Rust edition 2024, MSRV 1.88, and the existing three-crate layering.
- `basis` carries generic lifecycle types only: no ACP, websocket, or TTY
  dependencies.
- Parent cancellation and deadlines propagate to attached descendants.
- A parent's own completion is pending while attached descendants are still
  non-terminal; successful parents leave those children running, while failed
  or cancelled parents request downward cancellation.
- No await may hold an agent/session/resource lock or prevent the supervisor
  from processing control messages.
- A detached root has an explicit independent lifetime and a durable terminal
  state.
- Every blocking lifecycle request has a finite deadline (30 minutes by
  default, capped at seven days), and its live wait edge is released when the
  request finishes, times out, or is cancelled.

## Acceptance criteria

- [x] `basis <PROMPT>` and `basis spawn <PROMPT>` normalize to the same command.
- [x] `basis serve --acp` starts ACP; `basis serve --bridge` starts the websocket
      bridge; bare `basis` does not start a server.
- [x] Human-readable usage/errors provide one valid `next:` action without
      changing the existing JSONL run bookends.
- [x] A child handle has exactly one terminal state and can be waited on more
      than once without rerunning work.
- [x] `send` returns a durable message ID; `ask` and `send --await` wait for
      that message's correlated reply, and `wait --message` retries the same
      reply without rerunning the task.
- [x] Distinct accepted messages retain distinct bounded replies, even while
      the target task remains alive for later turns.
- [x] Cancelling a parent cancels attached descendants and settles their
      waiters.
- [x] A parent that finishes its own work remains pending until attached
      descendants settle; success leaves children running, while failure or
      cancellation propagates downward.
- [x] Dropping or cancelling a wait does not lose a completed child result.
- [x] `spawn --await`, `send --await`, `ask`, `wait`, and `watch` acquire a
      counted live caller-to-target wait lease; static ownership checks and a
      dynamic graph traversal reject an edge that would close a cycle.
- [x] For an unresolved wait, a parent/ancestor, self, or same-tree peer edge
      is rejected before work is started or a message is enqueued; completed
      terminal/reply snapshots are reads, and enqueue-only `send` remains safe
      for parent-facing updates.
- [x] `wait` observes terminal state, `wait --message` observes one message's
      reply, `watch` observes bounded progress/events, and `cancel` requests
      downward cancellation without acquiring a wait edge.
- [x] A saturated progress/event path cannot prevent terminal completion.
- [x] Detached work is visibly independent and has its own finite deadline and
      cancel policy.

## Assumptions

- The accepted default is structured ownership: attached children are
  descendants, while `--detached` creates a new root.
- `send` is enqueue-only by default and targets a running one-session task.
  Every accepted message has one durable ID and one bounded reply. `ask` is
  the explicit enqueue-and-await spelling; `send --await` has the same
  correlated message semantics.
- Parent-facing requests are delivered through an inbox and handled at a
  later model boundary rather than by synchronously blocking the parent turn.
- A blocking request owns a counted in-memory wait lease. The lease is not
  journaled, so a daemon restart cannot preserve a wait whose client no longer
  exists.

## Open questions

- None for the shipped first slice. Post-terminal multi-turn agents and an
  unrestricted peer-to-peer request/reply protocol require a separate
  decision.

## Shipped limits / follow-ups

- The first service uses loopback TCP with a private bearer descriptor and a
  bounded JSON journal. It is local-process coordination, not a remote RPC
  service; the capability is required even on loopback.
- `send` targets a running one-session task and is consumed at a model-turn
  boundary. A task accepts at most 16 messages over its lifetime; message
  bodies are capped at 256 KiB, task results at 1 MiB, and inbox body/reply
  previews at 4 KiB with truncation metadata.
- `ask` and `send --await` wait for the exact message reply. A client timeout
  or disconnect leaves the task and message running; `wait --message` can
  retrieve the durable reply later. If the task terminates before a reply,
  the message wait returns the tagged terminal outcome.
- The live wait graph is local to one daemon. It combines static ownership
  rules (no self, ancestor, or same-tree peer waits) with dynamic cycle
  detection for independent roots. Edges are counted leases, released on
  success, error, timeout, or client cancellation, and are not persisted.
- `wait` waits for terminal state, `watch` returns bounded/replayable progress
  and terminal snapshots (including on a watch timeout), and `cancel` is a
  downward control request. An attached task caller may cancel itself or
  descendants, not ancestors or peers; an external capability holder may
  request cancellation of any task in that service. A parent stores a pending
  terminal result while attached children run; success keeps them alive,
  failure/cancellation asks them to stop, and the parent becomes terminal only
  after the attached children settle. Detached roots do not hold or inherit
  the parent scope.
- Durable multi-turn agents after terminal completion and unrestricted
  cross-service/peer protocols remain deferred.

## Lifecycle operation scope

| Operation | Durable effect | Wait-graph behavior |
|---|---|---|
| `send` | Enqueue one message and return its ID | No wait edge; safe for a parent-facing update |
| `ask` / `send --await` | Enqueue one message and return its correlated reply | One caller-to-target lease while the reply is unresolved |
| `wait <ID>` | Read the task's repeatable terminal result | One lease while the task is non-terminal |
| `wait <ID> --message <MID>` | Read/retry one message's reply | One lease only when that reply is unresolved |
| `watch <ID>` | Read bounded events and a terminal snapshot | One lease for the live watch request |
| `cancel <ID>` | Mark the target tree for downward cancellation | No wait edge; task callers are self/descendant scoped, external capability holders are unrestricted |
| `inbox [ID]` | Read bounded message/reply summaries | No wait edge |

## References

- [ADR-0010 — The crate is the workflow surface](../adr/0010-the-crate-is-the-workflow-surface.md)
- [ADR-0011 — Layered crates](../adr/0011-layered-crates.md)
- [ADR-0015 — CLI grammar](../adr/0015-cli-grammar.md)
- [ADR-0016 — One delegation surface](../adr/0016-one-delegation-surface.md)
