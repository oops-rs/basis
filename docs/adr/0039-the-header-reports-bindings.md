# 0039 — The header reports bindings, not features

> Status: Accepted · 2026-10-14
> Extends [`0012-one-contract-many-bindings.md`](0012-one-contract-many-bindings.md):
> what a binding configured is reported in one shape, whichever binding it is.
> Amends [`0028-a2a-is-the-agent-to-agent-protocol.md`](0028-a2a-is-the-agent-to-agent-protocol.md)
> §2 and the `run_started` additions of 0.13 — `mcp_files`, `mcp_servers` and
> `a2a_files` are one `bindings` list.

## Context

`run_started` named what discovery found, one field pair per feature:
`mcp_files` and `mcp_servers`, then `a2a_files`. Every reader of the header
— `RunContext`, the event, the off-workspace run, the CLI's test fixtures —
enumerated them, so a new binding meant touching the core in four places
and the schema once more. ADR-0038 took A2A out of the core's vocabulary;
the header was the last place a feature's name was core.

What every binding reports is the same two things: the configuration files
that took effect, weakest first, and the names those files produced. Names
only, for the reason `mcp_servers` has always given: such a file carries
credentials, and this line travels into logs.

## Decision

**`run_started` carries `bindings`, a list of `{kind, files, names}`, and a
binding contributes its own entry.**

1. `Binding { kind, files, names }`, in `basis::event`. `kind` is the
   binding's name — `mcp`, `a2a`, or a host's own. `files` and `names` are
   omitted from the line when empty, as the fields they replace were.
2. Every binding is live. A run holds a `BindingSource` per binding
   (`PreparedRun::with_binding`), read each time a header is written, so a
   set that changed after mint — A2A's, ADR-0031 — is reported as it stands
   and one that is fixed costs a clone. There is no mint-time snapshot to
   fall out of date.
3. The workspace assembles the list at open from whichever bindings its
   build has; that assembly is the one place a feature's cfg appears.
4. `remote_agents` stays a field of its own: it is the core's — what
   `@<name>` can reach, from every resolver — not one binding's.
5. `EVENT_SCHEMA_VERSION` is 3: three fields are gone.

## Consequences

- Adding a binding adds one entry at the workspace's open and nothing in
  the event, the run context, the off-workspace run or any fixture.
- A reader of `run_started` written for schema 2 finds no `mcp_files`,
  `mcp_servers` or `a2a_files`; it reads `bindings` and picks its `kind`.
- `Workspace::mcp_servers` and `Workspace::remote_agents` are unchanged;
  the header is what moved.
