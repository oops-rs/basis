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

There are two modes and two utilities, and the whole CLI is five lines
([ADR-0015](docs/adr/0015-cli-grammar.md)):

```
lan                                # ACP server on stdio — what an editor spawns
lan "<prompt>"                     # shorthand: exactly `lan run "<prompt>"`
lan run "<prompt>" [--json]        # headless one-shot; `-` reads the prompt from stdin
lan bridge                         # the same ACP server on a websocket, for a browser
lan fingerprint                    # the workspace's hash, for a loop you write yourself
```

A positional argument that names no subcommand is a prompt, so the human path carries no
ceremony and the editor path is untouched. `--` escapes a prompt that collides with a
subcommand name (`lan -- run`).

The core has no opinions: task-specific behavior enters through data — the prompt, the
workspace (AGENTS.md, skills, prompt templates, `.mcp.json`), and config — never through code.

## Using it

Set a provider key — `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, or
`OPENROUTER_API_KEY` — and run a prompt against a repository:

```sh
lan "summarize what changed in the last three commits"
lan run -C ../other-repo --json "find the slowest test and explain why"
```

`--effort` accepts exactly `low`, `medium`, `high`, `xhigh`, or `max`.
LAN keeps those values provider-neutral: Responses-family APIs receive
`reasoning.effort`, while Anthropic receives `output_config.effort` and enables
adaptive thinking only on models that support it. Provider/model combinations
without a requested tier fail explicitly instead of silently lowering it;
omitting the flag leaves the provider default unchanged.

The agent can read and write files in the workspace and **run commands** — shell and
background tasks — because a harness that cannot run the test suite does very little real
work. `--no-shell` shuts the command tools for a run meant to read and report:

```sh
lan --no-shell "explain how the event stream is assembled"
```

Be clear on what that flag is. A run holds whatever authority the user account that started
it holds; nothing inside the process narrows that, and lan claims no sandbox anywhere. The
path roots and the `.git` carve-out below are hygiene — they shut the route a model reaches
for first — and a shell redirect walks straight past them. `--no-shell` narrows what *this
run* does; it does not confine the process, because an in-process check cannot confine a
command once it has started ([ADR-0013](docs/adr/0013-the-host-owns-the-boundary.md)).
Isolation, where you want it, is the OS's job:
[docs/containerization.md](docs/containerization.md) has the read-only-root pattern, what
it protects, and what it does not.

Independently of that, you can decide when the agent has to ask:

```sh
lan --approve prompt "tidy up the imports"        # ask before each change
lan --approve never  "what does this crate do?"   # look, don't touch
```

Read-only calls are never queued for approval — prompting for them just trains you to
approve without reading. Asking needs a terminal on stdin; without one a request is
denied rather than silently granted, so an unattended run fails visibly instead of
quietly doing as it pleases.

A run nobody is watching should say so itself, with bounds — all unset by default, since an
attended run has a person who tells "thinking hard" from "stuck" in a way no timer can
([ADR-0014](docs/adr/0014-watch-retired-runs-are-boundable.md)):

```sh
lan run --deadline 10m --tool-budget 40 --token-budget 200000 "bump the deps and fix the fallout"
```

Every bound ends the run *gracefully* rather than discarding it: the event stream closes the
way it always does, and whatever the model committed is kept. `--token-budget` is soft by
construction — usage is only known once a round has streamed in full, so the round that
crosses the line finishes and the run ends having succeeded.

Any OpenAI-compatible endpoint works too — a gateway, a proxy, or a local
server. Paste the URL as published; the trailing `/v1` is handled:

```sh
export LAN_BASE_URL=http://127.0.0.1:3455/v1
export LAN_API_KEY=…
lan run --model gpt-5.6 "explain the module layout"
```

Custom endpoints use complete local transcript replay and do not automatically
send `previous_response_id`. That optional extension is not part of LAN's
compatibility assumption; native provider presets retain Mentra's Hybrid state
chaining.

`--json` emits the event stream as newline-delimited JSON. The first line is always
`run_started` and carries the schema version; the last is always `run_finished`:

```jsonl
{"seq":0,"type":"run_started","schema":1,"lan":"0.1.0","workspace":"/repo","model":"…","provider":"anthropic","context_files":[{"path":"/repo/AGENTS.md","scope":"workspace"}]}
{"seq":1,"type":"assistant_delta","text":"Looking at "}
{"seq":2,"type":"run_finished","status":"ok"}
```

Exit codes are contract, so a caller branches without parsing anything:

| Code | Meaning |
|---|---|
| `0` | the run finished |
| `1` | the run failed, or lan could not start it |
| `2` | the invocation was wrong |
| `3` | a bound tripped (`--deadline`, `--tool-budget`); committed work was kept |

`3` is deliberately not `1`: "the model ran out of the time you gave it" and "the provider
refused the request" call for different reactions. A crossed `--token-budget` is absent from
that row on purpose — it ends the run gracefully, so the run succeeded and exits `0`.

In-process, the same run is one call — the binary is a thin shell over this:

```rust
let report = lan::run(
    lan::RunConfig::new("/repo", "summarize the recent changes"),
    lan::CollectingSink::new(),
).await?;
```

The bounds are builders on the same config, and `report.stopped_by` carries the distinction
the exit code makes — `Some(lan::Bound::Deadline)`, `Some(lan::Bound::ToolBudget)`, or
`None` when the work is what ended the run:

```rust
let config = lan::RunConfig::new("/repo", "bump the deps and fix the fallout")
    .with_deadline(Duration::from_secs(600))
    .with_tool_budget(40)
    .with_token_budget(200_000);
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
lan                                # what an editor spawns
lan acp --model gpt-5.6 --no-shell # same thing, configured
```

The client supplies the workspace (`cwd` on `session/new`) and the prompt, so the flags
here are only what a client cannot say. Sessions are mentra agents, which means
`session/load` resumes a conversation from a previous process, and `session/cancel` stops a
turn in flight. Permission requests become `session/request_permission`, so approval is the
client's UI rather than lan's ([ADR-0007](docs/adr/0007-acp-sessions-and-the-dispatch-loop.md)).

An editor spawning lan and a shell pipe look identical from inside the process — both are a
non-TTY stdin with no arguments — so `cat prompt.txt | lan` cannot be detected as a prompt
without breaking every editor. Instead of waiting silently on prose, the server answers once
the input proves it was never a client:

```
lan: expected an ACP client on stdio; did you mean 'lan run -'?
```

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

## Recurring runs

lan ships no scheduler: an interval belongs to whatever already runs things on your machine
— cron, systemd, CI, a tokio task in your own binary. What lan ships instead are the two
pieces that are easy to get wrong, and the loop is composition
([ADR-0014](docs/adr/0014-watch-retired-runs-are-boundable.md)):

```sh
last=""
while :; do
  now=$(lan fingerprint)
  if [ "$now" != "$last" ]; then
    lan run --json --deadline 10m --tool-budget 40 \
        "check for newly introduced TODOs and summarize them" > run.jsonl
    case $? in
      0) last=$now ;;                          # only a clean run moves the baseline
      3) echo "bound tripped; retry next tick" >&2 ;;
      *) echo "run failed" >&2 ;;
    esac
  fi
  sleep 1800
