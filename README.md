# basis

> **basis** — the minimal set everything else is built from.
> Nothing in it reduces to anything else; your host supplies the rest.

basis is an agent harness you **embed**. `basis` is the harness itself, as a library: open a
`Workspace`, mint runs from it, read one event stream, plug your own code into the seams. It carries
no protocol, no transport and no terminal code, so an embedding host's dependency graph states what
it uses ([ADR-0011](docs/adr/0011-layered-crates.md)); `basis-acp` and the binary are thin shells.

```rust
// [dependencies] basis = "0.4"
let workspace = basis::Workspace::open("/repo").await?;
let mut run = workspace.prepare("what does this repo do?")?;
let report = run.execute(basis::CollectingSink::default()).await?;
```

To try it as a command, set a provider key — `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`,
or `OPENROUTER_API_KEY` — and point it at a repository. At a shell a prompt runs and answers, while
leaving a durable handle behind it; any endpoint serving the OpenAI **`chat/completions`** API
works — Ollama, LM Studio, vLLM, llama.cpp, a gateway, a proxy — URL as published, `/v1` handled.
(That is the wire "OpenAI-compatible" means in the wild. OpenAI's own **Responses** wire is what
the `openai` provider already speaks; a proxy that serves it instead is reached by saying
`RuntimeBuilder::with_wire(Wire::Responses)`.)

```sh
cargo install basis-cli          # the binary is `basis`
basis "summarize what changed in the last three commits"
basis spawn -C ../other-repo --deadline 10m --tool-budget 40 "find the slowest test, explain why"

export BASIS_BASE_URL=http://127.0.0.1:3455/v1 BASIS_API_KEY=…
basis spawn --model gpt-5.6 "explain the module layout"

basis spawn --provider ollama --model qwen3 "explain the module layout"   # no key: none needed
```

A server that takes no key — Ollama and LM Studio by name, or any base URL with no key
exported — is spoken to with no `Authorization` header at all; one that wanted a key answers
401 in its own words rather than basis refusing up front.

## Lightweight, as a number

- **`basis`'s direct dependencies are `mentra` plus six utility crates** — `async-trait`,
  `serde`, `serde_json`, `serde_yaml_ng`, `thiserror`, `tokio`.
- **MCP compiles out.** `default-features = false` builds a core with no MCP concept at all: no
  `.mcp.json` discovery, no `McpConfig` on a run, no servers registered
  ([ADR-0012](docs/adr/0012-one-contract-many-bindings.md)). Custom tools remain, because MCP was
  only ever one of the ways to reach them.
- **~10 MB release binary** (10.7 MB measured at 0.6.0), down from cargo's 24 MB default. Each of the four profile settings that
  gets there is argued in the workspace manifest, including the one deliberately absent:
  `panic = "abort"` turns any panic into a dead process, which is what an embedded harness and a
  long-lived server exist to avoid.
- **~29k lines of Rust** across the three crates, ~40k with tests.

## The SDK

**A workspace opens once and mints runs.** Opening settles everything that belongs to the repository
rather than to the prompt — context documents, the resolved model, skills, templates, hooks, MCP
connections — so `prepare` is *synchronous*, and a twenty-way fan-out reads `AGENTS.md` once
([ADR-0010](docs/adr/0010-the-crate-is-the-workflow-surface.md)). `Workspace` is `Send + Sync`, so
those runs can be spawned tasks.

What belongs to the *process* rather than to the repository — the provider and its credential,
where history is kept, the host's own interceptors, the gate that puts a consequential call to a
run's `Approver` — is a `Runtime` ([ADR-0018](docs/adr/0018-the-runtime-owns-the-process.md)).
`Workspace::open("/repo")` builds a private one bound to that repository and is unchanged by
the split; a host opening N of them builds `Runtime::builder().build()?` once and hands each
workspace an `Arc` of it, so N repositories cost one provider resolution and one history store.

Keep the run and send again for a conversation — `run.send("and which of those is riskiest?", sink,
AllowAll)` — because the session survives the turn, and `run.agent_id()` is the handle
`Workspace::resume` takes in a later process.

Bounds are builders on `RunSpec` — one shape, however the run is made — and every bound ends the
run *gracefully*: the stream closes the way it
always does and whatever the model committed is kept. `report.stopped_by` is
`Some(Bound::Deadline | Bound::ToolBudget | Bound::TokenBudget)` when a bound ended the run rather
than the work ([ADR-0014](docs/adr/0014-watch-retired-runs-are-boundable.md)).
[docs/embedding.md](docs/embedding.md) is the full reference for the SDK sections that follow.

