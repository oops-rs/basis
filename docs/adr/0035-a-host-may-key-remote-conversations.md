# 0035 — A host may key a run's remote conversations

> Status: Accepted · 2026-10-12
> Extends [`0029-a-remote-agent-is-a-spawn-target.md`](0029-a-remote-agent-is-a-spawn-target.md)
> §5 — the conversation basis keeps for the model may belong to a host's key
> instead of to the calling agent.
> Preserves [`0031-remote-agents-are-live.md`](0031-remote-agents-are-live.md)
> §5 — records are still keyed by the remote agent's name, never its
> connection.

## Context

ADR-0029 §5 made the remote conversation basis's to keep: the A2A
`contextId` that makes successive `@agent` tasks one conversation, and the
task still outstanding — one the agent is working on, or one that asked a
question `@agent <answer>` answers. It is kept per calling agent, on disk for
an agent a workspace minted or resumed, so a resumed session finds it.

1. **Not every host resumes its agents.** iBot mints a fresh basis agent for
   every message of a chat; the chat is the conversation, the agent is not.
   When a remote agent asks a question, the user answers in the next message
   — a new agent, with no record — and `@nous <answer>` starts a new remote
   task instead of answering the one that asked. ADR-0034 makes a hosted agent
   asking routine, which makes this routine.
2. **The key is the host's to know.** basis cannot tell which runs are one
   chat; a host can name it. An agent id is only the default key.
3. **The record is already one small file, replaced whole.** Sharing it
   between runs means reading it afresh rather than once, and writing it so
   that two writers cannot interleave.
4. **A `RunSpec` cannot grow a field.** Its fields are public and it is not
   `#[non_exhaustive]`, so adding one would break a caller's struct literal.
   The key is bound on the `PreparedRun` a mint returns instead, before its
   first turn.

## Decision

**`PreparedRun::with_remote_conversations(key)` binds a run's remote
conversations to a host's key. Every run bound to one key shares one record;
runs on different keys never see each other's.**

1. **Binding.** The workspace's copy of `spawn` for the run is registered
   again on the session, bound to the key; nothing else about the run
   changes. It is a no-op for a run no workspace minted. A key binds one run:
   the host binds each run it mints or resumes. Delegated children keep their
   own conversations in memory, as before.

2. **Where it lives.** Beside the runtime's other A2A records, in
   `<a2a conversations>/keys/<file name>.json`. A key that is already a plain
   file name — lowercase letters, digits, `-` and `_`, at most 64 — is its own
   name; any other is made one from its plain characters, lowercased, with
   the rest replaced, followed by a stable digest (FNV-1a) of the whole key.
   No key names a path outside the directory, two keys a case-insensitive
   file system would confuse stay apart, and the `keys/` subdirectory keeps a
   key from ever sharing a file with an agent id. Forgetting an agent never
   removes a key's record; the key is the host's. A runtime whose history is
   kept nowhere keeps keyed records in memory, per workspace open.

3. **Reading and writing.** A keyed record is read from its file on every
   call. A write reads the file, replaces the one remote agent's entry it is
   about, writes the whole record to a staged file named for that write, and
   renames it over the record — so a reader sees the old record or the new
   one, never half, and two writers never share a staged file. Within one
   workspace open the read-change-write is serialized. Across opens or
   processes it is **last writer wins**: a change to another agent's entry
   made between a write's read and its rename is lost. Runs on one key are
   one chat's messages, normally one at a time, and the host decides whether
   they overlap.

4. **Reading it from outside.** `Workspace::remote_conversation(key, agent)`
   returns where a key's conversation with an agent stands — `Open`,
   `Working` or `InputRequired` — or `None` when there is none, so a host can
   tell its model, before a run, that `@nous` is waiting for an answer. The
   model learns it in the run too: the spawn result that recorded the
   question said to answer with `@nous <answer>`, and `@nous <task>` while the
   agent is still working is refused with how to wait.

## Consequences

- A host that mints an agent per message keeps a remote agent's question
  answerable across messages, and a remote context continuous across a chat.
- The default — no key — is unchanged, byte for byte on disk.
- The change is additive: one method on `PreparedRun`, one on `Workspace`,
  and a `#[non_exhaustive]` enum.
- Two runs on one key at once can lose a concurrent change to a different
  remote agent's entry. A file lock would close it; it is deferred until a
  host runs one chat's messages concurrently.
- Keyed records accumulate until the host deletes them; basis knows no chat's
  end.
