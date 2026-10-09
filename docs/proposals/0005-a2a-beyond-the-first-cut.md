# 0005 — A2A beyond the first cut

> Status: Deferred
> Created: 2026-10-09, with [ADR-0028](../adr/0028-a2a-is-the-agent-to-agent-protocol.md),
> which ships the narrow first cut this proposal extends.

ADR-0028 ships an A2A client in mentra, a `.basis/a2a.json` convention, and a
JSON-RPC server in `basis-a2a`. Each item below was left out on purpose. Each
names what would make it worth building and the properties any build must keep.

## 1. Approval by `INPUT_REQUIRED`

Today an inbound run is `never` or `always`, and `prompt` is refused at start-up.

**Shape.** A served task whose turn hits the approver moves to
`TASK_STATE_INPUT_REQUIRED`, with the pending tool call as a data part. The
caller relays it to its own human and answers with a message on the same
`taskId`. The turn stays parked inside a `basis-tasks` `PromptHost` whose
approver waits on that message.

**Trigger.** An agent people want to call over A2A that is useless without
some consequential tool, and its operator won't grant `always`.

**Must preserve.**
- Only one turn parks per task.
- The park has a deadline, and the answer `no` is the default when it expires.
- An approval answers exactly the call it was asked about: it carries the call
  id and is refused if the call id doesn't match.
- A cancel while parked cancels.

## 2. Call depth and budget across agents

An agent can call an agent that calls the first one back. Within one data
directory `basis-tasks`' wait-edge policy prevents cycles, but nothing carries
depth across the wire. Every served task is still bounded by its deadline and
budgets, so a loop is expensive, not infinite.

**Shape.** An A2A extension (a URI in the card, a field in message metadata)
carries a hop count and the remaining deadline. The client increments the hop
count. The server refuses past a cap and clamps its own deadline to what's
left.

**Trigger.** The first observed loop, or the first agent that fans out to more
than one other agent.

**Must preserve.**
- Absent metadata means hop 0, so agents that aren't basis still interoperate.
- A deadline can only shrink across a hop.

## 3. Push notifications

**Trigger.** A caller that can't hold a poll loop open: a serverless function,
or a chat bot whose process restarts more often than tasks finish.

**Must preserve.**
- Delivery is at least once.
- Delivery carries the configured credentials.
- A webhook URL is validated before anything is sent to it (no loopback or
  link-local targets unless the operator allows them).
- Config lives with the task directory, so it survives a restart as the task
  does.

## 4. More than one principal

Today every holder of the server's token sees every task.

**Trigger.** Two callers who shouldn't see each other's tasks share one served
agent.

**Must preserve.**
- `ListTasks` and `GetTask` are scoped to the caller.
- "Not found" and "not yours" are indistinguishable.
- The principal is recorded with the task, not inferred later.

## 5. A streaming client

The mentra client polls `GetTask`.

**Trigger.** A host that wants a remote agent's progress live in its own UI;
ibot's Feishu card is the likely first one.

**Must preserve.**
- A dropped stream falls back to `SubscribeToTask` and then to polling, never
  to a resend.
- Events are applied in order.

## 6. The HTTP+JSON and gRPC bindings

**Trigger.** A caller that speaks only one of them. gRPC also requires that
whoever needs it accept the `a2a-pb` (tonic/prost) weight in a crate of its own.

**Must preserve.**
- Every binding is functionally equivalent to JSON-RPC, as the spec requires.
- `basis` still links no transport.
