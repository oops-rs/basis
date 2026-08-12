# Structured Agent Concurrency

Status: approved  
Owner: lan  
Date: 2026-08-13

## Problem

LAN needs one simple command surface for starting work, communicating with an
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
- Hosts embedding `lan-core` and needing observable, cancellable child runs.

## Objectives

- Make `lan <PROMPT>` the canonical shorthand for `lan spawn <PROMPT>`.
- Make protocol serving explicit through `lan serve --acp` or
  `lan serve --bridge`.
- Make human-readable CLI results and usage errors end with one concise
  `next:` hint; structured lifecycle results will carry the same action as
  metadata rather than requiring documentation lookup.
- Give every attached child a structured owner, handle, deadline, and
  cancellation path.
- Keep communication one-way by default and make terminal results independent
  from progress/event delivery.
- Preserve a small CLI and a generic core with no task-specific vocabulary.

## Non-goals

- Arbitrary peer-to-peer synchronous request/reply in the first implementation.
- A new wire protocol in `lan-core`; transports remain in adapters or the
  binary.
- A TUI, process sandbox, or replacement for Mentra's agent loop.
- Making detached work implicitly inherit a parent's lifetime.

## Constraints

- Rust edition 2024, MSRV 1.88, and the existing three-crate layering.
- `lan-core` carries generic lifecycle types only: no ACP, websocket, or TTY
  dependencies.
- Parent cancellation and deadlines propagate to attached descendants.
- No await may hold an agent/session/resource lock or prevent the supervisor
  from processing control messages.
- A detached root has an explicit independent lifetime and a durable terminal
  state.

## Acceptance criteria

- [ ] `lan <PROMPT>` and `lan spawn <PROMPT>` normalize to the same command.
- [ ] `lan serve --acp` starts ACP; `lan serve --bridge` starts the websocket
      bridge; bare `lan` does not start a server.
- [ ] Human-readable usage/errors provide one valid `next:` action without
      changing the existing JSONL run bookends.
- [ ] A child handle has exactly one terminal state and can be waited on more
      than once without rerunning work.
- [ ] Cancelling a parent cancels attached descendants and settles their
      waiters.
- [ ] Dropping or cancelling a wait does not lose a completed child result.
- [ ] A parent/ancestor wait cycle is rejected or expires within a finite
      deadline; it never blocks forever.
- [ ] A saturated progress/event path cannot prevent terminal completion.
- [ ] Detached work is visibly independent and has its own deadline/cancel
      policy.

## Assumptions

- The accepted default is structured ownership: attached children are
  descendants, while `--detached` creates a new root.
- `send` is enqueue-only. `send --await` initially targets a child or an
  independent actor whose supervisor can answer without the caller's current
  model turn.
- Parent-facing requests are delivered through an inbox and handled at a
  later model boundary rather than by synchronously blocking the parent turn.

## Open questions

- None for the first implementation slice. Arbitrary peer request/reply will
  require a separate decision after the wait-graph tests exist.

## Out of scope / follow-ups

- Durable cross-process IPC and capability discovery after the in-process
  lifecycle foundation lands.
- `lan send`, `lan wait`, `lan cancel`, `lan watch`, and `lan inbox` CLI wiring
  over the lifecycle service.
- Dynamic wait-graph cycle detection for unrestricted peer requests.

## References

- [ADR-0010 — The crate is the workflow surface](../adr/0010-the-crate-is-the-workflow-surface.md)
- [ADR-0011 — Layered crates](../adr/0011-layered-crates.md)
- [ADR-0015 — CLI grammar](../adr/0015-cli-grammar.md)
- [ADR-0016 — One delegation surface](../adr/0016-one-delegation-surface.md)
