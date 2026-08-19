# 0018 — The runtime owns the process, the workspace owns the repository

> Status: Accepted · 2026-08-15
> Extends [`0010-the-crate-is-the-workflow-surface.md`](0010-the-crate-is-the-workflow-surface.md)
> and [`0011-layered-crates.md`](0011-layered-crates.md);
> related: [`0001-mentra-is-the-runtime.md`](0001-mentra-is-the-runtime.md).
> Spec: [`docs/spec/2026-08-15-runtime-and-filesystem-coordination.md`](../spec/2026-08-15-runtime-and-filesystem-coordination.md)

## Context

`Workspace` today conflates two lifetimes. Half its fields are
process-scoped infrastructure: a privately built mentra `Runtime`, the
provider and credential resolution, the history store policy
(`with_store_dir` / `with_ephemeral_history`), the host's interceptors. The
other half is repository-scoped discovery: context documents, skills,
templates, hooks configuration, `.mcp.json`. These change for different
reasons — one when the host process changes, one when the repository does —
and a host that opens N workspaces pays the process costs N times.

The architecture already knows the missing layer; it just has no noun for it.
The interception chain folds **host interceptors → global hooks → workspace
hooks** — host scope is real in the ordering and absent from the type system.
And the in-tree customer is already paying: `basis-acp`'s default
`SessionSource` builds a runtime per session from a `RunConfig`
(`server/config.rs`), so a server holding N editor sessions holds N mentra
runtimes, N provider resolutions, and N store handles in one process.

The identity check passes on two of its three arms: this makes embedding
cheaper for a Rust host, and it is a seam — the place host-scoped guards,
store policy, and provider configuration attach.

## Decision

**basis gains a `Runtime`: the process-scoped substrate every workspace
borrows. `Workspace` keeps only what the repository says.**

- `Runtime` owns mentra's runtime, provider/credential/base-URL and model
  *policy*, history store policy, and host interceptors. The builder knobs
  that describe the process — `with_api_key`, `with_provider`,
  `with_base_url`, `with_store_dir`, `with_ephemeral_history`,
  `with_interceptor` — move to `Runtime::builder()`.
- `Workspace` becomes discovery over a repository — context, skills,
  templates, hooks, `.mcp.json` — holding its `Runtime` through an `Arc`, so
  several workspaces share one substrate. MCP *connections* stay
  workspace-owned: they are minted from repository config and die with the
  workspace; the runtime never holds a connection whose config it cannot see.
- The resolved model remains a workspace fact (a workspace may override);
  the resolution *policy* — which provider, which credential, which default —
  is the runtime's.
- **`Workspace::open(path)` survives unchanged as sugar** that mints a
  private default runtime, the same wrapper pattern as the free functions
  over `Workspace` (`RunConfig::split` is the seam). The one-repository host
  never sees the third noun; only the N-repository host reaches for it.
- Naming: basis's `Runtime` owns mentra's and re-exposes it as
  `mentra_runtime()`, so the bargain that basis does not hide mentra survives
  the name. This is a breaking change to `Workspace::runtime()`, taken now
  because 0.1.0 is unpublished; the window closes at first release.
- Boundary: this is a structural extraction, not orchestration. No agent
  registry, no scheduler, no fleet manager enters `basis`; ADR-0010's
  line — orchestration is host-language code against the crate — holds.

## Consequences

- `basis-acp` holds one `Runtime` per server process and opens one `Workspace`
  per distinct `cwd`; sessions stop paying process costs, and the
  runtime-per-session `SessionSource` shape is retired.
- The interception chain's ordering gets its missing noun: host scope is
  runtime scope. Registration moves; the fold order does not.
- Tests inject a runtime once instead of threading provider and store knobs
  through every workspace builder call.
- [`0019-the-filesystem-is-the-coordination-surface.md`](0019-the-filesystem-is-the-coordination-surface.md)
  builds on this: the runtime owns the data-directory policy that lets any
  process resume any agent.
