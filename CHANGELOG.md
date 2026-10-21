# Changelog

All six crates — `basis`, `basis-host`, `basis-tasks`, `basis-acp`,
`basis-a2a` and `basis-cli` — share one version. Decisions behind each entry
live in `docs/adr/`; this file says what changed for someone upgrading.
Releases before 0.15.0 are described by their tags and ADRs only.

## Unreleased

Breaking: built on mentra 0.33. `OpenError` gains `ExtensionDeclared` and
`ExtensionRequired`, so a host matching it exhaustively needs arms.
`TurnOptions` gains a public `locals` field, so a struct literal without
`..TurnOptions::default()` needs it.

### A turn carries locals its tools read

- `TurnOptions::locals` / `with_locals` attach a `RunLocals` — values keyed
  by type, re-exported from mentra — that every tool the turn calls reads
  with `ToolContext::run_local::<T>()` (ADR-0044). The run's own
  (`PreparedRun::with_bounds`) are the floor; the turn's layer on top. A
  delegated child, blocking or background, reads its parent's. One
  workspace's native tools can now serve many callers, each turn seeing its
  own, instead of opening a workspace per caller.

### A hosted agent speaks an extension of its own

- `AgentIdentity::with_extension(AgentExtension)` declares a host's own
  extension on the card — uri, description, params — after basis's hops and
  approval extensions (ADR-0043). `A2aService::open` refuses one marked
  `required`, which nothing would enforce, and one whose URI basis declares
  or that is declared twice.
- `HostedStart` carries `metadata` (the message's), `request_metadata` (the
  `SendMessage` request's — where `dial --meta` and `spawn`'s `metadata`
  land) and `extensions` (the URIs the message names), each exactly as sent
  and empty when absent.

### A hosted task's deadline is kept

- A hosted task still unsettled at its deadline (`HostedStart::deadline_ms`)
  is now canceled by the server through `TaskHost::cancel`, once — as a
  `basis-tasks` task's deadline always ended it — including one resumed after
  a restart with its deadline already past. The server goes on awaiting it,
  so the standing the host reports is what the caller sees.

## 0.20.1 — 2026-10-20

No breaking changes.

### A host's task scope reaches a background child

- `RuntimeBuilder::with_task_scope` (and `SpawnTool::with_task_scope`) wraps
  every future `spawn` spawns for a background child. The hook is called
  synchronously on the spawning task, before the spawn, so a host whose
  tools read tokio task-locals — which conversation, which caller — can
  snapshot them there and re-scope them onto the future it hands back.
  Without it a background child's tools found the locals unset, since a
  task-local does not cross `tokio::spawn`; blocking delegations never had
  the problem. `basis::tools::{TaskScope, TaskFuture}` name the types. A
  scope that drops the future without running it is reported to the model
  as a failed delegation rather than waited for.

## 0.20.0 — 2026-10-20

Breaking: `SpawnMode` gains `Wait`, so a host matching it exhaustively needs
an arm. `spawn`'s input schema no longer requires `input` on a runtime with an
agent ledger (every one basis builds), since a wait has none.

### A delegation may run in the background ([ADR-0042](docs/adr/0042-a-delegation-may-run-in-the-background.md))

- `spawn` takes `background: true` beside `input`: the task — for a subagent,
  a teammate or a remote agent — starts on a task of its own and the call
  returns its handle at once: the subagent's id, the teammate's name or id,
  the remote agent's name. A command cannot run in the background.
- The answer arrives on its own: a child that finishes while the model is
  doing something else leaves its result with mentra's `Deferred` handle, and
  the runner puts it in front of the model before the next request, as a
  `<delegation-result agent="…" id="…" status="…">` carrying the delegation's
  own record and the child's end on the parent's stream.
- `wait: [handles]` in a call with no `input` collects one or more — all of
  them, or the first to finish with `any: true` — and `@<handle>` alone
  collects one. A result a wait collected is never also delivered on its own.
