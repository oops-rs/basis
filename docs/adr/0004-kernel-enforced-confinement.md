# ADR-0004 — Confinement is kernel-enforced; in-process checks are hygiene

**Status:** Accepted (v1) · Amended by
[`0013-the-host-owns-the-boundary.md`](0013-the-host-owns-the-boundary.md)
(2026-08-11): the claim stands — the boundary is the kernel's, in-process
checks are hygiene — but basis no longer *ships* a boundary. The Docker image is
withdrawn in favor of documented confinement patterns; the native sandbox of
`proposals/0002` remains a possible optional layer, not a default.

## Context

The one hard safety requirement: the agent must not modify files outside its
workspace. In-process path checks cannot deliver this — any shell command escapes
them. codex proves the per-command OS sandbox (Seatbelt on macOS, bubblewrap+seccomp
on Linux, `workspace-write` policy, `.git`/agent-config read-only *inside* the
workspace); pi ships no permission system at all and tells users to containerize.

## Decision

v1 confinement is **Docker**: `--read-only` container, workspace as the sole rw
mount, state on a named volume. Mentra policy hooks add *hygiene* inside the
boundary — write-deny on `.git/hooks` and agent config dirs (codex's anti-escape
carve-out) — but are never claimed as the boundary. The codex-style per-command
native sandbox is the committed v2 path for Docker-free installs, not an option
being weighed.

## Consequences

- The guarantee is the kernel's; basis code reviews don't carry safety burden.
- Unconfined bare-metal runs are possible and documented as such — matching pi's
  honesty rather than implying safety we don't enforce.
- Network egress stays open in v1 (LLM APIs need it); an allowlist proxy is a
  contained later addition.
