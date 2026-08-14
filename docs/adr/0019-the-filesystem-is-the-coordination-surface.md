# 0019 — The filesystem is the coordination surface

> Status: Accepted · 2026-08-15
> Amends [`0017-structured-agent-concurrency.md`](0017-structured-agent-concurrency.md):
> its ownership and communication semantics survive; its execution substrate —
> the per-workspace daemon — is retired.
> Builds on [`0018-the-runtime-owns-the-process.md`](0018-the-runtime-owns-the-process.md);
> related: [`0014-watch-retired-runs-are-boundable.md`](0014-watch-retired-runs-are-boundable.md).
> Spec: [`docs/spec/2026-08-15-runtime-and-filesystem-coordination.md`](../spec/2026-08-15-runtime-and-filesystem-coordination.md)

## Context

ADR-0017 shipped structured concurrency on a hidden per-workspace daemon.
Strip that daemon to what it irreducibly provides and three things remain:
unattended execution (a task advances after the spawning shell exits), live
supervision (instant cancellation, the in-memory wait graph), and
single-writer discipline over the task journal. Everything else is already
files — the journal is JSON on disk with atomic writes, and conversations
are mentra's store, durable and resumable at turn boundaries.

The daemon's price is its own lifecycle: spawn-on-demand, replacement of a
crashed or idled service, socket-path correctness, stale wait edges, recovery
handles. The five most recent commits on `main` are all fixes to exactly this
machinery. It also keeps a second durability layer beside mentra's, and it
fails the identity check on all three arms: not a seam, not a convention
other agents speak, and invisible to embedders — so not cheaper embedding.

The doctrine already decided this shape once. lan ships no scheduler because
an interval belongs to whatever already runs things on your machine; the same
cut applies one layer down. Keeping a process alive is the OS's job.

## Decision

**The daemon is retired. An agent is a checkpoint on disk; execution belongs
to whichever process is attached; liveness belongs to the OS.**

- One global data directory (workspace-keyed; `LAN_DATA_DIR`, else the XDG
  data home) holds task metadata and the mentra store. The repository's
  `.lan/` remains configuration only. This also retires the wart of a store
  keyed by the *process's* current directory rather than the workspace.
- **Attach** is the primitive: take the agent's `fs2` file lock — one writer,
  ever — resume from the last committed turn, execute, checkpoint at turn
  boundaries. `spawn`, `wait`, `ask`, and `send --await` all attach; the lock
  serializes concurrent attachers.
- `send` appends to the agent's inbox file, consumed at the next model-turn
  boundary — the semantics ADR-0017 already chose. Its bounds (16 messages, 4
  KiB summaries) are unchanged.
- `cancel` writes a marker honored at the next boundary. `watch` tails the
  agent's event JSONL, which makes replay the default rather than a feature.
  `wait` observes the terminal record, or attaches and produces it.
- The **terminal record** is written atomically as the executor's last act,
  and it is the completion signal — there is no second flag to keep
  consistent with it. **An agent is resumable iff its terminal record does
  not exist.** Terminal means immutable and repeatably observable; further
  conversation mints a *new* task on the same underlying conversation, so
  results are forever while the dialogue continues. A parent may not write
  its terminal record while an attached child lacks one — the scope rule as
  one ordering constraint, with no supervisor to enforce it.
- The wait graph is deleted. A wait is a process observing a file; a cycle is
  two observers, and the finite deadline already bounds it. Cycles stop being
  a deadlock hazard the moment there is no supervisor to deadlock.
- Unattended execution is the OS's: `&`, `nohup`, tmux, `systemd-run`, CI.
  Documented, not engineered.

## What is given up, named plainly

- **No progress without an attached process.** "Always resumable" replaces
  "always running." A spawned-and-abandoned agent sits recoverable until
  something attaches.
- **Cancellation is boundary-granular**, not instant. A hung tool call is
  ended by the deadline, not by `cancel`.
- **A crash mid-turn loses the in-flight round**, and re-driving the turn may
  repeat its tool side effects. A checkpoint restores state, never effects —
  a shell command that ran, ran. The daemon had the same property; this ADR
  states it in bold instead of implying it.
- **Stale locks need liveness detection.** A lock whose holder died must be
  detected and broken (holder PID plus a liveness probe). This is the one
  racy edge the design keeps, and it is far smaller than the daemon's
  replacement machinery, which existed to solve the same problem plus its
  own.

## Consequences

- `lan/src/local/` shrinks from roughly 4,400 lines to an estimated 1,500:
  `registry.rs`, `protocol.rs`, and the service actor go, and with them the
  failure modes the recent commits were fixing — a stale wait edge cannot
  recur in code that no longer exists.
- One durability story: mentra's store for conversations, thin metadata
  files for tasks.
- ADR-0017's semantic layer survives intact — the ownership tree, bounded
  inboxes, one correlated reply per accepted message, detached roots, the
  `next:` hints, the CLI grammar and exit codes. The contract stays; the
  substrate under it changes.
- The runtime (ADR-0018) owns the data-directory policy, so any process
  holding a `Runtime` can resume any agent — which is the sentence this
  design exists to make true.
