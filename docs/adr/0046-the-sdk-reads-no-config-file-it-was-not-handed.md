# 0046 — The SDK reads no config file it was not handed

> Status: Accepted · 2026-10-22
> Narrows how `.basis/config.json` is reached; the file's schema, its
> workspace/global trust split and its precedence are unchanged.
> Follows [`0045-a-gateway-ring-is-configuration.md`](0045-a-gateway-ring-is-configuration.md).

## Context

`Workspace::open` discovered `.basis/config.json` and the user's global
`config.json` unless the host passed a `Config`. The reasoning was the one
that still holds for `AGENTS.md` and `.mcp.json`: opening a path is what
reads a repository's conventions.

`config.json` is a different kind of file. It does not describe the
workspace an agent works in; it decides the *process's* connection — which
provider, which model, which endpoint, and since ADR-0045 which ring of
gateways. That is a CLI user's convenience: a person typing `basis spawn`
has no other place to pin a model. A host embedding basis already has its
own configuration, and the default meant that a file the host never named —
a developer's `~/.config/basis/config.json` on the machine a service
happened to run on, or a repository's `.basis/config.json` — could change
which model its product ran. Every embedder that noticed had to remember
`with_config(Config::default())`, and one that did not noticed late.

## Decision

1. **`WorkspaceBuilder` reads no `config.json` unless the host hands one
   over.** `with_config(Config)` as before, and `with_config_file(path)`,
   which reads exactly that file at open as the host's own (`base_url` and
   `gateways` honored; missing is an error). Unset is no configuration
   file. There is no discovery switch on the builder.

2. **The CLI's lookup is asked for by name.** `Config::discover` stays
   public, and `ConfigLookup` — `None` (default), `File(path)`, `Discover` —
   holds the same choice as a value for layers that open workspaces on
   someone else's behalf, before they know which: `basis-host`'s
   `Discovery::config`, `basis-a2a`'s `RunDefaults::with_config`,
   `basis-tasks`'s `RunSpec::with_config`. Each resolves it per workspace
   and passes the result to `with_config`.

3. **A task records its lookup.** A task may be attached by a process other
   than the one that spawned it, so `RunOptions` writes `config` always.
   A record from before the field was spawned when every open discovered,
   and it keeps discovering on resume rather than quietly running a
   different model than it started with.

4. **`basis spawn`, `basis acp` and `basis serve a2a` ask for `Discover`.**
   What a CLI user sees does not change.

5. **Only `config.json`.** `AGENTS.md`, skills, templates, hooks, declared
   tools and `.mcp.json` describe the workspace an agent works in and are
   still discovered by `Workspace::open`, with `without_discovery` as the
   switch. The environment variables (`BASIS_BASE_URL`, `BASIS_GATEWAYS`,
   the provider keys) are unchanged.

## Consequences

- An embedder that relied on `Workspace::open` reading `config.json` reads
  nothing now; it passes `Config::discover(..)` or a `ConfigLookup` to keep
  the old behaviour. Breaking for that host, silent at compile time, which
  is why the changelog says it first.
- `with_config(Config::default())` stays valid and is now the default.
- `basis-host`'s `Discovery`, `basis-a2a`'s `RunDefaults` and
  `basis-tasks`'s `RunSpec` each gain a field; a struct literal of
  `Discovery` needs it.
- `ConfigLookup` is `Serialize`: `"none"`, `"discover"`,
  `{"file": "/path"}`.
