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

`AGENTS.md` is discovered from a global config directory, then each ancestor of the
workspace outermost-inward, then the workspace root — later files are more specific and
take precedence.

## Status

**P1** — the crate and `lan run` work; ACP (P2), extension points (P3), and `watch` plus
Docker (P4) are next. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) §6 for the phase
plan.

Docs follow the nous layout: [docs/PROPOSAL.md](docs/PROPOSAL.md)
(why) · [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) (how) · [docs/adr/](docs/adr/)
(locked decisions) · [docs/proposals/](docs/proposals/) (deferred ideas).

## License

MIT