### Answers you can branch on

A run that answers in prose composes with nothing, because the next step has to parse English to
find out what happened. `output::<T>()` asks for a declared shape instead: the model is handed one
terminal tool whose input *is* the answer.

```rust
// findings_spec(): a name, a description, and a JSON Schema you write yourself
let output = run.output::<Findings, _, _>(SUBMIT, findings_spec(), sink, AllowAll).await?;

for finding in output.value.findings.iter().filter(|f| f.blocking) { … }
```

The schema is yours to write rather than derived from the type, because its field descriptions are a
*prompt*: they are what the model reads to decide what belongs in each field. One caveat worth
reading twice: by default a typed turn **shapes** rather than works, holding the answering tool
alone, so it answers from what earlier turns gathered. Ask it to read *and* answer and it returns a
well-formed answer from a model that opened nothing, reported as a success. Either read on an
ordinary turn and shape on the next, or hand the turn its tools back with `with_tools()`.

### One allowance across many runs

Dividing a limit across a fan-out starves the runs with something to say; granting it per run
multiplies the bill by N. A `BudgetPool` is the single figure in between:

```rust
let pool = basis::BudgetPool::new(500_000);
let mut reviewer = workspace.prepare(pool.spec("review the tests"))?;
```

It is soft, and honestly so: usage is known only once a round has streamed, so a job lands at up to
the limit plus one in-flight round per concurrent run. A turn drawing on a spent pool is refused with
`RunError::BudgetExhausted` *before* its prompt is sent — a decision with its own name, so a fan-out
stops minting on it instead of retrying it like a provider error.

Work a run delegates through `spawn` is inside the **bound** and, since mentra `5f303b8` and
basis `e22aa63`, inside the **tally** too: the subagent runs on the parent's accounting handle,
the child's usage is relayed onto the parent's stream, and `RunReport::usage` agrees with the
figure the bound stops on. [docs/REDESIGN.md](docs/REDESIGN.md) records the gap as closed.

### One stream for many runs

Each run wants a sink of its own; a host wants one view of all of them without losing which run said
what. `EventFanIn` mints one tagged sink per run and merges them:

```rust
let fan = basis::EventFanIn::new();
let (a, b) = (fan.sink("tests"), fan.sink("docs"));
let mut merged = fan.into_events();   // minting closes here
while let Some(tagged) = merged.recv().await { … }
```

The tag rides outside `Event`, so the versioned wire schema stays what its number promises. The
stream ends when the last sink is dropped — and a finished run hands its sink back inside its report,
so a report held past the join is a branch of the stream held open. That is the one sharp edge in the
design: hold the answers, drop the reports.

### Structured concurrency

In process, concurrency is the host's tokio: a fan-out is a `tokio::task::JoinSet`, a stop
button is the `CancellationToken` a `TurnOptions` hands back, and what keeps an unattended branch
finite is the bounds — deadline, tool budget, token budget — not a scheduler of basis's own
(`examples/review_workflow.rs` runs that shape live). The four ADR-0017 rules — `spawn` returns a
handle immediately, `wait` observes a terminal state without rerunning anything, `cancel` flows
downward to attached descendants, detached work is a new root — are the CLI's durable-task
contract below ([ADR-0017](docs/adr/0017-structured-agent-concurrency.md)), where a handle is
something any process can name and the wait rules earn their keep.
Stopping one turn is two signals:
`TurnOptions::cancellable()` abandons the turn and rolls it back, which is a client's stop button;
`TurnOptions::stoppable()` ends it at the next round boundary, keeping what the model committed.

## The seams

**`Approver`** answers *may this happen*. `AllowAll` (what a run with no approver gets) and `DenyAll`
ship in `basis`; everything between them — allow edits but deny the network, ask over Slack with a
timeout, escalate after the third refusal — is an impl, and a refusal names its reason, since that
reason is what the model reads as the call's result.

**Interception** answers *may this happen, in this form*, and is one contract with two bindings
([ADR-0012](docs/adr/0012-one-contract-many-bindings.md)): a repository declares a subprocess in
`.basis/hooks.json`, and an embedding host writes `Runtime::builder().with_interceptor(…)` — host
scope is runtime scope ([ADR-0018](docs/adr/0018-the-runtime-owns-the-process.md)) — so its own
compiled code gets the say, which is what you want when the guard needs a vault handle, a token
you just minted, or a regex that lives in a config struct. `intercept(&HookRequest)` answers
`HookOutcome::Allow`, `Deny`, or `Modify`, and `basis::async_trait` is re-exported so writing the
impl costs your manifest nothing.

