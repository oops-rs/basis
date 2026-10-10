# 0030 — A2A is spoken in full

> Status: Accepted · 2026-10-10
> Amends [`0028-a2a-is-the-agent-to-agent-protocol.md`](0028-a2a-is-the-agent-to-agent-protocol.md)
> §1, §4, §5 and §6 — the narrow first cut becomes the whole protocol over two
> bindings.
> Closes [proposal 0005](../proposals/0005-a2a-beyond-the-first-cut.md) §1–§5
> and the HTTP+JSON half of §6; gRPC stays deferred there.
> Applies [`0005-mentra-coevolution-discipline.md`](0005-mentra-coevolution-discipline.md):
> the client half is mentra's.

## Context

ADR-0028 shipped the smallest A2A that let one agent reach another: JSON-RPC
only, `SendMessage`/`GetTask`/`CancelTask` on the client, and a server that
refused push notifications, the extended card, non-text parts, a reply to a
question, and any second principal. Proposal 0005 deferred each with a
trigger. The triggers have arrived together, because the goal was never one
pair of agents: it is basis agents running for weeks among agents other people
build.

1. **A long-running caller cannot rely on one poll loop.** A task outlives a
   connection; a stream that drops must be picked up, not re-sent, and a caller
   that cannot hold a connection at all needs to be called back.
2. **Other people's agents choose their binding.** The spec requires an agent
   to serve only one of JSON-RPC, HTTP+JSON and gRPC. A client that speaks one
   reaches a third of them.
3. **A served agent shared by several callers must keep them apart.** One
   token that sees every task is a single-tenant server.
4. **A served agent that can never ask is useless for consequential work**
   whose operator will not grant `always`.
5. **Two agents can call each other.** Nothing carried a depth or a deadline
   across the wire, so a loop cost a deadline per hop.
6. **gRPC is still weight.** `a2a-pb` is tonic, prost and a vendored `protoc`
   (ADR-0028 Context 3). JSON-RPC and HTTP+JSON both ride the `reqwest` and
   `axum` already linked.

## Decision

**basis speaks A2A v1.0 in full — every method, both directions — over the
JSON-RPC and HTTP+JSON bindings. gRPC stays deferred for its weight.**

1. **Two bindings, both sides.** mentra's client selects the first interface
   in the card's own order whose binding it speaks (`JSONRPC` or `HTTP+JSON`)
   and whose protocol major is 1, so the agent's preference is honored. It
   sends the interface's `tenant` on every request that carries one.
   `basis-a2a` serves both and lists JSON-RPC first. The two are functionally
   equivalent, as the spec requires: one service, two routers.

2. **Every method.** The client gains `SendStreamingMessage`,
   `SubscribeToTask`, `ListTasks`, the four push-notification-config methods
   and `GetExtendedAgentCard`; the server answers all eleven.

3. **Streaming settles a task, and a dropped stream is never a resend.**
   mentra's exchange still submits with `returnImmediately` — the task id is
   what basis persists before waiting (ADR-0029 §5) — and then, when the card
   declares `streaming`, settles by `SubscribeToTask`, reporting status
   messages as progress. A stream that drops or errors falls back to polling
   `GetTask`; neither path ever sends the message again. The wait bound,
   cancellation and `CancelTask`-on-abandon are unchanged.

4. **Push notifications are served, durably.** A push config is stored beside
   the task it names and survives a restart. Every status change of a task
   with configs is delivered to each, at least once: retried with backoff, and
   re-delivered from the last undelivered state after a restart. The config's
   `authentication` becomes the `Authorization` header and its `token` the
   `X-A2A-Notification-Token` header. A webhook URL is checked before anything
   is sent and again at each delivery: `http` or `https` only, and a host that
   resolves to a loopback, private, link-local, unspecified or multicast
   address is refused unless the operator allows private webhooks. mentra's
   client exposes the config methods; basis's own `spawn` does not use push,
   because it holds a stream and persists its task id, and `basis` links no
   inbound transport.

5. **The extended card.** An operator may give the served agent an extended
   card (more skills, a longer description); the public card then declares
   it, and `GetExtendedAgentCard` serves it to an authenticated principal only.
   mentra's client fetches it at connect when the card declares one and the
   configuration carries credentials, and keeps the public card, with a
   warning, if that fails.

6. **Parts beyond text.** The server accepts text, data — handed to the model
   as JSON — and inline file parts whose media type is textual (`text/*`,
   JSON, XML, YAML) within the message size bound. A file by URL, and binary
   bytes, are refused with `ContentTypeNotSupportedError`: the server never
   fetches a URL a caller names. The card's input modes list exactly what is
   accepted. mentra renders data parts as JSON and file parts by name, media
   type and size (inline text decoded), all inside the remote agent's frame.

7. **A served agent can ask its caller.** `InboundApproval::AskCaller`, with a
   deadline, is the third policy beside `Never` and `Always`. A consequential
   call parks its turn and moves the task to `INPUT_REQUIRED`. The status
   message says, in text, ``reply `approve <call id>` or `deny <call id>` ``
   and carries a data part under the approval extension
   (`docs/a2a/approval-v1.md`) with the call id, the tool and its preview. The
   caller answers with a message on that task: the text form, or a data part
   with the call id and a decision. Then:
   - one question is open per task at a time; parallel calls in one turn wait
     their turn to ask;
   - an answer that names another call id, or no decision, is refused and the
     question stays open;
   - the deadline answers `deny`;
   - a cancel while parked cancels;
   - a restart re-drives the turn, which asks again under a fresh call id, so
     no answer outlives the question it was given for.

   The text form exists because the caller is usually a model: basis's own
   `spawn` already tells it to answer a question with `@<agent> <answer>`.

8. **More than one principal.** The server holds named bearer tokens. Each task
   records the principal that created it; every task-scoped method — get,
   list, cancel, subscribe, a follow-up message, the push configs — sees only
   the caller's own, and "not yours" is indistinguishable from "not found". A
   loopback server with no token is one anonymous principal.

9. **Hops and deadlines cross the wire.** The hops extension
   (`docs/a2a/hops-v1.md`) puts `{"hop": n, "deadline": <RFC 3339>}` in a
   message's metadata under the extension's URI. A served task records the hop
   it was reached at (absent is 0, so other people's agents interoperate); its
   own `@agent` delegations send `hop + 1` and the earlier of its deadlines.
   The server refuses a message past its hop cap (default 8) and clamps the
   task's deadline to the caller's. A deadline only ever shrinks across a hop.
   mentra stays generic: a request carries metadata and the extensions it
   activates; basis fills them.

10. **The card says what is served, and only that.** `streaming`,
    `pushNotifications`, `extendedAgentCard` and the two extensions are declared
    when, and only when, they are on.

## Consequences

- A basis agent reaches any A2A v1.0 agent that serves JSON-RPC or HTTP+JSON,
  and any client of either binding reaches a basis agent. A gRPC-only peer is
  still unreachable; proposal 0005 §6 keeps that trigger.
- A long wait survives a dropped connection, and a served task survives its
  caller going away: the caller can subscribe again or be called back.
- One served agent can be shared by several parties that cannot see each
  other's work, and its operator can keep a human — the caller's — in the loop
  without granting `always`.
- A loop between agents ends at the hop cap or the first deadline, whichever
  comes first.
- `ServeConfig` grows principals, the approval deadline, push and extended-card
  settings; `basis serve --a2a` grows the flags for them.
