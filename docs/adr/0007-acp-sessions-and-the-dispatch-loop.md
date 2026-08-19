# 0007 — The ACP session is a mentra agent, and the dispatch loop is never blocked

> Status: Accepted · 2026-08-10
> Implements [`0002-acp-is-the-protocol.md`](0002-acp-is-the-protocol.md).

## Context

P2 built the ACP server. Two questions came up that ADR-0002 does not answer,
and that the code cannot express twice without contradicting itself.

**What is an ACP session id?** ACP hands the agent a `cwd` and expects a
`SessionId` back, then expects `session/load` to resume that conversation —
possibly in a later process. mentra offers two identifiers that could serve:
`Session::id()`, minted fresh by `create_session`, and `Session::agent_id()`,
the primary key of the persisted agent row. They are not interchangeable:
`Runtime::resume_session` takes the agent id, and the session id is a new
value every time a session is constructed, including on resume.

**Who may block?** The `agent-client-protocol` 2.0 handler closures run inside
the connection's dispatch loop and hold it until they return. Meanwhile basis's
`Approver` is called from the event-forwarding task while the turn is blocked
inside mentra waiting for an answer. Both facts are load-bearing, and getting
their relationship wrong does not produce an error — it produces a hang.

## Decision

**The ACP session id is mentra's persisted agent id.**

`session/new` returns `PreparedRun::agent_id()`. `session/load` passes it
straight to `run::resume`, which is `Runtime::resume_session`. basis persists no
mapping table of its own, because there is nothing to map: the protocol's
identifier and the runtime's identifier are the same string.

**A handler that needs the client never runs on the dispatch loop.**

`session/prompt` spawns immediately and carries its `Responder` into the
spawned task. Everything a turn does — streaming updates, asking permission,
answering — happens there. So do `session/load` and `session/resume`: they
read a session's transcript from behind the turn lock, and a turn holds that
lock while it waits for the client, so taking it from the loop is the same
deadlock by another route. `initialize`, `session/new`, `session/set_mode` and
`session/close` answer inline, because they touch the disk and the provider's
model list but never the client and never a lock a turn can be holding.

**`Approver::approve` is async**, and the permission round trip awaits rather
than blocks. The forwarding task is an ordinary async task; parking it parks
nothing else. A synchronous approver would have to block a runtime worker, and
tokio refuses that outright: *"Cannot block the current thread from within a
runtime."*

## Consequences

- `session/load` costs nothing to support and works across processes, because
  mentra already persists agents to SQLite. basis advertises `loadSession: true`
  honestly.
- A client that reconnects with an id from a previous run gets its conversation
  back, with the transcript mentra kept.
- The session id a client sees is stable across resume, which is what a client
  storing it in a workspace file needs. `Session::id()` is not, so it is not
  what basis exposes.
- Every implementation of `Approver` is async now, including `TerminalApprover`,
  whose stdin read moved to `spawn_blocking`. That is a breaking change to a
  public trait, taken in P2 rather than later because the alternative is a
  signature that cannot express the protocol's own permission flow.
- The rule "spawn before driving a turn" is invariant, not a preference. It is
  stated in `acp/server.rs`'s module docs and depended on by
  `acp/approver.rs`'s. `tests/acp/` covers it with a real client over
  `Channel::duplex()`, under a timeout, because the failure it guards against
  is silence rather than a wrong answer.
- One conversation runs one turn at a time: `session/prompt` holds an async
  lock on the run for the turn's duration. The cancellation token deliberately
  sits *outside* that lock — `session/cancel` arrives while the turn holds it,
  so a token stored inside would make cancel wait for the thing it cancels.
- The session's permission mode sits outside that lock for the same reason.
  ACP says `session/set_mode` may arrive "whether the Agent is idle or
  actively generating", and a switch that waited for the turn it was meant to
  govern would arrive too late to govern it.
