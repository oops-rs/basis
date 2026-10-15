# 0041 — A delegation carries files

> Status: Accepted · 2026-10-15
> Extends [`0038-the-core-speaks-to-a-peer.md`](0038-the-core-speaks-to-a-peer.md)
> and [`0040-a-delegation-carries-parameters.md`](0040-a-delegation-carries-parameters.md);
> revises [`0030-a2a-is-spoken-in-full.md`](0030-a2a-is-spoken-in-full.md) §6.

## Context

An agent could hand another agent only words. A model holding a screenshot
had no way to send it with `spawn "@<agent> <task>"`: `peer::Message` carried
`text`, and the A2A peer sent one text part. Files coming back were worse off:
mentra summarized a binary part as `[file x.png (image/png): N bytes inline,
not shown]` and dropped the bytes, so neither the model nor any tool could
ever open what the agent returned.

The wire was never the obstacle. A2A carries file parts both ways, and dial
(the company gateway) forwards them unchanged. Four gaps sat in front of it:

1. `spawn` had no field for files, and the peer contract no place for them.
2. A file that came back was not kept anywhere.
3. A model could not look at an image even when one was on disk: mentra's
   `read` refused anything that is not UTF-8, and a tool result was text or
   JSON, never an image.
4. basis serving an agent refused binary parts — "no model behind basis is
   handed them" (ADR-0030 §6), no longer true since `PromptPart::Image` — and
   a durable task's answer was one string, so it could not return a file
   either.

## Decision

**A file is bytes with a name and a media type, carried beside a message in
both directions; received files are saved under the workspace's
`.basis/inbox/`, and a model sees an image by reading it.**

1. **The contract.** `agents::Attachment { name, media_type, data }` is a
   core type: a file is no more a protocol notion than `PromptPart::Image`.
   `peer::Message::files` carries the files sent; every `Outcome` carries the
   files that came back. A peer whose wire cannot carry a file refuses the
   message (`PeerError::Send`) rather than dropping it — the reason ADR-0030
   §6 refuses parts it cannot render.
2. **`spawn` takes `files`**, a list of workspace paths beside `input`, under
   the rules `metadata` follows (ADR-0040 §1, §4): typed, offered in the schema
   only when there are agents to send them to, and shown in the
   authorization preview. A path is resolved and authorized the way the file
   tools resolve a read, so nothing outside what the agent may read leaves.
   The media type is sniffed from the bytes, then the extension, then
   `application/octet-stream`. **Any file type is sent**; the receiver says
   what it accepts. All files together are bounded at 6 MiB, which is 8 MiB
   once A2A base64-encodes them: mentra's default reply bound and dial's
   request bound.
3. **Where files go.**
   - To a **remote agent**, as A2A `raw` file parts after the text part.
   - To a **local agent** (a teammate or one-off child), as a line naming the
     paths: a child shares the workspace, so copying bytes would be a second
     copy of a file it can already open.
   - A wait (`@<agent>` alone) refuses `files`, as it refuses `metadata`.
4. **Received files are saved** under
   `<workspace>/.basis/inbox/<task-id>/<name>`: the remote task's id, or a
   fresh id for a reply with no task. `<name>` is the part's file name
   reduced to one path component, made unique within the folder; a part
   without one is `file-<n>` with an extension for its media type. The tool
   result names each saved path, outside the remote agent's frame. A file
   **by URL** is never fetched — fetching a URL a remote agent names would
   make the caller a proxy, the rule ADR-0030 §6 applies to callers — and is
   passed along as the URL. The workspace folder is chosen over a run's
   scratch area so shell tools and later delegations can use what came back.
   (`basis-tasks`' per-task `inbox.json` is a different thing — a task's
   message queue under the data directory — and is unaffected.)
5. **A model sees an image by reading it.** mentra's `ToolOutput` gains
   images. The round's tool results come first in the turn, followed by the
   image blocks, which every wire mentra speaks already accepts. mentra's
   `read` shows PNG, JPEG, GIF and WebP files up to 5 MiB as images. `spawn`
   names saved files rather than showing them, so a model pays for an image
   only when it decides to look.
6. **basis serving an agent accepts any file** (revising ADR-0030 §6).
   - Textual inline files are rendered into the prompt as before.
   - Any other inline file is saved to the served workspace's inbox
     (`.basis/inbox/<id>/`) and named in the prompt.
   - A hosted backend (ADR-0033) receives the parts as before and decides for
     itself.
   - A file by URL is still refused.
   - The card's input modes become `*/*`, and the request body bound rises to
     16 MiB.
7. **A served task answers with files** through `attach_file`, a tool offered
   only on tasks spawned to answer a caller (`RunSpec::answering`). It copies a
   workspace file into the task's data directory at the moment of the call,
   so the answer is what the model chose then and not what the file became
   later. The total is bounded at 6 MiB, as for sending. The completed task's
   `result` artifact carries the text part and then one file part per
   attached file.

## Consequences

- A model can hand a screenshot to another agent and look at the one it
  sends back; a dial agent's file reaches iBot as a file.
- The core gains one plain type and two fields. All the A2A detail stays in
  the `a2a` feature and `basis-a2a`. The changes to mentra are generic:
  images in tool output, `read` showing images, and file parts on `A2aSend`.
- Images in history cost tokens on every later turn, as an image in a prompt
  already does. Compaction treats them as it treats any user content.
- `.basis/inbox/` grows with what agents return; it is the workspace owner's
  to clean and to ignore in version control.
- `Outcome`'s variants and `peer::Message` change shape, which is a breaking
  change for a host that implements `Peer`.
