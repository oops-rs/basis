# Changelog

All six crates — `basis`, `basis-host`, `basis-tasks`, `basis-acp`,
`basis-a2a` and `basis-cli` — share one version. Decisions behind each entry
live in `docs/adr/`; this file says what changed for someone upgrading.
Releases before 0.15.0 are described by their tags and ADRs only.

## 0.17.0 — 2026-10-13

### A server may authenticate its own way ([ADR-0036](docs/adr/0036-a-server-may-authenticate-its-own-way.md))

- `basis-a2a` decides who a request is from with an `Authenticator`: given
  the request's head (`RequestHead` — method, URI, headers, never the body),
  it returns a `Principal` or a `Refusal` (`401` with its `WWW-Authenticate`
  challenge, or `403`), asynchronously, before any body is read. Give one
  with `ServeConfig::with_authenticator`; a server behind a gateway that signs
  a token per request accepts exactly those.
- The authenticator declares the card's `Security` — its schemes, and which
  a caller must present — so a JWT checker can advertise HTTP bearer with
  `bearerFormat: "JWT"`. One that declares none is held to a tokenless
  server's rules: loopback only, no extended card, push and asking only with
  trusted anonymous callers.
- Named tokens and the tokenless anonymous caller are the default
  authenticator: with none given, every server, card and `401` is unchanged,
  including `basis serve --a2a`.
- `Principal::new` accepts `:` (for names like `anon:dev_…`).

### Breaking

- `OpenError` gains `TokensWithAuthenticator`: a config with both tokens and
  an authenticator is refused at `open`, since the authenticator replaces the
  tokens. A `match` on `OpenError` needs an arm for it.

## 0.16.2 — 2026-10-12

No breaking changes.

### A hosted task may ask its caller ([ADR-0034](docs/adr/0034-a-hosted-task-may-ask-its-caller.md))

- A `TaskHost` pauses a task on a question by reporting
  `HostedStanding::InputRequired { parts }`: the task is
  `TASK_STATE_INPUT_REQUIRED` with those parts as the agent's status message.
  A blocking send answers there, a stream closes there, push hears it, and the
  task still counts as live and can be canceled.
- The caller's reply — a message carrying the task's `taskId` — reaches the
  host through the new `TaskHost::reply(id, HostedReply)`, and the same task
  resumes. The method has a default that refuses ("this agent takes no
  replies"), so existing hosts compile and behave unchanged.

### A host may key a run's remote conversations ([ADR-0035](docs/adr/0035-a-host-may-key-remote-conversations.md))

- `PreparedRun::with_remote_conversations(key)` keeps what `spawn "@agent …"`
  remembers — the remote context and any outstanding task — under a host's
  key instead of the calling agent's id, so a host that mints a fresh agent per
  chat message can answer a remote agent's question in the next message.
  Records live in `a2a_conversations/keys/`, are replaced atomically, and are
  last writer wins across processes.
- `Workspace::remote_conversation(key, agent)` reports whether a key's
  conversation with an agent is `Open`, `Working` or `InputRequired`.

## 0.16.1 — 2026-10-12

- `ServeConfig::with_task_deadline` sets how long a served task may run,
  for a hosted server as well as a `basis-tasks` one. A caller's deadline
  still clamps it. Before this a hosted task could never run past
  `basis-tasks`' 30-minute default. No breaking changes.

## 0.16.0 — 2026-10-12

### A host may run the tasks it serves ([ADR-0033](docs/adr/0033-a-host-may-run-the-tasks-it-serves.md))

- `basis-a2a` serves either a workspace's `basis-tasks` (unchanged, the
  default) or a host's own tasks. A host implements `TaskHost` (`start`,
  `standing`, `cancel`, `settled`, `progress`) and opens
  `ServeConfig::hosted(identity, state_dir, host)`. Every A2A feature above
  the seam — both bindings, the card and extended card, principals, quotas,
  stream caps, hops, push — works the same for both.
