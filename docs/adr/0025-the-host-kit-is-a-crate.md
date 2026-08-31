# 0025 — The host kit is a crate

> Status: Accepted · 2026-09-01
> Applies [`0011-layered-crates.md`](0011-layered-crates.md) and
> [`0018-the-runtime-owns-the-process.md`](0018-the-runtime-owns-the-process.md);
> complements [`0022-the-task-layer-is-a-crate.md`](0022-the-task-layer-is-a-crate.md).

## Context

`basis-acp` began as a protocol adapter, but it had become the only owner of
several things that name no ACP type: a three-policy approval switch with
revocable session answers, the one-turn-at-a-time lock and cancellation pair,
the source/template a served session is built from, and the lazy process
runtime plus per-workspace pool. A third frontend could reuse `basis`'s run
surface and event schema and still have to reconstruct those rules from the
ACP implementation.

The duplication was already visible. ACP, durable tasks, and the CLI each had
their own enum for `always` / `prompt` / `never`, with conversions between
them. The ACP source and session machinery were concrete and behavior-tested;
the missing boundary was packaging, not a more general design.

## Decision

**Add `basis-host`, an adapter-neutral host kit over `basis`, and move the
existing concrete host machinery into it without introducing a frontend or
adapter abstraction.**

1. **One approval policy serves all three consumers.** `ApprovalPolicy` owns
   the stable lowercase identifiers and serde form used by ACP, tasks, and the
   CLI. `SessionApproval` owns the current policy and remembered per-tool
   answers; every successful policy change clears them. `PolicyApprover<A>`
   samples the policy before awaiting its inner approver, so a question
   already put to a client is answered under the policy that put it there.
   ACP keeps the read-only session's non-switchable offering rule, its mode
   descriptions and ids, and `ModeError` because those are protocol behavior.

2. **Turn coordination is host behavior.** `HostSession` owns one async turn
   lock and a cancellation token plus awaitable `Interrupt` reachable without
   that lock. ACP owns one registry entry pairing that concrete session with
   its `SessionId` and protocol mode presentation; the pairing is inserted,
   read, and removed under one mutex. Closing or deleting still removes the registry entry, trips
   cancellation, waits for the turn lock, drops the session, and only then
   removes persisted state.

3. **Served-session configuration and pooling move intact.** `SessionSource`,
   `Discovery`, `SessionTemplate`, and `ConfiguredSource` move from ACP. The
   configured source still builds one runtime lazily per process and one
   workspace lazily per canonical cwd plus supplied-MCP digest, never evicts,
   does not cache failures, and never holds the map mutex across an await. The
   key retains only a digest, not client configuration or secrets; remote MCP
   config and limits remain exhaustively destructured so an upstream field
   addition is a compile error. ACP's `ServeConfig`, session-info rendering,
   permission RPC, stop-reason/error mapping, and handler spawn rules stay in
   `basis-acp`.

4. **The dependency direction remains one way.** `basis-host` depends on
   `basis` and the concrete support crates the moved code needs. It has no ACP,
   JSON-RPC, clap, terminal, `basis-tasks`, or `basis-cli` dependency.
   `basis-acp`, `basis-tasks`, and `basis-cli` may depend on `basis-host`; none
   may create a reverse edge into the SDK.

## Consequences

- A new frontend can reuse the approval, session, sourcing, and workspace
  lifecycle that ACP proved without importing ACP or copying its rules.
- Existing ACP-visible bytes and behavior stay unchanged; its existing
  integration suite remains the compatibility proof, while the moved unit
  tests now live beside their owners in `basis-host`.
- The workspace has five crates at one version. `basis-cli` still publishes
  the `basis` binary; no version bump, tag, or publication is implied by this
  decision.
- `basis-tasks` continues to use ADR-0019's durable marker-file cancellation,
  honored at turn boundaries. It may later reuse the in-memory host-session
  discipline while attached, but this decision does not change its substrate,
  cancellation granularity, or recovery contract.
- Mentra remains the owner of generic runtime capabilities. Gaps such as a
  scoped swappable authorizer, revocable remembered-rule store, and
  per-session store tag are tracked upstream rather than shimmed here.
