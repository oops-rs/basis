# ADR-0008: The watch baseline is the workspace after the last successful run

Status: **retired by
[`0014-watch-retired-runs-are-boundable.md`](0014-watch-retired-runs-are-boundable.md)**
· 2026-08-11 (accepted 2026-08-10). The `watch` command is deleted; the
fingerprint and its semantics — §1–3 and the asymmetry rule — survive verbatim
in `Workspace::fingerprint()` / `lan fingerprint`. The baseline policy (§4)
moves to the caller's loop; `--always` (§5) becomes "don't call fingerprint".

## Context

`lan watch "<prompt>" --every 30m` runs the same prompt on a timer. Without a
reason not to, it pays a model on every interval regardless of whether anything
has happened — which is the difference between a scheduler and a bill.

So `watch` skips an iteration when there is nothing new to look at. Deciding
what "nothing new" means is the whole design, and it has one asymmetry that
settles most of it:

> A false "changed" costs tokens. A false "unchanged" silently stops the
> feature working, and nothing in the output says so.

They are not comparable failures. A watch that runs when it did not need to is
noticed on the next invoice; a watch that has quietly stopped is noticed when
somebody wonders why nothing has been reported since Tuesday.

The obvious candidates each fail on that asymmetry:

- **`git status --porcelain`** encodes which files are dirty but not what is in
  them. A file edited twice to different contents produces the same line both
  times — a false unchanged.
- **Newest mtime over the tree** misses deletions entirely, and reads a file
  restored from an older copy (a checkout, an mtime-preserving rsync) as
  unchanged — false unchanged, twice.
- **Hashing every byte** has no false unchanged at all, and reads the whole
  repository on every interval, which is the work a scheduler exists to avoid.

There is a second question the candidates do not raise at all: *when* the
comparison point is taken. A coding agent edits the workspace it was pointed
at. Fingerprinting before a run means the next iteration sees the run's own
edits as a change and runs again — a loop that never skips.

## Decision

**"Unchanged" means the workspace is identical to what the last successful run
left behind.**

1. **The fingerprint is a digest over `(path, length, mtime)` for every file
   the run could see, plus git's `HEAD`.** One `stat` per file, no reads. This
   is the trade `make`, `ninja`, and `cargo` all make; it misses only an edit
   that preserves both length and modified time, which requires deliberately
   forging a timestamp. `HEAD` is in the digest because `git commit` leaves the
   working tree's mtimes alone, so a commit would otherwise be invisible.

   It is a *fingerprint*, compared for equality and never for order, so an
   mtime that moves backwards reads as changed rather than as unchanged.

2. **Which files comes from `git ls-files --cached --others
   --exclude-standard`** when the workspace is inside a work tree: one process,
   `.gitignore` honoured without lan inventing an ignore convention of its own,
   and `.git`'s constant internal churn — which would make every iteration look
   changed — kept out. A workspace that is not a repository gets a plain walk
   that skips `.git` and follows no symlinks.

3. **Every uncertain answer is "changed".** A workspace that is not there, an
   enumeration that produced nothing, a walk that could not read a directory:
   each yields `Snapshot::Unknown`, and the scheduler runs on it. There is no
   path by which lan concludes "unchanged" from a question it could not answer.

4. **The baseline is recorded after a run, and only after one that succeeded.**
   After, so a run's own edits do not retrigger it. Only after success, because
   the baseline answers *what did the last completed run see?* — recording it
   for a failed run would mean an unchanged workspace is never retried, so one
   transient failure would silence the watch until a person happened to touch a
   file.

5. **`--always` switches detection off entirely** — the workspace is not looked
   at at all — for a prompt whose answer depends on something the workspace
   cannot show: the clock, an upstream repository, a service.

## Consequences

- Skipping is cheap: an index read and one `stat` per file, against an interval
  measured in minutes.
- lan spawns `git` for its own scheduling decision. This is lan reading the
  workspace the way it reads `AGENTS.md`, unrelated to the shell grant of
  ADR-0006, which governs what the *agent* may execute. A missing or
  uncooperative `git` degrades to the walk rather than failing.
- A commit made between two iterations, with no working-tree edit, still counts
  as a change. This is deliberate: `HEAD` is part of what a run sees.
- A run that fails while having edited the workspace is retried on the next
  interval, every interval, until it succeeds. Bounded by the interval, which
  is what a scheduler is for.
- The scheduler stays free of task vocabulary (Bet 4). It knows the workspace
  moved or did not; it has no idea what the prompt asks for, and adding one
  would be a design error rather than a feature.
- The 64-bit digest is compared only between consecutive observations within
  one process, so its collision probability is per-comparison rather than
  birthday-bound. If that ever stops being true — a fingerprint persisted
  across runs, say — the digest needs to be widened first.
