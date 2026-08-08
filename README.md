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

Status: design phase. Docs follow the nous layout: [docs/PROPOSAL.md](docs/PROPOSAL.md)
(why) · [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) (how) · [docs/adr/](docs/adr/)
(locked decisions) · [docs/proposals/](docs/proposals/) (deferred ideas).

## License

MIT
