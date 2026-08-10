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

For a conversation rather than a one-shot, keep the prepared run and send again — the
session survives the turn, so the model sees everything said so far:

```rust
let mut run = lan::run::prepare(config).await?;
run.execute(lan::NullSink).await?;
run.send("and which of those is riskiest?", sink, lan::AllowAll).await?;
```

`run.agent_id()` is the handle `lan::run::resume` takes, so a later process can pick the
same conversation back up.

See [`lan/examples/embed.rs`](lan/examples/embed.rs) for a host that reacts to events as
they arrive, and [`lan/examples/conversation.rs`](lan/examples/conversation.rs) for the
two-turn version (`cargo run -p lan --example embed -- "<prompt>"`).

## ACP

`lan` with no subcommand speaks the [Agent Client Protocol](https://agentclientprotocol.com)
on stdio, so any ACP client drives it with no lan-specific client code:

```sh
lan                                    # what an editor spawns
lan acp --model gpt-5.6 --allow-shell  # same thing, configured
```

The client supplies the workspace (`cwd` on `session/new`) and the prompt, so the flags
here are only what a client cannot say. Sessions are mentra agents, which means
`session/load` resumes a conversation from a previous process, and `session/cancel` stops a
turn in flight. Permission requests become `session/request_permission`, so approval is the
client's UI rather than lan's ([ADR-0007](docs/adr/0007-acp-sessions-and-the-dispatch-loop.md)).

[`scripts/acp-smoke.py`](scripts/acp-smoke.py) drives it by hand if you want to watch the
wire.

For a browser client, `lan bridge` puts the same server behind a websocket — the transport
only; the UI is [acp-ui](https://github.com/formulahendry/acp-ui), adopted rather than
built ([ADR-0002](docs/adr/0002-acp-is-the-protocol.md)):

```sh
lan bridge --allow-origin http://localhost:5173
```

It binds to loopback and serves **no page** until one is named. A websocket handshake is
exempt from the same-origin policy, so any page you visit could otherwise dial
`ws://127.0.0.1` and drive an agent with write access to your workspace; the `Origin`
allowlist is what stops that, and it starts empty.

## Watching

`lan watch` runs the same prompt on an interval and skips an iteration whose workspace has
not changed, so an idle repository costs nothing:

```sh
lan watch "check for newly introduced TODOs and summarize them" --every 30m
```

"Changed" is a fingerprint over `git ls-files` — path, length, mtime, plus `HEAD` — so
`.gitignore` is honored and `.git`'s own churn is ignored. Every uncertain case runs rather
than skips: a false "changed" costs tokens, while a false "unchanged" would silently stop
the watch working ([ADR-0008](docs/adr/0008-the-watch-baseline.md)). `--always` opts out
when the answer depends on something the workspace cannot show.

## What the workspace contributes

- **`AGENTS.md`** — discovered from a global config directory, then each ancestor of the
  workspace outermost-inward, then the workspace root. Later files are more specific and
  take precedence; all of them are named in the `run_started` event.
- **Skills** — `.lan/skills/` in the workspace, else `skills/` in the global config
  directory. The model loads one by name when it needs it, so only the descriptions cost
  context.
- **Prompt templates** — `.lan/templates/*.md`, markdown with a `description` and an
  optional `argument-hint` in frontmatter. `$ARGUMENTS` and `$1`, `$2`… are substituted; a
  nested path is a namespace, so `git/commit.md` is `git:commit`. ACP clients receive them
  as commands.
- **MCP servers** — `.mcp.json`, the same shape other agents read:
  `{"mcpServers": {"name": {"command": …, "args": […], "env": {…}}}}`. `${VAR}` expands from
  the environment, so a committed file need not carry a credential. An ACP client can also
  send servers with `session/new`, and both sets are honored.
- **Hooks** — `.lan/hooks.json` lists commands that get a JSON object on stdin and answer
  with one on stdout: `allow`, `deny` with a reason the model sees, or `modify` with a
  replacement input. Any language, process-isolated. A hook that breaks denies by default,
  because a guard that fails open is a guard nobody knows is gone.
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

**P0–P4 complete.** Everything the plan called a phase is built: the ACP server on stdio
(the default mode) with modes, session listing and history replay; multi-turn conversation
and resume; one-shot `lan run` in prose or JSONL; `lan watch` on an interval, skipping a
workspace nothing has touched; MCP servers from `.mcp.json` and from the client; prompt
templates surfaced as commands; subprocess hooks that allow, deny, or rewrite a tool call;
a websocket bridge for [acp-ui](https://github.com/formulahendry/acp-ui); and branching.
All with AGENTS.md discovery, skills, approval, and kernel-enforced confinement in the
image.

Still open, and named honestly: branching is **one-way** until
[mentra#15](https://github.com/oops-rs/mentra/issues/15) — an abandoned line of work can be
inspected but not returned to. Compaction tuning, the packages convention, and provider
OAuth remain (P5), and **nobody has driven this from Zed or JetBrains yet** — it is verified
against the protocol and its official client library, not against the ecosystem. There is no
CI, and lan itself is unpublished. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) §6.

Docs follow the nous layout: [docs/PROPOSAL.md](docs/PROPOSAL.md)
(why) · [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) (how) · [docs/adr/](docs/adr/)
(locked decisions) · [docs/proposals/](docs/proposals/) (deferred ideas).

## License

MIT
