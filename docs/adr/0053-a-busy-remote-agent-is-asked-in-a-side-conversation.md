# 0053 — A busy remote agent is asked in a side conversation

> Status: Accepted · 2026-11-20
> Amends [`0029-a-remote-agent-is-a-spawn-target.md`](0029-a-remote-agent-is-a-spawn-target.md)
> §5 (one conversation per remote agent, one task at a time in it) and
> [`0042-a-delegation-may-run-in-the-background.md`](0042-a-delegation-may-run-in-the-background.md)
> §2 (a remote agent's handle is its name).

## Context

iBot, live-testing Code Mode on basis 0.27.1, asked one remote agent three
independent questions in one turn: `spawn({input: "@ios/nous-ios …",
background: true})` three times, then `wait`. All three were sent within
0.1 s; only the last answer came back. Two faults combined:

- ADR-0042 §2 made a background remote delegation's handle the agent's name,
  on the ground that ADR-0029 §5 already allows one task per remote agent at
  a time. `Children::start` inserted under that name, so the second start
  replaced the first one's slot and its answer had nowhere to arrive.
- ADR-0029 §5's refusal reads the conversation record's outstanding task,
  which is written only once the remote agent names the task. Asks made in
  quick succession all read "nothing outstanding" and were all sent — into
  *one* A2A conversation, where the agent reads them as follow-ups, not as
  independent questions.

A patch fixed the loss first: a new task to a remote agent this agent has working in
the background is refused, and a running slot is never replaced. That keeps
ADR-0029 §5 but leaves the fan-out impossible. The workaround — background
subagents that each ask once, since a delegated child keeps its own
conversations (ADR-0042 §6) — does not reach most iBot callers: a person
scoped to some workflows may ask an agent directly but may not start the
workers that would each ask it once. For them, asking one agent several
things at once has to be `spawn`'s own.

The fact that matters is in ADR-0029 §5 itself: a remote agent's
*conversation* runs one task at a time. Nothing says an agent may be in only
one conversation with a caller. An A2A agent keys its work by `contextId`;
two contexts are two independent conversations to it.

## Decision

**A new task to a remote agent whose conversation is held by a background
delegation of the same caller opens a side conversation with it — a fresh
conversation of its own, under its own handle `<agent>#<n>` — instead of
being refused or sent into the busy one.**

1. **When.** The calling agent hands `@<agent> <task>` to a remote agent it
   has working in the background under `<agent>`. Blocking or background, the
   new task goes to a side conversation: it is an independent question, and
   the main conversation is busy. When the main conversation is not held by
   a running background delegation, nothing changes: the task continues the
   main conversation, and ADR-0029 §5's refusal of a task outstanding from an
   earlier call still stands, because that one is how a restart avoids
   handing the same work off twice.
2. **The handle.** Side conversations with one agent are numbered from 2 per
   calling agent and name — the main conversation is the bare name, and reads
   as the first — and are never renumbered. `<agent>#<n>` is the handle a
   background start returns, what `wait: [...]` and `@<agent>#<n>` alone
   collect, and what every guidance line names, so "write `@nous#2` alone to
   keep waiting" means the side conversation it came from.
3. **A side conversation can be continued.** `@<agent>#<n> <text>` continues
   side conversation *n* — answering a question it asked, or a follow-up —
   and `@<agent>#<n>` alone waits on its outstanding task when nothing is
   running under the handle. A number that was never opened is refused. A
   side conversation held by a running background delegation refuses a new
   task with ADR-0029 §5's words, naming the handle: a side conversation is a
   conversation, one task at a time.
4. **`#<n>` is not part of the target.** The parser reads the suffix into a
   field of its own. `spawn`'s preview keeps `target` the bare agent name and
   `reach`, `files` and `metadata` exactly as before, so an approver's rule
   and a host's gate written against the target match a side conversation as
   they match the main one, and the target charset (ADR-0021, ADR-0037 §8)
   is unchanged. A `#<n>` on a name that is not a remote agent is refused:
   only a remote agent has side conversations.
5. **The same path.** A side conversation is resolved by the same resolvers
   and exchanged through the same `Peer` as the main one; only the record it
   reads and writes differs. Whatever a host's resolver puts on the way out —
   iBot's `dial-on-behalf-of` and `Dial-Chain` — goes out on a side
   conversation too.
6. **Kept in memory, by name.** A side conversation's record lives beside the
   main ones, apart from them, for as long as the workspace open does, and is
   never written to disk or under a host's key (ADR-0035). It is keyed by the
   calling agent and the name as written, not by the resolved id: the handle
   is what the model names, and the main record still follows the agent's id
   (ADR-0037 §6). A process that restarts forgets its side conversations,
   and a side task outstanding then is not picked up again — the price of a
   conversation that exists to answer one question.

Width is not this decision's. How many delegations run at once is the
runtime's `with_delegation_width` (ADR-0042 §5), and a host's own concern;
side conversations count toward it like any background delegation.

## Consequences

- A fan-out to one agent is `spawn` with `background: true` once per question
  and, if the model wants the answers sooner than they come, one `wait` on
  `<agent>`, `<agent>#2`, `<agent>#3`. No host change is needed for it.
- `Spawn::side()` reports the suffix; `Spawn::target()` stays the bare name.
  The preview gains no key.
- `spawn`'s description, where remote agents and background work are both
  taught, says that a busy agent is asked in a side conversation and how to
  continue one.
- Not done: persisting side conversations, and a model-chosen side
  conversation for an idle agent (`@<agent>#new`). Each is a proposal when a
  host asks.