Both bindings speak the same allow/deny/modify vocabulary and are folded by one chain: interceptors
first (registration order), then global hooks, then workspace hooks — the further a participant is
from the workspace's own data, the earlier it speaks. First refusal wins, so your compiled guard can
refuse before a repository's program is spawned at all, and a participant that errors or panics
**denies**.

Both are asked again *after* a call — `review(&HookRequest)` in process, `"event":
"post_tool_use"` in the file — because whether a command printed a credential is not knowable
from its arguments. There the answers are keep, `Replace` with a different result, or `Deny`,
which shows the model the reason instead. Nothing there can un-run anything: the event stream
already carried what the tool really returned, so what this decides is the model's view and not
the record.

**Sinks** are the third seam: anything `FnMut(Event) -> io::Result<()>` is one, beside
`CollectingSink`, `NullSink`, `JsonlWriter`, and the tagged sinks above. **Where history goes** is
yours too — `RuntimeBuilder::with_store_dir` names a directory, `with_ephemeral_history` uses an
in-memory store that survives nothing. Unset, mentra keys a database by the *process's* current
directory.

## What the workspace contributes

The core has no opinions. Task-specific behavior enters through data — the prompt, the workspace, and
config — never through code in `basis`:

- **`AGENTS.md`** — a global config directory, then each ancestor outermost-inward, then the workspace
  root; later files are more specific, and all are named in `run_started`. `CLAUDE.md` is read where a
  directory has no `AGENTS.md`, so a repository that carries only the older name is not a repository
  with no instructions; a directory holding both contributes the `AGENTS.md`.
- **Skills** — four roots, most specific first: `.basis/skills/` and `.agents/skills/` in the
  workspace, then `skills/` in the global config directory and `~/.agents/skills/`. The `.agents`
  pair is the directory other harnesses read, so a skill written once is found here too. Loaded by
  name on demand, so only descriptions cost context, and a nearer root shadows a name rather than a
  whole directory.
- **Prompt templates** — `.basis/templates/*.md` with `$ARGUMENTS` and `$1`, `$2`…; a nested path is a
  namespace (`git/commit.md` → `git:commit`). ACP clients get them as commands.
- **The model choice** — `.basis/config.json`: `provider`, `model`, `effort`, schema-versioned, with
  `${VAR}` expansion, layered over a `config.json` in the global config directory. Without it, no
  `--model` means whatever the provider lists newest *today*, which is not a thing a repository
  chose. A flag still wins; the file still beats the environment. `base_url` is honored only from
  your own global file, and a workspace file that sets it is refused by name: a file a repository
  ships must not be able to point the model's traffic — and the key on it — somewhere you did not.
- **Declared tools** — `.basis/tools.json`: a name, a description, a JSON schema and an argv array,
  and the model gets a tool that pipes its input to that program's stdin and reads stdout back. No
  shell anywhere on the path, so nothing has to be quoted or encoded around quoting. Every one is
  consequential — the format cannot say "read-only" — and the approver is shown the command, not
  just the name.
- **MCP servers** — `.mcp.json`, the same shape other agents read, with `${VAR}` expansion. An ACP
  client can send servers on `session/new`; both sets are honored.
- **Hooks** — `.basis/hooks.json`: commands that take JSON on stdin and answer `allow`, `deny` with a
  reason the model sees, or `modify` with a replacement input. An entry that says `"event":
  "post_tool_use"` is asked after the call instead, is shown what the tool returned, and may
  `replace` it. Any language; one that breaks denies.
- **Memories** — `.md` files with `name`/`description`/`type` frontmatter, indexed into the system
  prompt at open: name, one line, path — the body stays on disk for the model to `read` when the
  description warrants it. Two roots: `memory/` in the global config directory, and `memory/` beside
  the runtime's store when history has a named directory; a workspace memory shadows a global one by
  name. No database and no memory tool — recall is `read`, search is `grep`, writing one is `write` —
  and zero memories cost zero context.

[docs/conventions.md](docs/conventions.md) is the one-page reference: every file, every directory,
every `BASIS_*` variable, and which wins when two say the same thing.

