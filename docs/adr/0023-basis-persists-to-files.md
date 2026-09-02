# 0023 — basis persists to files; memory is a directory convention

> Status: Accepted · 2026-08-25
> Precedent: [`0019-the-filesystem-is-the-coordination-surface.md`](0019-the-filesystem-is-the-coordination-surface.md)
> (E2 — the daemon-era registry was deleted whole, no migration written); related:
> [`0005-mentra-coevolution-discipline.md`](0005-mentra-coevolution-discipline.md)
> (an upstream gap is filed there, not duplicated here).

## Context

basis's durable state is not one shape today; it is two, arrived at
separately and never named as a pattern until now.

Task state — `meta.json`, `inbox.json`, `events.jsonl`, `terminal.json`
(ADR-0019) — is files under a documented directory convention, chosen
deliberately when the daemon was retired: no database to manage, readable
with ordinary tools, one atomic-replace write per update. Memory (0.6.0,
`docs/archive/REDESIGN.md`'s D2 row) followed the same shape by the same reasoning:
one `.md` file per memory, frontmatter for the three fields basis reads
(`name`, `description`, `type`), body free, under two roots basis resolves
and a host can override — no database, no tool, discovery folded into the
context render path memory already had to go through.

Conversation history is neither. It is mentra's `Session` store, reached
through `WorkspaceBuilder::with_store_dir`, and it is SQLite by default —
not because basis chose a database, but because basis has never chosen
anything here at all. The store's format is mentra's to decide, and mentra
has not decided files. A host inspecting basis's durable state today
therefore finds two directories of plain files it can read, version, and
back up with `cp` and `grep`, and one SQLite file it cannot without either
mentra's own store API or `sqlite3` directly — an inconsistency nobody
designed and everybody who has looked at the data directory has noticed.

This is worth writing down for a reason narrower than "SQLite is
unwelcome": it is that basis has, in fact, been choosing a direction for two
features running, and choosing it independently each time rather than
stating it once. The next feature that needs durable state should not have
to rediscover the argument from the shape of `local/` and `memory.rs`.

## Decision

**basis persists to files. Wherever basis's own code chooses the durable
format, the format is files under a documented directory convention, never
a database basis manages. Where the format is not basis's to choose — the
conversation store mentra owns — this ADR records the current state
honestly and names the upstream ask rather than building around it.**

1. **Task state and memory are the shipped instances**, not the whole of the
   rule — the rule is prior to both and is why the second one followed the
   first's shape without a discussion. A future basis-owned durable feature
   defaults to this pattern; departing from it needs its own stated reason,
   the way every deviation in this codebase does.

2. **The conversation store stays mentra's, unforked.** basis does not wrap
   or shadow it with a competing file format of its own — that would be two
   stores for one conversation, the same duplication ADR-0011 refuses for a
   protocol and ADR-0022 just refused for the task layer, applied to a
   store instead of a crate. The file-backed half of this is filed upstream
   as `oops-rs/mentra#28`; when and if it lands, basis adopts it through the
   same `with_store_dir` interface it already has — a directory either way,
   so no basis-side signature changes on that day.

3. **SQLite remains basis's conversation store until then, unconditionally.**
   Not a gap basis is quietly working around, not an interim measure — the
   honest current state, restated here so the next person reading the data
   directory's layout does not have to infer whether the inconsistency is a
   bug or a decision. It is a decision, and this is where it is recorded.

4. **No migration is planned**, on `mentra#28` landing or on anything else.
   The precedent is E2's own: when ADR-0019 retired the daemon, the
   daemon-era registry was deleted whole rather than converted, and every
   task that existed under it was gone with it — the new substrate applied
   going forward, not backward. The same ruling applies here. A conversation
   already in SQLite stays reachable through whatever reads SQLite (mentra's
   store, unchanged); a conversation started after a file-backed store lands
   lives wherever `with_store_dir` was pointed at when it was opened. There
   is no conversion pass, planned or implied, in either direction.

## Consequences

- A host inspecting or backing up basis's durable state finds two shapes:
  files (tasks, memory) usable with ordinary tools, and one SQLite file
  (conversations) that is not. This ADR does not close that gap — it names
  it, says why closing it is not basis's call to make unilaterally, and
  points at where the call is being made.
- The next basis-owned durable feature has a default to reach for without
  re-deriving it: the memory precedent, not the SQLite one.
- `oops-rs/mentra#28` landing is a prerequisite this ADR is content to wait
  on, not a blocker basis works around in the meantime. Building an interim
  file-backed conversation store here would itself be the fork ADR-0005's
  coevolution discipline exists to prevent — the fix belongs upstream
  because the store belongs to mentra, and basis meeting it once it lands is
  cheaper and more honest than basis maintaining a second implementation
  until it does.
- Nothing about `WorkspaceBuilder::with_store_dir`'s signature or behavior
  changes today; this ADR records a direction and a dependency, not a patch.
