# 0011 — Layered crates: lan-core, lan-acp, the binary

> Status: Accepted · 2026-08-11
> Refines [`0003-library-first-no-tui.md`](0003-library-first-no-tui.md);
> preserves [`0002-acp-is-the-protocol.md`](0002-acp-is-the-protocol.md).

## Context

The single `lan` crate unconditionally depends on `agent-client-protocol` and
`tokio-tungstenite`, so a Rust host embedding the harness in-process compiles a
JSON-RPC server and a websocket stack it never runs. ADR-0003 said "library
first"; the packaging never caught up. The seam already exists internally —
ADR-0007 routes ACP through the same event stream that feeds JSONL — so the
split is recognizing a boundary, not inventing one.

Cargo features could gate the same code, but features leak transitively and
make the layering invisible; separate crates make it auditable.

## Decision

The workspace splits into three:

- **`lan-core`** — the SDK of [`0010`](0010-the-crate-is-the-workflow-surface.md):
  workspace discovery (AGENTS.md, skills, templates, `.mcp.json`), run
  lifecycle, the event stream, the seams (approval, tools, hooks). No
  protocol, no transport, no TTY.
- **`lan-acp`** — the ACP adapter over `lan-core`'s event stream and seams.
  Opt-in by dependency; the *default mode of the binary*, unchanged, because
  what an editor spawns is still plain `lan`.
- **`lan`** (binary) — CLI over both, per the grammar of
  [`0015`](0015-cli-grammar.md). The terminal approver lives here.

MCP support is feature-gated in `lan-core` (`mcp`, default-on for the binary),
per [`0012`](0012-one-contract-many-bindings.md): discovery of `.mcp.json` is
convention, but no embedder pays for the client they don't use.

The websocket **bridge** stays in the binary for now and is flagged as what it
is: ACP-ecosystem tooling with zero lan-specific knowledge — every ACP agent
needs the same relay to reach a browser. It is a candidate for extraction or
upstreaming, and is never an identity argument for lan.

## Consequences

- An embedder's dependency graph states what they actually use; `lan-core`
  alone pulls no protocol stack.
- ADR-0002 is untouched: ACP remains the standard and the binary's default.
  What changes is who pays for it — only the surfaces that serve it.
- The two-audience story becomes structural: interactive clients get
  `lan-acp`, workflow builders get `lan-core`, and there is no third surface.
- The split is a pre-1.0 API break, accepted deliberately while the crate is
  unpublished (the cheapest it will ever be).
