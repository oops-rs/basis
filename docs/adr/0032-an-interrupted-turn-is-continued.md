# 0032 — An interrupted turn is continued, and a cancel interrupts

> Status: Accepted · 2026-10-10
> Amends [`0019-the-filesystem-is-the-coordination-surface.md`](0019-the-filesystem-is-the-coordination-surface.md)
> — two of its consequences: "cancellation is boundary-granular" and "a crash
> mid-turn loses the in-flight round".
> Applies [`0005-mentra-coevolution-discipline.md`](0005-mentra-coevolution-discipline.md)
> ([mentra#64](https://github.com/oops-rs/mentra/issues/64)).

## Context

ADR-0019 accepted two costs of having no resident process: a crash mid-turn
loses the round in flight, and a cancel waits for the turn to end. Running
basis agents for weeks, and serving them over A2A, showed both were worse in
practice than on paper.

1. **A crash in a tool round was recorded as success.** mentra's recovery
   kept a transcript as soon as any assistant message was committed, including
   one that only asked for tool calls. A process that died while those calls
   ran came back with unanswered calls and no turn to resume, and basis-tasks
   read the extra assistant message as "the prompt was answered". It settled
   the task **Succeeded** with partial text. The A2A server met it as a task
   parked on a question to its caller that completed empty after a restart.
2. **A cancel could not stop a long turn.** A tool round can run for an hour.
   A2A's `CancelTask` answers with the task as it stands, and a caller — the
   official SDK's tests among them — reads anything but `CANCELED` as a failed
   cancel.
3. **The pieces to do better already exist.** A turn runs under a
   cancellation token, and mentra's run is already the unit of rollback and
   bookkeeping.

## Decision

1. **mentra tells a finished turn from an interrupted one.** A run is
   finished only if its last committed message is an assistant message with
   no unanswered tool calls. Recovering any other committed run closes each
   unanswered call with an error result that says the process stopped while it
   ran, its effect is unknown, and it was not retried, so the model checks
   before repeating it. No call is ever re-executed by recovery. The turn is
   then *interrupted*, and `continue_interrupted` carries it on from the
   transcript's end without a new user message. A run with nothing committed
   rolls back and replays as before.
2. **basis-tasks continues an interrupted turn and never settles it.** On
   attach, an interrupted conversation continues the turn it belongs to: the
   in-flight message's, or the prompt's. The old rule — a committed turn past
   what the task inherited means the prompt was answered — holds only for a
   conversation that is not interrupted.
3. **A cancel cuts the running turn short.** While a turn runs, the executor
   watches for the cancel marker and cancels the turn's token when it appears.
   mentra unwinds the turn, and the task settles `Cancelled`. The deadline and
   the host going away still cut a turn short as before.
4. **A host that shuts down records nothing.** It stops its drivers, which
   abandon their turns as a crash would, and waits for them, so no thread
   outlives it to record a terminal. The next start continues the turn.

## Consequences

- A crash mid-turn no longer loses the round or fakes a success. A tool that
  was running is reported to the model as of unknown effect rather than run
  again, so a side effect still happens at most once.
- `basis cancel` and A2A `CancelTask` take effect within the poll interval
  (100 ms), not at the end of the turn. A tool that ignores its cancellation
  can still outlast that; the deadline remains the backstop.
- A served question asked before a restart is asked again under a fresh call
  id, and an answer to the old one is refused (ADR-0030 §7 holds as written).
- A continued turn re-sends the conversation so far to the model, which costs
  one more request than a turn that never crashed.
