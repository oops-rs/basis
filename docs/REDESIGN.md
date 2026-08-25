# basis — Redesign plan

> rev 14 · 2026-08-25 · The transition from the P0–P4 harness to the SDK-first
> shape decided in [ADR-0010](adr/0010-the-crate-is-the-workflow-surface.md)
> through [ADR-0017](adr/0017-structured-agent-concurrency.md). This document is the
> honest ledger of that transition: what exists, what is in between, what is not
> started. `README.md` and `ARCHITECTURE.md` describe the *shipped* state and
> are updated per phase as work lands — never ahead of it.
> **Phases A, B, C and D have landed** (rev 5), with one Phase D item — declared
> subprocess tools — deliberately **held** rather than built for seven revs,
> because no concrete use case existed for it on record and the phase's own rule
> is that its items ship only against one (Bet 7). **Rev 12 ships it**, against
> the use case that finally arrived: see the row in §2 and the item in §3's
> Phase D. The rule held — the feature waited for evidence, and the evidence
> shaped it — which is the outcome Bet 7 was betting on rather than a
> concession to it.
> **Rev 6 records a wave that built no phase.** The five upstream candidates
> §2's tally still had open were closed in mentra and met on basis's side, so the
> footnotes below are records of fixed holes rather than of open ones. What the
> wave did not do is make this document shorter: each fix is written with what
> it cost and what it newly exposes, and §2's tally names the candidates the
> work created on its way to closing the old ones.
> **Rev 7 builds ADR-0016**, which was decided after rev 6 and is the first ADR
> here that made basis register a tool of its own. `spawn` is now the model's only
> route to a command and to a subagent. It reopened the tally the day after it
> reached zero — three new upstream candidates, all named in §2 — which is what
> a new surface does when the discipline is working.
> **Rev 10 ships the local ADR-0017 communication refinement.** Serving is explicit, lifecycle
> results carry actionable `next:` hints, and the hidden per-workspace daemon
> provides durable handles, bounded messaging, detached roots, and repeatable
> observation. Its live wait graph rejects edges that would close a cycle;
> `ask`/`send --await` provide one correlated reply per accepted message, and
> `wait --message` retries that reply without rerunning the task. Parent scopes
> remain open until attached children settle; detached roots stay independent.
> **Rev 11 decides Phase E.** [ADR-0018](adr/0018-the-runtime-owns-the-process.md)
> splits the process-scoped substrate out of `Workspace` as `Runtime`;
> [ADR-0019](adr/0019-the-filesystem-is-the-coordination-surface.md) retires
> the daemon rev 10 shipped — its ownership and messaging semantics survive on
> files: attach-under-lock execution, checkpoints at turn boundaries, and an
> atomically written terminal record as the completion signal, with liveness
> handed to the OS. §3 Phase E sequences the two; E1 has landed across all
> three crates, and E2 has landed — the daemon, its registry, and the wait
> graph are deleted, and `basis/src/local` is the file surface §3 records.
> Phase E is complete.
> **Rev 12 closes Phase D**, which had been carrying one held item since rev 5.
> A production host embedding basis needed Jenkins access as tools, and without
> a registration surface those became shell scripts behind `spawn`, with their
> arguments base64-encoded into command lines to survive shell quoting — a
> concrete cost, on record, of the thing not existing. `.basis/tools.json` is
> the answer ADR-0012 sketched, and the evidence shaped it: the whole design
> turns on there being no shell on the path, and on three refusals — a
> declaration cannot call itself read-only, cannot hide its command from the
> approver, and cannot take a name the runtime already answers to. It reopened
> the §2 tally with two new candidates, both found the same way ADR-0016's
> three were: by registering a kind of tool mentra had not been asked for
> before.
> **Rev 13 records the second upstream wave.** Every candidate §2's tally had
> open went to mentra as one handoff and came back the same day
> (`026fbf5..bfe952b`); basis met each, and six more found while meeting them
> went up and came back the same afternoon. The tally is at zero open upstream
> candidates for the second time, with the one workaround it carries
> (mentra#21) still the only one. What the wave did instead of making this
> document shorter is the same as rev 6: each fix is written with what it
> cost and what it newly exposes, and the three candidates it created on the
> way — none upstream-shaped — are named where the old ones were.
> **Rev 14 is wave 1 — subtract.** The tree's own duplications go: the
> in-process `Supervisor` (ADR-0017's four rules live only as the CLI's
> durable-task contract now), the three spellings of the three bounds
> (`Bounds`), the third spelling of a run (`RunConfig`, and with it `split()`
> and the free `prepare`/`prepare_without_prompt`/`resume`), the four import
> cycles manufactured by `RunError`'s address, the second JSONL shape the
> journal kept beside the stream's, and the string re-encodings in task
> metadata. The wire enums are sealed `#[non_exhaustive]` behind wildcards
> that are not allowed to swallow, and mentra's memory engine is off by D2.
> §2 carries one row per item, each with what it cost and what it newly
> exposes; every public-API removal ships in 0.6.0.

## 1. The target in one paragraph

basis is `Workspace` (discover: AGENTS.md, skills, templates, tool manifests) →
runs (execute / converse / resume, bounded, cancellable, with typed output) →
one event stream → two seams (authorize, tools), plus two opt-in adapters
(ACP for interactive clients, MCP as one tool binding) and a compact CLI
grammar with lifecycle verbs. Orchestration is host-language Rust against the crate — no DSL, no
scheduler, no shipped container, no per-language SDKs. Identity check for any
future addition: does it make embedding cheaper for a Rust host, is it a
convention other agents already speak, or is it a seam? If none — it is the
host's code, the client's UI, or the OS's job.

## 2. Status ledger

Honesty matters here: several decided items are *partially* present today.
Phase A landed in three commits — `4fbe1fd` (watch), `35c9ccb` (shell posture
and CLI grammar), `a246722` (containerization) — and its rows carry them.
Phase B landed in three more — `fbcacb4` (the crate split), `a4c259c`
(approval as the trait alone, `mcp` as a feature), `6192230` (a denial names
its reason again, over mentra `15fdcfe`) — and its rows carry those. Phase C
landed in four — `8b52ebf` (the `Workspace` / run split), `07cf4d1` (typed
output, cancellation, usage, fan-in, over mentra `fce664a`), `e21d632` (the
`BudgetPool`), `0ff745c` (the two acceptance examples). Phase D landed in five
— `f3529be` (credential redaction and a resolution that answers to no shell),
`397ca13` (`with_store_dir`), `e81e5d8` (the interception seam's in-process
binding, and `session/list` working at all), `71cc59d`
(`with_ephemeral_history`), `f76617d` (the last rustdoc warning) — over three
mentra fixes, `0436bae` (delegated task accounting), `b1a83de` (store recovery
deferred to build) and `aa206b7` (the mock's volatile store).

Rev 6's wave belongs to no phase. It closed the five upstream candidates the
tally below had been carrying, in five mentra commits — `c04986a`
(`RuntimeBuilder` nameable), `5a2a68e` (a run records the bound it ended on),
`c30fa9c` (the Responses websocket transport behind a feature), `be65c00` (a
typed turn can keep its tools), `b895ea0` (a remembered refusal says why) — and
met each on basis's side: `ff5fc70` (the `async-trait` re-export), `27ab4c8` (the
websocket gate closed here too), `8e35f3e` and `a2d170a` (the token budget's
exit code, and the same fact on the stream), `b782e75` (the working typed
turn). Three refactors rode along, splitting the files footnote 19 had named —
`89ccce4` (`basis-acp/src/server.rs`), `665ced6` (`basis/src/main.rs`), `e37f4f3`
(the ACP integration suite).

Rev 7's wave belongs to no phase either, and to an ADR decided after rev 6:
ADR-0016's `spawn`. It landed in two, `74ef59f` (the tool, the roster change,
the depth floor, and the end-to-end suite behind them) and the commit carrying
this revision (the ACP kind map, the auto-mode example, README and this
ledger).

