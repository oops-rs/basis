# 0013 — The host owns the boundary

> Status: Accepted · 2026-08-11
> Supersedes [`0006-shell-requires-an-explicit-grant.md`](0006-shell-requires-an-explicit-grant.md);
> amends [`0004-kernel-enforced-confinement.md`](0004-kernel-enforced-confinement.md).

## Context

ADR-0004 established that in-process checks are hygiene and the workspace
guarantee is the kernel's. ADR-0006 built a posture on top: shell denied by
default, granted by flag on bare hosts, granted by the image author inside the
shipped Docker container — "the grant travels with the thing that makes it
true."

That posture assumed lan-the-binary running one supervised loop was the
primary unattended case. The redesign changes the premise: lan is embedded,
users run many lan processes inside their own programs and environments, and
the shipped container was already the wrong unit — a host embedding `lan-core`
in its own binary was never going to run inside lan's image. Meanwhile the
flag itself is theater once a process spawns: ADR-0006's own context admits a
path check cannot confine a running command. pi reaches the same conclusion
and ships the honest version: the agent runs with the user's permissions;
isolation, where wanted, comes from the OS, documented rather than shipped.

A harness that cannot run `cargo test` without a flag does very little real
work; ADR-0006 said as much ("this is most of the value") and then defaulted
the value off.

## Decision

**Shell and background execution are enabled by default. lan ships no
container; it documents confinement patterns. The seams stay; the labels stay
honest.**

- `RunConfig::shell` defaults to allowed. Disabling remains one line
  (config, flag, or a `DenyAll`-style approver) for read-only runs.
- The Dockerfile and the image's special-cased grant are removed. A
  `docs/containerization.md` replaces them: read-only-root Docker pattern,
  state volume, and the plain statement that a bare-host run has the user's
  full authority.
- The `.git/hooks` / `.git/config` write-deny carve-out **stays**, exactly as
  labeled in ADR-0006's close: hygiene that shuts the route a model takes
  (the file tools), never a boundary — a shell redirect still lands.
- The `Approver` and hook seams stay, so any embedder or workspace can
  reintroduce gating in one place. What lan stops doing is defaulting to it.
- Nothing in lan may *claim* confinement it does not have: no environment
  sniffing to imply safety (ADR-0006's argument against inference survives
  its default), no "sandboxed" language anywhere in docs or output.

## Consequences

- The honest quadrant to name: **unattended + shell + no OS boundary** is now
  reachable by default. It is the operator's decision, made in their
  environment, with lan's docs stating plainly what authority the process
  holds — pi's posture, adopted knowingly.
- The out-of-the-box experience matches the tool's purpose: the first
  `lan "run the tests"` works.
- ADR-0004's core claim is *amended, not reversed*: the boundary is still the
  kernel's — lan just stops shipping one instance of it and starts documenting
  the patterns. The codex-style native per-command sandbox
  ([`proposals/0002`](../proposals/0002-native-sandbox.md)) remains a
  possible future as an *optional* layer, not a revival of default-deny.
- `LAN_ALLOW_SHELL` and `--allow-shell` are retired; a disable knob replaces
  them.
