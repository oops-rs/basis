# Structured agent concurrency implementation plan

Spec: [docs/spec/2026-08-13-structured-agent-concurrency.md](../spec/2026-08-13-structured-agent-concurrency.md)

## Implementation plan

1. **Slice 1: lock the design** — files: the spec, ADR, and this plan.
   Acceptance: the accepted ownership, cancellation, communication, and CLI
   invariants are recorded with explicit non-goals.
2. **Slice 2: make the CLI grammar explicit** — files: `basis/src/cli.rs`,
   `basis/src/main.rs`, `basis/src/serve.rs`, `basis/src/shorthand.rs`, their tests,
   `README.md`, `docs/REDESIGN.md`. Acceptance: `spawn` is the canonical
   one-shot spelling, `run` remains an alias, and only `serve --acp` or
   `serve --bridge` starts a server.
3. **Slice 3: introduce the lifecycle state machine** — files:
   `basis-core/src/lifecycle.rs`, `basis-core/src/lib.rs`, and focused tests.
   Acceptance: handles have single terminal transitions, cancellation is
   downward, waits are repeatable and cancel-safe, and cycles are rejected by
   the initial descendant-only policy.
4. **Slice 4: wire in-process spawn/wait** — files: `basis-core` workspace/run
   integration and `basis` command handlers. Acceptance: an attached child can
   be spawned, awaited, cancelled, and observed without holding the parent
   turn or losing completion.
5. **Slice 5: add the durable local lifecycle service** — files: the binary's
   local transport, registry, journal, worker adapter, and the top-level
   lifecycle commands. This remained one dependency wave but landed in
   separately reviewable steps:

   - **5a: daemon and durable IPC** — a capability-scoped loopback service,
     bounded journal, and `send`/`wait`/`cancel`/`watch`/`inbox` survive the CLI
     process that created a task.
   - **5b: live wait graph** — every unresolved blocking operation owns a
     counted caller-to-target lease; static ownership rules and dynamic graph
     traversal reject cycles, and dropped clients release their leases.
   - **5c: attached parent scope** — a parent keeps its terminal result pending
     until attached children settle; success leaves them running, while
     failure or cancellation propagates downward. Detached roots remain
     independent.
   - **5d: correlated messaging** — `ask` and `send --await` wait for the exact
     accepted message's durable reply, while `wait --message` retries it after
     timeout or disconnect and `inbox` exposes bounded reply summaries.
   - **5e: acceptance and recovery tests** — focused unit and integration tests
     cover state transitions, durable recovery, IPC, cycle rejection, caller
     authority, parent settlement, and distinct per-message replies.

   Acceptance: the local connection preserves the lifecycle invariants across
   processes, every wait is bounded and cycle-checked, and terminal task state,
   correlated message replies, and advisory progress remain distinct.

### Overlap matrix

|   | S1 | S2 | S3 | S4 | S5 |
|---|----|----|----|----|----|
| S1 | — | docs only | ∅ | ∅ | ∅ |
| S2 |  | — | ∅ | shared CLI boundary | shared CLI boundary |
| S3 |  |  | — | shared lifecycle API | shared handle protocol |
| S4 |  |  |  | — | shared transport contract |
| S5 |  |  |  |  | — |

### Fan-out decision

- Candidates after collapse: one serial workflow.
- Passes fan-out gate: no; each later slice depends on the lifecycle and CLI
  contracts established by the previous slice.
- Shape: one worktree, serial slices, one commit per completed slice.
- Confirmation required: no; the user explicitly authorized implementation.
- Worktree: the repository checkout selected by the contributor.

## Progress

| Slice | Status | Shipped evidence |
|---|---|---|
| 1 — design | Shipped | Accepted spec, ADR-0017, and plan (`b2ec36d`) |
| 2 — explicit CLI grammar | Shipped | Canonical `spawn`, compatible `run`, explicit serve transports, and next hints (`f48c6de`) |
| 3 — lifecycle state machine | Shipped | Generic `basis-core` supervisor and focused transition tests (`df43494`) |
| 4 — in-process structured ownership | Shipped | Prepared runs execute under handles with repeatable waits and downward cancellation (`3db1317`) |
| 5a — local daemon and durable IPC | Shipped | Capability descriptor, bounded journal, lifecycle commands, and process-level integration tests (`a4539dc`); wakeup, registry ownership, and connection-bound hardening (`a1c8ed8`, `7f82e17`, `b1ebf0a`) |
| 5b — live wait graph and caller authority | Shipped | Counted wait leases, static ownership validation, dynamic cycle rejection, and downward-only task cancellation (`61c8d26`) |
| 5c — attached parent scope | Shipped | Pending terminal results, success/failure propagation rules, and detached-root isolation (`9d2179b`) |
| 5d — `ask` and correlated replies | Shipped | Durable per-message replies, `ask`, exact `send --await`, `wait --message`, and bounded inbox summaries (`89ee68a`) |
| 5e — acceptance and recovery tests | Shipped with each implementation slice | `basis-core/src/lifecycle.rs`, `basis-core/tests/lifecycle_run.rs`, `basis/tests/local_lifecycle.rs`, and colocated local-service/store/registry tests cover the accepted invariants without relying on a live remote CI run |