| Piece | Decided in | Status today |
|---|---|---|
| Shell default-on | 0013 | **Built** (`35c9ccb`) — `ShellAccess::Granted` is the default, `--no-shell` disables |
| `--allow-shell` / `BASIS_ALLOW_SHELL` retirement | 0013 | **Built** (`35c9ccb`) — both refused with a migration message; the bare-host warning is gone |
| Docker image removal + containerization doc | 0013 | **Built** (`a246722`) — Dockerfile and `.dockerignore` deleted; `docs/containerization.md` written |
| `.git` carve-out kept as hygiene | 0013 | **Built** — no change |
| `watch` deletion | 0014 | **Built** (`4fbe1fd`) — `watch.rs`, `watch/`, `watch_cli.rs`, the subcommand, and its tests are gone |
| Bounds on `RunConfig` / `basis run` | 0014 | **Built** (`4fbe1fd`) — `with_deadline` / `with_tool_budget` / `with_token_budget` and the three `basis run` flags, all defaulting to unset |
| `Workspace::fingerprint()` + `basis fingerprint` | 0014 | **Built** (`4fbe1fd`, `8b52ebf`) — `fingerprint.rs` with ADR-0008's semantics intact, plus the subcommand; the method landed on `Workspace` itself once Phase C had one to put it on, reading the tree as it is now rather than as it was at open |
| Exit-code contract | 0015 | **Built** (`35c9ccb`, `8e35f3e`, `a2d170a`) — 0 ok / 1 failed / 2 usage / 3 bound tripped, and `3` now covers all three bounds rather than two, since a run records which one ended it. `RunReport::stopped_by` carries the distinction in-process and `run_finished` carries it on the stream [11] |
| `basis "<prompt>"` shorthand, `spawn -`, explicit ACP signpost | 0015/0017 | **Built** (`35c9ccb`, `f48c6de`) — a positional naming no subcommand is a prompt, `--` escapes, `spawn -` reads stdin, `run` remains an alias, and a non-JSON first line under `serve --acp` exits with the signpost |
| Crate split (`basis` / `basis-acp` / binary) | 0011 | **Built** (`fbcacb4`, `27ab4c8`) — three crates on one version; `agent-client-protocol`, `blocking` and — since the upstream gate exists — `tokio-tungstenite` are all out of `basis`'s graph, the bridge stays in the binary marked extractable [1] |
| MCP behind a feature | 0011/0012 | **Built** (`a4c259c`) — `mcp`, default-on; `default-features = false` compiles a `basis` with no MCP concept at all [2] |
| Approval enum → trait impls | 0010 | **Built** (`a4c259c`, `6192230`) — `ApprovalPolicy` is gone: `ApprovalGate` authorizes, `AllowAll` / `DenyAll` decide, the terminal approver is the binary's, and `basis_acp::ApprovalMode` holds the protocol's mode list. `--approve` is unchanged [3] [4] [5] |
| `Workspace` / run split | 0010 | **Built** (`8b52ebf`) — `Workspace::open` settles context, credential, model, skills, templates, hooks, MCP connections and the approval gate once; `prepare(RunSpec)` mints a run *synchronously*, which is the honest signal that nothing is left to await. `Workspace::fingerprint()` lands on the type its row above promised it to. The free functions stay, as wrappers over `RunConfig::split` [6] [7] [14] |
| `.output::<T>()` structured output | 0010 | **Built** (`07cf4d1`, over mentra `fce664a`; docs corrected in `dae4765`; the second mode in `b782e75` over mentra `be65c00`) — `PreparedRun::output::<T>()` and `output_with_options`, with `OutputSpec` / `OutputReport` basis's own and the schema the caller's to write. basis asks mentra for the raw `Value` and deserializes itself, which buys `RunError::OutputMismatch`. `OutputSpec::with_tools()` keeps the ordinary toolset on the turn, so read-then-shape is a choice rather than a ceremony [9] [12] |
| `BudgetPool` | 0010 | **Built** (`e21d632`) — the pool *is* mentra's shared `token_usage` counter, so `spent()` is the number the turns are stopped against rather than a tally reconciled later. `RunSpec::with_budget` / `TurnOptions::with_budget` attach one; an exhausted pool refuses the turn with `RunError::BudgetExhausted` before the prompt is sent [10] [11] |
| Tagged sinks / event fan-in | 0010 | **Built** (`07cf4d1`) — `EventFanIn` mints one `TaggedSink` per run and merges them into `MergedEvents`; the tag rides outside `Event`, so the versioned wire schema is untouched [13] |
| Cancellation on the public API | 0010 | **Built** (`07cf4d1`) — `TurnOptions::cancellable()` / `stoppable()` / `with_cancel` / `with_stop`, `execute_with_options` and its neighbours on every entry point, and `CancellationToken` re-exported under the rule the commit writes down: every mentra type basis's surface makes a caller *name*, basis re-exports [8] |
| Recipe + review-workflow examples | 0010/0014 | **Built** (`0ff745c`) — `examples/watch.rs` and `examples/review_workflow.rs`, public-API only, both run live [12] |
| Declared subprocess tools | 0012 | **Built** (rev 12, the commit that carries these lines) — held for seven revs and shipped against the use case that arrived. **The evidence**: iBot, a production Rust host embedding basis, needed Jenkins operations available to the model. With no tool-registration surface at all they became shell scripts reached through `spawn`'s command mode — and because that mode takes *one string*, the SQL queries and free-text questions those scripts act on ended up **base64-encoded inside the command line** to survive shell quoting. The model was writing a shell command, so every value it carried had to be escaped by a model that cannot be relied on to escape anything, and the workaround was an encoding the model had to perform correctly instead. `.basis/tools.json` declares a name, a description, an input JSON schema and an argv array; the model fills the schema, basis writes that object to the program's stdin, and stdout comes back as the result. There is no shell on the path, so there is nothing to quote and nothing to encode around quoting. The manifest is an object keyed by name — `.mcp.json`'s shape rather than `hooks.json`'s array — because here the name *is* the tool: two hooks may share a name and both still run, two tools may not, so saying it twice is not expressible. Layering (workspace shadows global), `${VAR}` expansion and the no-echo error rule are `.mcp.json`'s, and two files moved so nothing is written twice: the expander to `basis/src/expand.rs`, and hooks' process supervisor to `basis/src/subprocess.rs`, where both of ADR-0012's subprocess bindings now share one deadline, one kill, and one answer to what a descendant holding a pipe means. **Three decisions are security rather than ergonomics**, each closing a way this could have been unsafe rather than merely awkward. The side-effect field has *no read-only variant* — `process` (the default) or `external` — because `is_consequential` waves `None` past the approver, so a spellable "read-only" would be a file a repository ships routing a subprocess around the approval gate by writing one word. `authorization_preview` is overridden to present `{tool, command, cwd, input}`, because the name in the roster was chosen by the same file that chose the program, so the name is not evidence — and `env` is deliberately *not* in it, since a preview is globbed against remembered rules and kept in the audit trail while the environment is where the credential is. And a name the runtime already answers to **cannot be claimed at all**: `ToolRegistry::register_tool` is a `HashMap::insert`, so without the claim a manifest declaring `spawn` would replace basis's own tool and inherit every rule an operator ever wrote about commands. Collisions refuse rather than suffix, which is the mirror image of the MCP claim and for a stated reason — a bridged name is synthetic (`mcp__server__tool`) and costs nothing to rename, a declared name is what an operator writes rules and `hooks.json` matchers against, so a silent rename is a guard that silently stops matching. On a shared runtime the same claim keeps one repository's tools out of another's roster, asserted on the wire beside the `mcp__*` case. **What it costs**: every call reaches the approver, which is correct and is also one prompt per Jenkins query for a host that installs a real one; a released claim is *remembered* rather than removed, because mentra has no public unregister and forgetting would make the same workspace's next open refuse its own tool, so a long-lived runtime keeps one map entry per declared name for its life; and "schema-checked" is, on basis's side, an object-shape and `required` check rather than JSON Schema (new candidate, §2). **What it newly exposes**: a repository can now put a program within the model's reach through a committed file, which is `.basis/hooks.json`'s exposure arriving at a second door — bounded the same way, by whatever confines the process (ADR-0004), and not by a check in basis. Deliberately unchanged: still no `WorkspaceBuilder::with_tool`. basis constructs the `DeclaredTool` itself, so mentra's by-value `with_tool` costs this binding nothing, and the hazard the ADR-0016 row records — a hand-forwarded shim that could drop `authorization_preview` — is untouched |
| Hooks re-founded as authorizer binding | 0012 | **Built** (`e81e5d8`) — interception is one contract with two bindings: an in-process `Interceptor` trait and subprocess hooks, folded by one `Chain` so first-refusal-wins and composing modifications hold for both by construction. `Approver` stays a *sibling* seam rather than a parent — asking a person and rewriting a call are different questions, and mentra keeps them apart for the same reason [15] [16] |
| Subagents / teams surfaced | 0010 | **Built for delegation, still open for teams.** `task` is no longer in the default reach at all: ADR-0016 hid it with `shell` and `background_run` and made delegation `spawn`'s agent mode, which is the deliberate surfacing this row asked for. The accounting that made "reachable by default" tolerable in the meantime is mentra `0436bae`, and half of it survives the change of route — a delegated run still shares the parent's handle and bounds, but the child's usage no longer reaches the parent's *stream*, because that relay is internal to the `task` intrinsic (new candidate, below). `team_*` is **no longer reachable and still awaiting a concrete use case** — the row below hid the seven of them, and `idle` with them, because "reachable because nobody hid it" was never the deliberate surfacing this row asked for, and a second delegation door beside `spawn` is precisely what ADR-0016 removed `task` for. Surfacing them remains open and is now an addition rather than an acquiescence [10] |
| History location on `WorkspaceBuilder` | 0010 | **Built** (`397ca13`, `71cc59d`) — `with_store_dir(dir)` says where, `with_ephemeral_history()` says nowhere, and `store::list_in` reads back what the first wrote. One private field, so last call wins structurally. Closes the data-directory hole footnote 6 had been recording since Phase C [6] |
| `session/list` over ACP | 0007/0010 | **Built** (`e81e5d8`) — it had never worked: basis filtered on the workspace's runtime identifier while writing every agent under mentra's `"default"`. `WorkspaceBuilder::open` now tags what it persists. Forward-only, deliberately [17] |
| Credentials never printed | — | **Built** (`f3529be`) — `ProviderChoice` and `McpServer`'s stdio env hand-write `Debug`: names kept so a misconfiguration stays fixable, values redacted. Provider resolution reads the environment through a passed-in lookup, so the suite passes identically with `BASIS_API_KEY`/`BASIS_BASE_URL` set and unset [18] |
| One delegation surface (`spawn`) | 0016 | **Built** (`74ef59f`, with the ACP map, the auto-mode example and these lines in the commit that carries them) — the model's only door to delegation *and* commands. `spawn("!cmd")` is parsed once, at the boundary, into `{mode, body, cwd}` — ADR-0021 later added a fourth key, `target`, without disturbing the three — and that typed shape is what `authorization_preview` presents — so the approver, the rule store, the hooks and the audit trail all dispatch on it and none of them re-reads the string. Both modes are consequential, so neither is waved through under the reads-are-never-asked rule; command mode executes only after the answer. `shell`, `background_run` and `task` left the model's roster via `ToolProfile::hide` while staying registered on the runtime, so ADR-0013's posture is untouched — the route changed, not the availability, and `--no-shell` still refuses at the policy on the path `spawn` calls, verified end to end. The depth guard is basis's own, since mentra's floor is name-specific and does not fire for a registered tool: an agent-id ledger with RAII cleanup, refusing *in the preview* so a remembered allow-rule cannot lift a structural floor (`MAX_DEPTH` 2). The policy ladder is existing machinery tiered — a pattern rule answers first and never reaches the approver, the `Approver` sees only the residue, and a remembered refusal now carries its own reason (mentra `b895ea0`) — and `basis/examples/reviewed_shell.rs` walks all three rungs live. Two things about the pattern tier are traps rather than features. mentra globs with `glob-match`, where a single `*` does not cross `/`, and the serialized input carries `cwd`, which is a path: a rule written with one star silently matches nothing, and the operator sees a reviewer they thought they had bypassed rather than an error. And a remembered *answer* is stored bare (`pattern: None`), so `AllowForSession` / `DenyForSession` on `spawn` covers both modes and every body — where an operator could once allow `task` and deny `shell` by name alone, drawing that line now means writing a pattern against the parsed `mode`, which is more expressive and less obvious. **Deviation from the ADR's sketch, deliberately**: no `WorkspaceBuilder::with_tool` and no `ExecutableTool` re-export. mentra's `RuntimeBuilder::with_tool` takes its tool by value and, at the time, nothing upstream implemented the trait for `Box` or `Arc`, so a public registration point would have needed a hand-forwarded shim — where forgetting `authorization_preview` would present a host's tool to the approver as its static descriptor, the exact failure this ADR exists to remove. (mentra#22 has since closed that gap — see the third upstream wave in §2 — which removes the hazard, not the ruling on scope.) `SpawnTool` is public instead; adding the method later is additive. Declared subprocess tools stay held: adjacent binding of the same contract, not this use case |
| `Runtime` split (E1, basis half) | 0018 | **Built** (E1, the commit that carries these lines) — `Runtime` + `RuntimeBuilder` own mentra's runtime, provider/credential/base-URL and model *policy*, store policy, host interceptors, and the command environment (executor infrastructure, added to the ADR's list deliberately); `Workspace` borrows through an `Arc`, keeps discovery, and `Workspace::open(path)` is unchanged sugar over a private runtime (`RuntimeBuilder::build_for`). `workspace.runtime()` is renamed `mentra_runtime()`. mentra 0.18 fixes pre-hooks and MCP at build time, so basis registers one `HookDispatch` pre-hook keyed on the agent's `base_dir` (per-workspace hooks, `ShellAccess` and the `.git` carve-out on shared runtimes) and owns MCP connections per workspace via `McpManager` + `register_tool`, with server-name claims and per-mint `hidden_tools` keeping one workspace's `mcp__*` tools out of another's roster — asserted on the wire. Known gap, recorded on `Runtime::mint` and `crate::store`: shared runtimes tag persisted rows `"basis:runtime"` until mentra grows a per-session persist identifier, so per-workspace `session/list` on a *shared* runtime waits on that upstream ask (the private path is unaffected); the listing acceptance test ships `#[ignore]`d. `basis-acp`'s one-runtime-per-process rewire is the row below; the binary's `task.rs` rewire landed in the same wave — a per-task `RuntimeBuilder` carrying the store directory and task environment, with the pre-split provider-before-effort error precedence restored and pinned after the rewire briefly inverted it |
| `Runtime` split (E1, basis-acp half) | 0018 | **Built** (E1, same commit) — the default `SessionSource` stops building a runtime per session (`run::prepare_without_prompt`, which opened a workspace, minted from it and dropped it before the first turn). `server/workspaces.rs` holds one `Runtime` for the process and one `Workspace` per key, both built on the first `session/new` that needs them — lazily, so a missing credential still reaches the client as `auth_required` instead of killing the server at startup, and neither cell caches a failure. The public surface does not move: `ServeConfig::new` / `with_source` / `with_initial_mode` are unchanged, so the binary and the bridge are untouched, and a host with its own runtime still comes in through `SessionSource`. The key is the canonicalized `cwd` **and** a digest of the `mcpServers` the client sent, because those arrive per session: keying on the directory alone would hand the second session the first one's roster and silently drop what it asked for, which reads exactly like a server with nothing to offer (values are digested, never held). Nothing evicts — a cached workspace's MCP connections and hook registration have to outlive every session minted from it, and `session/close` reaches basis after the fact. Two consequences of sharing, both basis's design rather than a choice made here: `--no-shell` and the `.git` carve-out are enforced by the hook dispatcher instead of by policy, so those denials now arrive in basis's words rather than mentra's; and `session/list` answers only for what a *private* runtime wrote, so conversations opened over ACP are invisible to it until the per-session persist identifier `Runtime::mint` is waiting on lands upstream — the ledger row above records the same gap, this is where it becomes user-visible. Pre-existing fix: `session/list` was registered unconditionally while `initialize` advertised it conditionally, so a source that cannot enumerate answered `[]` — the empty-list-for-a-workspace-that-has-some failure the capability exists to avoid; it now answers `-32601` itself |
| Structured agent concurrency | 0017 | **Built** (`9d2179b`, `89ee68a`) — `spawn` returns a durable per-workspace task handle; the hidden local service persists bounded state, accepts `send`/`ask`, and provides repeatable `wait` (including `--message`), `cancel`, `watch`, and bounded `inbox` reply summaries; attached deadlines/cancellation flow downward, a parent publishes terminal state only after attached children settle, detached roots are independent, static ownership rules reject self/ancestor/same-tree-peer waits, the live wait graph rejects cycles across ownership trees, and terminal state is separate from advisory progress. `send --await` waits for the correlated reply for its own message rather than unrelated task termination. **Substrate superseded by the row below** (E2/ADR-0019): every semantic here survives on files except the live wait graph, which is deleted — cycles across ownership trees are no longer rejected, they end at each observer's own finite deadline. **CLI routing narrowed by [ADR-0020](adr/0020-spawn-routing-is-decided-by-the-environment.md)**: handle-first is what a caller *inside a task* gets, because that is whose turn must not block; at a shell `spawn` drives the agent it minted and prints the answer, with `--resumable` as the opt-out |
| Files as the coordination surface (E2) | 0019 | **Built** (E2, the commit that carries these lines; `cargo fmt --all`, `cargo clippy -p basis --all-targets -- -D warnings` and `cargo test --workspace` — 775 passed, 0 failed — all green) — the per-workspace daemon is retired and an agent is a directory under one global, workspace-keyed data root (`BASIS_DATA_DIR`, else an absolute `XDG_DATA_HOME`, else the platform data home; `0700`), holding `meta.json`, `inbox.json`, `events.jsonl` and — as the executor's last act — `terminal.json`, whose existence *is* the completion signal, so an agent is resumable iff it has none and every crash before that write resolves toward resumable. Attach is the primitive: take the agent's `fs2` lock, resume from mentra's last committed turn, checkpoint at turn boundaries; `spawn --await`, `wait`, `ask` and `send --await` all attach, and a contended lock means someone else is executing, so the caller observes. Deleted whole: `registry.rs` (693), `protocol.rs` (233), `service.rs` (585), `service/task.rs` (617), `service/lifecycle/*` (1,505 across eight files), `client.rs` (659), `store.rs` (532) — with the old 12-line `mod.rs` that is all 4,836 lines of `basis/src/local`, replaced by 3,337 across eleven modules (2,453 implementation, 884 tests), every file under the ceiling. Outside it, `cli.rs` loses `Daemon`/`DaemonArgs` and `main.rs` its `__daemon` arm, while `shorthand.rs` keeps `__daemon` reserved so a pre-E2 script gets "unrecognized subcommand" rather than a task whose prompt is `__daemon`. What it costs, stated where the gain is: no progress without an attached process, cancellation granular to the turn boundary rather than instant, and no rollback of a re-driven turn's tool side effects — a checkpoint restores state, never effects. What it does not cost: mentra's store moves from beside the daemon's registry (under `XDG_RUNTIME_DIR`, which the platform may erase) to `<root>/workspaces/<key>/store`, so conversations are durable for the first time, at the price of not migrating the ones that were not. Three deviations from the plan, each forced by removing the daemon's mutex: the terminal record is written under the inbox lock, so an enqueue racing a settle is either accepted before the unanswered sweep or refused by the record it would have missed; the settle pass drives free-locked unfinished children on the success path too, without which a child spawned and never waited on would hold its parent open until the deadline; and `meta.json` carries no `next_seq`, since deriving it from the journal's last line cannot drift from the file after a crash |
| The roster is written down | 0013/0016 | **Built** (the commit that carries these lines; `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --workspace` all green) — **no new ADR: this finishes ADR-0016 and applies ADR-0013 unchanged.** basis had never stated which tools the model is offered; it hid three names and took whatever mentra registered for the rest, which is how a `basis "<prompt>"` came to show ~20 tools nobody had chosen. Two changes, one argument. **The file tools are mentra's split profile** — `read`, `ls`, `grep`, `glob`, `write`, `edit`, via a new `RuntimeBuilder::with_file_tools` defaulting to `Split` — in place of the batched `files`, whose input was an `operations` array over a nine-variant `oneOf` for every read. The names are the ones models in this class are trained on; two of the differences are capability rather than shape, since the batched `search` op pins `glob`/`ignore_case`/`literal`/`context`/`multiline` to their defaults and there is no batched `glob` at all — so a model wanting one reached for a shell command, a call that goes to the approver in place of a read that would not have. **And what basis has never surfaced leaves the roster**: `team_*` (a second delegation door beside `spawn`, which is exactly what ADR-0016 removed `task` for, leading somewhere basis neither mints nor reads), `idle` (that surface's exit — its whole effect is `should_end_turn`, a yield back to a teammate loop basis never starts, so a model calling it ends its own turn mid-task), `task_*` (a board nothing in basis reads: every call answers with plausible success and nothing observable happens), and `check_background` (which reports on `background_run`, hidden since ADR-0016). `compact`, `load_skill` and the three `memory_*` intrinsics stay; `memory_*` is **out of scope** here rather than settled. The whole visible set — `compact`, `edit`, `glob`, `grep`, `ls`, `memory_forget`, `memory_pin`, `memory_search`, `read`, `spawn`, `write`, plus `load_skill` once skills load — is read off the runtime's own registry in `the_default_roster_is_exactly_this`, so a tool mentra adds upstream arrives as a failing test rather than as a silent new door. **What it costs**: `.basis/hooks.json` matchers and remembered approval rules key on the exact tool name, so an entry naming `files` stops matching and nothing errors — ADR-0016's `shell` → `spawn` silence, a second time, with the same migration note in ARCHITECTURE.md §8 and `with_file_tools(Batched)` as the one-line way to keep the old roster. A hook that walked the `operations` array needs rewriting rather than renaming. **What it newly exposes**: nothing. Hidden was never a capability fact and is not one here — every hidden tool stays registered, ADR-0013's posture is untouched, and `--no-shell` still refuses at the policy on the path `spawn` uses. **Two hazards the work turned up**, each now pinned: the shared-runtime `.git/hooks` guard keyed on `context.tool_name == "files"` and would have let `write`/`edit` walk past it (the private path's `with_denied_write_root` never could, because it binds at mentra's `WorkspaceEditor` where both profiles' writers meet); and `basis-acp`'s `tool_kind` classified `read` by mutability, which `ToolQueued` always reports as `Unknown`, so every pending file read would have rendered to an ACP client as an edit |
| Compaction configured by basis | — | **Built** (the commit that carries these lines) — `Compaction` on `WorkspaceBuilder`: three knobs over mentra's nine-field `CompactionConfig`, and the first default basis sets *against* upstream's rather than beside it. **The evidence**: micro-compaction is not budget-driven. mentra rewrites the history before *every* provider request (`micro_compact_history`), blanking the content of each tool result over 100 bytes past the three most recent, with no event and no relation to the context window — so an agent that reads five files and edits the first is editing from a transcript where that file reads `[Previous: used files]`. Before this, `basis/src` referenced compaction in one comment and ARCHITECTURE §1 promised "mentra + glue" with no glue. `keep_recent_tool_results` therefore defaults to `usize::MAX` — mentra's own off switch, so this is a configuration of upstream and not a fork of it — and elision stays available by number for a host that knows its results are large and cheaply re-derived. The two summarizing numbers are *read off* `CompactionConfig::default()` rather than restated, so a number basis has no basis to choose cannot drift from upstream's, and the other six fields are untouched. `transcript_dir` is deliberately not a knob: it follows the store (`<store_dir>/transcripts`, mentra's own layout, so pointing `with_store_dir` at the default directory stays a no-op), which closes for snapshots the same process-cwd hole `397ca13` closed for the database. **What it costs**: tokens, and it is the trade the row exists to state — every turn now carries every tool result, so requests are larger and a long file-reading session reaches the summarizing threshold sooner than it did. Taken deliberately, because those tokens are visible and priced while a blanked result is neither. And `with_ephemeral_history`'s *nowhere* becomes honestly "the OS temp directory, per runtime": mentra persists a snapshot before it summarizes without consulting the store, and `max_persisted_transcripts: None` disables cleanup rather than writing, so basis moved the file instead of documenting a promise it cannot keep — the two doc comments that claimed otherwise are corrected. **What it newly exposes**: nothing. Every knob narrows what mentra was already doing, and nothing here reaches a new file, program or network. **What it does not do**: know the model's context window. The trigger is a fixed 50,000 tokens whatever the model, which is right for a small one and wasteful on a large one — two new candidates, below |
| Shared roots, `CLAUDE.md`, and a system-prompt seam | — | **Built** (the commit that carries these lines) — three items against §1's identity check, two on *a convention other agents already speak* and one on *a seam*, none of them an opinion in the core. **Skills** now come from four roots, most specific first: `<workspace>/.basis/skills`, `<workspace>/.agents/skills`, `<global config dir>/skills`, `$HOME/.agents/skills`. The `.agents` pair is what pi and Claude Code read, and the whole point of the SKILL.md format is that a skill written once is found by every harness that speaks it — basis read neither, so a repository that had already written one got nothing from basis. Purely discovery: `register_skills_dirs` has been additive with PATH-style shadowing since mentra 0.18.3 and the builder already registered the whole list in order, so the stale "one directory, for now" module doc went with the change. Within a scope the basis-specific root comes first, because naming basis is the more specific statement. `~/.agents/skills` is gated on `SkillsConfig::global_dir` being set — that field is already how a caller says whether this user's directories are read at all — but not *located* by it, since a fixed path is what makes a shared convention shared. **Context files** gain `CLAUDE.md` as a per-directory fallback: `AGENTS.md` first, `CLAUDE.md` only where a directory has none, one document per directory. A repository carrying only the older name used to hand basis an *empty system prompt* — with no `AGENTS.md` and no skills, `AgentConfig.system` was `None` — and the repositories that wrote a `CLAUDE.md` wrote it to instruct an agent. *Present* rather than *non-empty* decides, so which file is in effect never depends on its contents; an unreadable `AGENTS.md` stays an error rather than becoming a fallback. **The system prompt** gains `WorkspaceBuilder::with_system_prompt(SystemPrompt::{Replace, Append})`. Bet 4 argues for shipping no *default* prompt, which basis still does — the text in both variants is the host's — but it never argued for denying a host a seam, and without one an embedding host could not give its product a voice, or say *for my runs, answer in Chinese*, without writing into the user's repository's `AGENTS.md`. `Append` goes last, where the rendered block's own preamble says the most specific statement goes: a repository cannot know which product is running it, and a knob a repository could override by writing a file is not a knob — the weakest slot was already taken by the global `AGENTS.md`. One enum rather than two methods, so *both at once* is unspellable rather than undefined, and workspace-scoped rather than runtime-scoped, so a host on a shared `Runtime` (ADR-0018) gives each repository its own voice. **What it costs**: a repository that carries both `AGENTS.md` and `CLAUDE.md` contributes only the first, which is a change for anyone who kept different instructions in each; `~/.agents/skills` becomes a root a personal skill can now reach a basis run from, which is the point and is also one more place to look when a name resolves unexpectedly; and `Replace` drops discovery from the prompt while `run_started` still *names* the context files, because which files a workspace has has one true answer and the host that replaced the prompt is the party that already knows. **What nothing costs**: no public field changed type or arity, so `ContextConfig` and `SkillsConfig` are constructed exactly as before, and an unset `with_system_prompt` leaves the discovery-only prompt byte-identical. **Deliberately not done**: `ContextConfig` keeps `file_name: String` as the *first* name rather than becoming `file_names: Vec<String>`, and `SkillsConfig` grows no root list — both structs are constructed as exhaustive literals in `basis-acp` and `basis-cli`, so either change is a coordinated three-crate edit for configurability nobody has asked for; the fallback belongs to the default name, so a host that renames it reads only what it named. No ADR: each is a convention or a seam the ledger's own identity check already admits |
| A tripped bound is a stop reason | 0014/0002 | **Built** (the commit that carries these lines) — a run a deadline, tool budget or token budget ended reached the client as `-32603`, which is the one reading that is certainly wrong: the CLI has an exit code for exactly this case (`3`, ADR-0014's "committed work was kept") and the protocol was calling it a broken agent. `TokenBudget` → `MaxTokens`, `ToolBudget` → `MaxTurnRequests`. **What it costs**: `Deadline` also takes `MaxTurnRequests`, because ACP v1 has no time bound and every other candidate is worse — `Refusal` carries a documented consequence (the prompt is dropped from the conversation) that is false here, `Cancelled` is reserved for `session/cancel` and would report a stop button nobody pressed, and `EndTurn` hides the bound. So the unit is lost and the event is right; the exact bound is still on the stream as `RunFinished.stopped_by` for a client that wants it. **What it exposes**: the bound is read *before* the outcome, because `TokenBudget` is graceful and can arrive on a turn that answered — reporting that as `EndTurn` would drop the one fact the client needs, which is that there would have been more |
| Compaction is visible to the client | 0002 | **Built** (same wave) — `CompactionCompleted` becomes one `AgentThoughtChunk` naming both counts ("N earlier items replaced by a summary, M kept"), where before both compaction events were dropped for want of an ACP update kind. A client that hears nothing cannot explain why the agent stopped remembering what it was told twenty turns ago, which is the most confusing thing a long session does. **What it costs**: a deliberate widening of what "thought" means — it is the one update kind ACP gives an agent for saying something about *itself*, and basis already spends it on retries and recoverable errors. **What it exposes**: `CompactionStarted` stays silent, and the asymmetry is the argument. It carries an agent id the notification already names, so the only line it could produce is "compacting…", arriving moments before the one that says what happened |
| `session/set_config_option` for model and effort | 0002 | **Built** (same wave) — six of pi's RPC commands are "change model" / "change thinking level"; ACP 2.0 standardised that as one method with the options advertised on `session/new`, `session/load` and `session/resume` and every change echoed as a `ConfigOptionUpdate`. basis served none of it, so a client had one model and one effort for the life of a connection. `PreparedRun::set_model` / `set_effort` are the SDK half — thin forwards to mentra's `Session::set_model` / `set_reasoning`, which persist the agent record and are read live when the next request is built — and `basis-acp/src/options.rs` is the enumerable half, modelled on `mode.rs` because ACP wants an id, a label and a description per value and that is a protocol binding. No cargo feature: config options are stable v1 in `agent-client-protocol` 2.0.0, so the `=2.0.0` pin is untouched. **What it costs**: the model list has one entry, the model the session is on. mentra's `Runtime::list_models` asks the provider every call and caches nothing, and `session/new` answers on the dispatch loop, so a real catalogue would put a network round trip on every new session. The list is therefore advice rather than an allowlist — any id a client sends is accepted, since mentra does not check either and a self-hosted endpoint's model names are exactly the ones basis has never heard of. An effort basis never offered *is* refused, because unlike a model it names nothing a provider could know. **What it exposes**: the handler spawns rather than answering inline, unlike `session/set_mode` next door. Both settings live on the run, behind the turn lock a running turn holds while it waits for the client to answer a permission request — ADR-0007's deadlock by its second route. Waiting costs nothing observable, because both take effect from the next turn either way. One gap, recorded on `PreparedRun::effort`: mentra offers no way to read a reasoning level back off a session, so a session opened by an operator who asked for one reports "Default" until something sets it |
| Images in prompts | 0002 | **Built** (same wave) — `initialize` advertised `promptCapabilities.image: false` and the prompt handler dropped every non-text block with a `_ => None`. Nothing below basis was the obstacle: mentra carries inline image bytes on all three wires it serves — Responses as a `data:` URL, Anthropic as a base64 source, Gemini as `inlineData`, each with a serializer test of its own — and its session path passes a user turn's blocks through untouched. basis was the layer narrowing a prompt to a `String`. `PromptPart` widens it by exactly one thing, additively: `send_parts` sits beside `send`, which is now this with one text part. **What it costs**: one workspace dependency, `base64`, already in the tree via mentra-provider — ACP carries the payload encoded and mentra takes the bytes, and `basis-acp` is where the two meet. **What it exposes**: only the bytes variant is offered, never mentra's `ImageSource::Url`, because Gemini rejects a URL image outright rather than fetching it — a prompt that worked on two providers and failed on the third would be a portability trap, and who fetches a URL is not a decision basis should make silently. Audio stays unclaimed: mentra has no block for it, so claiming it would be offering a client something to drop. Two upstream notes, neither blocking: mentra's `extract_user_text` projects a turn to text for its `UserMessage` event, so an image-only turn emits an empty one, and its memory ingestion maps an image to nothing |
| The repository's model choice, and the prompt seam at a shell | — | **Built** (the commit that carries these lines) — **no ADR: a convention other agents already speak, plus a seam.** **What it is.** `.basis/config.json` layered over a `config.json` in the global config directory: `provider`, `model`, `effort`, schema-versioned like `.basis/tools.json`, `${VAR}`-expanded like `.mcp.json`, most specific wins *per key* so a repository that pins a model has not unsaid its owner's preferred effort. Every other knob basis had was a flag or a variable, and both describe an invocation; nothing could say *this repository uses X*, so no `--model` meant a live `/v1/models` fetch sorted by `created_at` — the same prompt in the same repository running a different model tomorrow, silently, for a reason nobody in the repository chose. pi's `settings.json` is the same convention; this is deliberately **not** a `models.json`, and carries no prices, no context windows, no capabilities — a model's properties are the provider's to state and would be stale in a file the day after they changed. **Where each answer lands**, since the file answers questions at three scopes: `RuntimeBuilder::with_config` fills provider, endpoint and model *policy* only where the host said nothing (order-independent, because what it reads is emptiness rather than who spoke last); `WorkspaceBuilder::open` reads the file itself and applies `model` as the override ADR-0018 already grants; `effort` waits on the `Workspace` until a `RunSpec` that asked for none is minted. Precedence, strongest first: **explicit builder call or CLI flag → workspace file → global file → environment → default** — a flag describes this invocation, a variable describes whoever started the shell, and the file describes the repository the work is in. The CLI and `basis-acp` needed no code: both reach a workspace through `WorkspaceBuilder`, and an ACP server holding one runtime for the process now gives each `cwd` the model that `cwd` chose. **What it exposes, and the one refusal.** `base_url` is honored **only from the user's own global file**; a workspace file that sets it fails the open, naming the file and the key. `.mcp.json` and `.basis/hooks.json` also come from a repository and also name something to run, and that is the point of the asymmetry: a program is bounded by whatever confines the process (ADR-0013's line, the OS's job), while a redirected `base_url` carries the credential basis just read out of the environment to a host of the file's choosing, and a leaked secret is bounded by nothing. `provider` from a workspace file stays safe by the same reasoning — it selects a *preset* URL and an environment variable, both the user's, and the worst a hostile one does is name a service the user has no key for. There is no `api_key` key at all, which `deny_unknown_fields` makes enforceable rather than documented, and no error repeats a value it read. **What it costs**: one more file on the discovery path, read once per workspace open; and a malformed or unknown-key file now *fails the open* rather than being skipped, which is `.mcp.json`'s rule — a repository that believes it pinned a model and misspelled the key should not quietly run another one. **One behavior change**: `RunConfig::split` stops restating `NewestAvailable` as a workspace override, because that value is how a caller who named no model is spelled (the field is not an `Option`) and passing it on would have made every `RunConfig` outrank a file that had something to say; asking for the newest explicitly and saying nothing are the same request, so nothing is lost. **Beside it, the prompt seam reaches a shell**: `RunConfig::with_system_prompt` carries the row above's `WorkspaceBuilder::with_system_prompt` through `split`, and `--system-prompt` / `--append-system-prompt` land on `spawn` and `serve`, mutually exclusive at clap because the enum below makes both-at-once unspellable. Recorded in the task's durable options, because ADR-0019 puts a gap between the process that spawns and the process that attaches — a flag that reached only the first would be a prompt that changed the moment a run resumed; both keys are `#[serde(default)]` so an older `meta.json` still loads. `basis-acp` needed nothing: `ServeConfig`'s template is a `RunConfig`, and every session is a clone of it split. **Deliberately not done**: no `config_files` field on `Event::RunStarted`. The header already names the `model` and `provider` this file decides, which is the whole of its effect — unlike `.mcp.json`, whose programs have no other line to appear on — and `.basis/tools.json`'s sources are already reported on `Workspace` and not on the stream, which is the precedent followed here. `Workspace::config()` and `config_files()` answer *which file said so* for a host that reports its own configuration; adding the event field later is one additive line |
| A way back into a conversation (`list`, `--continue`, `--session`) | 0015/0017/0019 | **Built** (the commit that carries these lines) — a shell user who closed the terminal had lost every handle basis printed, while the tasks themselves sat durable on disk. **What it is**: `basis list` reads `<data>/workspaces/<key>/agents/*/meta.json` back — handle, state, age, the prompt's first line, and what the task spent — newest first, 50 rows unless `--all`, one JSON object per line under `--json`. It takes no lock and writes nothing; a listing that minted a directory would be a listing that changed its own answer, so it reads the workspace marker rather than calling `ensure_workspace`. State is derived through the same `probe_state` `wait` and `watch` use — terminal record first, then the attach lock — so the three verbs cannot disagree. **Why continuing is a new task and not `send`**: a task holding a terminal record accepts no messages at all. `inbox::enqueue` refuses the moment `terminal.json` exists (ADR-0019: "a worker past its own turn accepts no new messages"), and that is deliberate — terminal means immutable and repeatably observable. So `--continue` (the newest task in this workspace that has a conversation) and `--session <TASK>` (the one that handle names) mint a *new* task recording the agent id it continues, and its first attach calls `Workspace::resume` on that id instead of `prepare`: new handle, one conversation, all four of ADR-0017's rules untouched. Bounds, model, effort and approval mode come from the new invocation, because a bound belongs to a run and the old task's are spent. **Two refusals, for two different reasons.** A task something is *driving* is refused with exit 1 and the state that caused it — one executor per conversation is the whole point of the attach lock, and a second resume of the same agent would interleave two dialogues into one transcript; the same command works once it settles. A handle from another workspace is refused with exit 2, because the key is half the handle and no amount of waiting makes it right. The attended `--json` route refuses both spellings for a third reason: it mints no checkpoint and never opens the workspace-keyed store, so it would silently run the prompt as if it opened the dialogue — ADR-0020's routing table is untouched rather than grown a cell. **What it costs**: a fourth persisted field, `answered_before`, and the reason is a correctness one rather than bookkeeping — the resume recovery reads "any committed assistant turn" as "the prompt was already answered", which on a continued conversation is true from the first turn, so without a baseline a continued task that crashed before committing would settle `succeeded` on the *previous* task's answer without ever asking its own. `list` also costs one `read_dir` plus one `meta.json` read per task, bounded by `MAX_TASKS` (1024) and by the 50-row default. **What it newly exposes**: nothing outside the data directory basis already owns. `list` shows the first line of prompts that were always in `meta.json`, on a `0700` tree, to the user who wrote them |
| Templates typeable at a shell (`/name`) | 0015 | **Built** (the commit that carries these lines) — `.basis/templates/**/*.md` has been discovered since P2 and surfaced in exactly one place: over ACP, as the `AvailableCommand`s a client offers. `grep -rn template basis-cli/src` found one unrelated comment, so the person who wrote `.basis/templates/git/commit.md` could not type it at a shell — a convention every peer CLI spells `/command`, reaching the editor and stopping there. **What it is**: a prompt whose first token is `/<name>` is a template invocation, rendered with the rest of the line as `$ARGUMENTS`/`$1`… and handed to `spawn` as the prompt. Discovery is `basis::templates::load`, the same function the workspace builder hands ACP its list from, so a name a client offers and a name a shell accepts are one set, layering included. It is resolved once in `main`, before ADR-0020's route is chosen, because every route takes a prompt — and the *rendered* text is what the task records, so `basis watch` and `basis list` show the question that was really asked. `send` and `ask` bodies resolve the same way, against the workspace their task recorded rather than whichever one the shell is standing in. **What it costs**: a name-shaped first token that names nothing is refused with exit 2 and the names that exist, rather than sent as prose — a typo'd `/comit` handed to the model is a run that answers the wrong question and bills for it, and the escape is one character (`basis spawn -` reads a literal prompt from stdin, which is why `-` is never expanded). A workspace with a template file that does not parse now fails the *spawn* as well as the ACP session, which is basis's existing rule about templates applied one door further along. **What it newly exposes**: nothing basis was not already reading. The files are the ones ACP has been loading since P2, from the same two roots in the same precedence order. A first token with a second slash is a path, not a command — template names never contain `/`, since nesting is namespacing and joins with `:` — so `basis "/usr/bin/x crashes on startup"` passes through untouched, which is what lets the rule apply to every prompt without an escape for the ordinary case |
| Usage where consumers can see it | 0014 | **Built** (the commit that carries these lines) — `RunReport::usage` was correct in-process and invisible everywhere else: the JSONL stream carried per-round `Event::Usage` and no total, the terminal record carried none, and the CLI printed none. basis rightly ships no price table — prices are the host's and they move — but that argument only holds if the counts arrive. **What it is**: `usage` on `Event::RunFinished`, sourced from the figure the run already summed; the same four numbers on the terminal record, so `basis wait --json` and `basis list --json` report what a settled task spent; and one compact `basis: 12.3k in · 1.2k out` at the end of a streamed run, on stderr with the rest of the progress, never on stdout — `basis "…" > answer.md` leaves a file holding the answer, not a receipt. The CLI banks the tally per turn in `meta.json` rather than recomputing it from the journal, because a task settles once but its turns may be driven by several processes and the journal is capped at 32 MiB. **What it costs**: one atomic `meta.json` write per model turn, which is nothing beside the round-trip that earned it; and every `run_finished` line grows a field. Not a schema bump — `EVENT_SCHEMA_VERSION` stays 1, on the rule `stopped_by` already set: the field is optional and skipped when absent, a schema-1 reader that ignores the key reads the line as before, and `Deserialize` defaults it so an older line still parses. **Absent is not zero, deliberately.** A provider that reports nothing leaves the counters at zero, so a `usage` object full of zeros would claim a measurement nobody made; the record and the stderr line both say nothing instead, and the field's absence means "not stated" rather than "free". **What it newly exposes**: token counts for a run, to whoever could already read that run's terminal or its `--json`. No prices, no per-agent attribution — mentra's usage report carries no agent id, so a delegated round lands in the same total as its parent's, which the `RunUsage` doc already said and this makes visible |
| `chat/completions` is the wire a base URL speaks | 0005/0018 | **Built** (`b3cfa3e`, `362df0b`, `c62e7ff`, `e35c8f4`, over mentra `62ac1c4`) — **the first-turn 404 is gone.** A `base_url` was spoken to in OpenAI's Responses wire, which Ollama, LM Studio, vLLM, llama.cpp and the gateways in front of them have never served; that is the wire "OpenAI-compatible" means everywhere except OpenAI, so the flag basis documents as its route to a local model failed on the first request, worded like a mistyped URL. Now a base URL gets `chat/completions` through mentra's own `with_openai_compatible` — filed under the id the choice resolved, so `--provider gemini --base-url …` finds its model instead of failing at the first turn under a name nobody registered (`362df0b`, a pre-existing bug the change forced into view) — and OpenAI's Responses wire is what the `openai` preset reaches with no base URL at all. A proxy that forwards Responses says so: `RuntimeBuilder::with_wire(Wire::Responses)`, basis's own two-variant enum rather than a re-export of `WireApi`, whose other two variants a base URL cannot be spoken to in. **What it costs**: every scripted endpoint in the suite spoke Responses and seven harnesses were converted; `tools[].name` became `tools[].function.name` in two assertions. **What it newly exposes**: nothing — one wire replaced another on the same connection. **Refused**: a `wire` key in `config.json` — a wire is not a fact a repository has about itself, and the operator who needs the other one is embedding basis rather than typing at it |
| A keyless endpoint is reached without inventing a key | — | **Built** (`05a8ce9`) — `--provider ollama` was refused for lacking a key it never needed, and a base URL with no key anywhere was refused outright; with the wire fixed that turned away exactly the servers it most often names. `ProviderChoice::api_key` is `Option<String>`; a local preset and a keyless base URL resolve with `None`, the provider is built with `AuthScheme::None` so the request carries no `Authorization` header at all — not an empty bearer a server would refuse — and a server that wanted one answers 401 in its own words. `NotKeyed` and `NoCompatibleCredential` went with the refusals they named. **What it costs**: a mistyped key variable now surfaces as the server's 401 rather than basis's "no key" — a worse message for the keyed case, taken because the keyless case is the common one and a heuristic ("loopback needs no key") would have been a guess. **What it exposes**: a request with no credential to a URL the operator named; nothing is sent anywhere the operator did not point it |
| `post_tool_use` in `.basis/hooks.json` | 0010 | **Built** (`5d685ba`, `cc32938`, `4eeac7b`, over mentra `145d4ef`) — the second half of the interception seam, which the contract had a word for and mentra had no hook for. One envelope for both events: `event: "post_tool_use"` adds `output` (the result as the runtime typed it — structured as itself, text as a JSON string) and `is_error` to the request a pre hook already gets, with `input` being what the tool *ran* with, after any `modify`. One `decision` vocabulary: `allow` is keep, `deny` shows the model the reason in place of the output with `is_error: true` (after the call nothing can be stopped — the stream already carried the real result), `replace` carries `output` and an optional `is_error` where omitted means unchanged, so redacting a failing result does not declare it a success. `HOOK_SCHEMA_VERSION` stays 1, because a hook written against the old shape is still right: never asked at an event it did not declare, byte-identical requests, unchanged answers — and `UnsupportedSchema` compares for equality, so a bump would refuse every `hooks.json` in existence. In-process, `Interceptor::review` is defaulted to allow, with `Box`/`Arc` forwarding explicitly so an indirection cannot silently swallow a guard's objection. **What it costs**: one map lookup per tool result on a runtime with no participants, since whether a workspace will declare a post hook is unknowable at build. **What it newly exposes**: a hook now reads every tool's *output*, which is the point and is also a second channel for a credential to leave the process — the same `on_failure: deny` default applies, and a broken guard replaces the result with its failure rather than letting it through |
| Compaction knows the window | — | **Built** (`8108e24`, `b9478b7`, `bd10912`, `e35c8f4`, over mentra `4883ba9`, `2c77792`, `11826ee`, `bfe952b`) — the row above's "what it does not do" done: `Compaction::with_auto_threshold_percent` beside the token trigger (75% of the window by default, read off mentra's own default), `PreparedRun::context_window()` read off the live session and `estimated_context_tokens()` with mentra's own estimator, and `basis-acp` sending a `UsageUpdate` after each turn when the window is known and nothing when it is not. The stale argument in `compaction.rs` — that nothing in basis or mentra knows the window — is retired with the default it argued against. **What it costs**: `estimated_context_tokens` is a floor — mentra adds a task-reminder banner and a skill block to the *effective* prompt, inside a private method — and a resumed conversation must have its model reapplied to get a window back. **What it exposes**: nothing new on the wire; a number the client was already entitled to |
| Session verbs: `compact`, `set_name`, `effort` | 0010 | **Built** (`1384a3a`, `e17d112`, `63ef04a`, `5522b69`, over mentra `ee01c30`, `bfe952b`) — `PreparedRun::compact(instructions, sink)` runs mentra's summarizing pass on demand and delivers mentra's own `CompactionStarted`/`CompactionCompleted` to the sink exactly once (the run subscribes before the pass and drains after, because the per-turn forwarder is not installed for a pass that is not a turn); `set_name`/`name()`; and `effort()` now reads `Session::reasoning()` instead of a field that said `None` for a session running at `high` because the level was applied at mint — which is what `basis-acp`'s picker had been drawing. `basis-acp` answers a built-in `/compact` beside the workspace templates; the built-in wins a name clash and a `compact.md` template is not advertised, because this direction's loss is a rename and the other's is a person's only compaction control silently replaced by someone else's prompt. **What it costs**: `/compact` arms no cancellation token — a summarizing pass has no round boundary for one to be read at. **Refused**: expanding templates over ACP (a visible inconsistency now, named in the tally) |
| Conversations carry timestamps, list by recency, and can be deleted | 0018 | **Built** (`b06e9bc`, `9cc10ec`, `51b5d10`, over mentra `ee01c30`) — `PersistedSession::{created_at, updated_at}`, `list_in` ordered newest first with `None` (a volatile store) deterministic, `SessionInfo::updated_at` on ACP's `session/list`; `session/delete` answered on `store::forget`/`forget_in` — not on `Workspace`, because `session/delete` carries no `cwd` and opening a workspace to delete a row would resolve a model over the network on a connection that had only listed. And the per-session persist identifier: `Runtime::mint` applies the workspace tag it had been accepting and ignoring since E1, so a shared runtime's `session/list` answers per workspace, and the test this ledger shipped `#[ignore]`d runs. **Refused**: ordering `basis list` by `updated_at` — it lists CLI tasks off the filesystem by ADR-0019's design and never touches the store; the real bug there is `--continue`'s, named in the tally |
| Schema checked upstream, names claimed without replacing | 0012/0018 | **Built** (`6b37ddb`, `a45b01d`, `5ebd1be`, over mentra `dd2e38a`) — mentra#23 and mentra#24, closed; the tally records what each was and what basis did with it. A second concurrent open of the same repository now *joins* the first's declared-tool registration rather than swapping the program under a running agent, and a dropped workspace takes its declared and bridged tools off the runtime instead of leaving tombstones — the first time a released claim is dropped rather than remembered |
| Delegated spend is tallied and the delegation recorded | 0016 | **Built** (`e22aa63`, `54f3d21`, over mentra `5f303b8`, `bfe952b`) — the three ADR-0016 candidates, closed; the tally records them. `RunUsage` now agrees with the bound (`delegated_spend_lands_on_the_budget_that_delegated_it` flipped from pinning a 10-vs-210 gap to asserting they match), `spawn` writes `DelegationRequest`/`DelegationResult` entries in the `task` intrinsic's shape, and the child is announced on the parent's stream by mentra rather than narrated by basis. Events other than usage are deliberately *not* relayed: a second run's tool calls and text on the parent's stream would be rendered as the parent's own |
| Usage says what was reasoning; a turn says how many images it carried | — | **Built** (`0293331`, over mentra `1e9a15c`, `3ef731a`) — `reasoning_tokens` and `thoughts_tokens` kept apart on `RunUsage` and `Event::Usage`, because the Responses wire counts reasoning inside `output_tokens` and Gemini counts thoughts outside it, so one sum is wrong for one of the two; `image_count` on `UserMessage`, absent when zero, so an image-only turn is no longer a blank message. Both read as zero from a record written before the split |
| `disable-model-invocation` carried out to the host | — | **Built, narrowly** (`e628beb`, over mentra `b44f53a`, `bfe952b`) — `LoadedSkill::model_invocable`, one field wide, so a host reading `Workspace::skills()` can tell an unreachable skill from a reachable one without re-reading every file; the docs in three places say what the flag does and why such a skill is not a command. The slash-command wiring is deferred on the one ground that still holds — `SKILL.md` carries no argument convention — and is named in the tally |
| The in-process `Supervisor` is deleted | 0017 | **Removed** (wave 1) — `lifecycle.rs` (783 lines), `PreparedRun::spawn`, and the eight root re-exports (`Supervisor`, `TaskHandle`, `TaskId`, `TaskState`, `TaskContext`, `Cancellation`, `LifecycleError`, `WaitError`) go, with no consumer found outside basis's own tree; a 0.6.0 removal. In process, concurrency is the host's `tokio::task::JoinSet` plus the `CancellationToken` a `TurnOptions` hands back, kept finite by the bounds — `examples/review_workflow.rs` was already the reference and stays it. What it cost: a second spelling of ADR-0017's four rules, which now live in exactly one place, the CLI's durable-task contract. What it newly exposes: nothing — the in-process wait graph the docs described never existed in the crate, and the docs now say what does |
| One `Bounds` type | 0010/0014 | **Built** (wave 1) — `Bounds { deadline, tool_budget, token_budget }` at the root; `RunSpec` and `TurnOptions` hold one by value where each had restated the three fields, and `bounded()` merges two `Bounds` instead of three fields at a time. The `with_*` sugar stays, so call sites hold; the raw fields go — a 0.6.0 removal, spelled `.bounds.deadline` now. `BudgetPool` stays a separate field deliberately: an allowance shared across runs is not a bound on one. What it newly exposes: nothing — the same three limits under one name, so the next carrier cannot restate them |
| `RunConfig` is deleted; the one-shot is `run(path, prompt)` | 0010/0018 | **Removed** (wave 1) — the third spelling of what `WorkspaceBuilder` + `RunSpec` already say goes, `split()` with it; `run`/`run_with_approver` take a path and a prompt and are implemented directly over open-and-prepare, and the free `prepare`/`prepare_without_prompt`/`resume` go too, their callers being the `Workspace` shape one call earlier. `prepare_with_session` — the bring-your-own-runtime seam — now takes the session, the path, a `RunSpec`, and the context config: the inputs its callers actually use, with `session_name`/`effort` honestly not read and templates discovered by default. The two real template holders simplified rather than ported: `basis-acp`'s `SessionTemplate` carries only what an operator can say — no placeholder workspace to replace, where `ConfiguredSource::recipe()` had re-derived what `split()` threw away — and the CLI's attach/attended paths build the builder and spec directly. What it cost: the acceptance test for "the template takes the client's cwd" is deleted as structurally impossible rather than rewritten — there is no template workspace left to forget. What it newly exposes: nothing; eighteen public fields stop existing — a 0.6.0 removal |
| `RunError` lives at the crate root | — | **Built** (wave 1) — the universal error moves to `error.rs`: four of `run`'s in-edges (store, event, runtime, budget) imported nothing from `run` but this type, which manufactured four of the crate's six two-cycles out of one name. `run` re-exports it, so `basis::run::RunError` still reads and the diff stays an import diff. What it newly exposes: nothing — one path added, no variant touched |
| One event schema on disk and on stdout | 0015/0019 | **Fixed** (wave 1) — `basis --json` streamed the flat `EventLine` while the durable journal wrote a nested `{seq, event}` wrapper of its own, and the renderer re-matched *type strings*, so a new variant fell into `_ => {}` and rendered as nothing. The journal now writes `EventLine`; the renderer deserializes `basis::Event` and matches typed variants, with the wildcard the sealed enum requires — and an event the build cannot name is printed as `unrecognized event` rather than swallowed. What it cost: the reader keeps accepting the nested shape, normalized to flat on the way out, because a task directory outlives the binary that minted it (the E2 rule: what is durable on disk is a compatibility surface); `watch --json` therefore emits one schema whatever vintage the journal is. What it newly exposes: a consumer written against the stream can now read the journal, and the reverse |
| Task metadata carries types, not strings | 0019 | **Fixed** (wave 1) — `meta.json` re-encoded the approve mode, the effort, the system prompt and the stopping bound as strings, with hand parsers on the read side; `parse_effort`, `bound_name`, `effort_name` and `approval_name` are deleted and the record holds `ApproveMode`, `Effort`, `SystemPrompt` and `Bound` themselves. `Effort` and `SystemPrompt` grew serde derives in basis (additive), and the typed serializations are byte-for-byte the strings the old writer spelled, so most old records need no shim. What compat remains: the pre-0.6 two-string system prompt folds into the typed field on load, and a `stopped_by` this build cannot name (`"unknown"`) reads as no bound rather than an unloadable record. An invalid approve mode is now unrepresentable — `validate_approval` lost its unreachable arm, and corruption fails at decode, named. The same pass makes the README agree with the tree: the four rules read as the durable-task contract (nothing in process implements them since the `Supervisor` went), the delegation tally reads as closed (`e22aa63`), and the binary line states a measurement — 10.7 MB at 0.6.0 |
| mentra's memory engine is off (D2's subtractive half) | D2 | **Built** (wave 1) — checked against the vendored 0.20.0 rather than guessed: mentra registers three memory intrinsics (`memory_pin`, `memory_forget`, `memory_search`) and `MemoryConfig::default()` ships `auto_recall_enabled: true`, so every basis run had been recalling from a store nothing in basis surfaces. `agent_config` now pins `auto_recall_enabled: false` and `write_tools_enabled: false`, and the three tools join `UNSURFACED_TOOLS` (14 → 17) with their reason written beside the others; basis's memory is a file convention arriving in a later wave. What it costs: nothing visible — the store was write-and-recall by mentra alone. What stays honestly on: mentra still ingests transcripts into its store on its own schedule (`AgentConfig` offers no switch), so `MemoryUpdated` can still appear on the stream — but nothing recalls what lands there into a prompt and no tool reaches it. The roster-pinning tests moved the three names from the offered list to the doors-stay-shut list |
| The wire enums are `#[non_exhaustive]` | 0002/0011 | **Built** (wave 1) — `Event`, `RunOutcome`, `RunError`, and the pair wave 0 deferred here, `McpServer` and `McpError`: one decision instead of five, so a variant can land without a semver event. What it costs: sibling crates need wildcard arms, and a wildcard is where variants go to be silently eaten — so none swallows: `update.rs` surfaces an unmapped event as a thought chunk naming its wire tag, the ACP workspace digest keys an unknown transport by its name and says why that is imperfect, the CLI records an unknown outcome as the failure it is, and a same-crate canary test in `event.rs` matches `Event` with no wildcard so the next variant breaks basis's own build first. What it newly exposes: nothing on the wire — matching narrows, construction does not |

