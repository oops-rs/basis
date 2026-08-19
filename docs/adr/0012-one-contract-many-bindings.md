# 0012 — One contract, many bindings

> Status: Accepted · 2026-08-11
> Extends [`0001-mentra-is-the-runtime.md`](0001-mentra-is-the-runtime.md);
> informs [`0011-layered-crates.md`](0011-layered-crates.md).

## Context

basis grew two parallel extension tracks: MCP servers for custom tools,
subprocess hooks for interception. Each is its own subsystem with its own
config, while mentra underneath already defines the contracts both are
partial views of — `ExecutableTool` (with descriptors: capabilities,
side-effect level, durability, execution mode) for tools, and the authorizer
seam for interception. Mentra even bridges MCP servers' tools into the same
`ToolRegistry` as builtins: internally, MCP is *already* just tools.

pi's answer to the same question was to refuse MCP and make in-process
TypeScript the only binding. basis cannot load same-language plugins into a
Rust binary cheaply, and does not need to: the contract, not the binding, is
the design.

## Decision

**One contract per seam; transports are adapters.**

Tools — the contract is mentra's `ExecutableTool`, surfaced first-class in
`basis`. Three bindings:

1. **Native Rust** — an embedder registers a tool as code.
2. **Declared subprocess** — new: a data file in the workspace declares a
   tool (name, description, JSON schema, command); basis wraps the command as
   an `ExecutableTool` speaking JSON over stdio — the same IO style hooks
   already use. pi's "CLI tools instead of MCP," but typed and schema-checked.
3. **MCP** — the existing mentra client, demoted from privileged subsystem to
   one adapter, behind a cargo feature.

Interception — the contract is the authorizer seam (mentra's `ToolAuthorizer`
plus basis's `Approver`). Two bindings:

1. **In-process Rust** — an embedder implements the trait.
2. **Subprocess hooks** — the existing `.basis/hooks.json` mechanism, re-founded
   as a binding of that seam rather than a parallel system. Fail-closed
   semantics carry over unchanged: a hook that breaks denies.

## Consequences

- The extension story is one sentence: *everything is a tool or an
  authorizer; each has an in-process binding for embedders and a subprocess
  binding for workspaces.*
- MCP support becomes droppable at compile time without losing the concept of
  custom tools.
- The declared-subprocess binding is the only new machinery, and it is small:
  discovery of a manifest plus a stdio wrapper over an existing contract.
- If a gap appears — an interception a hook cannot express that the seam
  should carry — the fix is widening the *contract* in mentra (filed per
  [`0005`](0005-mentra-coevolution-discipline.md)), never a third parallel
  mechanism in basis.
