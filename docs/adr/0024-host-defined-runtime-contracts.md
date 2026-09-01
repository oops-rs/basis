# 0024 — Hosts define complete runtime contracts

> Status: Accepted · 2026-08-27
> Precedent: [`0005-mentra-coevolution-discipline.md`](0005-mentra-coevolution-discipline.md)
> and [`0013-the-host-owns-the-boundary.md`](0013-the-host-owns-the-boundary.md).
> Source: Nous proposal 0116 and ADR-0117, accepted 2026-08-27.

## Context

basis is the generic Rust embedding layer over mentra, but its current public
surface still assumes that one workspace profile can decide most runtime and
run behavior. A host with a stricter product contract already knows more than
that profile can express: the exact registered provider, request transport,
model, reasoning, roster, tool-result policy, controls, discovery posture, and
state-lifecycle rule can all vary per run.

Nous is the first complete consumer to exercise that boundary. It already
builds customized Responses and Anthropic providers, retains the registered
Responses session for connection prewarm, protects structured tool results
from truncation, applies a grant-projected roster, varies request profiles and
budgets by run class, classifies typed failures before display conversion, and
owns request-local evidence/state cleanup. Replacing that direct integration
with basis is valid only if basis transports those choices without inferring,
widening, or silently downgrading them.

Most required primitives already exist in mentra. The two genuine upstream
gaps follow ADR-0005: mentra owns a lossless synchronous Session event tap, and
mentra-provider owns creation of a fresh Responses session scope. basis should
compose those primitives into an embedding contract rather than fork them.

## Decision

**A host may supply a complete immutable runtime/run contract. basis realizes
that contract through narrow typed APIs and fails before provider or tool
activity when it cannot honor a value. It does not recover missing behavior by
exposing raw mentra control as the normal embedding path.**

1. **Registered providers are a first-class runtime source.**
   `RuntimeBuilder::with_registered_provider` accepts mentra's provider-core
   `Provider` directly. The caller may retain a concrete provider clone whose
   Responses session state is shared with the registered runtime, so prewarm
   reaches the connection the run actually uses. This is separate from
   mentra's higher-level runtime `Provider` seam; basis re-exports the one
   provider-core type universe rather than defining another adapter.

2. **Runtime result policy is host-supplied through a narrow basis type.**
   A `ToolResultPolicy` carries byte limit, physical-line limit, and spill
   posture. `ToolResultPolicy::unlimited()` maps to mentra's unlimited/no-spill
   policy without exposing all of `RuntimePolicy`.

3. **Per-run profile and controls override workspace defaults explicitly.**
   A `RunProfile` can carry resolved model, exact roster, provider request
   options, maximum output (including explicit `None`), reasoning, compaction,
   result paging, and system prompt. `TurnOptions` additionally carries an
   absolute deadline, model-request budget, retry schedule, and retry budget.
   Existing workspace/runtime values remain fallbacks only when the run
   contract omits the corresponding value.

4. **Typed terminal failure remains in process.**
   `RunReport` retains the original mentra failure or an exact one-for-one
   typed mapping before the body-free `RunOutcome` display/wire projection is
   produced. Callers never parse an error string to recover behavior.

5. **Lossless observation is an in-process channel, not a wire expansion.**
   `PreparedRun::register_agent_event_tap` forwards mentra's complete
   occurrence-ordered `AgentEvent` values unchanged. The returned
   `AgentEventTapGuard` is a Basis-owned opaque guard that unregisters on drop.
   The existing JSONL/Event surface remains summary-oriented and body-free.
   Observation must retain a completed parallel result even when cancellation
   prevents later post-execution hooks from running.

6. **Discovery can be disabled as one coherent posture.**
   `WorkspaceBuilder::without_discovery()` skips repository/home config,
   context, hooks, declared tools, memory, skills, templates, and MCP discovery
   and connection work. Explicit host-supplied provider, prompt, and tools
   remain usable. Correctness never depends on hostile files being absent.

