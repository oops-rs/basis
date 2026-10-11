# 0033 — A host may run the tasks it serves

> Status: Accepted · 2026-10-11
> Amends [`0028-a2a-is-the-agent-to-agent-protocol.md`](0028-a2a-is-the-agent-to-agent-protocol.md)
> §3 — an A2A task is a `basis-tasks` task *by default*, no longer by
> construction.
> Preserves [`0030-a2a-is-spoken-in-full.md`](0030-a2a-is-spoken-in-full.md)
> in full: every method, both bindings, principals, hops, push and the
> extended card mean the same thing whoever runs the task.

## Context

`basis-a2a` serves one thing: a basis workspace's agent, each accepted
message one `basis-tasks` spawn. That is the right default and the wrong
limit.

1. **Some hosts already have a run machine.** A host built on basis — Nous is
   the first — drives its own runs: its own roster, its own evidence, its own
   persisted run rows and its own answer shape. Serving it through a
   `basis-tasks` spawn would put a second, generic agent in front of the one
   the caller wanted.
2. **What the server does is mostly not the task.** Of `basis-a2a`'s work,
   the wire is two bindings, the card, principals and their scoping, the
   live-task quota, stream caps, hops and deadlines, push delivery behind an
   SSRF guard, and the index that maps A2A ids, contexts and message ids. None
   of it depends on *who* runs a task. A host that rebuilt it would rebuild
   the hardening with it.
3. **The server touches the task layer at a handful of points.** It starts a
   task (with a prompt, a continuation, a deadline and a hop), reads whether
   the task has settled and how, reads whether it is running, cancels it,
   waits for it to settle, and reads its progress. Those points are the whole
   seam.
4. **Asking the caller is a `basis-tasks` mechanism.** `INPUT_REQUIRED` parks
   a turn inside a `PromptHost`; a host that runs its own tasks has no turn
   basis can park.

## Decision

**`basis-a2a` serves either a workspace's `basis-tasks` (the default) or a
host's own tasks, through one small trait. Everything above the seam is
shared.**

1. **`TaskHost` is the seam.** A host implements:
   - `start(HostedStart) -> Result<String, HostError>` — begin the work a
     message asks for and return the host's own id for it. `HostedStart`
     carries the caller's principal name, the `contextId`, the host id of the
     latest task in that context when the message continues one, the
     rendered prompt, the parts as sent, the hop, and the absolute deadline
     (already clamped to the caller's; the host must not run past it).
   - `standing(id) -> Result<HostedStanding, HostError>` — `Submitted`,
     `Working`, `Completed { parts }`, `Failed { error }` or `Canceled`.
   - `cancel(id)` — ask the task to stop. `CancelTask` then answers with
     the standing after the grace it already waits.
   - `settled(id)` — resolves once the task has settled (or at once, when it
     has). The service's driver awaits it where it would otherwise attach.
   - `progress(id)` — a stream of progress notes from now on, ending when
     the task settles. Each note becomes a `WORKING` status update, as an
     assistant message or a tool start does for a `basis-tasks` task.

   `HostError` says whose fault a refusal is: `Busy` (the context is still
   working — the caller waits, as with `basis-tasks`' one executor per
   conversation), `Refused` (told to the caller as invalid params) and
   `Internal` (logged; the caller is told only that the server failed).

2. **`ServeConfig::hosted(identity, state_dir, host)` opens it.** The index,
   the push store and every other file the server keeps live under
   `state_dir`; nothing is written to a workspace. The settings that
   configure a `basis-tasks` run — `with_shell`, `with_run_defaults`,
   `InboundApproval::AskCaller` — are refused at `open` with a hosted
   backend, by name, rather than ignored: they would mean nothing, and a
   setting that silently does nothing is a typo nobody hears about.

3. **A completed task's parts are the host's.** A `basis-tasks` task
   completes with one text artifact, as before. A hosted task completes with
   the parts its host returns — text, data, or both — as the one `result`
   artifact. The server checks nothing in them beyond what the wire requires;
   the answer's shape is the host's business.

4. **The A2A task id is the server's.** For a `basis-tasks` task it stays the
   handle's task part; for a hosted task the server mints one and keeps the
   host's id beside it in the index. A caller never sees a host id, and a
   host never parses an A2A one.

5. **Restart is the host's to survive.** On `open`, every hosted task the
   index holds that has not settled is awaited again through `settled`. A
   host whose task did not survive its own restart reports it `Failed` with
   the reason; the server never re-sends a message to `start`.

6. **Everything else is unchanged.** Principals and task scoping, the
   live-task quota, stream caps, hop cap and deadline clamping, push
   notifications, both bindings, the card and extended card behave the same
   for both backends, because none of it is in the seam.

## Consequences

- A host serves its own agent over A2A in full by implementing five methods,
  and inherits every hardening fix `basis-a2a` ships from then on.
- The `basis-tasks`-backed path is unchanged for every existing caller,
  including `basis serve --a2a`.
- `INPUT_REQUIRED` is a `basis-tasks` feature. A host that wants to ask its
  caller needs a sixth method; none is added until a host needs one.
- `basis-a2a` still links `basis-tasks`, because the default backend is it.
  Feature-gating that weight out for hosted-only embedders is deferred until
  one asks.
