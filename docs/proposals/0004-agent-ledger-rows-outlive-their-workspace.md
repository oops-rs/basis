# 0004 — An agent ledger row should outlive the workspace that recorded it

> Status: Deferred — a real defect with a known shape, latent in the shipped
> adapters and confined to unreleased 0.12.0 material. Deferred because the fix
> changes the lifetime of a core type and the wave that found it was scoped to
> something else; it should be closed before 0.12.0 ships.
> Trigger: anyone adding eviction to `basis_host::ConfiguredSource`'s workspace
> pool — see "How reachable it is".
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

## How reachable it is, and by whom

**Latent. It needs a host to go out of its way, and neither shipped adapter
does.** `basis_host::ConfiguredSource`'s workspace pool is insert-only — no
eviction path exists in the file — and `basis-acp` keeps no pool of its own, so
in both adapters every `Arc<Workspace>` lives for the process and a workspace is
never dropped while a run is live. The exposed shape is an SDK host writing
`let run = ws.prepare(..)?; drop(ws); run.execute(..)`, which is supported
(`PreparedRun::workspace` is `None` on that path precisely because the caller is
assumed to be holding the workspace) and warned against nowhere.

**The trigger condition is what makes this worth writing down.** The invariant
that saves `basis-host` today is deliberate, but it is load bearing for entirely
different reasons than this one — the pool refuses to evict so that a live
session cannot lose its MCP connections or its `.basis/hooks.json` registrations
mid-turn. Anyone who later adds eviction, for memory or for a real
`session/close`, will close *those* two hazards consciously and reopen this one
without ever learning it existed. Whoever does that is this proposal's reader.

## Release reach

**Not in any tagged release.** Both halves of the mechanism postdate `v0.11.0`
(`c7de759`): `ForeignToolGuard` and `runtime/agents.rs` are not ancestors of the
tag, and a released basis has neither the guard nor the ledger nor
`foreign_mcp_tools`. Probed on a clean `v0.11.0` checkout against its own mentra
0.25, a bridged tool registered after the mint is provably on the registry and
still neither offered nor callable:

```
PROBE V registry after register_tool = ["mcp__prod-db__query"]
PROBE V full roster                  = ["write","compact","read","grep","glob","ls","edit","spawn"]
PROBE V req1 tool result             = "Tool not found"
```

So this is unreleased 0.12.0 material: it should be fixed before 0.12.0 ships,
not handled as an incident against a published crate.

Two limits on that, stated because they bound what may be claimed. The probe
established v0.11.0's **outcome**, not its **mechanism** — mentra 0.25's
`Agent::can_use_tool` consults an agent-level hidden set separate from the
config's tool profile, and who populates it was not traced, so whether v0.11.0
blocks this by design or by accident is unknown. That is the difference between
"0.12.0 regressed something" and "0.12.0 exposed something new", and it is worth
the one archaeological pass before anyone repeats either claim. And only
`v0.11.0` was tested; 0.8.2 and earlier were not, and this area moved enough
across the 0.12 waves that extrapolating backwards would be guessing.
