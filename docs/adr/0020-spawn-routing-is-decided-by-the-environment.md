# 0020 — Spawn routing is decided by the environment

> Status: Accepted · 2026-08-16
> Extends [`0015-cli-grammar.md`](0015-cli-grammar.md),
> [`0017-structured-agent-concurrency.md`](0017-structured-agent-concurrency.md),
> and [`0019-the-filesystem-is-the-coordination-surface.md`](0019-the-filesystem-is-the-coordination-surface.md).
> Reconciles [`0014-watch-retired-runs-are-boundable.md`](0014-watch-retired-runs-are-boundable.md).

## Context

`basis spawn` had three possible fates — run here and answer, mint a checkpoint
and hand back a handle, or mint one and drive it — and nothing named the rule
that chose between them. The choice fell out of one condition in `main`:

```rust
if args.json && !args.detached && !args.await_result && !has_current_task()
```

So `--json`, a *rendering* flag, decided whether the agent executed. The person
typing `basis "hi"` got a handle to work that had not started; the script passing
`--json` got the attended run. The two audiences were served backwards.

This was not a decision anyone made. ADR-0015 defined the shorthand as
"identical to `basis run`" — the attended one-shot that printed the answer.
ADR-0017 renamed `run` to `spawn` and made `spawn` return a handle
immediately. The shorthand's spelling survived the rename; its meaning did not,
and the refinement note added to ADR-0015 claimed the opposite:

> **Refinement:** ADR-0017 supersedes only the no-subcommand server rule. The
> positional prompt shorthand … remain[s].

Three shipped commitments were left contradicted. The spec's target user is
"humans and scripts starting one-shot agent work from a repository"; `README.md`
promised "the human path carries no ceremony"; and `README.md`'s own
`basis --approve prompt "tidy up the imports"` example could not run at all,
because the only terminal approver lived on the path `--json` gated.

ADR-0019 then removed the reason the handle-first default existed. With no
daemon, execution belongs to whichever process holds the attach lock — so a
handle returned to a shell names an agent that is not merely unfinished but
*unstarted*, and stays that way until something attaches.

## Decision

**The environment picks the lifecycle; flags override it; rendering is
orthogonal to both.**

The environment is the right axis because it answers the only question that
matters: *is there someone with nothing better to do than wait?* A shell is
blocked on this process either way. A parent model turn is the opposite — it
holds a session another agent may need, and blocking it is how ADR-0017's
wait-for cycles start.

`BASIS_TASK_ID` is that question, already answered and already in the environment.

| `BASIS_TASK_ID` | flags | route |
|---|---|---|
| unset | *(none)* | attach |
| unset | `--json` | attended |
| unset | `--await` | attach |
| unset | `--json --await` | attach |
| unset | `--resumable` | resumable |
| set | *(none)* | resumable |
| set | `--json` | resumable |
| set | `--await` | attach |
| set | `--resumable` | resumable |

The three routes:

- **attended** — run in this process and render the stream. Mints no
  checkpoint, so there is no handle and nothing to `send` to. This is the path
  that owns the `run_started`/`run_finished` JSONL contract, which is why
  `--json` alone still selects it.
- **resumable** — mint the checkpoint, print the handle, drive nothing.
- **attach** — mint the checkpoint and drive it here until it settles. The
  handle is durable, so the work outlives this process; the answer is printed
  because this process stayed for it.

Consequent rules:

1. **`--resumable` is the opt-out**, and it is the only spelling that returns a
   handle for work nothing has started. It is what `--await` is not, so the
   pair is rejected rather than resolved by precedence.
2. **`--detached` keeps one meaning**: no parent. It no longer also implies
   nobody is driving. At a shell there was never a parent to detach from, which
   is why it read as a near-no-op there.
3. **`--json` never changes the lifecycle.** The one cell where it selects a
   different one — attended, at a shell, with no `--await` — is the JSONL
   contract predating this ADR, kept because consumers depend on its bookends.
4. **`--approve prompt` is legal exactly where a process is driving the agent
   and has a terminal on stdin.** Under ADR-0019 the executor is whoever holds
   the attach lock, so an attached terminal is what `prompt` needs; the
   attacher is asked, whether it spawned the agent or reached it by
   `basis wait`. Only `--resumable` work is refused, having nobody to ask.

## Reconciling ADR-0014

ADR-0014 decided that "an attended `basis spawn` still gets no implicit timer"
and that "with no shipped unattended mode, there is no surface for basis to
default it on." ADR-0017 then shipped exactly that unattended mode with a
30-minute default deadline, and never amended 0014.

The rule survives with its subject corrected: **the bound belongs to the
checkpoint, not to the attachment.** ADR-0014 was written when an attended run
was purely in-process and left nothing behind; the attended route above still
matches it exactly, taking no implicit deadline. But a checkpoint outlives the
process attached to it, and a person watching a terminal cannot bound an agent
they have already walked away from. So any route that mints a checkpoint —
attach included — records the finite default that `attach::drive` enforces.

## Consequences

- The human path is one command again, and it is the same command the machine
  path uses with a different renderer.
- `basis "…"` no longer returns immediately at a shell. Callers that relied on
  fire-and-forget add `--resumable`; the flag exists for exactly that, and the
  in-repo test fixtures were the first two users of it.
- An attended run at a shell now mints a durable agent directory where the
  older attended path minted nothing, so `send`, `inbox`, `watch`, and `wait`
  reach a run that already answered.
- The matrix is a table in `basis/src/route.rs` asserted cell by cell. A grammar
  change that is not also a change there is a change nobody decided — which is
  the failure this ADR exists to close.
- ADR-0015's refinement note is corrected in place: the shorthand's spelling
  was preserved by ADR-0017, but its meaning was not, and saying otherwise hid
  this defect for two ADRs.