Footnotes on the Phase B, C and D rows, because a ledger that records only
the wins is not a ledger:

1. **`cargo tree -p basis` showed `tokio-tungstenite`, and now does not.**
   The defect as Phase B recorded it: mentra-provider required the crate
   unconditionally for the Responses websocket transport, so there was no
   basis-side gate to close, and Phase B's acceptance named the clause it could
   not satisfy instead of quietly dropping it. mentra `c30fa9c` built the gate
   — `responses-websocket`, default-on at every level so an upgrade takes
   nothing away, with mentra's own provider dependency set to
   `default-features = false` so the forwarding bites — and basis `27ab4c8`
   closed basis's side: the workspace dependency turns the default off and
   `basis` re-offers the feature for an embedder who wants it. The Phase B
   acceptance clause is met in full for the first time.
   Two facts are worth keeping, because both are easy to get backwards. First,
   **the reason basis cannot reach that transport is not a missing capability**:
   `openai_definition()` advertises `supports_websockets: true`
   (`mentra-provider/src/responses.rs`). It is that the transport is chosen
   per request through `ProviderRequestOptions.responses.transport`, basis never
   sets that field, and the `AgentConfig` that would carry it is private to
   workspace construction — so the default field value, `HttpSse`, is the only
   one a basis run can have. Off by default is therefore a finding about
   reachability, not a preference, and a build without the feature does not
   silently fall back: selecting the websocket transport answers with a typed
   `UnsupportedCapability` naming the feature to rebuild with. Second, the
   `basis` **binary** still links tungstenite, through its own direct dependency
   for the bridge's websocket server. That is a different subsystem, it is
   what the bridge is, and it stays.
