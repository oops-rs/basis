# 0002 — Native per-command sandbox (Docker-free confinement)

> Status: Deferred — committed v2 direction per
> [`../adr/0004-kernel-enforced-confinement.md`](../adr/0004-kernel-enforced-confinement.md).
> Created: 2026-08-08
> Related: codex `codex-rs/sandboxing/` (reference implementation).

## Summary

Per-command OS sandboxing so bare-metal `basis` runs get the workspace guarantee
without Docker: Seatbelt (`sandbox-exec`) on macOS, bubblewrap + seccomp on Linux,
following codex's `workspace-write` design.

## Motivation

Docker delivers the v1 boundary but costs a daemon, an image, and mount ceremony —
friction for the "embed anywhere" story, and unavailable in some host contexts.
codex proves the per-command wrapper shape in production.

## Design notes from the codex read (2026-08-08)

- Wrap at spawn time, not process start: rewrite each shell command's argv
  (`sandbox-exec -p <profile>` / bwrap helper) immediately before exec.
- `workspace-write` policy: writable roots = workspace + `/tmp`; `.git`,
  agent-config dirs read-only *inside* writable roots (first-time-creation gap:
  deny both `literal` and `subpath`).
- Network default-deny is layered: seccomp filter + netns unshare (Linux), profile
  omission (macOS); fail closed when a proxy is configured but unresolvable.
- TOCTTOU: refuse to sandbox when a protected read-only path crosses a writable
  symlink.
- Denial detection is heuristic (stderr grepping) in codex — design a structured
  signal instead if mentra's shell tool can carry one.

## Properties any implementation must preserve

- Same policy vocabulary as the Docker preset, so config is portable across modes.
- Enforcement lives below the tool layer (mentra shell tool integration), not in
  basis's protocol code.
- Windows: out of scope; document WSL2 like everyone else.
