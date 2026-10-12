# 0034 — A hosted task may ask its caller

> Status: Accepted · 2026-10-12
> Amends [`0033-a-host-may-run-the-tasks-it-serves.md`](0033-a-host-may-run-the-tasks-it-serves.md)
> §1 and its Consequences — `TaskHost` gains a sixth method, with a default,
> and `HostedStanding` a sixth state.
> Preserves [`0030-a2a-is-spoken-in-full.md`](0030-a2a-is-spoken-in-full.md)
> §7 — the approval extension is still a `basis-tasks` mechanism, and a
> hosted server still refuses `InboundApproval::AskCaller`.

## Context

ADR-0033 let a host run the tasks `basis-a2a` serves, and left one thing
behind: asking. `INPUT_REQUIRED` was reachable only through the approval
extension, which parks a `basis-tasks` turn inside a `PromptHost`, and a host
that runs its own tasks has no turn basis can park. Its Consequences said a
sixth method would come when a host needed one.

1. **A host needs one.** Nous runs investigations that sometimes cannot go
   on without a fact only the caller has — which branch, which environment,
   which of two readings of the question. Failing the task and asking the
   caller to start again throws away the work done so far and the task id the
   caller is watching; answering on a guess is worse.
2. **`INPUT_REQUIRED` is core A2A.** The spec defines it as an interrupted
   state: a blocking `SendMessage` answers there, a stream closes there, and
   the caller replies with a message carrying the task's `taskId`. None of it
   needs an extension; the approval extension only standardises one kind of
   question and its answer.
3. **The server already answers a message with a `taskId`.** For a
   `basis-tasks` task it is the answer to an open approval question, checked
   for role, `messageId`, owner and `contextId`; anything sent to a task not
   asking is refused as "not waiting for input". The reply path exists; it
   has only one backend.
4. **Everything else follows from the standing.** Streams, push monitors,
   `GetTask`, `ListTasks`, the live-task quota and `CancelTask` all read where
   a task stands. A paused hosted task that reports itself as such needs no
   new machinery in any of them — only the places that wait for *settled*
   must also stop at *asking*, as they already do for a `basis-tasks` task.

## Decision

**A host pauses a task by reporting `HostedStanding::InputRequired { parts }`,
and receives the caller's reply through `TaskHost::reply`. The server maps
both to core A2A and nothing else.**

1. **The question is a standing.** `InputRequired { parts }` is
   `TASK_STATE_INPUT_REQUIRED`, its parts the status message, from the agent.
   It has not settled: the host's `settled` does not resolve on it, the
   server's driver keeps waiting through it, and a restart awaits the task
   again as it would any unsettled one.

2. **The reply is a method, refused by default.**
   `reply(id, HostedReply) -> Result<(), HostError>` hands the host the
   reply's `messageId`, its text — rendered as `HostedStart::prompt` is, so
   text parts joined by blank lines, data as labelled JSON — and its parts as
   sent. The default returns `HostError::Refused("this agent takes no
   replies")`, so every existing host compiles and behaves as before. On `Ok`
   the task must no longer be asking that question; it is working again
   under the same A2A task id.

3. **A message with a hosted task's `taskId` is checked as an approval
   answer is.** Role user and a `messageId`, a task the caller owns (another
   principal's is not found), a `contextId` that matches if given, and no push
   config. A task that is not `InputRequired` is refused as "not waiting for
   input", exactly as before. The parts are checked as any message's are. The
   host's refusal maps as `start`'s does: `Refused` is invalid params, `Busy`
   the wait error, `Internal` a logged server failure. The task stays asking.

4. **Then it is `SendMessage` on that task.** A blocking reply waits until
   the task settles or asks again; `returnImmediately` answers at once; a
   streaming reply follows the task. The server reads a hosted task's standing
   at the stream's cadence for this, since the seam has no question channel.

5. **Every other surface reads the standing.** A stream closes on the
   `INPUT_REQUIRED` update and `SubscribeToTask` after the reply streams the
   resumed task; push notifications hear the question and the resumption (a
   reply records the working status at once, so a host that asks again
   before the monitor's next look is still heard resuming, and two different
   questions in a row are two changes); `GetTask` and `ListTasks` show it;
   `CancelTask` cancels it; the live-task quota counts it.

6. **The card is unchanged.** `INPUT_REQUIRED` needs no declaration. A hosted
   server never lists the approval extension, and still refuses
   `InboundApproval::AskCaller` at `open`: what a host asks is its own
   question, not an approval of a tool call.

## Consequences

- Nous, and any host, can ask its caller and resume where it was, under the
  task id the caller is already watching.
- The change is additive: a `#[non_exhaustive]` variant, a defaulted trait
  method and a new `#[non_exhaustive]` struct. No existing host changes.
- A host that asks must keep its task alive across the wait — the caller may
  answer much later, or never; the task's deadline still bounds it, and
  cancelling it is the caller's way out.
- A reply is not part of the task's `history`, which remains the message that
  started it. A host that wants the exchange keeps it.
- A question is noticed by polling the host's standing, at the cadence a
  stream already polls; a host-side question channel is deferred until the
  latency matters.
