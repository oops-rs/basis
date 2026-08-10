# 0009 — An unattended iteration is bounded by default

> Status: **Superseded by
> [`0014-watch-retired-runs-are-boundable.md`](0014-watch-retired-runs-are-boundable.md)**
> · 2026-08-11 (accepted 2026-08-10). The bounds move to `RunConfig` and
> `lan run` flags, all defaulting to unset; the deadline-defaults-to-interval
> coupling dies with the interval. Graceful-trip semantics and the
> no-default-bound-when-attended rule survive in 0014.
> Extends [`0008-the-watch-baseline.md`](0008-the-watch-baseline.md).

## Context

`lan run` is watched by the person who typed it. If a turn goes wrong — the
model circling a problem it cannot solve, a tool loop that never converges —
they see it and press ctrl-C. That person *is* the bound, and it is a good one:
they can tell "thinking hard" from "stuck" in a way no timer can.

`lan watch` has no such person. It exists to run while nobody is looking, and
it was shipped passing only a cancellation token — no deadline, no tool budget,
no token budget. A stuck iteration ran until the model stopped itself, which for
a sufficiently confused model is "not soon", and the next interval arrived
regardless.

The failure is not hypothetical or exotic. It is the ordinary shape of an agent
meeting a hard bug, and it is discovered on an invoice.

## Decision

**Every `watch` iteration carries bounds, and the deadline has a default.**

- `--deadline` defaults to **the interval**. A turn that outlives its own period
  is not converging: the next tick is already due, and whatever the model is
  doing it is not finishing. Bounding it there costs a healthy run nothing — a
  healthy run finishes long before its own interval — and ends a stuck one on
  schedule rather than whenever someone notices.
- `--tool-budget` and `--token-budget` default to **unset**. A useful value for
  either depends on the prompt in a way a default cannot guess, and a default
  that is too small silently truncates good work, which is worse than no
  default at all. They exist for when the work is unattended and the bill
  matters.
- A bound that trips **fails that iteration and the watch continues**. A
  supervisor that dies with its first failure is not a supervisor
  (ADR-0008 already establishes this for run failures; a tripped bound is one).

An explicit `--deadline` longer than the interval is allowed. Overlapping the
next tick is a legitimate choice, and refusing it would substitute lan's
judgement for the operator's on a question only they can answer.

## Consequences

- The default is a bound rather than `None`, which is the inversion that
  matters: an operator who thinks about limits gets what they ask for, and one
  who does not still cannot leave an unbounded turn running overnight.
- A very short interval makes a very short deadline. `--every 2s` with real work
  will trip immediately — correctly, since a 2s cadence is not a claim about
  patience, it is a claim about frequency. Verified live: `--every 2s` tripped
  the deadline before the first tool call, and `--deadline 5m` on the same
  interval let it proceed.
- The token budget is soft by construction: usage is only known once a round has
  streamed in full, so the round that crosses the line always finishes. It ends
  the turn *gracefully*, keeping what was committed, so the work is not thrown
  away for being one round too long.
- `lan run` gains no default bound. It has a person, and a timer that
  interrupted someone mid-thought would be a worse harness, not a safer one.
