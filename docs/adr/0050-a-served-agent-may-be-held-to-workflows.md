# ADR-0050: A served agent may be held to workflows

Status: accepted

## Context

A served agent often offers several distinct capabilities, and its owner may
want some of them for some callers only. Who may use which is not this
server's to decide: a gateway in front of it knows the caller and signs what
they are allowed into the token it sends with each request (dial's ADR-0053,
whose agent side this is). The gateway cannot enforce the answer, because it
forwards a message and the capability that answers it is chosen here, often by
a model. So the server must enforce, from what the gateway signed.

Four facts shape how.

- An instruction is not enforcement. A model told not to use a tool can still
  call it. A tool the run was never offered cannot be called.
- A run's tools were fixed at its conversation's first mint. mentra persists
  an agent's tool profile, and `Workspace::resume` refused a profile that
  stated a roster, so a conversation continued for a caller allowed less kept
  the tools of the caller it began with. Skills had no per-run limit at all:
  they are registered on the runtime for every agent.
- `basis-tasks` builds a run in whichever process attaches, so what a run is
  offered must be recorded with the task.
- A task outlives the request that started it, and a caller's rights can
  change while it runs. Which set each later request carries is the
  gateway's to decide. dial's lets a task keep the workflows it started
  with (its ADR-0054): the task's own caller reads, watches and cancels it
  with that set, and only the gateway's own look at the task, and a reply
  to it, carry what is still allowed.

## Decision

**A server may be configured with workflows. Each is a skill on its card with
the tools and skills behind it. A request may use only the workflows its
authenticator verified for it; a run is built with those workflows' tools and
skills and no others; and a task is stopped once a `GetTask` or a reply shows
that a workflow it was built with has been withdrawn.**

### What a run is offered

- mentra gains `AgentConfig::skill_profile` beside `tool_profile`, and
  `Session::set_profiles` to replace both on a resumed session (mentra 0.36).
- `RunProfile` gains `with_skill_roster` (`SkillRoster::All` or `only`).
  `Workspace::resume` no longer refuses a stated roster: the rosters a resumed
  run states replace the stored ones, in one persisted write made before
  anything else. A run that states none keeps the conversation's own, so no
  existing host is silently widened.
- `basis_tasks::RunSpec` gains `with_tool_roster` and `with_skill_roster`.
  Both are written into the task's run options, so any attach builds the run
  with them, and a task that continues a conversation replaces the rosters
  that conversation last ran with.
- A child-policy roster override stays inside an allow-listed parent's list.
  It used to restore only the parent's hidden names, which an allow-list has
  none of.

### Workflows on a server

- `ServeConfig::with_workflows(Workflows)` is the one source for three things
  that must agree: the card's `skills[]` (one per workflow, in order), the
  extension the card declares, and each run's roster. `Workflows::declaring`
  takes the extension's URI, because which protocol decides the allowed set
  is the host's; basis names none.
- `Principal::with_workflows` carries the verified set: a list the server
  reads, not a string attribute. The server keeps the ids its card lists, in
  card order. A principal with none, on a server with workflows, is refused:
  the request did not come through whatever decides them. So is one allowed
  none of the card's workflows.
- A `basis-tasks` run gets `ToolRoster::only` of the allowed workflows' tools
  and the shared ones, and `SkillRoster::only` of their skills. `load_skill`
  is offered exactly when there is a skill to load. Nothing else is implied:
  `attach_file` reads workspace files, so a workflow that returns files names
  it.
- When some workflows are closed to the caller, the run's prompt is prefixed
  with a note from the host naming them and whom to ask, so the model says
  which workflow is closed rather than answer another way. The note is not
  the enforcement and is not written into the history a caller reads. It is
  in the prompt rather than the system prompt because a resumed conversation
  keeps the system prompt of its first run.
- A hosted task's host is handed the set in `HostedStart::workflows` and does
  its own dispatch inside it. A workflow's tools and skills configure a
  `basis-tasks` run, so a hosted config that names any is refused.

### A task is bound to its workflows

- The index record keeps the workflows a task was built with: everything the
  message was allowed, or the fewer a host names through
  `TaskHost::workflows` when its dispatch is fixed.
- Two requests can show a withdrawal: `GetTask`, and a reply (a message
  carrying the task's `taskId`). Each holds the task to its token's set, as
  signed. If a bound workflow is missing, the record is marked withdrawn
  with a reason naming the workflow and whom to ask, the run is cancelled,
  and from then on the task is `FAILED` with that reason whatever its
  backend later reports. A stream already open reports it too.
- A token carrying the set a task started with always passes, so the task's
  own caller can read it whatever was withdrawn since.
- `CancelTask` is never judged against the token's workflows. A cancel can
  only stop work, and whoever started a task may always stop it; a gateway
  that stops a task for them sends the same request.
- Nothing else is taken for a withdrawal. `ListTasks`, `SubscribeToTask`, the
  push-config methods and a new message in the task's conversation carry
  sets decided for themselves, and a caller may narrow a message to one
  workflow: a narrower set there says nothing about the task.
- Every request, whatever it names, is still refused when its principal
  carries no workflows at all.
- A task that had settled is left as it settled. An unsettled task with no
  recorded workflows was built before the server held anyone to them, with
  every tool the workspace has. It is stopped when the server opens, before
  anything drives it again, and never by a request.
- The card's skills are the workflows and nothing else. `A2aService::open`
  refuses skills added by the identity or the extended card, an empty set, an
  id given twice, and the workflows' extension declared a second time.

## Consequences

- The promise the declared extension makes is kept by construction for a
  model-chosen run: the closed workflows' tools and skills do not exist for
  it, on the first message of a conversation and on every later one.
- Enforcement is as good as the mapping. A tool that reaches another
  workflow's work by a different route — `spawn` on an agent granted a shell,
  an MCP tool that proxies — must be named only on the workflows that may do
  that work. Tool names are exact; there is no pattern for a whole MCP
  server.
- What an earlier run of a conversation read or wrote stays in its
  transcript, including a skill body it loaded. The roster decides what a run
  may do from now on.
- A task whose workflow is withdrawn is lost, not narrowed: the run was built
  with tools its caller may no longer use, and nothing narrower than stopping
  it is sound. It is stopped when a `GetTask` or a reply says so, which is
  when the gateway next looks, not the instant the mapping changes.
- The server cannot tell the caller's own `GetTask` from the gateway's look
  at the task: both are a `GetTask`, and the token is all that differs.
  That only the second ever carries a narrower set is the gateway's
  guarantee, not something checked here.
- `Workflows` is opt-in. A server without it is unchanged, and ignores what a
  principal says of workflows.
- mentra's `AgentConfig` gains a field and basis requires mentra 0.36.

## Verification

`basis-a2a/tests/workflows.rs` drives a served workspace over the wire with a
scripted model and an authenticator that reads the allowed set off each
request's token: a model that calls a closed workflow's tool by name does not
run it, a conversation continued for a caller who lost a workflow is rebuilt
without its tools and skills, a running task fails with the reason on a
`GetTask` or a reply whose token lacks its workflow, a narrower set on any
other request withdraws nothing, and a cancel cancels whatever its token
says. `basis-tasks/tests/rosters.rs` and
`basis/tests/run_profile.rs` cover the layers beneath.
