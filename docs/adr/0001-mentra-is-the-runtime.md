# ADR-0001 — Mentra is the runtime; basis does not re-implement the loop

**Status:** Accepted (v1)

## Context

basis needs an agent loop, provider access, builtin tools, and persistence. Mentra
(v0.11, same author) provides all four, tested: runtime orchestration, six provider
backends, async tool traits, SQLite-backed sessions, an MCP client, a compaction
engine, and a session event stream. Re-implementing any of this in basis would trade
proven code for control, and nous already validated the "rent the loop" shape
(nous ADR-0003).

## Decision

basis builds **on** mentra and duplicates none of it. Crate layering mirrors pi's
package layering: mentra-provider ≈ pi-ai, mentra ≈ pi-agent-core, basis ≈
pi-coding-agent minus TUI. Capabilities generic to any harness belong in mentra;
basis keeps conventions (AGENTS.md, skills discovery, templates), protocol (ACP,
JSONL), scheduling, and packaging.

## Consequences

- basis stays small; its tests target composition, not runtime behavior.
- basis inherits mentra's provider matrix and persistence for free.
- basis is coupled to mentra's release cadence — acceptable, same author (see
  ADR-0005 for the seam discipline).
