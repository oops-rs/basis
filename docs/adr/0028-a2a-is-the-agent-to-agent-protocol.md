# 0028 — A2A is the agent-to-agent protocol

> Status: Accepted · 2026-10-09
> Amends [`0002-acp-is-the-protocol.md`](0002-acp-is-the-protocol.md) — ACP stays
> the only protocol a *client* drives a session with; this adds the protocol one
> *agent* uses to reach another.
> Applies [`0005-mentra-coevolution-discipline.md`](0005-mentra-coevolution-discipline.md),
> [`0011-layered-crates.md`](0011-layered-crates.md),
> [`0022-the-task-layer-is-a-crate.md`](0022-the-task-layer-is-a-crate.md) and
> [`0025-the-host-kit-is-a-crate.md`](0025-the-host-kit-is-a-crate.md).
> Deferred work: [`proposals/0005-a2a-beyond-the-first-cut.md`](../proposals/0005-a2a-beyond-the-first-cut.md).

## Context

basis-built agents are starting to need each other, and so are agents other
people build. The first pair was ibot asking nous a question: ibot carries a
~1.5k-line, nous-only client (`POST /investigate`, a one-hour timeout, an
in-memory pending slot, a nous-specific answer parser) and states in its own
docs that a restart drops whatever was in flight. Every further pair would be
another such client, written against another bespoke contract — and for agents
basis does not own, there is no contract to write against at all.