- **Command targets** — a host can register executors by name, and a command can say which one
  it wants: `!@mac xcodebuild -list` where `!cargo test` runs where basis is running. For the
  container-on-a-Mac case. basis routes; the host writes the executor; nothing about it is
  confinement ([ADR-0021](docs/adr/0021-a-command-names-where-it-runs.md),
  [docs/targets.md](docs/targets.md)).

Details of each, and of the one `spawn` tool carrying both commands and delegation
([ADR-0016](docs/adr/0016-one-delegation-surface.md)), are in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## The same core, two shells

`basis serve --acp` speaks the [Agent Client Protocol](https://agentclientprotocol.com) (JSON-RPC 2.0
over stdio) over `basis`'s event stream, so any ACP client drives it with no basis-specific client
code. An ACP session *is* a mentra agent, so `session/load` resumes a conversation from a previous
process and basis stores no mapping of its own
([ADR-0007](docs/adr/0007-acp-sessions-and-the-dispatch-loop.md)); permission requests become
`session/request_permission`, so approval is the client's UI, not basis's. A client can change the
model and the reasoning effort per session through `session/set_config_option`, and send an image
alongside its text. `basis serve --bridge` puts
the same server behind a websocket for a browser client, binding loopback and serving nothing until
an `--allow-origin` is named — a websocket handshake is exempt from the same-origin policy.

The CLI is one small grammar ([ADR-0015](docs/adr/0015-cli-grammar.md)). A positional argument that
names no subcommand is a prompt; bare `basis` prints usage rather than starting a server:

```
basis "<prompt>"                     # shorthand: exactly `basis spawn "<prompt>"`
basis "/<template> <args>"           # a prompt that is a `.basis/templates` command
basis spawn "<prompt>"               # at a shell: run it here and print the answer
basis spawn "<prompt>" --resumable   # mint the agent, print its handle, drive nothing
basis spawn "<prompt>" --await       # inside a task: wait for the terminal result
basis spawn "<prompt>" --continue    # a new task on the conversation last worked in here
basis spawn "<prompt>" --session <TASK>  # the same, on the one that handle names
basis list                           # this workspace's tasks, last worked in first
basis send <ID> "<message>"          # enqueue a later turn and print its message ID
basis send <ID> "<message>" --await  # enqueue, then await that message's reply
basis ask <ID> "<question>"          # send with the correlated reply wait implied
basis wait <ID>                      # repeatably observe the task's terminal result
basis wait <ID> --message <MID>      # await/retry one message's correlated reply
basis cancel <ID>                    # request cancellation (attached descendants too)
basis watch <ID>                     # observe bounded/replayable progress
basis inbox [ID]                     # list bounded message/reply summaries
basis serve --acp                    # ACP server on stdio — what an editor spawns
basis serve --bridge                 # the same ACP server on a websocket, for a browser
basis fingerprint                    # the workspace's hash, for a loop you write yourself
```

Handles are durable: an agent is a checkpoint on disk under one global data directory
(`BASIS_DATA_DIR`, else `XDG_DATA_HOME`, else the platform data home), so `wait`/`watch`/`cancel`/`inbox`
still answer after the submitting process exits — there is no resident process of any kind
([ADR-0019](docs/adr/0019-the-filesystem-is-the-coordination-surface.md)). The liveness contract is
plain: **an agent advances only while a process is attached to it.** Which process that is follows
from where the command ran ([ADR-0020](docs/adr/0020-spawn-routing-is-decided-by-the-environment.md)):
at a shell, `basis spawn` is that process, so it drives the agent and prints the answer, and the
handle stays durable behind it. What that terminal sees is split by stream: **stdout is the
assistant's answer, streamed as it is produced; stderr is the work behind it** — the model, each
tool call and how it ended, and the one `next:` line at the end. So `basis "summarize this" >
notes.md` leaves a file holding the summary, and `2> progress.log` keeps the rest of it — with
one compact `12.3k in · 1.2k out` at the end, the same counts `--json` carries as `usage`.
Closing the terminal costs you nothing: `basis list` reads the same task directories back, last
worked in first, and `--continue` starts a *new* task on the conversation at the top of that list —
a new handle over the same history, because a settled task accepts no messages at all, and this
run's bounds and model are this run's. *Worked in* means a turn ran in the task or you sent it a
message; reading a run back — `watch`, or `wait` on one that has settled — does not move it, so the
list holds still while you read it. `--session <TASK>` names one instead, refusing a task something
is already driving. A prompt whose first token is `/name` is one of the workspace's
[`.basis/templates`](docs/ARCHITECTURE.md) commands, rendered with the rest of the line as its
arguments — the same names an ACP client offers; a first token with a second slash is a path and
passes through, and `basis spawn -` sends a literal one on stdin.
Inside another task (`BASIS_TASK_ID` set) it prints the handle of a
*resumable* agent instead, because a parent turn that blocks on its child is how a wait-for cycle
starts — `--await` is the parent's explicit opt-in, and `--resumable` is the shell's opt-out.
`basis wait <ID>` attaches to a resumable agent and produces the result, and
backgrounding is the OS's job — `basis wait <ID> &`, `nohup`, tmux, `systemd-run`, CI. Cancellation
is honored at turn boundaries (a hung tool call is ended by the deadline), and a crash mid-turn
loses the in-flight round: re-driving it may repeat tool side effects, because a checkpoint
restores state, never effects. Four rules are the load-bearing part — the durable-task contract
every handle obeys across processes (in process, concurrency is the host's tokio; see
"Structured concurrency" above):