done
```

`lan fingerprint` prints a digest over `git ls-files` — path, length, mtime, plus `HEAD` —
so `.gitignore` is honored and `.git`'s own churn is ignored:

```
$ lan fingerprint
cea476f305ecf3f5
```

Every uncertain case reports *changed* rather than unchanged: a false "changed" costs tokens,
while a false "unchanged" would silently stop the loop doing anything at all. Recording the
baseline only after a run you consider successful is the caller's policy, because the caller
is where the definition of "successful" lives — above, that is the `0` arm.

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
  command or the changed keys shown; `always` (the default on the CLI) and `never` are the
  other two settings. Over ACP the default is `prompt`, since there is a client to ask.
  Embedders implement `Approver` to answer however they like.
- **`.git` carve-out** — `.git/hooks` and `.git/config` are denied to the file tools by
  default: a file written there runs on the next commit, which turns an edit into code
  execution outside anything approval covers. The rest of `.git` stays writable, since git
  needs it. This binds the file tools, **not the shell** — a redirect inside `sh -c` still
  lands, because nothing parses shell. Hygiene, not a boundary.
- **Confinement** — the agent is scoped to the workspace; a write above it is refused by the
  file tools. This is hygiene, not a boundary
  ([ADR-0004](docs/adr/0004-kernel-enforced-confinement.md),
  [ADR-0013](docs/adr/0013-the-host-owns-the-boundary.md)). The boundary, if you want one,
  is the OS's — see [docs/containerization.md](docs/containerization.md).

## Status

**P0–P4 complete.** Everything the original plan called a phase is built: the ACP server on
stdio (the default mode) with modes, session listing and history replay; multi-turn
conversation and resume; one-shot `lan run` in prose or JSONL; MCP servers from `.mcp.json`
and from the client; prompt templates surfaced as commands; subprocess hooks that allow,
deny, or rewrite a tool call; a websocket bridge for
[acp-ui](https://github.com/formulahendry/acp-ui); and branching. All with AGENTS.md
discovery, skills, and approval.

**The redesign is underway.** [ADR-0010](docs/adr/0010-the-crate-is-the-workflow-surface.md)
through [ADR-0015](docs/adr/0015-cli-grammar.md) point lan at an SDK-first shape: the crate
is the workflow surface, the host owns the boundary, and the binary keeps only the grammar
above. [docs/REDESIGN.md](docs/REDESIGN.md) is the honest ledger of that transition.
**Phase A has landed** — `watch` retired with its bounds moved onto `RunConfig` and
`lan run`, the fingerprint kept as a utility, shell on by default, the shipped container
replaced by documented patterns, and the CLI grammar with its exit codes. Phases B (crate
split), C (the SDK proper) and D (bindings) are open, and this README describes only what
is built.

Still open, and named honestly: compaction tuning, the packages convention, and provider
OAuth remain, and **nobody has driven this from Zed or JetBrains yet** — it is verified
against the protocol and its official client library, not against the ecosystem. lan itself
is unpublished. CI runs `cargo fmt --all --check`, clippy at `-D warnings`, and the full
suite on Linux, macOS, and Windows, plus an MSRV job. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) §6.

Docs follow the nous layout: [docs/PROPOSAL.md](docs/PROPOSAL.md) (why) ·
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) (how) · [docs/REDESIGN.md](docs/REDESIGN.md)
(the transition) · [docs/adr/](docs/adr/) (locked decisions) ·
[docs/proposals/](docs/proposals/) (deferred ideas).

## License

MIT