- A turn does not end while a child it started is still running: basis's
  own round strategy (`JoinAtEnd`, composed in front of the host's) joins the
  children when the model ends its turn, and mentra runs one more round so the
  model reads what they answered. Children run on the parent's
  `RunOptions::child` — its cancellation, stop, deadline and token counter.
- `RuntimeBuilder::with_delegation_width` (and `SpawnTool::with_delegation_width`)
  caps how many of one agent's delegations run in the background at once;
  `DEFAULT_DELEGATION_WIDTH` is four. Checked in the preview like depth, so
  no approval and no remembered rule lifts it. Zero turns the feature off.
- The approver's preview carries `background: true` on a start and
  `wait`/`any` on a wait; a wait presents as read-only (`None`) and is never
  asked. A background start is mentra's `Background` approval category and
  `BackgroundJob` execution category. `SPAWN_BACKGROUND_FIELD`,
  `SPAWN_WAIT_FIELD` and `SPAWN_ANY_FIELD` name the fields.
- A child's remote conversations are its own, so two background children may
  each hand the same remote agent a task.
- Needs mentra 0.32.0.

## 0.19.0 — 2026-10-15

Breaking: `peer::Message` gains `files` and every `Outcome` variant gains
`files: Vec<Attachment>`, so a host that implements `Peer` or matches an
outcome exhaustively by field needs them. The A2A server's card lists `*/*`
as its input modes and `text/plain`, `*/*` as its output modes, and accepts
request bodies up to 16 MiB.

### A delegation carries files ([ADR-0041](docs/adr/0041-a-delegation-carries-files.md))

- `spawn` takes an optional `files` list of workspace paths beside `input`,
  read under the file tools' read policy and bounded at 6 MiB together. A
  remote agent receives them as A2A `raw` file parts after the text; an
  agent here shares the workspace and is told the paths. A command or a wait
  refuses them. Offered in the schema with `metadata`; shown in the
  authorization preview under `files`.
- Files a remote agent returns are saved under the calling agent's
  `.basis/inbox/<task-id>/` and named in the tool result, outside the
  agent's frame. A file by URL is never fetched; it stays a URL in the text.
- `read` shows PNG, JPEG, GIF and WebP images to the model (mentra), so a
  saved screenshot can be looked at.
- `basis::agents::Attachment` is the file type the peer contract carries;
  `basis::media` re-exports mentra's media-type helpers.
- `basis-a2a` accepts any inline file (revising ADR-0030 §6): a textual one
  is read into the prompt as before, any other is saved under the served
  workspace's `.basis/inbox/<id>/` and named; a file by URL is still refused.
- A task the A2A server spawns is offered `attach_file`, which copies a
  workspace file into its answer (`RunSpec::answering`,
  `Tasks::answer_files`); the completed task's `result` artifact carries
  them as file parts after the text.
- Needs mentra 0.31.0.

### Fixed

- A failed send's guidance no longer carries runs of spaces where its lines
  were meant to join.

## 0.18.1 — 2026-10-14

No breaking changes.

### A delegation carries parameters ([ADR-0040](docs/adr/0040-a-delegation-carries-parameters.md))

- `spawn` takes an optional `metadata` object beside `input`: parameters for
  the remote agent a task is handed to, as `{"project": "lama"}` when the
  agent's card asks for some. Over A2A they are the request's `metadata`
  (`params.metadata`), beside the message; the message's own metadata stays
  the hops extension's. Offered in the schema only when there are remote
  agents.
- Only `@<agent> <task>` takes it: a command, a local task, a wait and a name
  that is a teammate here refuse it, saying how to use it.
- The authorization preview shows it under `metadata`, absent when the call
  gave none. `peer::Message` gains `metadata` and `with_metadata`.
- Needs mentra 0.30.1.

## 0.18.0 — 2026-10-14

### The core speaks to a peer ([ADR-0038](docs/adr/0038-the-core-speaks-to-a-peer.md))

- Naming is core, A2A is a feature. `basis::agents` — `AgentResolver`, the
  resolver chain, the conversation record and `spawn`'s whole `@<name>`
  path — needs no cargo feature. A resolver answers with `Resolved::new(peer,
  name, endpoint)`, where `peer` is any `Arc<dyn Peer>`: one method hands a
  `Request` over within `ExchangeBounds` and returns an `Outcome` (replied,
  finished, working, needs input, needs authorization, failed) or a
  `PeerError`; one reports the output cap. The decision table in `spawn` is
  written against those and nothing else.
- `basis::a2a::A2aPeer` is mentra's client as a peer — the exchange, the
  hops extension, the framing — and `Resolved::over_a2a(client, name)` is the
  one-line way a host's resolver uses it. The `a2a` feature gates exactly
  that module: a2a.json, the configured set, reload, watch and the adapter.
- `WorkspaceBuilder::with_agent_resolver`, `Workspace::remote_agents`,
  `Workspace::remote_conversation` and `PreparedRun::with_remote_conversations`
  are plain API in every build. `RunError` gains `Forget` for a conversation
  record that could not be removed.
- The core's vocabulary is the core's: `Workspace::a2a_agents` is
  `remote_agents`, the `RunStarted` event's and `RunContext`'s `a2a_agents`
  field is `remote_agents`, and `Runtime::a2a_hop` /
  `RuntimeBuilder::with_a2a_hop` are `hop` / `with_hop` — a hop is a hop
  whatever carried it. `a2a_files` keeps its name: it is about a2a.json. A
  build without the feature sees no A2A name outside the `a2a` module.

### Frontmatter is read without a YAML library

- Templates and memories are read with `mentra::frontmatter` (mentra 0.30.0):
  the same flat `key: value` reader `SKILL.md` uses, so the three conventions
  share one scanner and one grammar. `serde_yaml_ng`, and the archived
  `unsafe-libyaml` under it that failed cargo-deny (RUSTSEC-2024-0449), are
  gone from basis. `MemoryKind::as_str` and `MemoryKind::parse` name the
  `type` values; `basis::memory::file_contents` writes quoted values the
  reader takes back exactly.
- mentra 0.30.0 also announces a teammate's later turns as
  `SubagentResumed` rather than a repeated spawn, and keeps one snapshot
  entry per teammate.

### The header reports bindings ([ADR-0039](docs/adr/0039-the-header-reports-bindings.md))

- `run_started` carries `bindings`, one `{kind, files, names}` entry per
  binding the build has — `mcp` for `.mcp.json` and its servers, `a2a` for
  `a2a.json` and its agents — in place of `mcp_files`, `mcp_servers` and
  `a2a_files`. `basis::event::Binding` is the type, and every binding is
  live: `PreparedRun::with_binding` takes a `BindingSource` read each time a
  header is written. A new binding adds one source at the workspace's open
  and touches nothing else. `EVENT_SCHEMA_VERSION` is 3.

### An agent is named ([ADR-0037](docs/adr/0037-an-agent-is-named.md))

- `@<name>` is resolved. `WorkspaceBuilder::with_agent_resolver` installs the
  host's `AgentResolver`, asked before the set `.basis/a2a.json`, the global
  file and `A2aConfig::supplied` configure; the first answer other than
  unknown stands. A resolver hands back a connected `A2aClient` plus the
  agent's id, canonical name and endpoint (`Resolved`); basis keeps the
  conversation under the id, the hop, the exchange and the one `spawn` door.
  `route`, `taught` and `on_change` have defaults, so a resolver over names it
  cannot list teaches one sentence instead.
- A name no resolver knows is a **teammate**: a subagent made under that name
  on the first `@<name> <task>` and continued by every later one, kept on the
  calling agent's ledger row for as long as it lives. It is minted through the
  child policy, whose `ChildContext::name` now says which name, and run on the
  parent's bounds. A reply opens with its name and id; `@<id>` reaches it
  whatever it is called, and `spawn`'s description lists the caller's
  teammates. A name already bound to a teammate stays bound when a remote
  agent appears under it.
- `spawn` gains an optional `reach` field — `auto` (default), `remote`,
  `local` — and `RuntimeBuilder::with_reach_policy` fixes the default and the
  allowed values. The authorization preview adds `reach` (the decision, never
  `auto`) and `agent_id` once known; a teammate declares `LocalState`, a
  remote agent `External`.
- A target name may be namespaced with `/` between parts: `@team/reviewer`,
  `!@team/box`. A namespaced name always means a resolver's agent.
- `Workspace::remote_agents` (`a2a_agents` until ADR-0038) reports every
  taught name, the host's resolver's first, one entry per name.

### Breaking

- **`@<unknown> <task>` makes a teammate instead of being refused.** A host
  that serves only remote agents, or wants the old refusal, sets
  `RuntimeBuilder::with_reach_policy(ReachPolicy::fixed(Reach::Remote))`.
- A remembered `spawn` rule that globs `target` with `*` no longer matches a
  namespaced name: mentra's glob does not cross `/`. Write `**` for every
  agent or `team/*` for a team, as the `cwd` key already requires.
- **Renamed** (ADR-0038): `Workspace::a2a_agents` → `remote_agents`, the
  `a2a_agents` field of `RunStarted` and `RunContext` → `remote_agents` (a
  wire-schema change for a client reading the event stream),
  `Runtime::a2a_hop` → `hop`, `RuntimeBuilder::with_a2a_hop` → `with_hop`.
- **`run_started` schema 3**: `mcp_files`, `mcp_servers` and `a2a_files` are
  gone; read `bindings` and pick the `kind`. `RunContext` loses the same three
  fields; bindings are sources on the run.
- A template or memory frontmatter block that used YAML beyond flat
  `key: value` lines — a list, a nested mapping, a block scalar — is refused
  with a message saying so, where it was silently ignored before. Quote a
  value to keep it as text.
- The authorization preview of a `@<name>` delegation carries two more keys,
  `reach` and (when known) `agent_id`; a rule matching the whole serialized
  input exactly needs updating, one globbing around `target` does not.

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
