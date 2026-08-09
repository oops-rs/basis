# lan

> **lan** — **L**ightweight **A**gent **N**ucleus.
> A LAN connects machines; lan connects agents to your codebase.

A full-functional, embeddable agent harness built on [Mentra](https://github.com/oops-rs/mentra).

Library first, binary second. No TUI — embedding is the front door:

1. **In-process** — depend on the `lan` crate (Rust hosts).
2. **ACP** — `lan` with no subcommand serves the [Agent Client Protocol](https://agentclientprotocol.com)
   (JSON-RPC 2.0 over stdio) for editors (Zed, JetBrains) and web UIs
   ([acp-ui](https://github.com/formulahendry/acp-ui)).
3. **Subprocess** — `lan run --json` streams JSONL events for scripts and CI.

```
lan                                   # ACP server on stdio (default)
lan run "<prompt>" [--json]           # headless one-shot
lan watch "<prompt>" --every 30m      # recurring headless runs
```

The core has no opinions: task-specific behavior enters through data — the prompt, the
workspace (AGENTS.md, skills, prompt templates, `.mcp.json`), and config — never through code.

## Using it

Set a provider key — `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, or
`OPENROUTER_API_KEY` — and run a prompt against a repository:

```sh
lan run "summarize what changed in the last three commits"
lan run -C ../other-repo --json "find the slowest test and explain why"
```

By default the agent can read and write files inside the workspace but **cannot run
commands** — a path check inside the process cannot confine a process once it starts, so
that authority has to be granted deliberately:

```sh
lan run --allow-shell "run the test suite and summarize the failures"
```

On a bare host that grant is real and lan says so. The container below is where it is
sound, and where it is on by default.

Independently of that, you can decide when the agent has to ask:

```sh
lan run --approve prompt "tidy up the imports"   # ask before each change
lan run --approve never  "what does this crate do?"   # look, don't touch
```

Read-only calls are never queued for approval — prompting for them just trains you to
approve without reading. Asking needs a terminal on stdin; without one a request is
denied rather than silently granted, so an unattended run fails visibly instead of
quietly doing as it pleases.

Any OpenAI-compatible endpoint works too — a gateway, a proxy, or a local
server. Paste the URL as published; the trailing `/v1` is handled:

```sh
export LAN_BASE_URL=http://127.0.0.1:3455/v1
export LAN_API_KEY=…
lan run --model gpt-5.6 "explain the module layout"
```

`--json` emits the event stream as newline-delimited JSON. The first line is always
`run_started` and carries the schema version; the last is always `run_finished`:

```jsonl
{"seq":0,"type":"run_started","schema":1,"lan":"0.1.0","workspace":"/repo","model":"…","provider":"anthropic","context_files":[{"path":"/repo/AGENTS.md","scope":"workspace"}]}
{"seq":1,"type":"assistant_delta","text":"Looking at "}
{"seq":2,"type":"run_finished","status":"ok"}
```

In-process, the same run is one call — the binary is a thin shell over this:

```rust
let report = lan::run(
    lan::RunConfig::new("/repo", "summarize the recent changes"),
    lan::CollectingSink::new(),
).await?;
```

See [`lan/examples/embed.rs`](lan/examples/embed.rs) for a host that reacts to events as
they arrive (`cargo run -p lan --example embed -- "<prompt>"`).

## What the workspace contributes

- **`AGENTS.md`** — discovered from a global config directory, then each ancestor of the
  workspace outermost-inward, then the workspace root. Later files are more specific and
  take precedence; all of them are named in the `run_started` event.
- **Skills** — `.lan/skills/` in the workspace, else `skills/` in the global config
  directory. The model loads one by name when it needs it, so only the descriptions cost
  context.
- **Approval** — `--approve prompt` puts every consequential call to you first, with the
  command or the changed keys shown; `always` (the default) and `never` are the other two
  settings. Embedders implement `Approver` to answer however they like.
- **Confinement** — the agent is scoped to the workspace; a write above it is refused.
  In-process this is hygiene, not a boundary
  ([ADR-0004](docs/adr/0004-kernel-enforced-confinement.md)). The boundary is the
  container's.

## Container

The image is where the workspace guarantee is real: a read-only root filesystem with the
workspace as the only writable mount, enforced by the kernel rather than by lan. Commands
are enabled there without a flag for exactly that reason
([ADR-0006](docs/adr/0006-shell-requires-an-explicit-grant.md)).

```sh
docker build -t oops/lan:latest .

docker run --rm \
  --read-only --tmpfs /tmp \
  --security-opt no-new-privileges \
  -v "$PWD":/workspace:rw \
  -v lan-state:/state \
  -e ANTHROPIC_API_KEY \
  oops/lan:latest run "run the tests and tell me what broke"
```

Inside it, a command that reaches past the workspace is refused by the kernel:

```
/bin/sh: 1: cannot create /etc/breach.txt: Read-only file system
```

`-v lan-state:/state` keeps session state across runs; without it `--rm` and `--read-only`
leave the agent nowhere to write its store.

## Status

**P1 complete, plus the container from P4.** What works today: one prompt against a
workspace, in prose or JSONL, in-process or as a subprocess, with AGENTS.md discovery,
skills, command execution, and kernel-enforced confinement in the image.

Not built yet, though this README's synopsis names them: the **ACP server** (the default
`lan` with no subcommand — P2) and **`lan watch`** (P4). Runs are also single-turn today:
there is no conversation or resume. MCP wiring, prompt templates, and subprocess hooks are
P3. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) §6.

Docs follow the nous layout: [docs/PROPOSAL.md](docs/PROPOSAL.md)
(why) · [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) (how) · [docs/adr/](docs/adr/)
(locked decisions) · [docs/proposals/](docs/proposals/) (deferred ideas).

## License

MIT