2. The `mcp` feature drops basis's half only. mentra has no `mcp` feature to
   forward — its client is unconditional — so the dependency graph does not
   shrink yet. What the feature delivers is the contract point of ADR-0012:
   one seam, one adapter, droppable at compile time. The day mentra grows a
   feature of its own, `basis`'s manifest is where it gets forwarded.
3. **`RuntimeBuilder` could not be named by any downstream code, and now can
   be.** The defect the split surfaced: `RuntimeBuilder` was `pub` inside a
   *private* `mod builder`, re-exported neither by `mentra::runtime` nor at
   the crate root, so `Runtime::builder()` returned a type no caller could
   write. Inference carried a chained build through — `basis::run::resolve`
   got by on `let builder = Runtime::builder()`, rebound as it went — but a
   helper *taking or returning* a half-built runtime could not state its
   signature at all. Fixed upstream rather than worked around, per ADR-0005:
   mentra `c04986a` re-exports it where `Runtime` already is, at
   `mentra::runtime::RuntimeBuilder` and the crate root, and pins it from
   outside the crate in `tests/public_api.rs`, where compiling *is* the claim.
   Footnote 7 is the same candidate's second sighting and closes with it.
4. The deny-reason gap was fixed upstream rather than worked around. `a4c259c`
   knowingly lost basis's descriptive denial, because `PermissionDecision`
   carried no reason field; mentra `15fdcfe` added one, and `6192230` restored
   the wording through `ApprovalAnswer`. That ordering is ADR-0005 working as
   written — the gap went upstream and basis waited for it.
