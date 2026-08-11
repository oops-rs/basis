# AGENTS

lan follows the same conventions as [mentra](https://github.com/oops-rs/mentra). The sections
below are the operative rules; when in doubt, mentra's AGENTS.md is the reference.

## Philosophy: First Principles

Agents must reason from first principles. Do not rely on conventions, copied patterns, or
assumptions without verification. Identify the fundamental facts, constraints, and invariants;
decompose problems to irreducible components; derive solutions logically from those facts.
Prefer the simplest design that satisfies all constraints, and explicitly verify assumptions
using available evidence. Solutions should be the result of facts → constraints → reasoning →
implementation.

## Project Rules

- Documentation layout follows nous: `docs/PROPOSAL.md` is the why (bets with
  reasons), `docs/ARCHITECTURE.md` is the how and the source of truth for scope and
  layering, `docs/adr/` holds locked decisions (numbered, Context/Decision/
  Consequences), `docs/proposals/` holds deferred ideas (numbered, with Status line,
  trigger conditions, and the properties any implementation must preserve). New
  significant decisions get an ADR; deferred ideas get a proposal, not a TODO.
  `docs/REDESIGN.md` is the ledger of the ADR-0010…0015 transition: what is built,
  what is in between, what is not started. `README.md` and `ARCHITECTURE.md` describe
  the *shipped* state and are updated as phases land, never ahead of them.
- The core principle: **the core has no opinions** — no task-specific types,
  pipelines, or vocabulary (PROPOSAL.md Bet 4).
- **mentra/lan split**: capabilities generic enough for any harness (session branching,
  compaction lifecycle, hook points, MCP client) belong in mentra, not here. lan keeps
  conventions and protocol: context discovery, ACP mapping, the CLI grammar. When lan
  hits a mentra gap, file a mentra issue even if fixing it immediately.
- **Three crates, split by dependency weight** (ADR-0011), so new code has one right home:
  `lan-core` is the SDK and carries no protocol, no transport, and no TTY code; `lan-acp`
  is the ACP adapter over it; `lan` is the binary over both. Anything that would put a
  JSON-RPC, websocket, or terminal dependency into `lan-core` belongs in one of the other
  two. MCP lives behind `lan-core`'s default-on `mcp` feature (ADR-0012).

## Workflow Discipline

- Commit each completed step before starting the next step. Do not batch multiple distinct
  steps into one uncommitted working state.

## Commit Style

- Use Conventional Commits: `<type>(<scope>): <summary>`.
- Types: `feat`, `fix`, `docs`, `refactor`, `chore`, `test`.
- Prefer narrow, concrete scopes matching the actual files or feature area. Avoid generic
  scopes like `core` when they do not name a real, specific area of this repository.
- Write summaries in the imperative mood; describe the change, not the activity.

## Rust Programming

- Prefer `foo.rs` plus `foo/` over `foo/mod.rs`.
- Prefer current edition idioms (edition 2024, MSRV 1.88).
- Run `cargo fmt` after Rust edits.
- Use `cargo check` for fast feedback, `cargo test` for verification, and
  `cargo clippy --all-targets --all-features -- -D warnings` for lint-clean changes.
- Keep modules focused; split large files by responsibility.
- Use the type system to model domain constraints instead of comments or unchecked conventions.
