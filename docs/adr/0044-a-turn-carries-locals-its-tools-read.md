# 0044 — A turn carries locals its tools read

> Status: Accepted · 2026-10-21
> Builds on [`0042-a-delegation-may-run-in-the-background.md`](0042-a-delegation-may-run-in-the-background.md)
> (whose `with_task_scope` covered task-locals for background children only)
> and mentra's `RunLocals` (mentra 0.33).

## Context

A host that serves many callers from one workspace has facts that belong
to one turn, not to the workspace: which caller's code workspace a tool
should read, which tenant, which trace. A native tool is registered once
per workspace (`WorkspaceBuilder::with_tool`) and closes over what the host
had at registration, so it could not tell two callers' turns apart.

What hosts did instead was each a workaround with a cost:

- **A workspace per caller**, each with tools closing over that caller's
  value. Two live opens of one directory may not register the same host
  tool name (the claim ledger cannot compare two closures), so each open
  needed a directory of its own, and every MCP connection reopened per
  turn.
- **A side table keyed by agent id**, which a child's new id misses.
- **Tokio task-locals**, which do not cross `tokio::spawn`; ADR-0042's
  `with_task_scope` re-scopes them onto background children, but mentra's
  parallel tool lane is not covered.

The fact belongs on the run. mentra already threads a run's `RunOptions`
into every tool call, in both lanes, and into every child through
`RunOptions::child`.

## Decision

**A turn carries typed locals, and its tools — and its children's — read
them.**

1. mentra holds the slot: `RunLocals`, a copy-on-write map keyed by type, on
   `RunOptions::locals`, read with `ToolContext::run_local::<T>()` (and the
   parallel lane's). `RunOptions::child` carries it. That is a capability
   any harness needs, so it is mentra's (AGENTS.md's split), not basis's.
2. basis passes it through: `TurnOptions::locals` and `with_locals`,
   re-exporting `RunLocals`. The run's configured `TurnOptions`
   (`PreparedRun::with_bounds`) are the floor and the turn's values layer on
   top, one type at a time — the rule `bounded` already applies to every
   other option a turn leaves unset.
3. Keyed by type, at most one value per type: a host names its fact with a
   type of its own, so two hosts' strings cannot collide and nothing in
   basis or mentra knows what any value means.

## Consequences

- One workspace, one toolset, many callers: a hosted agent puts the caller's
  value on each turn and its tools read it. The workspace-per-caller
  workaround and its temp directories are unnecessary.
- `spawn`'s children, blocking or background, read the parent's locals: both
  derive their options from `ToolContext::child_run_options`.
- MCP-bridged tools see nothing of it: a server is a separate process and
  gets only its arguments. A host that needs a per-turn value in an MCP call
  wraps the call in a native tool.
- `RunOptions` and `TurnOptions` each gain a public field, which breaks a
  struct literal that does not end in `..Default::default()`.