5. **A remembered refusal said why only once, and now says why every time.**
   The defect: the first denial carried the approver's reason, but later calls
   never reached the approver — mentra's `RuleStore` answered them from a
   `RememberedRule` that kept the verdict and its scope and no reason, so the
   model read "blocked by remembered session rule". Nothing actionable in it,
   and nothing to stop the model asking again. basis-acp masked the gap:
   `ModedApprover` remembers the "…for this session" answers itself and
   restates the reason each time, so the gap showed only for an embedding host
   whose `Approver` returns `DenyForSession` — the one path that reaches
   `deny_and_remember`. Threading a reason through `RememberedRule` was wider
   than mentra `15fdcfe` needed to be, so it was left as the third upstream
   candidate under ADR-0005 rather than papered over.
   mentra `b895ea0` closes it. The rule carries the refusal's reason, written
   at remember time — refusals only, since an allow explains itself by
   happening — and a remembered denial restates it as
   "«original reason» — remembered from an earlier refusal, so asking again
   will not change it", with the original words in front because they are the
   part that says what to do instead. `RuleStore::matching_rule` hands back the
   whole rule where `check()` gave only the verdict, on the same
   glob-over-bare precedence. Old rows load reasonless and keep the old generic
   message; the SQLite store grows the nullable column on open by the same
   migration pattern `project_id` used, and a new-code database still opens
   under old code, whose queries name their columns.
   One consequence for basis, named and **not** acted on: `ModedApprover`'s
   masking is now redundant on the `DenyForSession` path, since the reason
   survives without it. That makes it a simplification candidate, not a
   finished one — the wrapper does other work for the mode list, and nobody
   has checked what else depends on it restating the reason itself.
6. **The suite wrote a real database under the user's data directory** — and
   Phase D closed it end-to-end, so this footnote is now a record of a fixed
   hole rather than an open one. A runtime with no store configured takes
   mentra's default — `~/Library/Application Support/mentra/workspaces/<hash>/runtime.sqlite`
   on macOS, `data_local_dir()` elsewhere (`mentra/src/default_paths.rs`) —
   and basis had been building real `Runtime`s in tests since P1 (`af04f9d`):
   `cargo test -p basis --test approval` alone touched that file, no
   `Workspace` involved. Phase C made it ordinary rather than causing it, since
   `Workspace::open` makes "this test drives a real runtime" the default shape.
   Two facts made it worse than it looked. The default path is keyed by the
   *process's* current directory rather than by the workspace basis opened, so
   every test binary in one `cargo test` shares one file whatever temp
   directory it opened — verified by running two test binaries and watching one
   `runtime.sqlite` change under both. And mentra opened that default store
   *eagerly*, at handle construction, before `with_store` could rebind it, so
   even a runtime that named its own store still touched the machine-wide file
   on the way past — which `397ca13` named as an upstream item rather than
   papering over.
   Three changes close it. `397ca13` added `WorkspaceBuilder::with_store_dir`
   and moved the test suites onto scratch stores; `71cc59d` added
   `with_ephemeral_history` and moved them again, onto mentra's in-memory store,
   which is where a test that is not testing persistence belongs; mentra
   `b1a83de` made store recovery wait for the store the builder ends with, so
   constructing one only records a path and the first `open()` is what touches
   disk. The honest metric is now flat in both directions: across a full
   `cargo test --workspace`, agent rows in the machine-wide default database
   move by zero and the file's mtime does not move either — measured on all four
   paths a basis test binary could key (`b5a71edc0abf57d2` for the workspace root,
   `e8f5371f626eb964` for `basis`, `9e7efd0f1007c4b0` for `basis-acp`,
   `cc9c24177d9e277d` for `basis`). Temp directories left behind per run: zero.
   The rows those databases already hold — 1,046 under `basis`'s hash, 260
   under the binary's, 110 under the workspace root's — are the historical
   accumulation, and nothing deletes them; the claim is about what a run adds
   from here, which is nothing.
7. **The second sighting of footnote 3, in the place the split made most
   visible — and it closes with it.** `WorkspaceBuilder::open` folded the
   discovered MCP servers into the builder inline
   (`basis/src/workspace/builder.rs`, the
   `servers.into_iter().fold(builder, …)` block) because a helper taking or
   returning a half-built `RuntimeBuilder` could not name its type. mentra
   `c04986a` makes the type nameable, so the fold is now an inline expression
   by choice rather than by constraint. Recorded rather than deleted, because
   two independent sightings of one papercut is what made the case for fixing
   it upstream.
8. **A graceful stop after a tool round reports a failed turn, though the work
   is kept.** `TurnOptions::stop` ends a turn at the next round boundary and
   nothing is rolled back — but mentra still owes its caller a final assistant
   message, and when the last committed message is a tool result there is
   none, so the turn comes back as an error. The work is kept either way; the
   report is what disagrees. Pinned by
   `basis/tests/cancellation.rs::a_graceful_stop_after_a_tool_round_keeps_its_work_but_reports_failure`
   and documented on the field itself.
9. **Two different failures share one upstream error.** "run completed without
   invoking the expected terminal tool" and a genuinely malformed provider
   stream are both `RuntimeError::MalformedProviderEvent`
   (`mentra/src/agent/terminal_output.rs`), and basis reports both as
   `RunError::Runtime` rather than matching on error prose to tell them apart.
   `RunError::OutputMismatch` is basis's own precisely because it does not need
   prose: basis asks mentra for a `Value` and deserializes it here, so an answer
   that does not fit `T` is basis's finding and a caller can retry it with a
   clearer schema.
10. **Delegated tokens used to escape every bound basis could set. They no longer
    do.** Phase C found the hole and Phase D's upstream wave closed it, so what
    follows is both halves. The hole: mentra's `task` intrinsic spawned a
    subagent and drove it on `RunOptions::default()` — a fresh, zeroed counter
    and no bound at all — while `RunOptions::child()` sat beside it documenting
    exactly the inheritance that path needed. A delegating run's subagent
    tokens therefore reached neither `RunUsage` nor `TurnOptions::token_budget`
    nor a `BudgetPool`, and basis sets no `tool_profile`, so `task` is available
    to every run by default.
    The fix is mentra `0436bae`, and it closes the gap on both sides.
    Accounting: the parent's in-flight options reach the spawn site through
    `ToolContext::child_run_options`, so the delegated run shares the parent's
    accounting handle and `token_budget` and ends with its cancellation, stop,
    and deadline (`mentra/src/runtime/intrinsic/execute.rs`). Observation: the
    child's `UsageReport` events are relayed onto the parent's bus for the
    duration of the run, so an observer summing basis's event stream gets the
    same total the accounting handle reports — pinned upstream by
    `delegated_subagent_usage_counts_against_the_parent_token_budget` and
    `delegated_usage_reports_reach_the_parent_event_stream`. `child()`'s own
    rustdoc no longer claims mentra never spawns a child run itself, which it
    had.
    One edge follows from round-boundary softness and is pinned rather than
    hidden: a delegation issued *after* the budget is already crossed inherits
    a spent allowance, does zero rounds, and fails visibly instead of
    succeeding empty (`delegating_with_the_budget_already_spent_fails_the_delegation`).
    `Session::spawn_subagent` is deliberately unchanged — host-initiated, with
    nothing in flight to inherit — and gained `spawn_subagent_with_options` for
    a host that wants the inheritance anyway.
    One live observation from Phase C stays on the pile without being a claim
    about either crate: on the gateway the acceptance runs used, one model
    failed *every* reading turn with
    `malformed provider event: assistant turn ended before MessageStopped`
    where two others on the same gateway did the same work, and what set it
    apart is that it reached for `task` where they read files directly. One
    gateway, one model, no reduction — recorded because the delegating path is
    where the unknowns were.
11. **A crossed token budget was a silent success upstream, and is now a
    reported one.** The defect, and it shaped two basis decisions for two
    phases: `mentra/src/agent/runner.rs` answered
    `options.token_budget_exceeded()` with a plain `return Ok(())` at the round
    boundary — the transcript kept, the turn over, and nothing typed saying
    *why* it ended. basis cannot report a stop it cannot observe, so `Bound` had
    no `TokenBudget` variant and `--token-budget` could not produce exit `3`.
    That also left a tension standing from Phase A: ADR-0014 calls
    `--token-budget` a bound and ADR-0015 promises "distinct nonzero codes for
    run failure and for a tripped bound", which read together would have exit
    `3` cover all three flags. It covered two, because the third was a bound
    basis could not observe being tripped, and a code invented for it would have
    been a guess dressed as a contract.
    mentra `5a2a68e` supplies the observation. `EarlyEnd` is a write-once slot
    on `RunOptions`, the counterpart of the `token_usage` counter on the same
    struct — same `Arc` sharing, same read-from-a-clone rule (`ended_early()`)
    — and the runner records at both boundary shapes. The combined
    stop-or-budget check is split so that the order which *decides* is also the
    order which *reports*, with stop winning when both held: an instruction the
    caller issued outranks an ambient bound that merely also held. `child()`
    derives a fresh slot, so a delegated run's ending is never read as the
    parent's. basis `8e35f3e` maps it to `Bound::TokenBudget` and consults the
    runner's own record on both finish arms, because the load-bearing case is
    an `Ok`: a run can end on its budget with an answer already committed, and
    nothing else in that result tells "the model was done" from "the allowance
    ran out". `EarlyEnd::StopRequested` maps to nothing on purpose — a caller's
    own stop button does not belong on the same exit code as running out of
    budget. basis `a2d170a` puts the same fact on the stream: `run_finished`
    carries `stopped_by` (`"deadline" | "tool_budget" | "token_budget"`) when a
    bound ended the run and **omits the key otherwise** — absent, not null, so
    an unbounded finish is byte-identical to what a schema-1 consumer already
    reads and the wire version does not move.
    Four things about the shipped signal are worth more than the headline.
    **The bound reports only when the runner decided something.** A run whose
    model finishes inside the round that crosses the line exits `0`, because
    the runner never refused a round — verified live both ways against the
    gateway, a tool-round crossing giving `3` and a one-round answer giving
    `0`. That is narrower than "the budget was exceeded", and it is the honest
    reading: the exit code names a decision, not an arithmetic fact.
    **The zero-budget pin changed its answer, honestly.**
    `basis/tests/budget.rs::a_zero_token_budget_is_what_refusing_avoids`
    used to show a provider-shaped `EmptyAssistantResponse` for an accounting
    decision, because mentra compares `reported >= budget` and a zero budget is
    already crossed before the first round. The run still does nothing, but the
    report now names the bound — so what a `BudgetPool`'s pre-refusal with
    `RunError::BudgetExhausted` buys is narrower than it was, and still real:
    refusing before the prompt is committed beats committing it and reporting
    why afterwards.
    **On a bound-ended typed turn the stream is the sole carrier.** A typed
    turn with no value returns `Err`, and the report that would otherwise carry
    `stopped_by` is not handed back — there is nothing to hand it back with. So
    the only place the bound is named is `Event::RunFinished`, pinned by
    `basis/tests/output.rs::a_working_turn_out_of_budget_says_so_on_the_stream`.
    A host that drives typed turns and reads only reports will see a
    `RunError::Runtime` with no bound in it.
    **A run that answers *and* is bounded exits `3` printing nothing on
    stderr.** `basis/src/run.rs` announces on stderr only from the
    `RunOutcome::Error` arm, so an `Ok` result that carries a bound exits `3`
    silently. It is unreachable from today's CLI — reaching it needs a queued
    steer sitting behind a committed final message, and basis has no steering
    surface — but it becomes live the day basis grows one, and it is named here
    rather than discovered then.
12. **A typed turn was a shaping turn and nothing else; it can now be a working
    one.** The defect, and it cost a live fan-out before it was understood:
    while a run answered into a schema it held exactly one tool. Registering
    the generated terminal tool opened a gate on the agent
    (`mentra/src/agent/terminal_output.rs`), and while that gate was open
    `tools()` filtered the whole toolset down to that one tool and
    `tool_choice()` forced it (`mentra/src/agent.rs`). It could not read a
    file, run a command, or reach an MCP server on that turn — so asking a
    reviewer for findings in one `output` call returned an empty list from a
    model that had opened nothing, *and returned it as a success*. Not
    hypothetical: the first live fan-out did exactly that, and the wording that
    invited it was basis's own — the doctest asked a typed turn to "review the
    diff on this branch", and the guidance on `OutputSpec::description` held up
    "call this once you have reviewed every file" as the description to
    imitate. `dae4765` corrected three doc sites to say what the turn could
    actually do, and read-then-shape as two turns is what
    `examples/review_workflow.rs` documents at length.
    The mechanism was upstream's stated contract rather than an oversight, so
    this was filed as an ADR-0005 candidate on *ergonomics* rather than on
    truth: a mode that kept the ordinary toolset alongside the terminal tool
    would remove the two-turn ceremony, and a contract needing three doc
    corrections in one commit to state plainly is one worth making harder to
    get wrong. mentra `be65c00` built that mode as
    `TerminalOutputSpec::with_tools()`, and basis `b782e75` surfaced it as
    `OutputSpec::with_tools()`. The default is unchanged; the ceremony is now a
    choice.
    What the working mode gives up is **the forcing**, and that is the whole
    trade. A shaping turn is *made* to answer. A working turn narrows nothing
    and forces nothing — `Auto` uniformly, since forcing the terminal tool
    would end the turn before any work happened and a configured `Tool{..}`
    would keep it from ever reaching the call that ends it — so it can settle
    for prose, or be refused another round by a bound while it is still
    gathering. On both of those paths there is no value, `output` returns
    `Err`, and the report is dropped, which makes the event stream the sole
    carrier of why (footnote 11). Two turns also stay the better shape when the
    reading should *not* share a context with the answering: one reader per
    reviewer, as `examples/review_workflow.rs` still does it.
    The live check reproduced the original shape both ways against the gateway.
    Default mode: zero findings from a model that read nothing, reported as a
    success, at 290 tokens. `with_tools`: both planted bugs found and named
    specifically, in one call, at 3,876 tokens. That ratio is the honest
    summary of the trade — the working turn costs an order of magnitude more
    and answers the question. The three doc sites that used to teach only the
    old contract now teach both modes, keeping the original warning for the
    default and adding its inverse for the working turn: nothing makes it stop
    and answer, so the description is where the stopping condition belongs.
    One reporting change reaches the default mode as well — a typed run whose
    provider answered nothing is now reported as the missing terminal call it
    is, rather than as `EmptyAssistantResponse`.
