# 0040 — A delegation carries parameters

> Status: Accepted · 2026-10-14
> Extends [`0037-an-agent-is-named.md`](0037-an-agent-is-named.md) and
> [`0038-the-core-speaks-to-a-peer.md`](0038-the-core-speaks-to-a-peer.md):
> `spawn` gains one more typed field beside `input` and `reach`, and the
> peer `Message` one more fact.

## Context

Some remote agents need more than the task in words. An agent that answers
about several products' code asks which one in its card ("specify
`metadata.project`"), and refuses a task that does not say:
`params.metadata.project` is how A2A carries it — the request's metadata,
beside the message rather than in it. `spawn "@<agent> <task>"` could say only
words, so a model that read the card had no way to do what it said, and the
agent kept asking.

## Decision

**`spawn` takes an optional `metadata` object, sent beside a task for a remote
agent.**

1. The field is typed, beside `input`, never read out of the string — the
   rule `reach` follows (ADR-0016's one reader). It is offered in the schema
   only when there are remote agents.
2. It goes with `@<agent> <task>` and nothing else. A command, a local task,
   a wait (`@<agent>` alone) and a name that turns out to be a teammate here
   refuse it, saying how to use it: parameters with nowhere to go would be a
   call quietly unlike the one written.
3. `peer::Message::metadata` carries it to the peer. Over A2A it is the
   request's `metadata` (mentra 0.30.1 `A2aSend::with_request_metadata`);
   the message's own `metadata` stays the extensions' (the hops extension).
4. The authorization preview shows it under `metadata`, additively: absent
   when the call gave none, so a rule written against the other keys still
   matches.

## Consequences

- A model can follow an agent card that asks for parameters, and a host's
  approver sees them before the task leaves.
- What the parameters mean is the agent's to say; basis forwards an object
  and validates only that it is one.