[A2A v1.0](https://a2a-protocol.org/v1.0.0/specification) is the standard for
exactly this edge: an Agent Card at `/.well-known/agent-card.json` describing
an agent, and a task lifecycle (`SendMessage`, `GetTask`, `ListTasks`,
`CancelTask`, `SubscribeToTask`) over JSON-RPC, gRPC or HTTP+JSON. Its three
neighbours do not overlap it: **ACP** is a person's editor driving an agent,
**MCP** is an agent calling tools, **A2A** is an agent handing work to another
agent whose insides it cannot see.

Facts that bound the design:

1. **The client half is generic.** Fetching a card, sending a message, polling
   a task and bridging a remote agent into a tool roster are what *any*
   harness wants — the same test ADR-0005 applies to mentra's MCP client.
2. **The server half is basis-shaped.** A2A's task model — server-minted ids,
   terminal states, a task that refuses new messages once terminal, a
   `contextId` grouping follow-ups — is what `basis-tasks` already is: `spawn`
   mints a handle, `terminal.json` is the terminal record, and
   `Continuation::Named` continues a conversation. A2A is a projection of it,
   not a second lifecycle.
3. **The official Rust SDK splits by weight.** `a2a-lf` (a2aproject/a2a-rs) is
   types only — serde, chrono, uuid, base64, all but one already in basis's
   tree. Its `a2a-client-lf` and `a2a-server-lf` both pull `a2a-pb`, which is
   tonic + prost + a vendored `protoc`, and `reqwest` 0.13 beside the 0.12
   mentra links. The wire types are worth taking; the transports are not.
4. **The reference server refuses a request with no `A2A-Version`** — the
   spec reads an absent version as 0.3.
5. **Nobody is there to approve.** A caller on the wire is another agent; a
   tool approval put to it has no human to answer and no RPC to arrive on.

## Decision

**A2A is basis's agent-to-agent protocol. The client is mentra's, the
convention and the server are basis's, and both speak `a2a-lf`'s types over
JSON-RPC.**

1. **mentra owns the client, behind its `a2a` feature.** It fetches and
   validates an Agent Card, speaks the JSON-RPC binding over the `reqwest`
   mentra already links, always sends `A2A-Version: 1.0`, and bridges each
   configured remote agent into the roster as one tool, `a2a__{agent}`. The
   tool submits with `returnImmediately` and polls `GetTask` to a terminal or
   interrupted state within its own bound; a task still working at the bound
   comes back with its ids so a later call can resume it instead of resending.
   The tool declares `External` side effects, so basis's `ApprovalGate` puts
   every call to the approver exactly as it does an external MCP tool. A remote
   agent's text is returned framed as that agent's output, never as the
   harness's own words. Filed upstream per ADR-0005.

2. **basis owns the convention.** `.basis/a2a.json` in the workspace and
   `a2a.json` in the global config directory name remote agents — a card URL
   and the headers that authenticate to it. Every rule `.mcp.json` follows
   carries over unchanged: host-supplied → workspace → global by name, **no
   parent walk**, `${VAR}` expansion, a name may not contain `__` or end in
   `_`, a file that exists but names no `agents` is an error, and a committed
   file cannot grant its headers plaintext passage to a non-loopback host. It
   sits behind `basis`'s default-on `a2a` feature, beside `mcp`, so the file
   means the same thing in every host.

3. **basis owns the server, as a sixth crate.** `basis-a2a` depends on
   `basis-tasks` (and through it `basis-host`'s approval policy) and serves the
   JSON-RPC binding with SSE streaming over axum. It is the only crate that links an HTTP server; `basis`
   still links no transport (ADR-0011). The projection:

   | A2A | basis-tasks |
   | --- | --- |
   | task id | `TaskHandle`, minted by `spawn` |
   | `contextId` | the handle of the task that opened the conversation; a follow-up spawns with `Continuation::Named` |
   | `COMPLETED` / `FAILED` / `CANCELED` | `Terminal::Succeeded` / `Failed` / `Cancelled` |
   | the result | one text artifact from `Succeeded { result }` |
   | `SubscribeToTask` | the task's `events.jsonl`, replayed from the start |
   | `CancelTask` | `Tasks::cancel` |

   The serving process drives every task it accepts, and at start-up resumes
   driving the non-terminal ones it accepted before — a restart costs latency,
   not work. `basis serve --a2a <addr>` is the binary's door to it.

4. **Inbound runs never prompt.** A served agent runs under `never` (the
   default) or `always`; `prompt` is refused at start-up, not discovered on the
   first tool call. Shell still needs the explicit grant ADR-0006 requires.

5. **The first cut is narrow, and says so on the wire.** JSON-RPC is the one
   binding; version `1.0` is the one version, and a missing version is refused
   as the reference server refuses it. Push notifications answer
   `PushNotificationNotSupportedError`, the extended card
   `UnsupportedOperationError`, a non-text part `ContentTypeNotSupportedError`,
   and a message to a task that is not waiting for input
   `UnsupportedOperationError`. The card's `capabilities` advertise only what
   is served.

6. **The server authenticates one principal and does not terminate TLS.** The
   operator names an environment variable holding a bearer token; every call
   but the public card must carry it, and the card declares the scheme. Binding
   a non-loopback address without a token is refused. TLS — which the spec
   requires in production — belongs to whatever fronts the port. Every holder
   of the token sees every task this server minted.

## Consequences

- Every basis-built agent can call any A2A agent — basis-built, someone else's,
  or another language's SDK — by adding a name and a URL to a file, and
  ibot's nous-only client becomes one entry in it.
- Every basis agent can be called by any A2A client with one flag, and a task
  accepted over A2A is as durable as one spawned from the CLI: it survives the
  server restarting, and `basis tasks`/`basis watch` see it.
- An A2A text result crosses into a model's context. It is external content
  with the same standing as an MCP result or a web page, and the approval gate
  is the only thing between a model and calling a remote agent.
- Approval-by-`INPUT_REQUIRED`, call-depth and budget propagation across
  agents, push notifications, multi-principal visibility, a streaming client,
  and the REST and gRPC bindings are deferred with their triggers in
  [proposal 0005](../proposals/0005-a2a-beyond-the-first-cut.md).
- An agent that runs its own task model — nousd is one — does not adopt
  `basis-a2a`; it serves A2A over its own runs with `a2a-lf`'s types, and
  basis's client reaches it like any other.
