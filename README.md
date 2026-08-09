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
- **Confinement** — the agent is scoped to the workspace; a write above it is refused.
  Per [ADR-0004](docs/adr/0004-kernel-enforced-confinement.md) this is hygiene, not a
  security boundary — that arrives with Docker in P4.

## Status

**P1** — the crate and `lan run` work; ACP (P2), extension points (P3), and `watch` plus
Docker (P4) are next. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) §6 for the phase
plan.

Docs follow the nous layout: [docs/PROPOSAL.md](docs/PROPOSAL.md)
(why) · [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) (how) · [docs/adr/](docs/adr/)
(locked decisions) · [docs/proposals/](docs/proposals/) (deferred ideas).

## License

MIT
