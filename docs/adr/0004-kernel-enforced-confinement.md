# ADR-0004 — Confinement is kernel-enforced; in-process checks are hygiene

**Status:** Accepted (v1)

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

- The guarantee is the kernel's; lan code reviews don't carry safety burden.
- Unconfined bare-metal runs are possible and documented as such — matching pi's
  honesty rather than implying safety we don't enforce.
- Network egress stays open in v1 (LLM APIs need it); an allowlist proxy is a
  contained later addition.