7. **The initial lifecycle is explicitly fresh-only.**
   Gate 1a may construct a fresh runtime and allow attached subsequent turns on
   the same prepared run, but a second independent mint is refused before
   provider/tool activity. This makes non-reuse a supported contract rather
   than caller discipline.

8. **Safe reuse consumes and rebuilds; it does not scrub one runtime in place.**
   Gate 1b requires a fresh Responses session scope from mentra-provider. basis
   consumes the uniquely owned old runtime, drops its agents/stores/scoped
   tools/background state, creates a fresh provider scope and runtime, prewarms
   that replacement's actual session, and only then returns it for pooling.
   Outstanding ownership or any rebuild/prewarm failure drops the entry.

9. **The contract stays in the `basis` SDK core.**
   No new feature flag is introduced. `basis-tasks`, ACP, CLI, MCP discovery,
   durable resume, and product-specific evidence/orchestration vocabulary do
   not enter this work.

## Consequences

- Embedders can state strict run contracts without depending on basis defaults
  or reaching through `mentra_runtime()` / `session_mut()` for normal
  execution.
- basis gains several public core types and methods, so the completed change
  requires a minor release and a downstream public-API/package probe.
- mentra remains the owner of generic event and provider-session primitives;
  basis remains the owner of embedding conventions and discovery posture.
- At acceptance, fresh-only execution could ship before safe reuse; pooling and
  full cutover remained blocked until the consume/rebuild contract and A→B/B→A
  isolation probes passed. The implementation note below records that closure.
- The public JSONL schema remains compatible because complete tool bodies stay
  on the in-process observation channel.
- Supporting strict hosts increases the conformance matrix: registered
  provider, result policy, profiles, controls, typed failure, observation,
  roster denial, discovery-off, and lifecycle each require focused tests.

## Implementation note (Basis 0.8)

Basis 0.8 implements this decision against Mentra 0.23.3. The public seams are
`PreparedRun::register_agent_event_tap` and its opaque guard,
`RuntimeBuilder::with_reusable_registered_provider` /
`into_reusable_recipe`, `WorkspaceBuilder::with_runtime_recipe`, the consuming
`Workspace::bind_host_tools`, and the async consuming
`Workspace::rebuild_for_reuse`. `BudgetPool::with_token_allowance` derives a
tighter nested view without allocating a second counter or widening the parent
allowance.

The reuse proof covers Basis-attached runs, observer guards, event forwarders,
workspace registrations, and ephemeral history. Basis enforces the declared
provider id and the factory/build/warm call order; a Responses host must return
`fresh_session_scope()` from each factory call and make `warm` prewarm that
session-sharing clone. Raw mentra access permanently disables reuse. Mentra
team/background/`spawn` execution and custom tools that detach work are
excluded; Basis does not reject those names automatically. A reusable host must
omit those routes from its exact roster and await every bound-tool effect.
`bind_host_tools` validates the supplied names and collisions, while the host
owns completeness and correspondence with that roster. Because binding
consumes the workspace, failure returns no reusable entry.

Retired in Basis 0.12 by
[`0026-the-rebuild-half-of-reuse-is-deferred.md`](0026-the-rebuild-half-of-reuse-is-deferred.md),
which is the separate decision the last alternative below defers to. Gate 1a —
`fresh_only` and its one-attempt claim — is unaffected.

## Alternatives considered

- **Expose the entire mentra runtime/configuration surface.** Rejected: that
  makes basis a transparent bag of upstream knobs and leaves no stable
  embedding contract.
- **Use `mentra_runtime()` / `session_mut()` escape hatches.** Rejected for
  normal execution: ownership remains split and missing seams become permanent
  downstream workarounds.
- **Keep observation in post-execution hooks.** Rejected: cancellation of a
  parallel batch can occur after one full result event but before hooks run.
- **Reset one retained runtime in place.** Rejected: runtime state spans private
  agent, collaboration, scoped-tool, store, observer, and provider-session
  structures, and late old-scope work can repopulate cleared state.
- **Disable pooling permanently.** Deferred to a separate decision. An accepted
  host pooling contract cannot be silently retired to avoid implementing safe
  lifecycle semantics.
