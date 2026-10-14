# 0038 — The core speaks to a peer; A2A is one way to reach it

> Status: Accepted · 2026-10-14
> Amends [`0037-an-agent-is-named.md`](0037-an-agent-is-named.md) §2 — a
> resolver answers with a peer, not with mentra's A2A client.
> Amends [`0029-a-remote-agent-is-a-spawn-target.md`](0029-a-remote-agent-is-a-spawn-target.md)
> §6 — mentra keeps the A2A protocol; the exchange `spawn` runs is written
> against a contract of basis's own, which A2A implements.
> Preserves [`0012-one-contract-many-bindings.md`](0012-one-contract-many-bindings.md)
> and [`0028-a2a-is-the-agent-to-agent-protocol.md`](0028-a2a-is-the-agent-to-agent-protocol.md):
> A2A stays a default-on feature, droppable at compile time.

## Context

ADR-0037 put the resolver — who `@<name>` is — in `basis::agents`, behind
the `a2a` feature. Reviewing it, the author ruled that the resolver is
something basis exposes for anyone to implement, so it cannot sit behind a
feature; and, separately, that A2A is a feature and must not be in the core.

1. **The gate was the answer type.** A resolver handed back
   `Arc<mentra::a2a::A2aClient>`, and `spawn`'s remote path ran mentra's A2A
   exchange over it. Nothing else about naming, the resolver chain, the
   conversation record or the `reach` grammar is A2A.
2. **What `spawn` needs of a remote agent is small.** Hand it a message,
   continuing a conversation or answering a task it asked about; wait again
   on a task it has not finished; cancel when the run is cancelled; know how
   much of its output to show. Which conversation, which task, and what the
   model is told are basis's (ADR-0029 §5).
3. **Every host already needs the feature on.** `basis-tasks` watches
   `a2a.json` unconditionally, so dropping the feature was considered and
   rejected: it would have made the A2A client core, the opposite of the
   ruling.

## Decision

**The core defines the contract a remote agent is spoken to through; A2A
implements it behind its feature.**

1. **`basis::agents::Peer`**, no feature. One method hands a request over
   and returns what came back, told the run's bounds and a progress sink;
   the peer reports the task it started before it waits, so a record exists
   if the process dies mid-wait. One method cancels. One reports the output
   cap.

   ```rust
   #[async_trait]
   pub trait Peer: Send + Sync {
       async fn exchange(&self, request: Request, bounds: &ExchangeBounds<'_>) -> Result<Outcome, PeerError>;
       fn max_output_bytes(&self) -> usize;
   }
   ```

   `Request` is `Send(Message { text, context_id, answering, hop, deadline })` or
   `Wait { task_id }`. `Outcome` is `Replied`, `Finished`, `Working`,
   `NeedsInput`, `NeedsAuthorization` or `Failed`, each carrying the ids basis
   records and the text, already framed as the remote agent's own words.
   `PeerError` is `Send`, `Check { task_id }`, `UnknownTask`, `Abandoned` or
   `Unrecognized`, which is every distinction `spawn`'s decision table makes.

2. **`Resolved` carries `Arc<dyn Peer>`.** The resolver, the chain
   (`RemoteAgents`), the conversation record and `spawn`'s whole `@<name>`
   path are core, with no cfg. `WorkspaceBuilder::with_agent_resolver` and
   `PreparedRun::with_remote_conversations` are plain API.

3. **`basis::a2a::A2aPeer`** adapts mentra's client: submit with
   `returnImmediately`, settle by stream or poll, cancel on abandon, the hops
   extension on every message, mentra's framing of the output. The configured
   set resolves to it, and `Resolved::over_a2a(client, name)` is how a host's
   resolver does. The feature gates exactly this module: a2a.json, the
   configured set, reload and watch, and the adapter.

4. **The record is unchanged on disk.** A conversation is still one
   `context_id` and at most one outstanding task id with the endpoint it was
   handed to, under `a2a-conversations/`. What is behind an id is the peer's
   business.

## Consequences

- A host implements `Peer` for anything that can take a task and answer —
  an in-process agent, a queue, another protocol — and resolves names to it
  with no A2A anywhere in its build.
- `--no-default-features` builds keep `@<name>`, teammates, resolvers and the
  conversation record; they lose a2a.json and the A2A adapter.
- The decision table in `spawn` is tested against `Outcome` with no server
  and no A2A type; the adapter's mapping is tested on its own.
- Breaking for a resolver written against ADR-0037's first cut:
  `Resolved::new` takes a peer and an endpoint; `Resolved::over_a2a` keeps
  the one-line case.
- `Runtime::a2a_hop` and the `a2a_*` fields of `RunStarted` keep their names:
  a hop is a hop whatever carried it, and the header's shape is a client's.
