# 0014 — watch is retired; runs are boundable

> Status: Accepted · 2026-08-11
> Retires [`0008-the-watch-baseline.md`](0008-the-watch-baseline.md);
> supersedes [`0009-bounded-iterations.md`](0009-bounded-iterations.md);
> bounding rule reconciled by
> [`0020-spawn-routing-is-decided-by-the-environment.md`](0020-spawn-routing-is-decided-by-the-environment.md) · 2026-08-16.

> **Reconciliation (ADR-0020):** "bounding is now explicit, everywhere" held
> only while a run left nothing behind it. ADR-0017 shipped durable tasks with
> a 30-minute default deadline and did not amend this ADR. The rule survives
> with its subject corrected: the bound belongs to the *checkpoint*, not to the
> attachment. The attended one-shot below still takes no implicit timer; any
> route that mints a checkpoint records the finite default, because a person
> watching a terminal cannot bound an agent they have walked away from.

## Context

`basis watch` was three things wearing one subcommand: a timer, a
change-detector, and per-iteration bounds. The timer is a scheduler opinion —
and scheduling belongs to the host (tokio interval, cron, systemd, CI), which
the SDK direction of [`0010`](0010-the-crate-is-the-workflow-surface.md) makes
practical for the first time. With the loop trivially writable in any language
that can call a CLI or a crate, shipping one inside basis violates Bet 4: the
core acquiring an opinion about the host's event loop.

The other two pieces are not scheduler concepts at all and never were:

- The bounds (deadline, tool budget, token budget) are properties of *any run
  nobody is watching* — every workflow fan-out agent, not just a watch
  iteration. Mentra's run options already carry all three plus a cancellation
  token; basis had merely attached them to the wrong surface.
- The fingerprint's judgment calls (digest over `git ls-files` + `HEAD`;
  every uncertain answer is "changed") took ADR-0008 to settle and are easy
  to get wrong when re-derived.

## Decision

**The `watch` subcommand is deleted. Its pieces move to where they were
always pointed:**

- **Bounds move to `RunConfig`** and to `basis spawn` as `--deadline`,
  `--tool-budget`, `--token-budget`. All default to unset: ADR-0009's
  deadline-defaults-to-interval coupling dies with the interval. An attended
  `basis spawn` still gets no implicit timer (ADR-0009's last consequence
  survives; `basis run` remains an alias); an unattended caller states its bounds, and the recipe shows
  how. A tripped bound stays a graceful end — committed work is kept.
- **The fingerprint survives as a utility**: `Workspace::fingerprint()` in
  `basis-core` and a `basis fingerprint` subcommand printing the hash for shell
  composition. ADR-0008's semantics carry over verbatim — `git ls-files`
  enumeration, `HEAD` in the digest, `stat`-only reads, uncertain resolves to
  changed. The *baseline policy* (record only after success) moves to the
  caller, where the definition of "success" now lives.
- **Exit codes become part of the CLI contract**
  ([`0015`](0015-cli-grammar.md)): success, run failure, and tripped bound
  are distinguishable, because a script can only branch on what the process
  reports.
- **The recipe replaces the feature**: an `examples/` entry showing interval
  + fingerprint + bounded run in a few lines of host code — kept less as
  documentation than as the standing acceptance test that the SDK surface is
  sufficient. If that example stops being trivial, the regression is in the
  API.

## Consequences

- The binary's story simplifies to the grammar of
  [`0015`](0015-cli-grammar.md); the scheduler module, its CLI, and
  `--always`/`--every` vocabulary are deleted.
- Callers who roll their own loop and skip change detection will burn tokens
  on idle repos; the recipe and the `fingerprint` subcommand exist so the
  right thing is one line. basis no longer prevents the wrong thing — that is
  the host's loop now.
- ADR-0008 is retired with its command, but its asymmetry rule (a false
  "changed" costs tokens; a false "unchanged" silently kills the loop) is
  load-bearing inside `fingerprint()` and must survive any reimplementation.
- ADR-0009's inversion ("the default is a bound") no longer holds anywhere:
  with no shipped unattended mode, there is no surface for basis to default it
  on. Bounding is now explicit, everywhere, by design.
