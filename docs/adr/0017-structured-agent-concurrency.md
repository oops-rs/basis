# 0017 — Structured agent concurrency

> Status: Accepted · 2026-08-13  
> Extends [`0010-the-crate-is-the-workflow-surface.md`](0010-the-crate-is-the-workflow-surface.md),
> [`0011-layered-crates.md`](0011-layered-crates.md), and
> [`0015-cli-grammar.md`](0015-cli-grammar.md).  
> Spec: [`docs/spec/2026-08-13-structured-agent-concurrency.md`](../spec/2026-08-13-structured-agent-concurrency.md)

## Context

LAN is becoming a small agent process rather than only a one-shot wrapper. A
parent must be able to start work, observe it, cancel it, and receive a result
without making the parent model turn the only owner of the child lifecycle.

The fundamental hazard is a wait-for cycle. A parent that awaits a child while
holding the parent actor's turn cannot answer a child request that awaits the
parent. A full progress queue can create the same cycle if completion is sent
through a queue whose consumer is waiting for completion. No choice between a
thread, a Tokio task, or a subprocess changes that graph.

Swift's structured task tree gives the useful ownership rule: attached child
work is bounded by the parent scope and cancellation flows downward. Its
reentrant actors give the useful supervisor rule: control messages must still
be processed while a child is suspended. Rust and Tokio provide handles,
cancel-safe waits, and explicit abort/detach operations, but do not provide
logical deadlock freedom. LAN needs the combination, expressed in its own
generic lifecycle types.

## Decision

### Ownership and execution

1. An attached child is a descendant of one parent. The parent owns the
   child's cancellation and deadline, and the child cannot synchronously await
   an ancestor.
2. `spawn` returns a handle immediately. `wait` observes the child's terminal
   state; it does not execute the child or rerun it.
3. A supervisor owns the lifecycle registry and remains able to process spawn,
   send, cancel, and completion events while callers wait.
4. State transitions happen synchronously inside the supervisor. Long work is
   spawned before the command handler returns; completion comes back as an
   event. No agent/session/resource lock is held across an await.
5. `--detached` creates a new root. It does not inherit parent cancellation,
   deadline, or model context implicitly; its lifetime is explicit and its
   terminal state is durable.

### Communication

1. `send` is enqueue-only and returns an acknowledgement or an error.
2. `send --await` is initially restricted to a child or an independent actor
   whose supervisor can answer without requiring the caller's current model
   turn. Parent-facing requests enter an inbox and are handled at a later
   round boundary.
3. Terminal completion uses a control path separate from progress/events.
   Event backpressure must never prevent a job from reaching a terminal state.
4. Every wait has a finite deadline and is cancellation-safe. A cancelled
   waiter does not lose a result; an abandoned owner settles descendants as
   cancelled or orphaned according to their ownership mode.
5. Arbitrary peer request/reply is deferred until a wait-for graph can reject
   an edge that would close a cycle.

### CLI

```text
lan <PROMPT>                  # shorthand for lan spawn <PROMPT>
lan spawn <PROMPT>            # one-shot work
lan send ...                  # enqueue a message
lan wait <ID>                 # wait for terminal state
lan cancel <ID>               # request cancellation
lan watch <ID>                # observe progress/events
lan serve --acp               # ACP over stdio
lan serve --bridge            # ACP over websocket
lan fingerprint               # workspace hash
```

Bare `lan` is usage output, not a server. ACP is an explicit adapter mode, so
an editor or host must say which transport it is starting. Existing `run` is
retained as a compatibility alias for `spawn` during migration.

## Consequences

- The common path is short without making a long-lived server accidental.
- Attached work has a provable ownership tree; detached work is visibly a
  separate lifecycle.
- A supervisor is required even for an in-process implementation. An async
  function that simply awaits a child is not sufficient.
- Cancellation is cooperative for model and async work, so shell/process work
  additionally requires kill-and-reap handling in its owning supervisor.
- The first implementation deliberately does not promise arbitrary peer
  request/reply. That is a safety boundary, not missing convenience.
