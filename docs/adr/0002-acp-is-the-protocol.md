# ADR-0002 — ACP is the wire protocol; no bespoke RPC

**Status:** Accepted (v1)

## Context

Embedding needs a wire protocol. Options: invent one (pi's RPC mode — bespoke JSONL,
each integrator writes a client), adopt a proprietary one (codex's app-server —
explicitly "not true JSON-RPC 2.0", bypassed by its own SDKs), or adopt the standard.
ACP (Agent Client Protocol) is JSON-RPC 2.0 over stdio, LSP-style, v1 stable, with
official Rust/TS/Python/Kotlin/Java libraries and shipping clients: Zed, JetBrains,
acp-ui (web/desktop/mobile), acp-mobile.

## Decision

lan speaks **ACP** as its only wire protocol, via the official
`agent-client-protocol` Rust crate. `lan` with no subcommand serves ACP on stdio.
The headless `run --json` JSONL stream is an *output format* for scripts, not a
protocol — no requests flow inward on it. Browser access is acp-ui plus a thin
WebSocket↔stdio bridge; lan ships no web UI.

## Consequences

- Every existing ACP client works day one; the web UI is adopted, not built.
- lan's session/permission/event model must map onto ACP's — mentra's
  `SessionEvent` and `PermissionRequest` are near-1:1 (p0-groundwork §3).
- Bet on ACP's evolution; if it stalls, the protocol layer is one module, and the
  crate API remains the protocol-free surface.