13. **A held `RunReport` holds a fan-in's merged stream open.** A finished run
    hands its sink back inside the report, so a report kept alive is a branch
    of `MergedEvents` kept alive — and a host that awaits its runs and its
    consumer in one `tokio::join!` will wait forever unless it lets the
    reports go inside the branch that produced them. The sharp edge of a
    design that otherwise has none; named in the `MergedEvents` rustdoc and
    pinned by
    `basis/tests/fan_in.rs::a_held_report_holds_its_branch_of_the_stream_open`.
14. `ProviderError` gained `UnattributedCredential` — a key supplied with
    neither a provider nor a base URL to attribute it to is refused rather
    than guessed at — and the enum is not `#[non_exhaustive]`, so that is a
    breaking addition for anyone matching it exhaustively. Accepted
    knowingly: basis is unpublished, and pre-1.0 the crate API is the stated
    compatibility surface.
15. **`HookRunner::decide` now refuses when interceptors are registered**, which
    is a behavior change on a public method and a deliberate one. `decide` is
    synchronous; an `Interceptor` is `async` by contract; there is nowhere in a
    synchronous call to await one. The two options were to skip the
    interceptors — silently removing a control the host believes is in place,
    the exact failure the module is arranged to avoid — or to deny with a
    reason naming `decide_async`. It denies. A runner with no interceptors is
    unaffected, so nothing that worked before this change behaves differently;
    what changed is that a *new* combination fails closed rather than quietly.
    `basis/src/hooks/runner.rs`.
16. **Implementing a basis trait used to cost the host an `async-trait`
    dependency.** `Interceptor` and `Approver` are both `#[async_trait]`, and
    `basis` did not re-export the macro, so a host writing either impl added
    `async-trait = "0.1"` to its own manifest to spell an attribute basis's docs
    asked for without saying so. A consistent papercut rather than a defect —
    mentra's own hook trait has the same shape and the reason is the same one
    (a participant that reads a file or takes a lock must not block a runtime
    worker) — but it was a line of someone else's `Cargo.toml`. Closed basis-side
    in `ff5fc70`: `basis::async_trait` is re-exported at the crate root under
    the rule already governing `BuiltinProvider` and `ModelSelector` — a name
    basis's surface makes a caller write is a name basis re-exports, and the rule
    reads the same for a macro as for a type. The interceptor doctest and the
    README example spell it `#[basis::async_trait]`, which is what pins the
    re-export rather than merely asserting it.
17. **`session/list` had never worked, and the fix is forward-only on purpose.**
    basis filtered listings on the workspace's runtime identifier
    (`store::runtime_identifier`) while `WorkspaceBuilder::open` never set one,
    so mentra tagged every agent `"default"` and no workspace's list ever
    matched a row. `e81e5d8` sets the tag. Three upstream facts decide what
    happens to rows written before it. Listing is one query —
    `SELECT id FROM agents WHERE runtime_identifier = ?1`
    (`SqliteRuntimeStore::list_agents_by_runtime`) — so an untagged row cannot
    appear. Resuming does not consult the tag at all: `load_agent` is
    `… FROM agents WHERE id = ?1`, so every pre-existing conversation is still
    resumable by id. And the agent upsert re-tags on conflict
    (`ON CONFLICT(id) DO UPDATE SET runtime_identifier = excluded.runtime_identifier`),
    so an old conversation joins its workspace's list the first time it is
    resumed and used. That is why there is no migration: nothing is stranded,
    and the gap heals on use. The alternative — falling back to reading
    `"default"` when a workspace's own query comes back empty — would be
    strictly worse, because `"default"` is also *every other mentra program's*
    tag in the same shared database, so a client would be offered conversations
    that were never the user's.
18. **The redactions are hand-written, which is the cost of having them.**
    `ProviderChoice` printed its `api_key` through a derived `Debug` — which is
    how a failing resolution test once put a live key in a terminal, since
    `expect()` formats the `Ok` it did not want — and `McpServer`'s stdio `env`
    had the same shape, holding already-expanded values like a real
    `GITHUB_TOKEN` and deriving its way into every `{:?}` of an `McpConfig`,
    `McpSource`, or `RunConfig`. Both now write `Debug` by hand: names kept,
    because naming `env.GITHUB_TOKEN` is what makes a misconfiguration fixable;
    values redacted. The edge that comes with it is the ordinary one for a
    hand-written impl — a field added later is not redacted until someone adds
    it here — and it is the reason the SSE side was left alone, since it
    already self-redacts through mentra's `SecretString`. The same commit made
    provider resolution read the environment through a passed-in lookup, the
    idiom `crate::mcp` already uses for `${VAR}` expansion, so its rules are
    pinned by fixtures rather than by whatever the shell that started the tests
    exported: the suite passes identically with `BASIS_API_KEY`/`BASIS_BASE_URL`
    set and unset, and the `env -u` ritual that used to precede every
    invocation is retired.
19. **Tests move to their own file at the 800-line ceiling**, adopted as a
    convention in Phase D rather than declared: `basis/src/hooks/runner.rs`
    and `basis/src/workspace/builder.rs` both ended `mod tests;` with the
    cases in `runner/tests.rs` and `builder/tests.rs`, which is what kept them
    under the limit while growing. Three files were named as still over it and
    all three have since been split, each at a seam that already existed rather
    than at a line count. `basis-acp/src/server.rs`, 1,089 lines, became 337 plus
    four modules (`89ccce4`): `config.rs` holds `ServeConfig` and the
    `SessionSource` seam, `lifecycle.rs` the handshake and session bookkeeping,
    `turn.rs` the one handler that runs the agent, and the tests their own
    file. `basis/src/main.rs`, 1,073 lines, became 104 plus six (`665ced6`) —
    the grammar stays whole in `cli.rs` because ADR-0015 defines it as a unit,
    and the exit-code contract got `exit.rs` so the whole promise fits on one
    screen. `basis-acp/tests/acp.rs`, 872 lines, was the case the convention's
    own remedy could not reach, since it already *was* a tests file; `e37f4f3`
    made it `tests/acp/main.rs` plus six modules as **one** test crate,
    deliberately — the mock runtime and the scripted client are 380 of those
    lines and every test needs both ends, so separate `tests/*.rs` crates would
    compile the harness once per file and a shared `common/` would need a
    dead-code allow that hides genuinely dead scaffolding forever. One Rust
    detail that cost time and is worth writing down: a `mod` declared in
    `tests/acp.rs` resolves against `tests/`, not against `tests/acp/`, because
    that file is a crate root — which is why the directory form takes a
    `main.rs`.
    **The ceiling is nonetheless not held today, and by this wave's own work.**
    `b782e75` took `basis/tests/output.rs` from 474 lines to **841** and
    `basis/src/run/prepared.rs` from 797 to **808**, and
    `basis/src/run.rs` sits at exactly **800**. So the score is three files
    brought under and two pushed over in the same series of commits, which is
    the honest shape of a convention that is real but not enforced by anything
    — no lint, no CI gate, only a number in a footnote somebody has to look at.
    Named here on the same rule as before: the ceiling stays a real number
    rather than an aspiration only if the misses are written down as
    faithfully as the hits.
20. **mentra's `MockRuntime` littered, and could collide.** With no store
    configured, `MockRuntime::builder().build()` minted
    `$TMPDIR/mentra-mock-runtime-<nanos>.sqlite` (`mentra/src/test.rs`) and
    nothing removed it: a full `cargo test --workspace` in basis left 58 such
    files behind, measured as a before/after delta against mentra `b1a83de`.
    That is litter rather than a correctness problem. The correctness question
    was the second use of the same clock — the mock's runtime identifier is
    `mock-runtime-<nanos>` from the same `now_nanos()` — so two mocks built
    inside one nanosecond tick would have shared both a store path and a
    runtime identifier, and each would have listed the other's agents. That is
    offered as a *suspected* mechanism and nothing more: a flake in
    `basis/tests/hooks.rs` was seen exactly once, has not reproduced since,
    and was never reduced to a failing case, so the honest statement is that
    the collision was possible in principle and unproven in this instance.
    The mentra fix — `MockRuntime` defaulting to the volatile store, with the
    SQLite path kept as an explicit `with_store`, and an atomic counter beside
    the timestamp because a wall clock alone is not a source of uniqueness —
    landed as mentra `aa206b7`, and with it the same before/after delta is
    zero.
    **A standing observation, carried because sightings accumulate faster than
    explanations.** Three unexplained single-run test failures are now on
    record rather than dismissed. `aa206b7`'s gate saw one test fail once in a
    full-workspace run and pass in five subsequent runs, without reproducing
    and without printing a name. `be65c00`'s gate saw a single lib-test flake
    that did not recur across seven full-suite reruns. The third came from
    rev 6's own docs gate and is the first with a name attached:
    `basis/tests/hooks.rs::a_hook_is_told_which_schema_it_is_talking_to`
    failed in a `cargo test --workspace` run with
    `hook 'version' … did not answer within 5000ms and was killed`, then passed
    twenty-for-twenty across five consecutive runs of that target alone, which
    complete in about 0.3 s each. A hook subprocess missing a five-second
    deadline in a file that finishes in a third of a second is a margin of
    roughly four orders of magnitude, so the mechanism it points at is
    scheduling starvation on a loaded machine — that run shared the box with
    several concurrent agents — rather than anything in `exec.rs`.
    That is a *candidate* mechanism for the other two and not an explanation of
    them: neither was reduced, one has no name, and the three span two
    repositories and three suites. What it does suggest is a class — the suite
    is full of subprocess and timeout assertions, `NOT_STUCK` among them, and
    those are exactly the assertions that turn machine load into a red test.
    Worth knowing before a fourth sighting is read as a regression.

Discoveries that shrank the plan: structured output and per-run bounds +
cancellation were assumed to be new mentra work; both already exist upstream
(`agent/terminal_output.rs`; `RunOptions`). Phase B corrected the other half
of this paragraph — a mentra change *was* required and was made rather than
worked around: `PermissionDecision` gained a reason field (mentra `15fdcfe`)
so a basis denial can say why it refused. Phase C made a second one, for the
same reason: the typed path wanted a *session*-level entry point so a typed
turn would emit the same events as any other, and mentra grew
`Session::append_turn_to_output` (`fce664a`) rather than basis reaching past the
session to the agent. Phase D made three more, and they are the first that
fixed something *already wrong* upstream rather than adding a door basis needed:
`0436bae`, `b1a83de`, and `aa206b7`. Rev 6's wave made five, and they are the
first that were *scheduled* — the tally below is what scheduled them.

What Phase C mostly discovered, though, is where the honest edges are, and the
two revs since are where they stopped being edges. The running tally, because
a list of "candidates" that only grows says nothing about whether the ADR-0005
discipline works. **Nine were named across Phases B–D. Eight are fixed
upstream** — `task`-delegated accounting, which now shares the parent's handle
and relays its usage (mentra `0436bae`, footnote 10); the eager default-store
open, which now waits for the store the builder ends with (mentra `b1a83de`,
footnote 6); `MockRuntime` defaulting to the volatile store, which took the
temp litter and a possible identifier collision with it (mentra `aa206b7`,
footnote 20); a run that ends on a bound now recording which one, so basis can
report a stop it can finally observe (mentra `5a2a68e`, footnote 11);
the Responses websocket transport behind a feature, so `basis`'s graph no
longer carries a websocket stack it cannot reach (mentra `c30fa9c`, footnote
1); a typed turn that can keep its tools, so read-then-shape is a choice
(mentra `be65c00`, footnote 12); a `RememberedRule` that carries its refusal's
reason (mentra `b895ea0`, footnote 5); and `RuntimeBuilder` re-exported where
downstream code can name it (mentra `c04986a`, footnotes 3 and 7). **The
ninth was basis's own** rather than mentra's — a store knob on `WorkspaceBuilder`
— and `397ca13` plus `71cc59d` built it (footnote 6). **Zero were open, for one
day.** ADR-0016's first wave then found three more, all upstream-shaped and all
open, rev 12's declared tools found two more after them, and wiring compaction
found two more again. All seven are named further down — and, as of rev 13,
all seven are closed, kept there as what they were.

That is still the first time this ledger has been clean, and it is worth being
precise about what it measures. Not that mentra is finished, and not that basis
found everything: it measures ADR-0005's discipline — that a gap basis hits goes
upstream and basis waits for it, instead of growing a basis-side workaround that
nobody else ever benefits from and that quietly becomes the API. Nine gaps,
nine fixes at the layer that owned them, no workarounds carried. A tally that
only accumulated would have said the discipline was a filing cabinet.

A clean tally is also the moment a ledger is most tempted to stop being one, so
the candidates this wave *created* are named here rather than waiting for
someone to hit them. Three are new, none blocking, none built. On a typed turn
ended by a bound the report is dropped, so the event stream is the sole carrier
of which bound it was — a host reading only reports sees an untyped failure
(footnotes 11 and 12). A run that answers *and* is bounded exits `3` with
nothing on stderr; unreachable from today's CLI, live the day basis grows a
steering surface (footnote 11). And `ModedApprover`'s masking of the remembered
refusal gap is now redundant on the `DenyForSession` path, which makes it a
simplification candidate and **not** a finished one — nobody has checked what
else depends on it restating the reason itself (footnote 5). Older and still
open, on a list of its own: footnote 8, where a graceful stop after a tool
round reports a failed turn though the work is kept. It is the one
upstream-shaped edge the tally never counted, because it was written down as a
pinned behavior rather than filed as a candidate — which is its own small
dishonesty, and easier to see on the one day the counted column was empty.

**Then ADR-0016 put three back in it**, and a tally that only *drained* would
say something as wrong as one that only accumulated. All three were found by
registering a tool for the first time, which is exactly where a working
discipline finds holes, and all three had the same shape: a door mentra opened
for its own `task` intrinsic and had not opened for a registered tool. **All
three are closed** (rev 13), and are kept here as what they were.

- **A delegated child's usage was bounded but invisible.** Agent mode drives
  its subagent on `ToolContext::child_run_options`, so the spend counted
  against the parent's counter and its bounds — but the relay that puts a
  child's `UsageReport` on the parent's event bus was `pub(crate)`, written
  for the intrinsic. So a run could be stopped by a total more than ten times
  what `RunReport::usage` admitted to, which
  `basis/tests/spawn.rs::delegated_spend_lands_on_the_budget_that_delegated_it`
  asserted in both directions. Closed: `ToolContext::relay_subagent_usage`
  (mentra `5f303b8`), held for the child's whole run by `spawn` (basis
  `e22aa63`); the test now asserts the two figures agree, and footnote 10's
  second half is true of the route basis actually uses.
- **A subagent's events reached no basis stream.** The event bus is
  per-agent and the bridge between two of them was mentra's to expose.
  Closed in two halves: `relay_subagent_events` exists (mentra `5f303b8`) and
  basis deliberately does *not* call it — relaying a child's tool calls and
  text onto the parent's stream would have a host render a second run's work
  as the parent's own — while `register_subagent` now announces the child
  itself (mentra `bfe952b`), which reaches basis as a `TaskUpdated` of kind
  `Subagent` and made `spawn`'s own narration redundant (basis `54f3d21`).
- **Delegation transcript artifacts were unwritable from a host tool.**
  `record_delegation_request` / `record_delegation_result` are public on
  `ToolContext` (mentra `5f303b8`) and `spawn` writes both in the `task`
  intrinsic's shape (basis `e22aa63`), so a reader following delegation edges
  no longer sees `spawn`'s delegations as work the parent did itself.

**And rev 12's declared tools put two more in it**, found the same way — by
registering a kind of tool mentra had not been asked for before. Both were
about the registry rather than about execution, which is where a
*declaration*-driven binding differs from a code-driven one: the thing being
registered arrives from a file that a repository ships, so the registry's
defaults become a security question rather than an ergonomic one. **Both are
closed** (rev 13).

