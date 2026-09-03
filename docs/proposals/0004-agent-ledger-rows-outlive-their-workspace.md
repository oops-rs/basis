# 0004 — An agent ledger row should outlive the workspace that recorded it

> Status: Implemented — shape A, plus the withdrawal of
> `PreparedRun::into_session`, before 0.12.0 ships. Latent in the
> shipped adapters and confined to unreleased 0.12.0 material: it arrived with
> the mentra 0.26 adoption and no released basis has had it.
> Created: 2026-09-03 as a deferred proposal, when the wave that found it was
> scoped to something else; promoted to its own wave the same day once the reach
> was understood.
> Trigger, had it stayed deferred: anyone adding eviction to
> `basis_host::ConfiguredSource`'s workspace pool — see "How reachable it is".
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

**There is a second door, and it is why neither half of the lifetime is enough
on its own.** [`PreparedRun::into_session`](crate::PreparedRun::into_session)
hands the session back and drops the rest of the run. That session is still
live, still in its workspace's tool audience, and still judged by the chain a
sibling open of that directory installed — so a row released with the *run*
vanishes under it exactly as a row released with the *workspace* vanishes under
a run that outlived its workspace. The first shipped implementation of this
proposal held only the run half and reopened the defect through this door; it
was caught by an adversarial probe before merge, and both tests are in
`basis/tests/runtime.rs`.

**One ordering the two holders cannot cover — closed by removing its only door.**
The row has exactly those two holders, so dropping the workspace *and then*
handing the session back would leave neither, and the session would fall back to
the unjudged default while a sibling open's chain still judged it. That ordering
failed identically on the code before this proposal, so it was residual rather
than introduced; but it was still a hole, and a third holder has nowhere to
live — a session with no workspace and no run has nothing left to hang a
lifetime on, and parking the hold on the registry with no release event trades a
bound that can be reasoned about for one that cannot.

So `PreparedRun::into_session` is withdrawn. It was the only way to hold a live
session past its run, it had no consumer anywhere in basis or in nous, and
removing it makes the ordering unreachable rather than documented. A documented
isolation limit guarding an API nobody calls is dead weight and a standing risk
at once.

**Migration.** A host that needs a session for longer than one run keeps the
*workspace* alive for as long as it uses the session; that was always the
supported shape and is now the only one. Should a raw-session escape hatch ever
be genuinely wanted, it comes back redesigned *with* an answer to the third
holder — and this section is the statement of what that answer has to solve.

The workspace's half of the hold stays even though nothing in basis can now
reach past a run, because property 1 should hold by construction rather than by
the continued absence of an API.

**The native arm is not affected.** A native tool name is in the claim ledger
only because a live basis workspace put it there, and a session with no audience
cannot resolve one at all — so that arm defaults to refusing an unattributable
caller, and there is no legitimate caller behind that default. The bridged arm
cannot take the same default: a host driving `Runtime::mentra_runtime()` itself
can genuinely own a bridged server.

## Why it was not fixed where it was found

The wave that found it was scoped to per-workspace native tools. The correct fix
changes when an agent ledger row is released, which is a lifetime change to
`PreparedRun` — a core type — and doing that inside an unrelated wave would be
the wrong place to decide it.

That reasoning was about *where*, not *whether*. Once the reach was understood —
a cross-client credential path in material intended for release — it became a
wave of its own, ahead of the closeout, so that what ships is measured and
reported without a known isolation hole in it. The two-consumer question the
deferral raised is answered by the defect itself: the guard needs the row to
outlive the workspace to do its job at all, which is one consumer, and this
proposal's own door is the second.

## Two shapes, and which one wins

**Decided: A.** The mechanism above is what settles it. `v0.11.0` had a
structural barrier underneath all of this and nobody knew, so the guard's design
already assumes "a live basis session has a row" — A makes that assumption true,
which is restoring the invariant rather than working around its absence. B leaves
the lifetime wrong and adds a second question one arm can ask instead; this wave
already produced two bindings needing the same answer within days of each other,
and a third would find the lifetime still wrong. B also has a corner and A has
none: B denies a mentra-driving host its own server when the name collides with a
live basis claim, which is a behaviour change for exactly the caller the
allow-on-missing-row default exists to protect. And the no-eviction constraint
falls out of A by construction — the row stops depending on the workspace at all,
so eviction becomes a non-event, where B satisfies it for the bridged arm only.

