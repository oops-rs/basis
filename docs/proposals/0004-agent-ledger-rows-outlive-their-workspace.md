# 0004 — An agent ledger row should outlive the workspace that recorded it

> Status: Deferred — a live defect with a known shape, deferred because the fix
> changes the lifetime of a core type and the wave that found it was scoped to
> something else.
> Created: 2026-09-03
> Related: [ADR-0018](../adr/0018-the-runtime-owns-the-process.md) (one runtime,
> many workspaces), `basis/src/runtime/agents.rs`.

## Summary

`runtime::agents` records which workspace each live agent answers for, and
`ForeignToolGuard` reads that row on every call to decide whether the caller may
reach a tool. The row lives for the **workspace**; the thing that needs it lives
for the **session**. A host that holds a run and drops the workspace takes the
row away underneath a live session, and the guard — which allows any bridged call
it cannot attribute — stops protecting it.

## The defect, as observed

Reproduced on `0784577` by execution, on a clean checkout with no unreleased
work applied:

```
owner opens /repo with an authenticated `prod-db` MCP server
stranger opens /repo with no `mcpServers` at all      (same root, so one audience)
stranger.prepare("go")                                 (mints; nothing to hide yet)
drop(stranger)                                         (the ledger row goes)
stranger's run calls mcp__prod-db__query               → the owner's server answers
```

Two live opens of one directory is not a corner: it is the shape `basis-host`
produces on purpose when one repository is opened for two clients with different
client-supplied `mcpServers`. The mint-time hide cannot cover this ordering,
because a claim carries no bridged tool names until its connection succeeds —
which is exactly why the guard exists. `basis/tests/runtime.rs`'s
`a_bridged_tool_is_refused_to_the_same_root_open_that_did_not_configure_it`
pins the intended behaviour; it simply never drops the workspace.

`Workspace::prepare(&self)` does not attach the workspace to the run it returns,
so `let run = ws.prepare(..)?; drop(ws); run.execute(..)` is a supported shape
rather than a misuse.

**The native arm is not affected.** A native tool name is in the claim ledger
only because a live basis workspace put it there, and a session with no audience
cannot resolve one at all — so that arm defaults to refusing an unattributable
caller, and there is no legitimate caller behind that default. The bridged arm
cannot take the same default: a host driving `Runtime::mentra_runtime()` itself
can genuinely own a bridged server.

## Why it is not fixed here

The wave that found it was scoped to per-workspace native tools. The correct fix
changes when an agent ledger row is released, which is a lifetime change to
`PreparedRun` — a core type — for what is today a single caller. Doing it inside
an unrelated wave would be the wrong place to make that decision.

## Two shapes, with the trade-off stated

**A. The row's lifetime becomes the session's** (recommended). `WorkspaceAgents::record`
returns a hold in the shape of `AdoptedChild`, and `PreparedRun` carries one.
The row then survives any workspace drop, and `forget_if_owned` fires when the
last of {workspace, live runs} goes. Fixes the actual defect and closes both
arms at once.

**B. The bridged arm asks basis's own ledger** (cheaper, arm-specific). For an
unattributable caller, ask whether any live workspace claimed a server of that
name (`mcp_claims`, populated at `claim_mcp_server` — so it is filled before the
connection and covers the window that defeats the hide); deny if so, allow if
not. No lifetime change, one ledger read.

Its cost, which is why it is not simply the smaller correct answer: a host
driving mentra directly that bridges a server whose *name* collides with one a
live basis workspace claimed would be denied its own tool. Basis's suffixing
(`prod-db-<hash>`) only disambiguates claims basis made, so it cannot tell that
pair apart. Rare, but a real behaviour change for the caller the current default
deliberately protects.

## Properties any implementation must preserve

1. A row outlives every live session minted or resumed against it, not only the
   workspace that recorded it.
2. Rows stay removable — no unbounded growth on a long-lived runtime.
3. Same-root takeover keeps working (`forget_if_owned`), and the reason it is
   safe stays stated: mentra leases one live session per agent id, and both
   stores refuse a second acquire even to the same owner.
4. An agent basis genuinely did not mint stays unjudged for the bridged arm.
5. `spawn`'s `AdoptedChild` keeps inheriting the parent's row for the length of
   the delegation.
6. Hiding and refusing stay separate concerns: a roster decides what the model is
   told, the guard decides what may run, and neither is asked to be the other.

## Open question this proposal does not answer

Whether the same exposure is reachable in a **published** basis. `ForeignToolGuard`
and the agent ledger are unreleased 0.12.0 material, so the probe above does not
describe any release. Released `v0.11.0` protected a same-root sibling by
mint-time hiding alone, with no execution guard — reading that code suggests a
sibling that mints *before* another open bridges is exposed without needing any
workspace drop, which would be broader than what is probed here. That reading is
**not verified by execution** and must be before it is reported as affecting a
published version.