- A hosted task completes with the parts its host returns (text, data or
  both) as its one `result` artifact. Its A2A id is minted by the server.
- A hosted config refuses `with_shell`, `with_run_defaults`,
  `with_data_dir` and any approval but `Never` at `open`
  (`OpenError::NotForHostedTasks`), rather than ignoring them.

### Breaking

- `A2aService::get_task`, `list_tasks` and `subscribe_to_task` are now
  `async`: a hosted backend answers where a task stands asynchronously.
- `OpenError` gains `NotForHostedTasks` and `StateDir` (it is not
  `#[non_exhaustive]`).

## 0.15.1 — 2026-10-10

- The minimum supported Rust is now **1.95** (was 1.88), the release that
  stabilizes the `if let` match guards `spawn` and the declared-tool manifest
  use; CI's MSRV job and current stable's lints are aligned with it (#2). No
  API changes. A crate on edition 2024 (resolver 3) that declares an older
  `rust-version` keeps resolving 0.15.0.

## 0.15.0 — 2026-10-10

Built on mentra 0.29.0, with `rust-version = "1.88"`.

### A2A in full ([ADR-0030](docs/adr/0030-a2a-is-spoken-in-full.md))

- Every A2A v1.0 method, in both directions, over JSON-RPC and HTTP+JSON
  (served under `<endpoint>/json`). gRPC remains deferred.
- `spawn "@agent …"` follows the remote task's stream when the card offers
  one, falls back to polling, and never sends a task twice.
- The server serves push notifications (durable, retried for 24 h, webhook
  addresses checked; `--push`, `--allow-private-webhooks`), an operator's
  extended card (`--extended-card`), data parts and inline text files.
- A served agent can ask its caller to approve a consequential call
  (`--approve prompt`, `--ask-deadline`; `docs/a2a/approval-v1.md`).
- Several principals per server (`--principal NAME=VAR`), each seeing only
  its own tasks, with per-principal caps on live tasks and open streams.
- Hops and deadlines travel with every delegation (`docs/a2a/hops-v1.md`,
  `--hop-cap`, default 8); a deadline only ever shrinks across a hop.
- A tokenless loopback server refuses requests a web page could forge, and
  refuses to start with push or asking enabled unless
  `--trust-anonymous-callers` is given. Callers are authenticated before a
  body is read.

### Live remote agents ([ADR-0031](docs/adr/0031-remote-agents-are-live.md))

- `Workspace::reload_a2a` re-reads `a2a.json` and applies the difference by
  name; a file that fails to read leaves the set untouched.
- `Workspace::watch_a2a` polls the files and reloads on a settled change. On by
  default in the ACP host and whenever basis-tasks drives a task (which
  covers `basis serve --a2a`); `SessionTemplate::with_a2a_watch` turns it off.
- An unreachable agent is retried when asked for, at most every 30 s.
- Running sessions are told the new set on their next model request.

### Interrupted turns and cancel ([ADR-0032](docs/adr/0032-an-interrupted-turn-is-continued.md))

- A crash during a tool round used to settle the task **Succeeded** with
  partial text. It now leaves an interrupted turn: the running call is closed
  as of unknown effect, never re-run, and the next attach continues the turn.
- A cancel cuts the running turn short instead of waiting for it to end, so
  A2A `CancelTask` answers `CANCELED`.
- A host that shuts down records nothing; `A2aService::shutdown` and
  `Tasks::with_stop` make that explicit.

### Breaking

- `Workspace::a2a_agents()` returns `Vec<String>` (the set can change).
- `Workspace::watch_a2a` returns `Result<A2aWatch, A2aWatchError>`.
- `A2aService`'s request methods take `caller: &Principal`; `OpenError` has
  new variants (`Principals`, `ExtendedCardWithoutToken`, `NeedsToken`).
- `InboundApproval` gains `AskCaller { deadline }`.
- mentra 0.29.0's own breaks reach a host that uses `mentra::a2a` directly:
  `A2aRequest`, `A2aSend` and `A2aError` are `#[non_exhaustive]`.
