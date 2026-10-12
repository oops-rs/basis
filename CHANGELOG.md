# Changelog

All six crates — `basis`, `basis-host`, `basis-tasks`, `basis-acp`,
`basis-a2a` and `basis-cli` — share one version. Decisions behind each entry
live in `docs/adr/`; this file says what changed for someone upgrading.
Releases before 0.15.0 are described by their tags and ADRs only.

## Unreleased

### A host may run the tasks it serves ([ADR-0033](docs/adr/0033-a-host-may-run-the-tasks-it-serves.md))

- `basis-a2a` can serve a host's own tasks instead of `basis-tasks` spawns:
  implement `TaskHost` (`start`, `standing`, `cancel`, `settled`,
  `progress`) and open the server with `ServeConfig::hosted(identity,
  state_dir, host)`. Principals, quotas, stream caps, hops and deadlines,
  push, both bindings and the extended card behave as for a workspace.
- A hosted task completes with the parts its host returns; its A2A id is
  minted by the server, and a restarted server awaits unsettled hosted tasks
  again rather than re-sending them.
- `with_shell`, `with_run_defaults`, `with_data_dir` and any approval but
  `Never` are refused by name at `open` on a hosted server.
- The `basis-tasks`-backed path, including `basis serve --a2a`, is unchanged.

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
