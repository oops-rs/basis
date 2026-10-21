# 0043 — A host may speak an extension of its own

> Status: Accepted · 2026-10-21
> Extends [`0033-a-host-may-run-the-tasks-it-serves.md`](0033-a-host-may-run-the-tasks-it-serves.md):
> the card declares a hosted agent's own extensions, and `HostedStart`
> carries what a message says beyond its parts.

## Context

A host that runs its own tasks often speaks a protocol of its own on top of
A2A. An agent answering a knowledge service's structured requests is one; an
agent that serves several products' code, and is told which one through
`--meta project=…`, is another. A2A has the place for both: a card's
`capabilities.extensions` (uri, description, required, params) says what an
agent speaks, and a message's `metadata` and `extensions` say what one
message uses, beside the request's own `metadata` (ADR-0040 sends exactly
that).

basis-a2a dropped every one of them on the way to a host. The card's
extensions were a fixed list of basis's own (hops, approval), and
`HostedStart` carried the parts, the rendered prompt, the hop and the
deadline, and nothing else: a host could read a data part but not a
parameter beside it, and could not say on its card what it spoke.

## Decision

**A host declares its own extensions, and is handed what a message says.**

1. `AgentIdentity::with_extension(AgentExtension)` adds an extension to the
   card, after basis's own. basis does not read it: what it means is the
   host's.
2. `A2aService::open` refuses one the card could not truthfully carry
   (ADR-0028 §5, a card claims what is served):
   - `required: true` — `OpenError::ExtensionRequired`. Nothing here refuses
     a message that does not use the extension, so the claim would be false.
     Enforcing it is a later decision, with its own trigger: a host that
     must refuse such messages.
   - a URI basis declares itself (hops, approval — whether or not the
     feature is on), or one declared twice — `OpenError::ExtensionDeclared`.
3. `HostedStart` gains `metadata` (the message's), `request_metadata` (the
   `SendMessage` request's) and `extensions` (the URIs the message names),
   each exactly as sent and empty when absent. The hops entry stays in
   `metadata` after the server has read it; the host may ignore it.

## Consequences

- An agent's protocol stays out of basis: basis carries a URI, a params
  object, and maps of JSON, and interprets none of them (PROPOSAL Bet 4).
- A caller's `--meta` parameters reach a hosted agent as `request_metadata`,
  so an agent that asks for `metadata.project` on its card can read it.
- A `basis-tasks` served agent still sees neither: its task is a prompt.
  Handing parameters to a workspace's model is a different question, and
  nobody has asked it yet.
- A reply to `INPUT_REQUIRED` (`HostedReply`) still carries only its parts
  and text.
