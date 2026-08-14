# Runtime and Filesystem Coordination

Status: approved
Owner: lan
Date: 2026-08-15
ADRs: [`0018`](../adr/0018-the-runtime-owns-the-process.md) ·
[`0019`](../adr/0019-the-filesystem-is-the-coordination-surface.md)

## Problem

`Workspace` conflates process-scoped infrastructure (mentra's runtime,
provider/credential resolution, store policy, host interceptors) with
repository-scoped discovery, so a host opening N workspaces — including
`lan-acp`, which builds a runtime per session — pays the process costs N
times, and the host scope the interception chain already orders by has no
type.

Separately, ADR-0017's structured concurrency runs on a hidden per-workspace
daemon whose irreducible contribution is unattended execution, live
supervision, and single-writer journal discipline — everything else it holds
is already files. The daemon's lifecycle machinery (spawn-on-demand,
replacement, stale wait edges, recovery handles) is where recent defect work
concentrates, and it maintains a second durability layer beside mentra's
store.

One migration, sequenced: the runtime split first, because the files design
assumes the runtime owns the data-directory policy that lets any process
resume any agent.

## Target users

- Rust hosts embedding `lan-core` across one or many repositories.
- `lan-acp` serving many editor sessions from one process.
- Humans and scripts driving `spawn`/`send`/`ask`/`wait`/`cancel`/`watch`
  from a shell, including under `nohup`/tmux/systemd/CI.
- Operators who require that no resident lan process outlive an invocation.

## Objectives

### E1 — the `Runtime` split (ADR-0018)

- `Runtime` + `RuntimeBuilder` own mentra's runtime, provider/credential/
  base-URL and model policy, history store policy, and host interceptors;
  `with_api_key`, `with_provider`, `with_base_url`, `with_store_dir`,
  `with_ephemeral_history`, and `with_interceptor` move there.
- `Workspace` keeps discovery (context, skills, templates, hooks,
  `.mcp.json`) and borrows its `Runtime` through an `Arc`; MCP connections
  stay workspace-owned; the resolved model stays a workspace fact.
- `Workspace::open(path)` is preserved verbatim as sugar minting a private
  default runtime.
- `workspace.runtime()` is renamed `mentra_runtime()`; lan's `Runtime`
  re-exposes mentra's so the "lan does not hide mentra" bargain survives.
- `lan-acp` holds one `Runtime` per server process and one `Workspace` per
  distinct `cwd`; the runtime-per-session `SessionSource` shape is retired.

### E2 — files as the coordination surface (ADR-0019)

- One global data directory, workspace-keyed: `LAN_DATA_DIR`, else the XDG
  data home. Task metadata and the mentra store live there; the repository's
  `.lan/` remains configuration only.
- Attach protocol: take the agent's `fs2` lock (one writer, ever), resume
  from the last committed turn, checkpoint at turn boundaries. `spawn`,
  `wait`, `ask`, and `send --await` attach; concurrent attachers serialize.
- `send` appends to the agent's inbox file, consumed at the next model-turn
  boundary; ADR-0017's bounds (16 messages lifetime, 4 KiB summaries) are
  unchanged.
- `cancel` writes a marker honored at the next boundary; `watch` tails the
  agent's event JSONL; `wait` observes the terminal record or attaches to
  produce it.
- The terminal record is written atomically as the executor's last act and
  is the completion signal. An agent is resumable iff no terminal record
  exists; terminal means immutable and repeatably observable; a follow-up
  conversation mints a new task on the same conversation. A parent may not
  write its terminal record while an attached child lacks one.
- The daemon is deleted: `registry.rs`, `protocol.rs`, the service actor,
  and the wait graph. Deadlines bound wait cycles; the CLI grammar, exit
  codes, and `next:` hints are unchanged.
- `README.md` and `docs/containerization.md` state the liveness contract
  plainly: an agent advances only while attached; backgrounding belongs to
  the OS.

## Non-goals

- No keepalive, scheduler, or resident process of any kind.
- No instant cancellation; a hung tool call is ended by the deadline.
- No effect rollback: a checkpoint restores state, never effects, and a
  re-driven turn may repeat tool side effects.
- No orchestration, agent registry, or fleet manager in `lan-core`
  (ADR-0010's line holds).
- No cross-machine coordination; the data directory is one machine's. Remote
  is a different design with a different ADR.

## Constraints

- E1's breaking rename lands before any crate is published; after first
  release the rename is off the table.
- mentra's store remains the sole source of conversational truth; task
  metadata files never duplicate what it holds.
- Every uncertainty resolves toward *resumable*: a crash anywhere before the
  terminal record leaves an agent a later attach can finish.
- Atomic writes and advisory locks must hold on Linux, macOS, and Windows —
  the three CI platforms. Where Windows file semantics differ (tailing an
  open journal, breaking a stale lock), the difference is handled in code,
  not documented away.
- Old registry state under `XDG_RUNTIME_DIR` is not migrated: runtime
  directories are ephemeral by platform contract, and conversations were
  always mentra's. The removal is documented in the E2 change notes.

## Acceptance criteria

- `Workspace::open` callers — the examples and doctests — compile unchanged
  after E1 apart from the `mentra_runtime()` rename.
- An `lan-acp` server holding two sessions on one `cwd` performs one provider
  resolution and holds one store handle (observable in a test via the
  data-directory probe).
- `kill -9` of an attached executor mid-turn leaves no terminal record; a
  later attach resumes from the last committed turn and completes; `wait`
  then observes the same terminal result repeatedly.
- Two concurrent `send --await` on one agent serialize on the lock; both
  receive their correlated replies.
- `cancel` on a detached agent is honored at that agent's next attached
  turn boundary; a cycle of two waiting processes ends by deadline, exit 3.
- A parent cannot reach terminal while an attached child lacks a terminal
  record, verified under process kill between the two writes.
- After any completed CLI invocation, no lan process remains (checked in an
  integration test, all three platforms).
- `cargo test --workspace` green on Linux, macOS, Windows; clippy at
  `-D warnings`; the Phase D data-directory probe stays zero against the
  *new* global directory's expected paths.

## Assumptions

- `fs2` advisory locking is sufficient for same-machine mutual exclusion;
  nothing but lan cooperates on these files.
- mentra's store recovery (`b1a83de`) makes resume-after-crash safe at the
  conversation layer; this spec adds only the task-metadata layer above it.
- The bounded-journal size cap from the daemon design carries over to the
  per-agent metadata files unchanged.

## Open questions

- Stale-lock breaking: PID liveness is racy under PID reuse; decide between
  PID+start-time fingerprint and lock-file lease timestamps before E2 code
  review.
- `watch` tail semantics on Windows while an executor holds the journal
  open — share-mode flags or a segmented event file.
- Whether a detached root needs anything beyond an ordinary agent directory
  with no parent edge (expected: no).

## References

- [`../adr/0018-the-runtime-owns-the-process.md`](../adr/0018-the-runtime-owns-the-process.md)
- [`../adr/0019-the-filesystem-is-the-coordination-surface.md`](../adr/0019-the-filesystem-is-the-coordination-surface.md)
- [`../adr/0017-structured-agent-concurrency.md`](../adr/0017-structured-agent-concurrency.md)
  and its spec, whose semantic layer this design keeps.
- [`../adr/0014-watch-retired-runs-are-boundable.md`](../adr/0014-watch-retired-runs-are-boundable.md)
  — the deadline that bounds what the wait graph used to reject.
