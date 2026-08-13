# Structured agent concurrency implementation plan

Spec: [docs/spec/2026-08-13-structured-agent-concurrency.md](../spec/2026-08-13-structured-agent-concurrency.md)

## Implementation plan

1. **Slice 1: lock the design** — files: the spec, ADR, and this plan.
   Acceptance: the accepted ownership, cancellation, communication, and CLI
   invariants are recorded with explicit non-goals.
2. **Slice 2: make the CLI grammar explicit** — files: `lan/src/cli.rs`,
   `lan/src/main.rs`, `lan/src/serve.rs`, `lan/src/shorthand.rs`, their tests,
   `README.md`, `docs/REDESIGN.md`. Acceptance: `spawn` is the canonical
   one-shot spelling, `run` remains an alias, and only `serve --acp` or
   `serve --bridge` starts a server.
3. **Slice 3: introduce the lifecycle state machine** — files:
   `lan-core/src/lifecycle.rs`, `lan-core/src/lib.rs`, and focused tests.
   Acceptance: handles have single terminal transitions, cancellation is
   downward, waits are repeatable and cancel-safe, and cycles are rejected by
   the initial descendant-only policy.
4. **Slice 4: wire in-process spawn/wait** — files: `lan-core` workspace/run
   integration and `lan` command handlers. Acceptance: an attached child can
   be spawned, awaited, cancelled, and observed without holding the parent
   turn or losing completion.
5. **Slice 5: add IPC and messaging** — files: a transport adapter and the
   top-level `send`, `wait`, `cancel`, `watch`, and `inbox` commands.
   Acceptance: a capability-scoped local connection survives CLI clients and
   preserves the same lifecycle invariants across processes.

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

- [x] Slice 1 — lock the design (`b2ec36d`).
- [x] Slice 2 — make the CLI grammar explicit (`f48c6de`).
- [x] Slice 3 — introduce the lifecycle state machine (`df43494`).
- [x] Slice 4 — wire in-process spawn/wait.
- [x] Slice 5 — add IPC and messaging (local daemon, capability descriptor,
  durable journal, `send`/`wait`/`cancel`/`watch`/`inbox`, detached roots).
