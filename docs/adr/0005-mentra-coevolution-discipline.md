# ADR-0005 — Co-evolution discipline: mentra gaps become mentra issues

**Status:** Accepted (v1)

## Context

The same author owns mentra and lan. That removes the usual friction of upstream
contribution — and the usual forcing function that keeps a consumer honest. The
easy path when lan hits a mentra gap is a lan-side workaround; enough of those and
mentra's API story stops being legible to anyone else (the original zentox problem,
reproduced internally).

## Decision

When lan needs something mentra-shaped, it lands **in mentra**, and it is **filed
as a mentra issue even when fixed immediately** in the same sitting. The test for
"mentra-shaped": would a *different* harness plausibly want it? Current queue
(p0-groundwork §4): entry-tree session branching, compaction checkpoint semantics,
public skills API, tool profiles, assembly-level test harness. lan may carry a
temporary workaround only with a linked mentra issue and a removal note.

## Consequences

- mentra's issue tracker becomes the real API-feedback channel (the zentox loop,
  made permanent); outside users see why the API moves.
- lan PRs stay reviewable: conventions and protocol only.
- Slightly slower than hacking in place — accepted; the seam is the product.
