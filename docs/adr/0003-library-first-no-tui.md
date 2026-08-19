# ADR-0003 — Library first, binary second; no TUI

**Status:** Accepted (v1)

## Context

Coding agents default to shipping as terminal products; embedding surfaces come
later and show it (codex, pi). basis's stated purpose is the inverse: be embeddable
anywhere — in-process, via protocol, or as a subprocess — with presentation owned
by clients.

## Decision

The `basis` crate is the product; the `basis` binary is a thin shell over it. Three
embedding surfaces, in order of preference: (1) in-process via the crate (Rust
hosts), (2) ACP for anything speaking the protocol, (3) `basis spawn --json` as a
subprocess for scripts and CI (`basis run` remains a compatibility alias). No TUI,
themes, or keybindings — ever, in this
repo. Terminal interactivity, if wanted, is an ACP client someone else ships.

## Consequences

- API design is judged by the in-process consumer first; the binary cannot grow
  features the crate lacks.
- No presentation code to maintain; clients own UX entirely.
- Casual terminal use is deliberately worse than a TUI product — accepted;
  that's what existing products are for.
