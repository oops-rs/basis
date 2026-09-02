# 0026 — The rebuild half of the reuse contract is deferred

> Status: Accepted · 2026-09-01
> Supersedes the reuse half of [`0024-host-defined-runtime-contracts.md`](0024-host-defined-runtime-contracts.md);
> answers the "Disable pooling permanently" alternative it deferred to a separate decision.
> Applies [`0005-mentra-coevolution-discipline.md`](0005-mentra-coevolution-discipline.md).

## Context

ADR-0024 shipped two gates in Basis 0.8. Gate 1a — `WorkspaceBuilder::fresh_only`,
the mint posture behind it, and two `RunError` variants — is 48 lines of
implementation. Gate 1b — the repeatable provider recipe, the consuming bind,
and the consuming rebuild — is roughly 900 implementation lines, 1,300 test
lines, and **17 of the crate's 45 `RunError` variants**, spread across
`runtime/reuse.rs`, `runtime/builder.rs`, `runtime/builder/provider.rs`,
`runtime/builder/provider_settlement.rs`, `workspace.rs`,
`workspace/builder.rs`, `workspace/lifecycle.rs`, `run/prepared.rs`,
`run/prepared/observer.rs`, and `error.rs`.

Gate 1b has no consumer. Every call site in this workspace is a test or a doc
example, and the one named external host ADR-0024 was written for exercises
Gate 1a only: it opens with `with_runtime_builder` + `without_discovery` +
`fresh_only` + `with_resolved_model` + `ToolRoster::only`, and never names a
recipe, a bind, or a rebuild.

The machinery is that large for one upstream reason. mentra-provider offers no
way to mint a fresh provider session scope from an existing provider, so Basis
carries a type-erased `Arc<dyn Fn() -> …>` provider factory with a separately
deferred async warm step — because a synchronous `build` has no honest way to
await warming — and a hand-written 27-line `replay_with_host_provider` clone,
because mentra takes providers and tools by value and an ordinary builder must
stay non-`Clone`. ADR-0024 named this as one of its two genuine upstream gaps.
It is tracked as [oops-rs/mentra#46](https://github.com/oops-rs/mentra/issues/46).

## Decision

**Retire Gate 1b's surface until mentra can mint a fresh provider session scope
from an existing provider. Gate 1a stays exactly as shipped.**

1. **The rebuild path is removed, not deprecated.** `RuntimeRecipe`,
   `RuntimeBuilder::with_reusable_registered_provider`,
   `RuntimeBuilder::into_reusable_recipe`,
   `WorkspaceBuilder::with_runtime_recipe`, `Workspace::bind_host_tools`, and
   `Workspace::rebuild_for_reuse` are gone, with the 17 `RunError` variants only
   they raised. A deprecation window would keep the whole lifecycle — the lease,
   the poison calls, the seal, the five-clause posture ladder — alive to serve
   nobody.

2. **Gate 1a is untouched.** `fresh_only`, its irreversible one-attempt claim,
   `FreshOnlySharedRuntime`, and `FreshOnlyRunAlreadyAttempted` keep their
   behavior byte for byte. ADR-0024's own consequence permitted exactly this
   shape: *"At acceptance, fresh-only execution could ship before safe reuse."*
   One doc sentence changes, because `fresh_only`'s documentation explained its
   irreversibility by pointing at Gate 1b's absent scrub contract; the rule it
   states is unchanged.

3. **A pooling host opens a fresh workspace per checkout.** That is what Gate 1a
   plus `without_discovery` already supports, and what the external host does
   today. Basis makes no reuse claim it cannot prove.

4. **The gap is filed upstream, not shimmed here** (ADR-0005). Reinstatement is
   triggered by mentra#46 closing, and reopens *this decision* rather than
   restoring this code: a fresh-scope primitive upstream would make the recipe,
   the deferred warm step, and the hand-written replay unnecessary, so the
   contract would be rebuilt against it rather than resurrected. That
   primitive has since landed on mentra's main (mentra#46), unreleased as of
   this writing; once it ships, a pooling host consumes it from mentra
   directly — the retirement here stands either way.

## Consequences

- **Breaking, in the next breaking release.** Six public items and 17
  `RunError` variants are removed. `#[non_exhaustive]` does not soften this: a downstream `match` or
  `matches!` arm naming a removed variant fails to compile. This is ADR-0024's
  own "downstream public-API/package probe" consequence coming due a second
  time.
- `fresh_only`, `without_discovery`, `with_resolved_model`, `ToolRoster::only`,
  `RunProfile`, `TurnOptions`, `ToolResultPolicy`, `RunReport::failure`, and
  `register_agent_event_tap` are untouched. `AgentEventTapGuard` keeps its name,
  its `#[must_use]`, and its drop semantics — it no longer holds a lifecycle
  lease, which no caller could observe.
- `Workspace::mentra_runtime` and `PreparedRun::session` / `session_mut` /
  `into_session` no longer poison a generation. Their signatures are unchanged;
  the withdrawn promise was about a reuse entry that no longer exists.
- `RunError` goes from 45 variants to 28 (27 without the `mcp` feature), and the
  crate loses ~2,200 lines.
- The `responses-websocket` feature keeps in-crate coverage. Its only test was a
  rebuild-isolation test; it is re-homed onto the surviving prewarm seam, where a
  host clones its `ResponsesProvider`, registers one clone, opens the WebSocket
  through the clone it kept, and the run rides that connection. The `futures` and
  `tokio-tungstenite` dev-dependencies stay for its harness.
- ADR-0024 is not rewritten. It records the decision that was accepted and the
  contract that shipped; this ADR records what happened to half of it.
