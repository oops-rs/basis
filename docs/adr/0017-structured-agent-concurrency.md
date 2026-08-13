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

1. `send` is enqueue-only by default. Each accepted message gets a durable,
   opaque message ID and a bounded inbox record. `send --await` waits for the
   reply produced by that message's turn; `ask` is the explicit spelling that
   combines enqueue and the correlated reply wait. It never waits for an
   unrelated task termination. `wait <ID> --message <MID>` retries the same
   reply after a client timeout or disconnect and never reruns the task. If a
   task terminates before producing a reply, the message wait returns that
   terminal outcome tagged with the message ID.
2. A blocking lifecycle operation (`spawn --await`, `send --await`, `ask`,
   `wait`, or `watch`) acquires a live caller-to-target wait edge. An attached
   caller may wait on a descendant, but not itself, an ancestor, or a peer in
   the same ownership tree. A task in another ownership tree is eligible only
   when the reverse path is absent.
3. The supervisor checks the complete live wait-for graph before it starts the
   worker or enqueues the awaited message. An edge that would close a cycle is
   rejected. Edges are counted leases and are released on success, error,
   timeout, or waiter cancellation; they are intentionally not persisted,
   because a service restart also ends the request handlers that owned them.
4. Terminal completion uses a control path separate from progress/events.
   Event backpressure must never prevent a job from reaching a terminal state.
5. Every wait has a finite deadline and is cancellation-safe. A cancelled
   waiter does not lose a result; an abandoned owner settles descendants as
   cancelled or orphaned according to their ownership mode.
6. Enqueue-only `send` acquires no wait edge, so a child can safely leave an
   ancestor a message for its next round. Inbox bodies and replies are bounded
   summaries (with truncation metadata), and the journal has a fixed message
   capacity. Post-terminal messages and unrestricted peer-to-peer protocols
   remain out of scope.

### Parent scope

An attached task's own model work may finish before its descendants. The
supervisor records that completion as pending, keeps the parent externally
`running`, and publishes its terminal state only after every attached child
settles. Successful parent work does not cancel children; failed, cancelled, or
orphaned work requests downward cancellation. Once a worker has finished, its
scope is closed to new messages and children. `--detached` removes the ownership
edge and gives the new root an independent deadline and cancellation lifetime.

### CLI

```text
lan <PROMPT>                  # shorthand for lan spawn <PROMPT>
lan spawn <PROMPT>            # one-shot work
lan send ...                  # enqueue a message (or --await its reply)
lan ask <ID> <QUESTION>       # enqueue and await the correlated reply
lan wait <ID>                 # wait for terminal state
lan wait <ID> --message <MID> # retry one message's reply
lan cancel <ID>               # request cancellation
lan watch <ID>                # observe progress/events
lan inbox [ID]                # bounded message/reply summaries
lan serve --acp               # ACP over stdio
lan serve --bridge            # ACP over websocket
lan fingerprint               # workspace hash
```

Bare `lan` is usage output, not a server. ACP is an explicit adapter mode, so
an editor or host must say which transport it is starting. Existing `run` is
retained as a compatibility alias for `spawn` during migration.

Every human-readable result that needs follow-up ends with one concise
`next:` line naming valid commands. This is an affordance for agents consuming
CLI output, not a second documentation system. Lifecycle results carry the
same hint as metadata, while the attended run JSONL bookends remain unchanged.

## Consequences

- The common path is short without making a long-lived server accidental.
- Attached work has a provable ownership tree; detached work is visibly a
  separate lifecycle.
- A supervisor is required even for an in-process implementation. An async
  function that simply awaits a child is not sufficient.
- Cancellation is cooperative for model and async work, so shell/process work
  additionally requires kill-and-reap handling in its owning supervisor.
- The live graph prevents logical wait cycles among requests known to one
  workspace service. It does not claim that arbitrary model-generated
  protocols, external processes, or cross-service dependencies are deadlock
  free.
- The local service deliberately does not promise post-terminal multi-turn
  agents or unrestricted peer-to-peer request/reply. Its first communication
  contract is intentionally narrower: one accepted message has one durable,
  bounded correlated reply, retrievable by message ID.