The native arm's deny-by-default stays alongside A. They are complementary rather
than alternatives: A makes the row present so the posture rarely fires, and the
posture is still the right answer for a name only a live basis workspace can have
put in the ledger.

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
2. Rows stay removable, and bounded by the distinct agent ids a live workspace
   has recorded — not by its mints, and not by anything a dropped workspace once
   held. A host that keeps one workspace for the process and mints a fresh agent
   id per turn makes that a process-lifetime set; that is the host's choice, it
   is what basis did before 0.12, and it is one map entry per turn rather than
   per call. Read the property as forbidding growth *relative to that baseline*,
   because holding the row for the workspace as well as the run is what closes
   the `into_session` door and the literal reading would reject it.
3. Same-root takeover keeps working (`forget_if_owned`), and the reason it is
   safe stays stated: mentra leases one live session per agent id, and both
   stores refuse a second acquire even to the same owner.
4. An agent basis genuinely did not mint stays unjudged for the bridged arm.
5. `spawn`'s `AdoptedChild` keeps inheriting the parent's row for the length of
   the delegation.
6. Hiding and refusing stay separate concerns: a roster decides what the model is
   told, the guard decides what may run, and neither is asked to be the other.
7. **No leak-shaped fix for a leak-shaped bug.** A run that never ends is a live
   session and its row is legitimately pinned; rows outliving runs that *ended*
   are the failure. Dropped-without-executing, a panic between mint and execute,
   and `spawn`'s adopted children must all release, which they do if the hold is
   a field on `PreparedRun`.
8. **The hold must not be reachable from the ledger entry it keeps alive.** A
   self-pinning row is never freed, and looks exactly like correct behaviour
   until the process runs long enough.
9. **The workspace's own hold must not grow faster than what shipped.** Its
   `recorded` set is insert-only today — one `String` per distinct id, for the
   workspace's life — but the ledger *entry* beside it was already one full row
   per distinct id for exactly as long. So replacing that set with a map of
   holds costs one `String` and one pointer per id against a row that was there
   anyway, and is not the failure this property guards against. What would be:
   keying the holds by anything that grows faster than distinct agent ids, or
   holding rows a workspace no longer has any claim on.

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

## Where it came from

**This is not a defect basis inherited. It arrived with the mentra 0.26 adoption,
and no released basis has ever had it.**

mentra 0.25 deep-copied the tool registry into every derived runtime handle
(`clone_tooling_services`), and `build_session` went through one of those clones
on its way to `Agent::new`. So every session got a **private registry snapshot
taken at its creation**, and anything registered on the runtime afterwards was
invisible to that session permanently. Proven at pointer level on a `v0.11.0`
checkout: the agent's registry and basis's are two different allocations, and
the agent's stays at its creation-time count while basis's grows.

That barrier is gone in 0.26 — `share_tooling_services` gives every handle one
live `Arc<RwLock<ToolRegistry>>` — and that is not a regression upstream should
undo. It is what the audience ladder is built on, and basis wanted it: adopting
it is what made cross-*directory* isolation explicit and correct. The same step
made the same-*directory* case reachable for the first time.

So the property that protected `v0.11.0` **was real, was upstream's, was
undocumented, and is gone.** basis cannot be said to have regressed something it
relied on, because it never knew the barrier was there; and the exposure is not
purely new either. Both halves matter for choosing a fix: there is nothing to
restore upstream, so the repair is basis's own attribution problem.

`v0.11.0`'s protection was also coherent rather than lucky, which is worth
recording because it is the shape of a complete answer. The snapshot and the
mint-time `mcp__` hide cover exact complements: a sibling that bridged *before*
the mint is in the snapshot and gets hidden by name; one that bridges *after* is
not in the snapshot and is unreachable forever. The dropped-workspace door does
not exist there either — the hidden set lives in the persisted agent config, so
dropping the workspace takes nothing away from a live session.

**A retraction belongs here**, because the first answer to this question was
right for the wrong reason. An earlier probe reported that the exposure "does not
reproduce on v0.11.0" and treated that as settling the release-reach question. The
observation was correct and the reading was not: the probe's tool never entered
the agent's registry at all, so it never exercised the hide-or-guard question it
was taken to answer. The conclusion survives on stronger ground — on 0.25 the
reach is *structurally impossible*, not merely unobserved.

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
