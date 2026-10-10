# A2A extension: approval, v1

> URI: `https://github.com/oops-rs/basis/blob/main/docs/a2a/approval-v1.md`
> Status: Stable · decided by [ADR-0030](../adr/0030-a2a-is-spoken-in-full.md) §7

An agent that serves this extension can put a consequential tool call to its
caller and wait for an answer, instead of refusing or allowing every such call
up front. The key words MUST, MUST NOT, SHOULD and MAY are as in RFC 2119.

## Declaration

An agent that may ask lists the extension in its card's
`capabilities.extensions`, with `required: false`. An agent that never asks
MUST NOT list it. A client needs nothing beyond A2A v1.0 to answer: the
question is readable as text, and so is the answer.

## The question

When a task's turn reaches a call that needs approval, the agent parks the
turn and moves the task to `TASK_STATE_INPUT_REQUIRED`. The status `message`
(role `ROLE_AGENT`) lists the extension's URI in `extensions` and carries:

1. A text part ending in the instruction
   ``Reply `approve <call id>` or `deny <call id>`.``
2. A data part whose `data` holds, under the extension's URI:

   ```json
   {
     "https://github.com/oops-rs/basis/blob/main/docs/a2a/approval-v1.md": {
       "callId": "3f2a9c0e7b1d4e8f9a6b5c4d3e2f1a0b",
       "tool": "write",
       "preview": "Writes a file.\n{\n  \"path\": \"notes.txt\" ..."
     }
   }
   ```

   - `callId` names this question and no other. The agent MUST mint a fresh
     one per question, and MUST NOT reuse one after the question closes.
   - `tool` is the name of the tool the agent wants to call.
   - `preview` is a human-readable description of the call and its input. It
     MAY be truncated.

A task has at most one open question. Calls that need approval while one is
open wait their turn to be asked.

## The answer

The caller answers with a message on the task (`taskId` set, role
`ROLE_USER`), through `SendMessage` or `SendStreamingMessage`. The message
carries one of:

- a text part `approve <call id>` or `deny <call id>`: the two words only,
  compared case-insensitively, surrounding whitespace ignored; or
- a data part `{"callId": "<call id>", "decision": "approve" | "deny"}`,
  bare or under the extension's URI as the question's is.

The agent MUST refuse with `InvalidParams` (JSON-RPC `-32602`, HTTP 400) an
answer that names a call id other than the open question's, or that gives no
decision, and the question stays open. An answer to a task with no open
question is refused with `UnsupportedOperationError`, as any reply to a task
not waiting for input is.

An accepted answer resumes the turn. The response to the answering message
is the task, as `SendMessage` answers: when it settles, or when it next waits
for input, unless the caller asked to return immediately.

## Endings

- **Deadline.** The agent waits a bounded time for an answer. When it passes,
  the call is denied, and the task continues without it.
- **Cancel.** `CancelTask` on a task with an open question denies that
  question and every later one, and the task ends `TASK_STATE_CANCELED`.
- **Restart.** An agent that restarts while a question is open MUST NOT honor
  an answer to it, and MUST NOT make the call on the strength of one. It
  drives the task again: a re-run turn asks again under a new call id, and an
  answer naming the old one is refused as a mismatch, or as an answer to a
  task with no question open.

## Streams

A stream closes when the task reaches `TASK_STATE_INPUT_REQUIRED`, as A2A
streams close at any interrupted state. The caller answers, then subscribes
again.