- **Ownership is a tree.** An attached child inherits its parent's cancellation and the narrower
  deadline. A successful parent keeps attached children in scope until they settle; a failed or
  cancelled one requests downward cancellation and publishes its terminal state only after they do.
  `--detached` starts an independent root. An agent's own commands inherit `BASIS_TASK_ID`, so
  `!basis spawn "…"` from inside a task attaches a child to it.
- **Waits cannot deadlock.** Self, ancestor, and same-tree peer waits are rejected outright. A
  wait between independent trees is a process observing a file; a cycle is two observers, and each
  ends at its own finite deadline with exit `3` and a durable retry handle.
- **The inbox is bounded.** At most 16 messages over a task's lifetime; bodies and replies are
  summaries capped at 4 KiB with truncation metadata. A worker past its own turn accepts no new
  messages or children.
- **Waiting is not owning.** If a wait times out the task continues, and `basis wait <ID> --message
  <MID>` retries the same durable reply without rerunning it. Local tasks carry a finite 30-minute
  default deadline, since the submitter exits, and it binds an agent nobody attached to: the first
  attach after it lapses settles the task as failed with `stopped_by: deadline` instead of starting
  a run whose time is already spent.

E2 change note, for an upgrade: the hidden per-workspace daemon and its registry are gone, and
nothing moves with them. `BASIS_REGISTRY_DIR` is removed, and whatever the registry held — under it,
under `BASIS_CONFIG_DIR/agents`, under `XDG_RUNTIME_DIR`, or in the temp directory — is **not
migrated**, conversations included, because the daemon kept mentra's store beside its registry in a
directory the platform is entitled to erase. Pre-E2 task handles therefore do not resolve.
`BASIS_DATA_DIR` is the override that replaces it (`BASIS_CONFIG_DIR` still names the *config*
directory and is unchanged), and conversations now live at `<data-dir>/workspaces/<key>/store` — a
data home rather than a runtime directory, which is what makes "resume it tomorrow" mean anything.
`basis spawn` reports `resumable` rather than `running` for an agent with no attached process, and
the durable `orphaned` terminal state is retired: nothing restarts out from under a task anymore.

`--json` gives one bounded JSON object per lifecycle command; at a shell `basis spawn --json
"<prompt>"` streams the attended JSONL event stream, first line always `run_started` with
the schema version, last always `run_finished` carrying `stopped_by` when a bound ended the run.
`basis run` is a compatibility alias. Exit codes are contract, so a caller branches without parsing:

| Code | Meaning |
|---|---|
| `0` | the run finished |
| `1` | the run failed, or basis could not start it |
| `2` | the invocation was wrong |
| `3` | a bound tripped (`--deadline`, `--tool-budget`, `--token-budget`); committed work was kept |

`3` is deliberately not `1`: "the model ran out of the time you gave it" and "the provider refused
the request" call for different reactions. basis ships no scheduler either — an interval belongs to
cron, systemd, CI, or a tokio task in your own binary — but it ships the piece that is easy to get
wrong. `basis fingerprint`, and `Workspace::fingerprint()` in process, digests `git ls-files` (path,
length, mtime, plus `HEAD`) and reports *changed* in every uncertain case: a false "changed" costs
tokens, a false "unchanged" silently stops the loop. The loop itself is composition, written out in
[docs/ARCHITECTURE.md §8](docs/ARCHITECTURE.md).

