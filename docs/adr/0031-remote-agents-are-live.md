# 0031 — A workspace's remote agents are live

> Status: Accepted · 2026-10-10
> Amends [`0029-a-remote-agent-is-a-spawn-target.md`](0029-a-remote-agent-is-a-spawn-target.md)
> §3 and §4 — the set an open connected is no longer fixed for its lifetime.
> Applies [`0005-mentra-coevolution-discipline.md`](0005-mentra-coevolution-discipline.md):
> re-reading a registered tool's description is mentra's.
> Amended by [`0037-an-agent-is-named.md`](0037-an-agent-is-named.md): the live
> set is the configured resolver, one resolver among others.

## Context

ADR-0029 connects a workspace's remote agents once, at open: the card is
fetched once, an unreachable agent stays unreachable, and every agent minted
from that open is taught that set for its lifetime. That is right for a
one-shot run and wrong for anything that runs for weeks.

1. **The list changes while the host runs.** An operator adds an agent, rotates
   a credential or moves a URL; an editor session or a served agent should not
   need a restart to see it.
2. **A remote agent comes and goes.** One that was down when the workspace
   opened is down forever today, though it came up a minute later.
3. **A card changes.** A remote agent's skills, its description, even its
   endpoint are its own to change.
4. **Who can reach whom is already data, read per call.** `spawn` routes
   `@<agent>` through the caller's agent row, which holds the open's set by
   reference. Making that set live changes routing for every agent that holds
   it, at once, without touching a row.
5. **What the model is told is snapshotted at registration.** mentra captures a
   tool's description when it is registered and re-reads the roster on every
   model request, so a description computed from live state needs only a way
   to be re-captured.
6. **A file being edited is briefly wrong.** A reload that dropped every agent
   because the operator saved a half-typed file would be worse than no reload.

## Decision

**The remote agents an open connected are a live set. A host reloads it
explicitly or lets basis watch the files, and every agent holding it routes —
and is taught — the current set from its next model request on.**

1. **`Workspace::reload_a2a` re-reads the same three sources** — host-supplied,
   workspace, global — under every rule ADR-0028 §2 set, and applies the
   difference by name:
   - **added** — connected;
   - **removed** — no longer routable;
   - **changed** (URL or headers) — reconnected;
   - **unchanged** — its card fetched again, since a card is the remote
     agent's to change;
   - **unreachable** — retried.

   It reports what changed. A source that fails to read or validate leaves the
   current set exactly as it was and returns the error: a typo never drops an
   agent.

2. **An unreachable agent is retried when it is asked for.** `@<agent>` for a
   configured agent whose card could not be fetched tries again, at most once
   per backoff interval, so an agent that came up after the workspace is
   reached without a reload. The model is told which configured agents are
   unreachable now, so it can still ask.

3. **Teaching follows the set.** Every agent a workspace mints or resumes gets
   its exact-agent `spawn` copy (ADR-0029 §4) bound to the live set, whether or
   not the set is empty at that moment, so an agent added later is taught to
   sessions already running. The copy's description is computed from the set;
   with nothing configured it is the plain `spawn` description, word for word.
   After any change basis asks mentra to re-capture registered descriptions
   (`refresh_tool_descriptors`), so the next model request carries the new
   list.

4. **basis can watch.** `Workspace::watch_a2a(interval)` polls each source's
   path for existence, length and modification time — no new dependency, and
   it notices a file that did not exist at open — and reloads on a change. The
   returned handle stops it when dropped. The long-running hosts turn it on by
   default: the ACP host, the A2A server, and the CLI's interactive session. A
   one-shot run does not.

5. **A removal keeps the conversation.** The record ADR-0029 §5 keeps is not
   deleted with the agent. `@<agent>` for a removed agent is refused by name,
   and re-adding the same name continues the conversation. A task that is
   outstanding when its agent's URL changes is resumed against the new
   endpoint; one the new endpoint does not know ends the outstanding record and
   says so.

6. **Nothing in flight is interrupted.** A delegation holds the client it
   started with, so a reload changes what the *next* `@<agent>` reaches, never
   an exchange already waiting.

## Consequences

- A long-running host picks up an edited `a2a.json`, a rotated credential, a
  remote agent that came back up, or a changed card, without restarting.
- Every workspace agent carries one exact-agent registration whether or not it
  has a remote agent yet — the cost ADR-0029 paid only for agents that had one.
- A change to the set changes the tool description mid-session, which costs a
  prompt-cache miss on the next request. It happens only when the set changes.
- mentra gains `refresh_tool_descriptors` (filed upstream per ADR-0005).