- **A tool's declared `input_schema` was never checked against the call.**
  Filed as [mentra#23](https://github.com/oops-rs/mentra/issues/23); closed by
  mentra `dd2e38a`, which validates a call against the tool's schema before
  authorization — deliberately partial (`required`, scalar types, `enum`,
  `additionalProperties: false`) and ignoring keywords it does not implement,
  because an unimplemented feature must never fail a valid call; terminal
  output tools exempt, since double-checking turned a clean failure into an
  infinite retry. It caught mentra's own `edit` accepting a shape its schema
  never described. basis's cheap half is deleted (`6b37ddb`): the one check
  that survives is that the input is an object, because a manifest may omit
  `type` and with no `type` keyword the validator cannot reject a non-object
  this binding pipes to a program's stdin. An audit of every tool basis
  registers found no schema/parser disagreement to fix.
- **`ToolRegistry::register_tool` replaced on a duplicate name, and nothing
  could ask it not to.** Filed as
  [mentra#24](https://github.com/oops-rs/mentra/issues/24); closed by the same
  commit — `try_register_tool` refuses and leaves the registry untouched,
  `unregister_tool` is public. basis uses both (`a45b01d`, `5ebd1be`): the
  last holder to release a declared tool's name takes it off the runtime
  rather than leaving a tombstone, a second open of the same repository joins
  the first's registration instead of swapping the program under a running
  agent, and a workspace's bridged MCP tools come off with it. A cross-
  workspace refusal names both claimants. What did **not** change, on
  purpose: `RuntimeBuilder::with_tool` still replaces, because a host naming
  `spawn` is the policy owner making a choice.

**And one more, which is the first this ledger carries a workaround for.** It
was found where the other five were — by a production host, iBot, hitting it —
and it is the only candidate so far whose absence made a *documented* feature
unwritable rather than merely awkward.

- **`SessionEvent::PermissionRequested` drops the authorization preview.**
  `Approver`'s own module doc has named "allow edits but deny the network" as
  the policy the seam exists for since ADR-0010, and it could not be written:
  the event carries five strings, none of them the `side_effect_level` the
  authorizer had in hand a moment earlier — and its `preview` field, despite the
  name, is the tool's `structured_input` rather than the
  `ToolAuthorizationPreview` it is read from. So a host that wanted the policy
  had to re-derive levels from tool names, which is a policy that silently stops
  covering the next MCP server a workspace connects. Filed as
  [mentra#21](https://github.com/oops-rs/mentra/issues/21); **closed** by
  mentra 0.20.0, which puts `classification: Option<ToolClassification>` on
  the event itself — always `Some` on a live request, `None` only on a stream
  recorded before the field existed. The
  `ToolQueued`-correlation route is not an alternative and was checked: mentra
  hardcodes that event's `mutability` to `Unknown`, and even wired it would
  collapse `Process` and `External`, which is the distinction the policy turns
  on.
  **The interim, under ADR-0005's one exemption** — "basis may carry a temporary
  workaround only with a linked mentra issue and a removal note" — was
  `basis/src/approval/levels.rs`, a side channel through basis's own
  gate: `ApprovalGate::authorize` sees the whole preview and runs strictly
  before mentra emits the event, so it writes `tool_call_id → level` into a
  handle the forwarder takes it back out of when it builds the
  `ApprovalRequest`. Keyed on `tool_call_id` because it is the only field the
  authorization request and the permission event share; taken rather than read,
  because a request is resolved exactly once; capped at 256 with the oldest
  evicted, because a request whose event a lagging receiver dropped leaves an
  entry nobody comes for and a `Runtime` lives as long as the process. Every way
  it can miss — an unwired host, an evicted entry, two runs colliding on one
  provider-assigned id — reads as `None`, which the approver is told to judge as
  `External`. `ApprovalRequest::side_effect_level` is the part that survived
  the fix, since `Option` is also the honest shape for an event replayed from
  a stream recorded before the field existed; `SideEffectLevels`,
  `ApprovalGate::levels`, `PreparedRun::with_side_effect_levels` and the
  `Runtime` field between them went with the file the day it landed — ~360
  lines, the eviction cap's false-`None`, and the four tests whose subject
  was the channel itself. What it newly exposes: the whole classification —
  capabilities, durability, the execution and approval categories — now
  reaches the forwarder, and basis reads exactly one field of it. basis's own
  `Event` still does not carry the classification, deliberately: putting it
  there is a wire-format decision this bump does not make.

**And wiring compaction put two more in it**, found the way the last five were:
by being the first caller that needed the thing. Neither was filed, and
neither blocked the row above — basis's defaults worked, and both were about
doing better than a fixed number rather than about doing it at all. **Both are
closed** (rev 13).

- **`keep_recent_tool_results: 3` was the wrong default for a coding agent.**
  basis overrode it, which is a downstream fix to an upstream default, and a
  default is the one thing that should not need overriding by everyone who
  hits it. Closed by mentra `7a539be`: the default is `usize::MAX`, the
  elision is opt-in by number on both sides, and basis's `None` now maps to
  mentra's own default rather than against it. The knob stays; the argument
  in `compaction.rs` against a default that no longer exists is gone
  (`8108e24`).
- **A window-relative trigger and overflow recovery could not be built
  downstream.** Three facts closed the route: `ModelInfo` carried no window,
  `estimated_request_tokens` was `pub(crate)`, and no `ProviderError` meant
  "context overflow". Closed by mentra `4883ba9`, `2c77792`, `11826ee`, and
  — found on the way — `bfe952b`, which fixed `resolve_model` synthesizing a
  bare `ModelInfo` for a pinned id so the window-relative threshold had
  applied to no `--model` at all. basis exposes the percent trigger beside
  the token one, reads `context_window()` off the live session, estimates
  with mentra's own estimator, and sends an ACP `UsageUpdate` after each turn
  when the window is known and nothing when it is not (`8108e24`, `b9478b7`,
  `bd10912`, `e35c8f4`). What it costs: a resumed conversation starts with no
  window, because the record stores a model id and nothing about the model,
  so `Workspace::resume` reapplies the workspace's model exactly while the
  conversation is still on it. What stays unknown: Anthropic's and the OpenAI
  wires' listings report no window, so on those the 50,000-token fallback is
  still the trigger.

**Six more went up while those were being met, and came back the same
afternoon** (mentra `bfe952b`). They are listed in §3's Rev 13 rather than
here because none was ever open for longer than it took to write it down: a
builder-level door for a compatible endpoint, keyed or not; `Session::compact`
emitting on the session stream out of turn; `Session::context_window()` and a
pinned model resolved through the listing; a body for a skill the model may
not invoke; `MockRuntimeBuilder::with_post_hook`; and `register_subagent`
announcing what it registers. One was refused on basis's own account — a
`try_` form of `RuntimeBuilder::with_tool`, which basis does not want — and is
recorded in §2's row rather than asked for.

**What the wave created, named now rather than waited for.** Three, none
upstream-shaped, none built. A skill marked `disable-model-invocation` is
exactly what `basis-acp` advertises a workspace *template* as, and now that
mentra hands a host the body, the one thing between it and an ACP slash
command is that `SKILL.md` carries no argument convention — adding one beside
`$1`/`$ARGUMENTS` and `argument-hint` would be a second templating system, so
it waits for a case that wants it. `basis-acp` advertises templates as
commands and does not expand them — a client sending `/review the diff` has
the literal text forwarded — which was invisible until `/compact` started
working and is now an inconsistency a reader can see. The third — `--continue`
picked the newest task by *start* time, so `basis spawn A; basis spawn B;
basis send A …; basis --continue` resolved to B — is **fixed** (`a4257e7`),
and more cheaply than the fix sketched here: no new field and no second
writer on `meta.json`, because both clocks already existed on disk —
`meta.updated_ms` (written at every executor step, previously write-only) and
each inbox message's `created_ms` — so the listing derives activity as the
`max()` of the two, with `#[serde(default)]` and a `created_ms` floor so a
pre-0.5.1 task still parses and still resolves. `basis list` orders *and
ages* by the same clock, so the top continuable row is exactly the one
`--continue` takes, and `--json` carries `last_activity_ms` beside the
unchanged `started_ms`. Reads (`watch`, `wait`, `list`) are not activity, so
a tail left open in another terminal never steals the flag. Writing that
test-first found a fourth candidate, **fixed the same session** (the commit
after `a4257e7`): `scan()` propagated the JSON decode error from one task's
`terminal.json`, so a single half-written record failed the whole of
`basis list`, against that function's own doc comment. The row now degrades
to `"unknown"` — the answer `task_state` already gives a terminal whose
`state` field is not a string — while `wait` and `watch` on the damaged task
stay loud, because asking about one task is a different question from
surveying them all. What remains open here is the two named above: ACP
template expansion, and skills as commands.

**The third upstream wave — mentra 0.20.0.** Taken as one version bump, no
`[patch.crates-io]`; each row says what the fix cost here and what it newly
exposes.

- **mentra#21 is closed and the relay is deleted.** The row above records
  both halves; the cost was a net deletion and the exposure is the full
  classification arriving on the event, of which basis reads the
  side-effect level and nothing else yet.
- **mentra#22 is closed and the `with_tool` closure is retired.** Host tools
  are stored as `Box<dyn ExecutableTool>`: mentra implements the tool traits
  for `Box`/`Arc` (`T: ?Sized`) at the traits' owner, forwarding every method
  explicitly, so the hand-forwarded shim ADR-0016 refused to write never gets
  written anywhere. What it cost: nothing — the closure goes, the public
  signature stands. What it newly exposes: a boxed or shared tool registers
  as itself, which is the shape a per-workspace host-tool surface would need;
  none is built, because no one has asked for one.
- **The wave is a semver event, recorded now for the release that ships
  it.** Removed pub API: `SideEffectLevels`, `ApprovalGate::levels`,
  `PreparedRun::with_side_effect_levels`, `McpError::UnsupportedTransport`;
  added: `McpServer::Http`. The next release is 0.6.0, and `basis-acp`'s
  internal `basis = { version = … }` requirement moves in lockstep when it is
  cut. `#[non_exhaustive]` on `McpServer` and `McpError` is deferred to wave
  1's sweep beside `Event`, where it is one decision instead of three.
- **mentra#20 is closed and Streamable HTTP connects.** The third transport
  arrives as `McpServer::Http` over mentra's `connect_streamable_http`:
  `.mcp.json` takes `type: "http"` (or `"streamable-http"`) with the same
  `${VAR}` expansion as SSE, and an ACP client's `McpServerHttp` translates
  instead of failing `session/new`. `McpError::UnsupportedTransport` is
  deleted with its last two construction sites — pre-1.0, the variant goes
  rather than lingers. Two decisions carry their own comments: a bare `url`
  with no `type` still means SSE, because a file written before the third
  transport existed keeps its meaning; and the ACP workspace digest hashes
  the transport discriminant, without which two servers differing only in
  transport would key one workspace. What it newly exposes: a workspace file
  or a client can now point basis at any HTTP endpoint current MCP servers
  ship. The `{:?}` line holds — Streamable HTTP headers are mentra
  `SecretString`s and redact themselves — and recovery semantics are
  mentra's: a request that died indeterminate is surfaced, never
  auto-retried, and reconnection is one bounded re-handshake.

## 3. Phases

Ordering rule: honesty first (cheap deletions and default flips, so docs stop
describing a shape we've decided against), then structure (crate split, so SDK
work lands in its final home), then the SDK, then bindings.

### Phase A — Posture and pruning (small, mostly deletions) — **landed**

1. ✅ Delete `watch` (`watch.rs`, `watch_cli.rs`, subcommand, docs). Move
   `--deadline` / `--tool-budget` / `--token-budget` onto `basis run` and
   `RunConfig`, defaults unset. — `4fbe1fd`
2. ✅ Extract `fingerprint()` from the watch module; add `basis fingerprint`.
   — `4fbe1fd`
3. ✅ Flip the shell default; retire `--allow-shell` / `BASIS_ALLOW_SHELL`; add
   the disable knob. — `35c9ccb`
4. ✅ Remove the Dockerfile; write `docs/containerization.md`. — `a246722`
5. ✅ CLI grammar: prompt shorthand, `run -`, ACP first-line signpost, exit
   codes. — `35c9ccb`
6. ✅ Update `README.md` (two-mode story, posture) and `ARCHITECTURE.md` §2/§6.

Acceptance: met. The shell recipe in `README.md` runs against the built binary
— `basis fingerprint` prints the hash, a bounded `basis run --json` exits `0`/`1`/`3`
by the contract — and no sentence in `README.md` describes deleted machinery.

### Phase B — Structure — **landed**

1. ✅ Workspace split: `basis`, `basis-acp`, `basis` binary (ADR-0011). Bridge
   stays in the binary, marked extractable. — `fbcacb4`
2. ✅ MCP behind a `mcp` feature in `basis`, default-on in the binary.
   — `a4c259c`
3. ✅ Dissolve `ApprovalPolicy`: `AllowAll` (default) and `DenyAll` in core,
   `TerminalApprover` + `--approve` flag wiring in the binary. Document the
   fail-closed rule on the trait. — `a4c259c`, with the denial reason restored
   in `6192230` once mentra `15fdcfe` gave it somewhere to go.
4. ✅ Update `README.md` (the embedding story on `basis`, the `mcp` feature,
   approval as trait + impls), `ARCHITECTURE.md` §4 (layering and diagram), and
   this ledger.

Acceptance: **met in full, and only as of rev 6.** For two revs this read "met
in substance, with the one clause it cannot literally satisfy named rather than
quietly dropped": `cargo tree -p basis` was free of `agent-client-protocol`
and of `blocking`, but `tokio-tungstenite` was still in there through
mentra-provider's unconditional Responses websocket transport. That was an
upstream gate to ask for rather than a basis defect, and asking for it is what
eventually got it — mentra `c30fa9c` built the feature and basis `27ab4c8` turned
it off here, so the graph is now free of all three (footnote 1). `cargo build
-p basis --examples` compiles both embedder examples against `basis`
alone, and `cargo check -p basis --no-default-features --all-targets` is
clean — the crate really does build with no MCP concept in it.

### Phase C — The SDK (the point of the exercise) — **landed**

1. ✅ `Workspace` / run split: context, skills, templates, MCP connections, and
   provider setup prepared once; runs minted cheaply from it. — `8b52ebf`
2. ✅ `.output::<T>()` over mentra's typed-output path, which grew a
   session-level entry point to carry it. — `07cf4d1`, over mentra `fce664a`;
   its own docs corrected to the shaping-turn contract in `dae4765`
3. ✅ Cancellation token on the run API — both signals, abandon and graceful
   stop, on every entry point. — `07cf4d1`
4. ✅ `BudgetPool` shared across runs. — `e21d632`
5. ✅ Tagged sinks with a fan-in helper. — `07cf4d1`
6. ✅ The two acceptance examples, written against the public API only:
   `basis/examples/watch.rs` (interval + fingerprint + bounded run, and the
   ≲ 20 lines of loop logic the criterion asked for — it is nine) and
   `basis/examples/review_workflow.rs` (fan-out with structured findings,
   shared budget, fan-in verification). — `0ff745c`

Acceptance: met, on the criterion as it was written. Both examples compile,
both were run live against a local OpenAI-compatible gateway, and neither
needed a private door — the surface did not change in `0ff745c`, which is what
"if either fights the surface, the surface is wrong" was there to detect. The
fan-out held the ship on a planted panic and reported 38,662 of 80,000 tokens
spent. A separate run of the same example is the sharper evidence: against a
scratch project with two bugs planted in one file, the correctness reviewer
found both and invented no third, the folded verdict came back `ship: false`
naming both, and the pool closed at 32,088 of 120,000 — precision, not only
detection. The watch ran, skipped at an unchanged fingerprint, ran again after
an edit, then skipped at the *new* fingerprint, which is the whole
baseline-only-after-success policy demonstrated in one loop. `watch.rs` stays
in the tree as a standing acceptance test: if that loop ever stops being
trivial, the regression is in the API.

One thing the examples taught rather than confirmed, and it belongs here
rather than in a footnote alone: a typed turn held exactly one tool, so reading
and shaping were two turns (footnote 12). The first live fan-out is how that
was learned — reviewers submitted empty findings, having read nothing, and the
runs reported success — and basis's own rustdoc had been asking for exactly that
mistake, so `dae4765` corrected three sites to describe what the turn could
actually do. A fact about the surface the acceptance criterion could not have
predicted, found by writing the example and paid for in doc corrections rather
than in API changes. Rev 6 is the second half of that story: the constraint
became a default rather than a law (`OutputSpec::with_tools()`, over mentra
`be65c00`), which is the outcome ADR-0005 is for. The examples still spend two
turns, deliberately — a fan-out wants each reviewer's reading in a context of
its own — so what changed is the ceremony's status, not this example's shape.

### Phase D — Bindings (evidence-gated) — **landed**

1. ✅ Declared subprocess tools: manifest discovery + stdio wrapper over
   `ExecutableTool`. — rev 12, the commit that carries these lines. **Held for
   seven revs, then built against a use case on record**, which is the rule
   working rather than the rule bending: the row in §2 carries the evidence, the
   design it forced, and what it cost. Worth saying here is what the wait bought,
   because a held item that ships unchanged proves nothing. It did not ship
   unchanged. The ADR sketched "a data file declares a tool (name, description,
   JSON schema, command)"; the use case — arguments the model had been
   base64-encoding to get them past a shell — is what made *no shell anywhere on
   the path* the property everything else is arranged around, and what turned
   three fields the sketch does not mention into non-negotiable ones: a
   side-effect level with no read-only spelling, a preview that shows the
   command rather than the name, and a name that cannot be taken from the
   runtime. A version of this built in rev 5 against nobody's problem would have
   had the manifest and none of the three.
2. ✅ Hook/authorizer unification per ADR-0012. — `e81e5d8`. It came out as one
   contract with two bindings rather than as a merge: `hooks::contract` holds
   the types both bindings speak, one `Chain` decides what any answer means, and
   `HookRunner` owns the order — interceptors first in registration order, then
   global hooks, then workspace hooks, on the rule that the further a
   participant is from the workspace's own data, the earlier it speaks. That
   ordering is load-bearing rather than cosmetic: since the first refusal
   short-circuits, it is what lets a host's compiled guard refuse before a
   repository-supplied program is spawned at all. Fail-closed carries over
   unchanged — an erroring or panicking interceptor denies, on its own task so
   a panic cannot take the turn — and the one behavior change is the honest
   consequence of a sync method meeting an async contract (footnote 15). The
   `Approver` seam is deliberately untouched: approval-with-a-person and
   execution-policy-with-rewriting are sibling seams upstream, and merging them
   would trade two honest contracts for one vague one.
3. ◐ Surface mentra `task`/`team_*` deliberately rather than by default.
   **The `task` half is done, by ADR-0016 rather than by this phase.** It is
   hidden from the model's roster along with `shell` and `background_run`, and
   delegation is reached deliberately, as `spawn`'s agent mode, through a tool
   basis wrote and governs. Between rev 6 and that change the reason this row was
   tolerable was accounting: mentra `0436bae` made a delegated turn spend
   against the parent's bounds (footnote 10), and that half still holds on the
   new route — the half that does not is the child's usage reaching the
   parent's *stream*, which is one of ADR-0016's three new candidates in §2.
   Deciding `team_*`'s place is what remains, and it waits on a concrete use
   case like item 1 — but the *default* no longer waits with it: `team_*` and
   `idle` are hidden alongside `task`, so the question is now "surface these?"
   rather than "keep tolerating that they are already surfaced?".

Beyond the three planned items, Phase D shipped what the work turned up:
`WorkspaceBuilder::with_store_dir` and `with_ephemeral_history` with
`store::list_in` as the reading mirror (`397ca13`, `71cc59d`), which closed the
data-directory hole footnote 6 had been carrying since Phase C; `session/list`,
which had never worked in any release (`e81e5d8`, footnote 17); and credential
redaction with a provider resolution that answers to a passed-in lookup rather
than to the ambient shell (`f3529be`, footnote 18). None of the three was
planned; each was found by doing the planned work.

Each Phase D item ships only against a concrete use case, per Bet 7. Gaps
found here are filed as mentra issues even when fixed immediately (ADR-0005).
Item 1 is the only place in this document where that rule was ever *tested* —
the other items all had their evidence up front — and the test is the seven revs
it spent unbuilt, not the rev it shipped in.

Acceptance: met, with item 1 held rather than claimed. `cargo test --workspace`
was 625 passed, 0 failed at the time this was written, and it was that in both
directions — with `BASIS_API_KEY`/`BASIS_BASE_URL` exported and with them scrubbed,
which is the claim `f3529be` makes and the reason the `env -u` ritual is
retired. The data-directory probe is zero: across a full suite run, agent rows
in the machine-wide default database move by zero and no `runtime.sqlite` under
any of basis's four candidate paths changes mtime (footnote 6), and no temp
directory is left behind. `RUSTDOCFLAGS="-D warnings" cargo doc -p basis
--no-deps` is clean, which `f76617d` is the last commit of, and the `basis`
doctests pass under the scrubbed environment. Two hygiene notes belong with
that rather than in the win column: the phase adopted tests-in-their-own-file
at the 800-line ceiling and named the files then over it (footnote 19), and
mentra's `MockRuntime` left 58 stray SQLite files in the temp directory per
suite run until `aa206b7`, which takes that to zero (footnote 20).

### Rev 6 — the upstream wave (no phase) — **landed**

Not a phase and deliberately not numbered as one: nothing in ADR-0010…0015
called for this work. It is the tally in §2 being spent rather than filed. Each
of the five open upstream candidates was closed in mentra and met on basis's
side, and each footnote above now reads as a record of a fixed hole with its
original defect intact.

1. ✅ `RuntimeBuilder` re-exported where downstream code can name it. — mentra
   `c04986a` (footnotes 3, 7)
2. ✅ A run that ends on a bound records which one; basis maps it to
   `Bound::TokenBudget`, `basis run --token-budget` exits `3`, and `run_finished`
   carries `stopped_by`. — mentra `5a2a68e`, basis `8e35f3e`, `a2d170a`
   (footnote 11)
3. ✅ The Responses websocket transport behind `responses-websocket`;
   `cargo tree -p basis` is tungstenite-free and Phase B's last acceptance
   clause is met. — mentra `c30fa9c`, basis `27ab4c8` (footnote 1)
4. ✅ A typed turn can keep its tools, so read-then-shape is a choice rather
   than a constraint. — mentra `be65c00`, basis `b782e75` (footnote 12)
5. ✅ A remembered refusal carries its reason. — mentra `b895ea0` (footnote 5)

Riding along, because the ceiling footnote 19 named was the one piece of
housekeeping nothing else was going to do: `basis-acp/src/server.rs` (`89ccce4`),
`basis/src/main.rs` (`665ced6`) and `basis-acp/tests/acp.rs` (`e37f4f3`) split at
seams they already had, zero behavior change each. Also `ff5fc70`, which
re-exports `basis::async_trait` and closes footnote 16 — basis's own papercut
rather than mentra's, and the only one on this list that needed no upstream
change at all.

Acceptance: the tally in §2 reaches zero open candidates, which is the whole
claim and is checked by reading it. `cargo test --workspace` is 641 passed, 0
failed. Three things are deliberately not claimed. The wave left the 800-line
ceiling **worse** than it found it in net terms — three files brought under,
two pushed over by the typed-turn work in the same series (footnote 19). A
clean tally is a measurement of discipline, not of completeness: three new
candidates were named on the way through and none was built, footnote 8 stays
open, and both facts are in §2 rather than here so the tally and its caveats
stay on one page. And the suite went red once on the way to that 641, in
`basis/tests/hooks.rs`, on a five-second subprocess deadline that the same
target clears in a third of a second when run alone — the third such sighting,
recorded in footnote 20 rather than rerun until green and forgotten.

### Rev 13 — the second upstream wave (no phase) — **landed**

Rev 6's shape again, one size larger, and with the other author in the room:
mentra and basis have the same author, so the list that would once have been
filed as issues went to the mentra session as a handoff and came back the same
day as seventeen commits (`026fbf5..bfe952b`). Every candidate §2's tally was
carrying — the three ADR-0016 put in, the two rev 12's declared tools put in,
the two wiring compaction put in — is closed in mentra and met on basis's side,
and six more found *while* meeting them went up and came back in the same
afternoon. The tally below records them as fixed holes, each with its
original defect intact.

1. ✅ The `chat/completions` wire, and a base URL that speaks it by default —
   mentra `62ac1c4`, basis `b3cfa3e`, `362df0b`, `c62e7ff`, `e35c8f4`.
2. ✅ `keep_recent_tool_results` defaults to *keep everything* upstream, so
   basis's override is a configuration of upstream again rather than a
   correction of it — mentra `7a539be`, basis `8108e24`.
3. ✅ A model carries its context window, the trigger is a share of it, a
   pinned id is looked up too, an overflow compacts and retries once, and
   the estimate is public — mentra `4883ba9`, `2c77792`, `11826ee`, `bfe952b`;
   basis `8108e24`, `b9478b7`, `bd10912`, `e35c8f4`.
4. ✅ A post-execution hook — mentra `145d4ef`, basis `5d685ba`, `cc32938`,
   `4eeac7b`.
5. ✅ `Session::compact` / `set_name` / `reasoning()`, timestamps on the
   summary, `delete_agent`, and compaction events out of turn — mentra
   `ee01c30`, `bfe952b`; basis `1384a3a`, `e17d112`, `b06e9bc`, `9cc10ec`,
   `63ef04a`.
6. ✅ A per-session persist identifier — mentra `ee01c30`; basis `51b5d10`,
   which un-ignores the one test this ledger had been carrying `#[ignore]`d
   since E1.
7. ✅ mentra#23 and mentra#24: a call is validated against its schema before
   authorization, and a name can be claimed without replacing — basis
   `6b37ddb`, `a45b01d`, `5ebd1be`.
8. ✅ A registered tool relays and records its delegations, and announces the
   child it registers — mentra `5f303b8`, `bfe952b`; basis `e22aa63`, `54f3d21`.
9. ✅ `disable-model-invocation` honored, and a body for a host to run —
   mentra `b44f53a`, `bfe952b`; basis `e628beb` (the slash-command wiring is
   deferred, below).
10. ✅ Usage split into reasoning and thoughts, an image-only turn counted,
    the Anthropic cache tail, tail-keeping truncation, an ignore-aware search
    walk, an SSE idle timeout — mentra `1e9a15c`, `3ef731a`, `46756e4`,
    `1996c3b`, `fb36945`, `77d6449`; basis `0293331`.

Acceptance: the tally in §2 reaches zero open *upstream* candidates for the
second time, with mentra#21 — the one it carries a workaround for — still
open and still the only one. `cargo test --workspace` is 1149 passed, 0
failed, 0 ignored, against mentra `bfe952b` through the `[patch.crates-io]`
at the foot of `Cargo.toml`; nothing is publishable until mentra 0.19.0 and
mentra-provider 0.6.0 are cut, and the patch's comment says so. Three things
are deliberately not claimed. The 800-line ceiling is no better: `prepared.rs`
came back under (774) and two hook test files were split, while
`runtime/builder.rs` (1095), its tests (1060), `run.rs` (920), `cli.rs` (870)
and four integration suites sit over it, unchanged (footnote 19). The suite
went red on the known five-second subprocess deadline in `basis/tests/hooks/`
twice on the way here, and passed alone both times (footnote 20). And every
scripted endpoint in the suite is still its own copy — seven of them, patched
identically in `0cbcf5a` when a pinned model started asking for a listing —
which is the day's clearest case for a shared test-support crate, and is
named here rather than built.


### Phase E — The runtime and the files — **landed**

Decided by [ADR-0018](adr/0018-the-runtime-owns-the-process.md) and
[ADR-0019](adr/0019-the-filesystem-is-the-coordination-surface.md) on
2026-08-15; one spec carries both:
[`spec/2026-08-15-runtime-and-filesystem-coordination.md`](spec/2026-08-15-runtime-and-filesystem-coordination.md).
E1 lands before E2, because the files design assumes the runtime owns the
data-directory policy that lets any process resume any agent.

1. **E1 — the `Runtime` split (ADR-0018).** The process-scoped half of
   `Workspace` — mentra's runtime, provider/credential/model policy, store
   policy, host interceptors — moves into `Runtime` + `RuntimeBuilder`;
   `Workspace` keeps repository discovery and borrows through an `Arc`.
   `Workspace::open` survives unchanged as sugar minting a private default
   runtime, so the one-repository host never sees the new noun.
   `workspace.runtime()` becomes `mentra_runtime()` — the breaking rename
   taken while nothing is published. `basis-acp` drops runtime-per-session for
   one `Runtime` per process and one `Workspace` per distinct `cwd`, which is
   the concrete use case the phase ships against.
2. **E2 — files as the coordination surface (ADR-0019).** The per-workspace
   daemon is retired. Task metadata moves to a global workspace-keyed data
   directory beside mentra's store; attach — an `fs2` lock, resume from the
   last committed turn, checkpoint at turn boundaries — replaces the service
   actor; the atomically written terminal record becomes the completion
   signal, so an agent is resumable iff it has none, and a parent may not
   write its record while an attached child lacks one.
   `send`/`cancel`/`watch`/`wait` become file operations with ADR-0017's
   bounds and semantics unchanged. `registry.rs`, `protocol.rs`, the service
   actor, and the wait graph are deleted; unattended execution is documented
   as the OS's job.

What the phase deliberately gives up is part of the decision, not a gap
found later: no progress without an attached process, boundary-granular
cancellation, no effect rollback on a re-driven turn, and a stale-lock
liveness probe as the one racy edge kept. ADR-0019 names each in bold, and the
README's durable-handles section states the first three plainly now that the
work exists. The fourth turned out not to be an edge at all: the implementation
takes advisory `fs2` locks, which the kernel releases at process death, so
there is no stale lock to detect and nothing to break — the ADR budgeted for a
liveness probe it did not need.

E2 landed 2026-08-15, in the commit that carries these lines.
`basis/src/local` went from 4,836 lines across the daemon substrate to 3,337
across eleven modules — 2,453 of implementation and 884 of tests, every file
under the 800-line ceiling — and `basis/tests/local_lifecycle.rs` was rewritten
daemon-free beside a new `basis/tests/attach.rs`. The hidden `__daemon`
subcommand and `BASIS_REGISTRY_DIR` are gone (`__daemon` stays a reserved word
in `shorthand.rs`, so a pre-E2 script is told the subcommand no longer exists
instead of having the word become a prompt). The spec's three open questions
closed as: the `fs2` lock is the sole liveness authority, since the kernel
releases an advisory lock when the holder's handle closes — SIGKILL included —
so a lock is held-by-a-live-process or free and never stale, and the PID
fingerprint written into it is diagnostic only (leases rejected: they would
reintroduce the heartbeat obligation the phase exists to delete); std's file
sharing lets a watcher tail an executor-held `events.jsonl` on every platform,
so no segmented file was needed; a detached root is an ordinary agent
directory with no parent edge. The spec's kill-window criteria run as
`basis/tests/attach.rs` against a loopback-scripted endpoint.

Acceptance: the spec's criteria, among them — `kill -9` mid-turn leaves an
agent with no terminal record that a later attach completes; two concurrent
`send --await` on one agent serialize on the lock; no basis process survives
any completed CLI invocation; `cargo test --workspace` green on all three
platforms with the data-directory probe still zero.

## 4. Explicitly not planned

- A scheduler, a DSL, an embedded scripting language (ADR-0010; proposal
  0001 rejected).
- Per-language client SDKs; the JSONL stream and ACP remain the non-Rust
  surfaces (ADR-0010).
- A shipped container or any claimed sandbox (ADR-0013); the native sandbox
  proposal 0002 stays parked as an optional future layer.
- wasm as a compile target — revisit only with a concrete driver.
- Packages convention (proposal 0003) — unchanged, deferred.

## 5. Risks

- **Posture flip is public-facing.** Shell-on-by-default changes what a
  `basis run` can do on a bare host. It lands with the docs rewrite in the same
  commit series, never separately, so no released state has the new default
  under the old README.
- **The crate split churns every import.** Accepted now, pre-publication,
  because it is the cheapest it will ever be (ADR-0011). *Landed in `fbcacb4`:
  the churn was most of the diff, and the suite stayed green through it.*
- **~~`run_to_output` is unproven in basis's flow.~~** *Retired in `07cf4d1`.*
  basis drives it now, through the session-level entry point mentra grew for it
  (`fce664a`). The spike found what a spike is for: a typed turn held exactly
  one tool, so read-then-shape was two turns (footnote 12), and mentra reports
  "never called the terminal tool" with the same error as a malformed stream
  (footnote 9). Neither cost a surface change; the first cost three doc
  corrections (`dae4765`), which is the cheap way to find that out — and then,
  a rev later, an upstream mode (`be65c00`, `b782e75`) that made the ceremony
  optional. The doc corrections were not wasted: they are still what the
  default mode needs said about it.
- **Token accounting is honest about less than it looks like** — narrower now
  than when this line was written, in two steps, and half a step wider again
  since ADR-0016. `RunUsage`, a `--token-budget` and a `BudgetPool` all count
  what providers *report*, and that caveat is permanent. What is no longer true
  is the rest. All three were blind to what a run delegated through `task`, and
  mentra `0436bae` closed that in both directions, accounting and event stream
  (footnote 10). And a crossed budget used to end a run without saying so,
  which mentra `5a2a68e` and basis `8e35f3e` fixed, so the bound is now reported
  where it is spent (footnote 11). The half step back is that delegation no
  longer runs through `task`: on `spawn`'s route the accounting direction still
  holds and the event-stream direction does not, so a run that delegated is
  bounded correctly and *reports* less than it spent (§2, first of ADR-0016's
  three candidates). The numbers are real; their scope is "what was reported
  for this run, plus everything it delegated where the bound is concerned and
  not where the tally is", and the rustdoc on each is what has to keep saying
  so.
- **Bridge limbo.** Neither core nor extracted; revisit when acp-ui usage is
  real or an upstream home appears.
