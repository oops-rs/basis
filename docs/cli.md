# Using the Basis CLI

Install and configure a provider with the [quick start](../README.md#quick-start).
The installed binary is `basis`. Bare `basis` prints usage; it does not start a
server. Use `basis <command> --help` for every flag and its accepted values.

## Prompts and conversations

```text
basis "Explain this repository"                  # shorthand for `basis spawn`
basis spawn -C ../other-repo "Review the tests"   # choose a workspace
basis spawn -                                   # read a prompt from stdin
basis list --all                                # tasks, last worked in first
basis spawn --continue "And which test is slowest?"
basis spawn --session <TASK> "Explain that finding"
```

These commands use the CLI's default `--approve always`; add `--approve prompt`
to ask at a terminal, or `--approve never` to deny consequential calls. See
[approvals and isolation](#approvals-and-isolation) before running unfamiliar work.

`--continue` and `--session` start a **new task on an existing conversation**.
A settled task accepts no new messages, and a conversation already being driven
cannot be continued concurrently. Running a turn or sending a message updates its
position in `basis list --all`; reading it with `wait` or `watch` does not.
Without `--all`, `basis list` shows only the most recently worked task.

A prompt starting with `/name` invokes a discovered
[prompt template](conventions.md#basistemplates). For a literal prompt starting
with `/`, send it on stdin with `basis spawn -`. A prompt that matches a subcommand
name needs `--`, for example `basis -- run`.

## Durable tasks

At a shell, `spawn` drives the agent and prints its answer. With `--resumable`, it
only creates the task and prints a handle. **An agent advances only while a process
is attached.** There is no daemon or scheduler: use `basis wait`, your shell's
backgrounding, tmux, or your existing job runner to drive it.

```text
basis spawn --resumable "<prompt>"   # create a task without driving it
basis wait <ID>                      # attach or observe its terminal result
basis watch <ID>                     # observe progress without driving it
basis send <ID> "<message>"          # enqueue a turn; print its message ID
basis send <ID> "<message>" --await  # enqueue, then wait for that reply
basis ask <ID> "<question>"          # send with the reply wait implied
basis wait <ID> --message <MID>      # retry a correlated reply wait
basis inbox <ID>                     # bounded message/reply summaries
basis cancel <ID>                    # cancel, including attached descendants
```

Inside another task (`BASIS_TASK_ID` set), `spawn` defaults to resumable work and
prints its handle; `--await` opts into waiting. Attached children inherit
cancellation and the narrower deadline. A successful parent waits for attached
children to settle; a failed or cancelled parent cancels them before settling.
`--detached` creates an independent root.

Self, ancestor, and same-tree peer waits are rejected. A wait timing out does not
cancel the task, and retrying a wait does not rerun it. Inboxes accept at most 16
messages over a task's lifetime; message and reply summaries are capped at 4 KiB
with truncation metadata. Once a worker is past its own turn, it accepts no new
messages or children. See the [concurrency contract](adr/0017-structured-agent-concurrency.md).

Tasks and conversations live under `BASIS_DATA_DIR`, otherwise the platform data
home (with `XDG_DATA_HOME` honored). Keep this directory to resume work across
processes or container restarts. Task records do not store provider credentials;
the attaching process supplies them. See [storage paths](conventions.md#directories-basis-writes)
and [migration notes](ARCHITECTURE.md#where-task-state-lives-and-what-e2-does-not-migrate).

If a process dies mid-turn, the next attach continues the interrupted turn. A tool
call interrupted by the crash is recorded as having unknown effects and is not
replayed; the model is told to check before repeating it. A checkpoint restores
conversation state, not external effects.

## Bounds and output

```sh
basis spawn --deadline 10m --tool-budget 40 --token-budget 100000 "Review the changes"
basis spawn --json "Explain the project" > events.jsonl
basis "Summarize the project" > summary.md 2> progress.log
```

Local tasks have a 30-minute default deadline. `--deadline` narrows that allowance,
including for a task nobody has attached to yet: attaching after it expires settles
the task as failed instead of starting work. Token budgets are soft; usage becomes
known after a round finishes. Hitting a bound keeps work already committed.

For attended runs, stdout carries the assistant's answer and stderr carries
progress, tool activity, usage, and the next-step hint. At a shell, `spawn --json`
without `--await` or `--resumable` runs directly without a durable task handle and streams
JSONL events from `run_started` (with the schema version) through `run_finished`
(with `stopped_by` when a bound ended the run). Use `spawn --json --await` for a durable
task and one JSON result instead. Lifecycle commands such as `send`,
`wait`, and `cancel` return a bounded JSON object; `list` and `watch` emit JSONL.
`basis run` remains a compatibility alias for `basis spawn`.

| Exit code | Meaning |
| --- | --- |
| `0` | The operation finished successfully. |
| `1` | The run failed or could not start. |
| `2` | Invalid invocation. |
| `3` | A run bound or wait timeout was reached; committed work is kept. |

For a recurring workflow, use `basis fingerprint` to detect workspace changes
and drive the schedule outside Basis. The [operator notes](ARCHITECTURE.md#the-recurring-run-loop-written-out)
and [watch example](../basis/examples/watch.rs) show that pattern.

## Approvals and isolation

**Basis is not a sandbox.** It runs with the authority of the account that started
it. File tools are scoped to the workspace and deny `.git/hooks` and `.git/config`,
but shell commands can bypass those checks. Use
[OS isolation](containerization.md) when you need a security boundary.

Commands are enabled by default. `--no-shell` disables command execution but still
allows file writes. Approval modes govern consequential calls (including commands,
delegation, and edits); read-only calls do not require approval:

- `always`: allow consequential calls; the CLI default.
- `never`: deny consequential calls.
- `prompt`: ask at the terminal of the process driving the task. Requires a TTY and
  is rejected for `--resumable` work, which has nobody to ask.

Workspace files can declare executable tools, hooks, and MCP servers. Review those
files before opening an untrusted repository; approval settings do not confine the
process or its discovery-time programs. See the [file conventions](conventions.md).

## Protocol servers

```sh
basis serve --acp                       # ACP over stdio, launched by a client
basis serve --bridge --allow-origin https://your-client.example
basis serve --a2a 127.0.0.1:8080         # serve this workspace to other agents
```

**ACP** lets a client choose the workspace, send prompts, resume sessions, and
answer permission requests. It defaults to asking the client for approval. The
WebSocket bridge serves the same protocol on loopback and rejects connections
until an `--allow-origin` is named. ACP has been checked against the protocol and
official client library; Zed and JetBrains integrations have not yet been verified.

**A2A** serves v1.0 over JSON-RPC and HTTP+JSON, with streaming, push notifications,
and task lifecycle methods. Its defaults are `--approve never` and no shell;
`--shell` explicitly enables command execution. With `--approve prompt`, the
*calling agent* answers `approve <call id>` or `deny <call id>`. Unanswered requests
are denied after `--ask-deadline` (10 minutes by default). See
[approval-v1](a2a/approval-v1.md) for the wire contract.

A2A messages can carry files as well as text and data. Inline text files enter the
prompt; other inline files are saved under the served workspace's `.basis/inbox/`
and named for the agent to read. A served task can return workspace files with
`attach_file`. Files supplied by URL are refused; the server never fetches them.

When delegating, the model can pass `"files": ["screens/login.png"]` beside the
`spawn` tool's input, up to 6 MiB in total. Remote agents receive the files; local
agents share the workspace and receive their paths. Returned files are saved in
`.basis/inbox/<task>/`, where `read` can show an image. See the
[file delegation contract](adr/0041-a-delegation-carries-files.md) for limits and
storage ownership.

Before exposing an A2A server:

- Non-loopback addresses require bearer authentication: `--token-env VAR`, or
  repeatable `--principal NAME=VAR` entries. Each principal sees only its own tasks.
- An unauthenticated loopback server rejects browser pages but trusts local
  processes. `--push` and `--approve prompt` require a token unless you explicitly
  opt into that trust with `--trust-anonymous-callers`.
- Webhooks accept only HTTP(S) URLs resolving to public addresses, checked again
  on delivery. `--allow-private-webhooks` opts into private-network destinations.
- Basis does not terminate TLS. Put a TLS proxy in front and advertise its address
  with `--public-url`.

Delegation carries hop counts and deadlines; `--hop-cap` defaults to 8 to bound
agent-to-agent loops. See [hops-v1](a2a/hops-v1.md), the
[A2A configuration reference](conventions.md#basisa2ajson), and
[the protocol decision](adr/0030-a2a-is-spoken-in-full.md). Interoperability was
manually checked with the official Python `a2a-sdk` 1.2.2 in both directions over
both bindings; that external SDK check is not part of CI.