## Security posture

basis claims no sandbox. A run holds whatever authority the user account that started it holds, and
nothing inside the process narrows that ([ADR-0013](docs/adr/0013-the-host-owns-the-boundary.md)).
What is in-process is **hygiene**: the agent is scoped to the workspace, and `.git/hooks` and
`.git/config` are denied to the *file tools*, because a file written there runs on the next commit.
A shell redirect walks straight past both, since nothing parses shell.

Commands are on by default, because a harness that cannot run the test suite does little real
work. `--no-shell` shuts them off; file writes still land, so a run that must change nothing wants
`--approve never`. Both narrow what *this run* does rather than confining the process. `--approve`
is `always` (the CLI default), `never`, or `prompt`; read-only calls are never queued, but neither a
command nor a delegation is a read, so both `spawn` modes reach an approver. `prompt` needs someone
to ask, and asks at the terminal of whichever process is driving the agent: it is the default over
ACP and works wherever a run is attached to a terminal, while `--resumable` work **rejects** it
rather than silently allowing. Task state lives in a user-private (0700) data
directory and never records a credential — an agent's executor is whichever process attached to
it, holding that shell's environment, so there is nothing on disk to leak and no daemon holding a
key on your behalf; the bridge's `Origin` allowlist starts empty. The boundary, where you want
one, is the OS's — [docs/containerization.md](docs/containerization.md) has the read-only-root
pattern, what it protects, and what it does not. A command routed to a registered target
(`!@mac …`) is no different: it runs with whatever authority the executor the *host* wrote
holds, basis never calls a target "the host", and [docs/targets.md](docs/targets.md) says so
in its own words.

## Examples

`cargo run -p basis --example <name> -- …`, with a provider key set.
[`embed.rs`](basis/examples/embed.rs) reacts to events as they arrive,
[`conversation.rs`](basis/examples/conversation.rs) takes two turns on one session,
[`watch.rs`](basis/examples/watch.rs) is the recurring-run loop, and
[`reviewed_shell.rs`](basis/examples/reviewed_shell.rs) is an `Approver` reviewing the agent's
commands with a cheap typed turn of its own. [`review_workflow.rs`](basis/examples/review_workflow.rs)
composes the lot: one workspace, one budget, typed findings, one merged stream, a folded verdict.

## Status

This README describes only what is built, and all of the above is: P0–P4 and the SDK-first redesign
through Phase E — the ACP server with modes, session listing and history replay; conversation and
resume; durable `spawn`/`send`/`ask`/`wait`/`cancel`/`watch`/`inbox` over the filesystem; MCP from
`.mcp.json` and from the client; templates as commands; hooks and interceptors; the websocket
bridge; branching; compaction, whose defaults are basis's own — every tool result the model was
shown stays in front of it — and whose knobs are `WorkspaceBuilder::with_compaction`; and the SDK
proper. Named honestly, still open: context-window awareness — nothing here knows how large a
model's window is, so the summarizing trigger is a fixed token count rather than a fraction of it —
the packages convention, and provider
OAuth; the delegation bound-vs-tally gap above; and **nobody has driven this from Zed or JetBrains
yet** — it is verified against the protocol and its official client library, not against the
ecosystem. The three crates are on crates.io at one version, as is mentra. CI runs fmt, clippy at
`-D warnings`, and the full suite on Linux, macOS and Windows, plus MSRV (1.88, edition 2024).

Every addition faces one check: does it make embedding cheaper for a Rust host, is it a convention
other agents already speak, or is it a seam? If none of the three, it is the host's code, the
client's UI, or the OS's job — hence no TUI, no scheduler, no container, no workflow DSL.

Docs: [PROPOSAL.md](docs/PROPOSAL.md) (why) · [ARCHITECTURE.md](docs/ARCHITECTURE.md) (how, with §8
for `--effort`, custom endpoints, and the hooks `shell`→`spawn` migration) ·
[conventions.md](docs/conventions.md) (every file and variable basis reads, in precedence order) ·
[embedding.md](docs/embedding.md) (the SDK in detail) · [targets.md](docs/targets.md) (running a
command somewhere else) · [REDESIGN.md](docs/REDESIGN.md) (ledger) ·
[adr/](docs/adr/) (21 locked decisions) · [proposals/](docs/proposals/) (deferred ideas).

## License

MIT. See [LICENSE](LICENSE).
