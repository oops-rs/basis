# lan — Redesign plan

> rev 7 · 2026-08-12 · The transition from the P0–P4 harness to the SDK-first
> shape decided in [ADR-0010](adr/0010-the-crate-is-the-workflow-surface.md)
> through [ADR-0016](adr/0016-one-delegation-surface.md). This document is the
> honest ledger of that transition: what exists, what is in between, what is not
> started. `README.md` and `ARCHITECTURE.md` describe the *shipped* state and
> are updated per phase as work lands — never ahead of it.
> **Phases A, B, C and D have landed** (rev 5), with one Phase D item — declared
> subprocess tools — deliberately **held** rather than built, because no
> concrete use case exists for it on record and the phase's own rule is that
> its items ship only against one (Bet 7).
> **Rev 6 records a wave that built no phase.** The five upstream candidates
> §2's tally still had open were closed in mentra and met on lan's side, so the
> footnotes below are records of fixed holes rather than of open ones. What the
> wave did not do is make this document shorter: each fix is written with what
> it cost and what it newly exposes, and §2's tally names the candidates the
> work created on its way to closing the old ones.
> **Rev 7 builds ADR-0016**, which was decided after rev 6 and is the first ADR
> here that made lan register a tool of its own. `spawn` is now the model's only
> route to a command and to a subagent. It reopened the tally the day after it
> reached zero — three new upstream candidates, all named in §2 — which is what
> a new surface does when the discipline is working.

## 1. The target in one paragraph

lan is `Workspace` (discover: AGENTS.md, skills, templates, tool manifests) →
runs (execute / converse / resume, bounded, cancellable, with typed output) →
one event stream → two seams (authorize, tools), plus two opt-in adapters
(ACP for interactive clients, MCP as one tool binding) and a five-line CLI
grammar. Orchestration is host-language Rust against the crate — no DSL, no
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
met each on lan's side: `ff5fc70` (the `async-trait` re-export), `27ab4c8` (the
websocket gate closed here too), `8e35f3e` and `a2d170a` (the token budget's
exit code, and the same fact on the stream), `b782e75` (the working typed
turn). Three refactors rode along, splitting the files footnote 19 had named —
`89ccce4` (`lan-acp/src/server.rs`), `665ced6` (`lan/src/main.rs`), `e37f4f3`
(the ACP integration suite).

Rev 7's wave belongs to no phase either, and to an ADR decided after rev 6:
ADR-0016's `spawn`. It landed in two, `74ef59f` (the tool, the roster change,
the depth floor, and the end-to-end suite behind them) and the commit carrying
this revision (the ACP kind map, the auto-mode example, README and this
ledger).

| Piece | Decided in | Status today |
|---|---|---|
| Shell default-on | 0013 | **Built** (`35c9ccb`) — `ShellAccess::Granted` is the default, `--no-shell` disables |
| `--allow-shell` / `LAN_ALLOW_SHELL` retirement | 0013 | **Built** (`35c9ccb`) — both refused with a migration message; the bare-host warning is gone |
| Docker image removal + containerization doc | 0013 | **Built** (`a246722`) — Dockerfile and `.dockerignore` deleted; `docs/containerization.md` written |
| `.git` carve-out kept as hygiene | 0013 | **Built** — no change |
| `watch` deletion | 0014 | **Built** (`4fbe1fd`) — `watch.rs`, `watch/`, `watch_cli.rs`, the subcommand, and its tests are gone |
| Bounds on `RunConfig` / `lan run` | 0014 | **Built** (`4fbe1fd`) — `with_deadline` / `with_tool_budget` / `with_token_budget` and the three `lan run` flags, all defaulting to unset |
| `Workspace::fingerprint()` + `lan fingerprint` | 0014 | **Built** (`4fbe1fd`, `8b52ebf`) — `fingerprint.rs` with ADR-0008's semantics intact, plus the subcommand; the method landed on `Workspace` itself once Phase C had one to put it on, reading the tree as it is now rather than as it was at open |
| Exit-code contract | 0015 | **Built** (`35c9ccb`, `8e35f3e`, `a2d170a`) — 0 ok / 1 failed / 2 usage / 3 bound tripped, and `3` now covers all three bounds rather than two, since a run records which one ended it. `RunReport::stopped_by` carries the distinction in-process and `run_finished` carries it on the stream [11] |
| `lan "<prompt>"` shorthand, `run -`, ACP first-line signpost | 0015 | **Built** (`35c9ccb`) — a positional naming no subcommand is a prompt, `--` escapes, `run -` reads stdin, and a first line that is not JSON-RPC exits with the signpost |
| Crate split (`lan-core` / `lan-acp` / binary) | 0011 | **Built** (`fbcacb4`, `27ab4c8`) — three crates on one version; `agent-client-protocol`, `blocking` and — since the upstream gate exists — `tokio-tungstenite` are all out of `lan-core`'s graph, the bridge stays in the binary marked extractable [1] |
| MCP behind a feature | 0011/0012 | **Built** (`a4c259c`) — `mcp`, default-on; `default-features = false` compiles a `lan-core` with no MCP concept at all [2] |
| Approval enum → trait impls | 0010 | **Built** (`a4c259c`, `6192230`) — `ApprovalPolicy` is gone: `ApprovalGate` authorizes, `AllowAll` / `DenyAll` decide, the terminal approver is the binary's, and `lan_acp::ApprovalMode` holds the protocol's mode list. `--approve` is unchanged [3] [4] [5] |
| `Workspace` / run split | 0010 | **Built** (`8b52ebf`) — `Workspace::open` settles context, credential, model, skills, templates, hooks, MCP connections and the approval gate once; `prepare(RunSpec)` mints a run *synchronously*, which is the honest signal that nothing is left to await. `Workspace::fingerprint()` lands on the type its row above promised it to. The free functions stay, as wrappers over `RunConfig::split` [6] [7] [14] |
| `.output::<T>()` structured output | 0010 | **Built** (`07cf4d1`, over mentra `fce664a`; docs corrected in `dae4765`; the second mode in `b782e75` over mentra `be65c00`) — `PreparedRun::output::<T>()` and `output_with_options`, with `OutputSpec` / `OutputReport` lan's own and the schema the caller's to write. lan asks mentra for the raw `Value` and deserializes itself, which buys `RunError::OutputMismatch`. `OutputSpec::with_tools()` keeps the ordinary toolset on the turn, so read-then-shape is a choice rather than a ceremony [9] [12] |
| `BudgetPool` | 0010 | **Built** (`e21d632`) — the pool *is* mentra's shared `token_usage` counter, so `spent()` is the number the turns are stopped against rather than a tally reconciled later. `RunSpec::with_budget` / `TurnOptions::with_budget` attach one; an exhausted pool refuses the turn with `RunError::BudgetExhausted` before the prompt is sent [10] [11] |
| Tagged sinks / event fan-in | 0010 | **Built** (`07cf4d1`) — `EventFanIn` mints one `TaggedSink` per run and merges them into `MergedEvents`; the tag rides outside `Event`, so the versioned wire schema is untouched [13] |
| Cancellation on the public API | 0010 | **Built** (`07cf4d1`) — `TurnOptions::cancellable()` / `stoppable()` / `with_cancel` / `with_stop`, `execute_with_options` and its neighbours on every entry point, and `CancellationToken` re-exported under the rule the commit writes down: every mentra type lan's surface makes a caller *name*, lan re-exports [8] |
| Recipe + review-workflow examples | 0010/0014 | **Built** (`0ff745c`) — `examples/watch.rs` and `examples/review_workflow.rs`, public-API only, both run live [12] |
| Declared subprocess tools | 0012 | **Not started** — held for a concrete use case (Bet 7) |
| Hooks re-founded as authorizer binding | 0012 | **Built** (`e81e5d8`) — interception is one contract with two bindings: an in-process `Interceptor` trait and subprocess hooks, folded by one `Chain` so first-refusal-wins and composing modifications hold for both by construction. `Approver` stays a *sibling* seam rather than a parent — asking a person and rewriting a call are different questions, and mentra keeps them apart for the same reason [15] [16] |
| Subagents / teams surfaced | 0010 | **Built for delegation, still open for teams.** `task` is no longer in the default reach at all: ADR-0016 hid it with `shell` and `background_run` and made delegation `spawn`'s agent mode, which is the deliberate surfacing this row asked for. The accounting that made "reachable by default" tolerable in the meantime is mentra `0436bae`, and half of it survives the change of route — a delegated run still shares the parent's handle and bounds, but the child's usage no longer reaches the parent's *stream*, because that relay is internal to the `task` intrinsic (new candidate, below). `team_*` is still reachable and still awaiting a concrete use case before lan surfaces it deliberately [10] |
| History location on `WorkspaceBuilder` | 0010 | **Built** (`397ca13`, `71cc59d`) — `with_store_dir(dir)` says where, `with_ephemeral_history()` says nowhere, and `store::list_in` reads back what the first wrote. One private field, so last call wins structurally. Closes the data-directory hole footnote 6 had been recording since Phase C [6] |
| `session/list` over ACP | 0007/0010 | **Built** (`e81e5d8`) — it had never worked: lan filtered on the workspace's runtime identifier while writing every agent under mentra's `"default"`. `WorkspaceBuilder::open` now tags what it persists. Forward-only, deliberately [17] |
| Credentials never printed | — | **Built** (`f3529be`) — `ProviderChoice` and `McpServer`'s stdio env hand-write `Debug`: names kept so a misconfiguration stays fixable, values redacted. Provider resolution reads the environment through a passed-in lookup, so the suite passes identically with `LAN_API_KEY`/`LAN_BASE_URL` set and unset [18] |
| One delegation surface (`spawn`) | 0016 | **Built** (`74ef59f`, with the ACP map, the auto-mode example and these lines in the commit that carries them) — the model's only door to delegation *and* commands. `spawn("!cmd")` is parsed once, at the boundary, into `{mode, body, cwd}`, and that typed triple is what `authorization_preview` presents — so the approver, the rule store, the hooks and the audit trail all dispatch on it and none of them re-reads the string. Both modes are consequential, so neither is waved through under the reads-are-never-asked rule; command mode executes only after the answer. `shell`, `background_run` and `task` left the model's roster via `ToolProfile::hide` while staying registered on the runtime, so ADR-0013's posture is untouched — the route changed, not the availability, and `--no-shell` still refuses at the policy on the path `spawn` calls, verified end to end. The depth guard is lan's own, since mentra's floor is name-specific and does not fire for a registered tool: an agent-id ledger with RAII cleanup, refusing *in the preview* so a remembered allow-rule cannot lift a structural floor (`MAX_DEPTH` 2). The policy ladder is existing machinery tiered — a pattern rule answers first and never reaches the approver, the `Approver` sees only the residue, and a remembered refusal now carries its own reason (mentra `b895ea0`) — and `lan-core/examples/reviewed_shell.rs` walks all three rungs live. Two things about the pattern tier are traps rather than features. mentra globs with `glob-match`, where a single `*` does not cross `/`, and the serialized input carries `cwd`, which is a path: a rule written with one star silently matches nothing, and the operator sees a reviewer they thought they had bypassed rather than an error. And a remembered *answer* is stored bare (`pattern: None`), so `AllowForSession` / `DenyForSession` on `spawn` covers both modes and every body — where an operator could once allow `task` and deny `shell` by name alone, drawing that line now means writing a pattern against the parsed `mode`, which is more expressive and less obvious. **Deviation from the ADR's sketch, deliberately**: no `WorkspaceBuilder::with_tool` and no `ExecutableTool` re-export. mentra's `RuntimeBuilder::with_tool` takes its tool by value and nothing upstream implements the trait for `Box` or `Arc`, so a public registration point would need a hand-forwarded shim — where forgetting `authorization_preview` would present a host's tool to the approver as its static descriptor, the exact failure this ADR exists to remove. `SpawnTool` is public instead; adding the method later is additive. Declared subprocess tools stay held: adjacent binding of the same contract, not this use case |

Footnotes on the Phase B, C and D rows, because a ledger that records only
the wins is not a ledger:

1. **`cargo tree -p lan-core` showed `tokio-tungstenite`, and now does not.**
   The defect as Phase B recorded it: mentra-provider required the crate
   unconditionally for the Responses websocket transport, so there was no
   lan-side gate to close, and Phase B's acceptance named the clause it could
   not satisfy instead of quietly dropping it. mentra `c30fa9c` built the gate
   — `responses-websocket`, default-on at every level so an upgrade takes
   nothing away, with mentra's own provider dependency set to
   `default-features = false` so the forwarding bites — and lan `27ab4c8`
   closed lan's side: the workspace dependency turns the default off and
   `lan-core` re-offers the feature for an embedder who wants it. The Phase B
   acceptance clause is met in full for the first time.
   Two facts are worth keeping, because both are easy to get backwards. First,
   **the reason lan cannot reach that transport is not a missing capability**:
   `openai_definition()` advertises `supports_websockets: true`
   (`mentra-provider/src/responses.rs`). It is that the transport is chosen
   per request through `ProviderRequestOptions.responses.transport`, lan never
   sets that field, and the `AgentConfig` that would carry it is private to
   workspace construction — so the default field value, `HttpSse`, is the only
   one a lan run can have. Off by default is therefore a finding about
   reachability, not a preference, and a build without the feature does not
   silently fall back: selecting the websocket transport answers with a typed
   `UnsupportedCapability` naming the feature to rebuild with. Second, the
   `lan` **binary** still links tungstenite, through its own direct dependency
   for the bridge's websocket server. That is a different subsystem, it is
   what the bridge is, and it stays.
2. The `mcp` feature drops lan's half only. mentra has no `mcp` feature to
   forward — its client is unconditional — so the dependency graph does not
   shrink yet. What the feature delivers is the contract point of ADR-0012:
   one seam, one adapter, droppable at compile time. The day mentra grows a
   feature of its own, `lan-core`'s manifest is where it gets forwarded.
3. **`RuntimeBuilder` could not be named by any downstream code, and now can
   be.** The defect the split surfaced: `RuntimeBuilder` was `pub` inside a
   *private* `mod builder`, re-exported neither by `mentra::runtime` nor at
   the crate root, so `Runtime::builder()` returned a type no caller could
   write. Inference carried a chained build through — `lan_core::run::resolve`
   got by on `let builder = Runtime::builder()`, rebound as it went — but a
   helper *taking or returning* a half-built runtime could not state its
   signature at all. Fixed upstream rather than worked around, per ADR-0005:
   mentra `c04986a` re-exports it where `Runtime` already is, at
   `mentra::runtime::RuntimeBuilder` and the crate root, and pins it from
   outside the crate in `tests/public_api.rs`, where compiling *is* the claim.
   Footnote 7 is the same candidate's second sighting and closes with it.
4. The deny-reason gap was fixed upstream rather than worked around. `a4c259c`
   knowingly lost lan's descriptive denial, because `PermissionDecision`
   carried no reason field; mentra `15fdcfe` added one, and `6192230` restored
   the wording through `ApprovalAnswer`. That ordering is ADR-0005 working as
   written — the gap went upstream and lan waited for it.
5. **A remembered refusal said why only once, and now says why every time.**
   The defect: the first denial carried the approver's reason, but later calls
   never reached the approver — mentra's `RuleStore` answered them from a
   `RememberedRule` that kept the verdict and its scope and no reason, so the
   model read "blocked by remembered session rule". Nothing actionable in it,
   and nothing to stop the model asking again. lan-acp masked the gap:
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
   One consequence for lan, named and **not** acted on: `ModedApprover`'s
   masking is now redundant on the `DenyForSession` path, since the reason
   survives without it. That makes it a simplification candidate, not a
   finished one — the wrapper does other work for the mode list, and nobody
   has checked what else depends on it restating the reason itself.
6. **The suite wrote a real database under the user's data directory** — and
   Phase D closed it end-to-end, so this footnote is now a record of a fixed
   hole rather than an open one. A runtime with no store configured takes
   mentra's default — `~/Library/Application Support/mentra/workspaces/<hash>/runtime.sqlite`
   on macOS, `data_local_dir()` elsewhere (`mentra/src/default_paths.rs`) —
   and lan had been building real `Runtime`s in tests since P1 (`af04f9d`):
   `cargo test -p lan-core --test approval` alone touched that file, no
   `Workspace` involved. Phase C made it ordinary rather than causing it, since
   `Workspace::open` makes "this test drives a real runtime" the default shape.
   Two facts made it worse than it looked. The default path is keyed by the
   *process's* current directory rather than by the workspace lan opened, so
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
   paths a lan test binary could key (`b5a71edc0abf57d2` for the workspace root,
   `e8f5371f626eb964` for `lan-core`, `9e7efd0f1007c4b0` for `lan-acp`,
   `cc9c24177d9e277d` for `lan`). Temp directories left behind per run: zero.
   The rows those databases already hold — 1,046 under `lan-core`'s hash, 260
   under the binary's, 110 under the workspace root's — are the historical
   accumulation, and nothing deletes them; the claim is about what a run adds
   from here, which is nothing.
7. **The second sighting of footnote 3, in the place the split made most
   visible — and it closes with it.** `WorkspaceBuilder::open` folded the
   discovered MCP servers into the builder inline
   (`lan-core/src/workspace/builder.rs`, the
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
   `lan-core/tests/cancellation.rs::a_graceful_stop_after_a_tool_round_keeps_its_work_but_reports_failure`
   and documented on the field itself.
9. **Two different failures share one upstream error.** "run completed without
   invoking the expected terminal tool" and a genuinely malformed provider
   stream are both `RuntimeError::MalformedProviderEvent`
   (`mentra/src/agent/terminal_output.rs`), and lan reports both as
   `RunError::Runtime` rather than matching on error prose to tell them apart.
   `RunError::OutputMismatch` is lan's own precisely because it does not need
   prose: lan asks mentra for a `Value` and deserializes it here, so an answer
   that does not fit `T` is lan's finding and a caller can retry it with a
   clearer schema.
10. **Delegated tokens used to escape every bound lan could set. They no longer
    do.** Phase C found the hole and Phase D's upstream wave closed it, so what
    follows is both halves. The hole: mentra's `task` intrinsic spawned a
    subagent and drove it on `RunOptions::default()` — a fresh, zeroed counter
    and no bound at all — while `RunOptions::child()` sat beside it documenting
    exactly the inheritance that path needed. A delegating run's subagent
    tokens therefore reached neither `RunUsage` nor `TurnOptions::token_budget`
    nor a `BudgetPool`, and lan sets no `tool_profile`, so `task` is available
    to every run by default.
    The fix is mentra `0436bae`, and it closes the gap on both sides.
    Accounting: the parent's in-flight options reach the spawn site through
    `ToolContext::child_run_options`, so the delegated run shares the parent's
    accounting handle and `token_budget` and ends with its cancellation, stop,
    and deadline (`mentra/src/runtime/intrinsic/execute.rs`). Observation: the
    child's `UsageReport` events are relayed onto the parent's bus for the
    duration of the run, so an observer summing lan's event stream gets the
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
    reported one.** The defect, and it shaped two lan decisions for two
    phases: `mentra/src/agent/runner.rs` answered
    `options.token_budget_exceeded()` with a plain `return Ok(())` at the round
    boundary — the transcript kept, the turn over, and nothing typed saying
    *why* it ended. lan cannot report a stop it cannot observe, so `Bound` had
    no `TokenBudget` variant and `--token-budget` could not produce exit `3`.
    That also left a tension standing from Phase A: ADR-0014 calls
    `--token-budget` a bound and ADR-0015 promises "distinct nonzero codes for
    run failure and for a tripped bound", which read together would have exit
    `3` cover all three flags. It covered two, because the third was a bound
    lan could not observe being tripped, and a code invented for it would have
    been a guess dressed as a contract.
    mentra `5a2a68e` supplies the observation. `EarlyEnd` is a write-once slot
    on `RunOptions`, the counterpart of the `token_usage` counter on the same
    struct — same `Arc` sharing, same read-from-a-clone rule (`ended_early()`)
    — and the runner records at both boundary shapes. The combined
    stop-or-budget check is split so that the order which *decides* is also the
    order which *reports*, with stop winning when both held: an instruction the
    caller issued outranks an ambient bound that merely also held. `child()`
    derives a fresh slot, so a delegated run's ending is never read as the
    parent's. lan `8e35f3e` maps it to `Bound::TokenBudget` and consults the
    runner's own record on both finish arms, because the load-bearing case is
    an `Ok`: a run can end on its budget with an answer already committed, and
    nothing else in that result tells "the model was done" from "the allowance
    ran out". `EarlyEnd::StopRequested` maps to nothing on purpose — a caller's
    own stop button does not belong on the same exit code as running out of
    budget. lan `a2d170a` puts the same fact on the stream: `run_finished`
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
    `lan-core/tests/budget.rs::a_zero_token_budget_is_what_refusing_avoids`
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
    `lan-core/tests/output.rs::a_working_turn_out_of_budget_says_so_on_the_stream`.
    A host that drives typed turns and reads only reports will see a
    `RunError::Runtime` with no bound in it.
    **A run that answers *and* is bounded exits `3` printing nothing on
    stderr.** `lan/src/run.rs` announces on stderr only from the
    `RunOutcome::Error` arm, so an `Ok` result that carries a bound exits `3`
    silently. It is unreachable from today's CLI — reaching it needs a queued
    steer sitting behind a committed final message, and lan has no steering
    surface — but it becomes live the day lan grows one, and it is named here
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
    invited it was lan's own — the doctest asked a typed turn to "review the
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
    `TerminalOutputSpec::with_tools()`, and lan `b782e75` surfaced it as
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
    `lan-core/tests/fan_in.rs::a_held_report_holds_its_branch_of_the_stream_open`.
14. `ProviderError` gained `UnattributedCredential` — a key supplied with
    neither a provider nor a base URL to attribute it to is refused rather
    than guessed at — and the enum is not `#[non_exhaustive]`, so that is a
    breaking addition for anyone matching it exhaustively. Accepted
    knowingly: lan is unpublished, and pre-1.0 the crate API is the stated
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
    `lan-core/src/hooks/runner.rs`.
16. **Implementing a lan trait used to cost the host an `async-trait`
    dependency.** `Interceptor` and `Approver` are both `#[async_trait]`, and
    `lan-core` did not re-export the macro, so a host writing either impl added
    `async-trait = "0.1"` to its own manifest to spell an attribute lan's docs
    asked for without saying so. A consistent papercut rather than a defect —
    mentra's own hook trait has the same shape and the reason is the same one
    (a participant that reads a file or takes a lock must not block a runtime
    worker) — but it was a line of someone else's `Cargo.toml`. Closed lan-side
    in `ff5fc70`: `lan_core::async_trait` is re-exported at the crate root under
    the rule already governing `BuiltinProvider` and `ModelSelector` — a name
    lan's surface makes a caller write is a name lan re-exports, and the rule
    reads the same for a macro as for a type. The interceptor doctest and the
    README example spell it `#[lan_core::async_trait]`, which is what pins the
    re-export rather than merely asserting it.
17. **`session/list` had never worked, and the fix is forward-only on purpose.**
    lan filtered listings on the workspace's runtime identifier
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
    exported: the suite passes identically with `LAN_API_KEY`/`LAN_BASE_URL`
    set and unset, and the `env -u` ritual that used to precede every
    invocation is retired.
19. **Tests move to their own file at the 800-line ceiling**, adopted as a
    convention in Phase D rather than declared: `lan-core/src/hooks/runner.rs`
    and `lan-core/src/workspace/builder.rs` both ended `mod tests;` with the
    cases in `runner/tests.rs` and `builder/tests.rs`, which is what kept them
    under the limit while growing. Three files were named as still over it and
    all three have since been split, each at a seam that already existed rather
    than at a line count. `lan-acp/src/server.rs`, 1,089 lines, became 337 plus
    four modules (`89ccce4`): `config.rs` holds `ServeConfig` and the
    `SessionSource` seam, `lifecycle.rs` the handshake and session bookkeeping,
    `turn.rs` the one handler that runs the agent, and the tests their own
    file. `lan/src/main.rs`, 1,073 lines, became 104 plus six (`665ced6`) —
    the grammar stays whole in `cli.rs` because ADR-0015 defines it as a unit,
    and the exit-code contract got `exit.rs` so the whole promise fits on one
    screen. `lan-acp/tests/acp.rs`, 872 lines, was the case the convention's
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
    `b782e75` took `lan-core/tests/output.rs` from 474 lines to **841** and
    `lan-core/src/run/prepared.rs` from 797 to **808**, and
    `lan-core/src/run.rs` sits at exactly **800**. So the score is three files
    brought under and two pushed over in the same series of commits, which is
    the honest shape of a convention that is real but not enforced by anything
    — no lint, no CI gate, only a number in a footnote somebody has to look at.
    Named here on the same rule as before: the ceiling stays a real number
    rather than an aspiration only if the misses are written down as
    faithfully as the hits.
20. **mentra's `MockRuntime` littered, and could collide.** With no store
    configured, `MockRuntime::builder().build()` minted
    `$TMPDIR/mentra-mock-runtime-<nanos>.sqlite` (`mentra/src/test.rs`) and
    nothing removed it: a full `cargo test --workspace` in lan left 58 such
    files behind, measured as a before/after delta against mentra `b1a83de`.
    That is litter rather than a correctness problem. The correctness question
    was the second use of the same clock — the mock's runtime identifier is
    `mock-runtime-<nanos>` from the same `now_nanos()` — so two mocks built
    inside one nanosecond tick would have shared both a store path and a
    runtime identifier, and each would have listed the other's agents. That is
    offered as a *suspected* mechanism and nothing more: a flake in
    `lan-core/tests/hooks.rs` was seen exactly once, has not reproduced since,
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
    `lan-core/tests/hooks.rs::a_hook_is_told_which_schema_it_is_talking_to`
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
so a lan denial can say why it refused. Phase C made a second one, for the
same reason: the typed path wanted a *session*-level entry point so a typed
turn would emit the same events as any other, and mentra grew
`Session::append_turn_to_output` (`fce664a`) rather than lan reaching past the
session to the agent. Phase D made three more, and they are the first that
fixed something *already wrong* upstream rather than adding a door lan needed:
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
footnote 20); a run that ends on a bound now recording which one, so lan can
report a stop it can finally observe (mentra `5a2a68e`, footnote 11);
the Responses websocket transport behind a feature, so `lan-core`'s graph no
longer carries a websocket stack it cannot reach (mentra `c30fa9c`, footnote
1); a typed turn that can keep its tools, so read-then-shape is a choice
(mentra `be65c00`, footnote 12); a `RememberedRule` that carries its refusal's
reason (mentra `b895ea0`, footnote 5); and `RuntimeBuilder` re-exported where
downstream code can name it (mentra `c04986a`, footnotes 3 and 7). **The
ninth was lan's own** rather than mentra's — a store knob on `WorkspaceBuilder`
— and `397ca13` plus `71cc59d` built it (footnote 6). **Zero were open, for one
day.** ADR-0016's first wave then found three more, all upstream-shaped and all
open, named further down.

That is still the first time this ledger has been clean, and it is worth being
precise about what it measures. Not that mentra is finished, and not that lan
found everything: it measures ADR-0005's discipline — that a gap lan hits goes
upstream and lan waits for it, instead of growing a lan-side workaround that
nobody else ever benefits from and that quietly becomes the API. Nine gaps,
nine fixes at the layer that owned them, no workarounds carried. A tally that
only accumulated would have said the discipline was a filing cabinet.

A clean tally is also the moment a ledger is most tempted to stop being one, so
the candidates this wave *created* are named here rather than waiting for
someone to hit them. Three are new, none blocking, none built. On a typed turn
ended by a bound the report is dropped, so the event stream is the sole carrier
of which bound it was — a host reading only reports sees an untyped failure
(footnotes 11 and 12). A run that answers *and* is bounded exits `3` with
nothing on stderr; unreachable from today's CLI, live the day lan grows a
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
discipline finds holes, and all three have the same shape: a door mentra opened
for its own `task` intrinsic and has not opened for a registered tool.

- **A delegated child's usage is bounded but invisible.** Agent mode drives its
  subagent on `ToolContext::child_run_options`, so the spend counts against the
  parent's counter and its bounds — but the relay that puts a child's
  `UsageReport` on the parent's event bus is `pub(crate)`, written for the
  intrinsic, and a host-registered tool cannot reach it. So a run can be stopped
  by a total more than ten times what `RunReport::usage` admits to, which
  `lan-core/tests/spawn.rs::delegated_spend_lands_on_the_budget_that_delegated_it`
  asserts in both directions. It also makes footnote 10's second half — "an
  observer summing lan's event stream gets the same total the accounting handle
  reports" — true of `task` and no longer true of the route lan actually uses.
- **A subagent's events reach no lan stream.** A delegation's inner turns are
  visible only in that agent's own transcript, so a client watching a run sees
  the tool call and its answer and nothing in between. Same root: the event bus
  is per-agent and the bridge between two of them is mentra's to expose. Read
  off a test rather than asserted by one —
  `delegation_stops_at_the_floor` can only find the depth refusal by reading
  the deepest agent's transcript, because it never reaches the parent.
- **Delegation transcript artifacts are unwritable from a host tool.**
  `DelegationArtifact` and `DelegationEdge` are public types, and
  `Agent::record_delegation_request` / `record_delegation_result` — the only
  things that write them — are `pub(crate)`. So the delegation `spawn` performs
  leaves no edge in the transcript where mentra's own would, and this one is an
  absence with nothing to assert against, which is why it is named here rather
  than pinned.

None is blocking and none is built. They go upstream rather than into a
lan-side workaround, which is the whole of the discipline this tally measures.

## 3. Phases

Ordering rule: honesty first (cheap deletions and default flips, so docs stop
describing a shape we've decided against), then structure (crate split, so SDK
work lands in its final home), then the SDK, then bindings.

### Phase A — Posture and pruning (small, mostly deletions) — **landed**

1. ✅ Delete `watch` (`watch.rs`, `watch_cli.rs`, subcommand, docs). Move
   `--deadline` / `--tool-budget` / `--token-budget` onto `lan run` and
   `RunConfig`, defaults unset. — `4fbe1fd`
2. ✅ Extract `fingerprint()` from the watch module; add `lan fingerprint`.
   — `4fbe1fd`
3. ✅ Flip the shell default; retire `--allow-shell` / `LAN_ALLOW_SHELL`; add
   the disable knob. — `35c9ccb`
4. ✅ Remove the Dockerfile; write `docs/containerization.md`. — `a246722`
5. ✅ CLI grammar: prompt shorthand, `run -`, ACP first-line signpost, exit
   codes. — `35c9ccb`
6. ✅ Update `README.md` (two-mode story, posture) and `ARCHITECTURE.md` §2/§6.

Acceptance: met. The shell recipe in `README.md` runs against the built binary
— `lan fingerprint` prints the hash, a bounded `lan run --json` exits `0`/`1`/`3`
by the contract — and no sentence in `README.md` describes deleted machinery.

### Phase B — Structure — **landed**

1. ✅ Workspace split: `lan-core`, `lan-acp`, `lan` binary (ADR-0011). Bridge
   stays in the binary, marked extractable. — `fbcacb4`
2. ✅ MCP behind a `mcp` feature in `lan-core`, default-on in the binary.
   — `a4c259c`
3. ✅ Dissolve `ApprovalPolicy`: `AllowAll` (default) and `DenyAll` in core,
   `TerminalApprover` + `--approve` flag wiring in the binary. Document the
   fail-closed rule on the trait. — `a4c259c`, with the denial reason restored
   in `6192230` once mentra `15fdcfe` gave it somewhere to go.
4. ✅ Update `README.md` (the embedding story on `lan-core`, the `mcp` feature,
   approval as trait + impls), `ARCHITECTURE.md` §4 (layering and diagram), and
   this ledger.

Acceptance: **met in full, and only as of rev 6.** For two revs this read "met
in substance, with the one clause it cannot literally satisfy named rather than
quietly dropped": `cargo tree -p lan-core` was free of `agent-client-protocol`
and of `blocking`, but `tokio-tungstenite` was still in there through
mentra-provider's unconditional Responses websocket transport. That was an
upstream gate to ask for rather than a lan defect, and asking for it is what
eventually got it — mentra `c30fa9c` built the feature and lan `27ab4c8` turned
it off here, so the graph is now free of all three (footnote 1). `cargo build
-p lan-core --examples` compiles both embedder examples against `lan-core`
alone, and `cargo check -p lan-core --no-default-features --all-targets` is
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
   `lan-core/examples/watch.rs` (interval + fingerprint + bounded run, and the
   ≲ 20 lines of loop logic the criterion asked for — it is nine) and
   `lan-core/examples/review_workflow.rs` (fan-out with structured findings,
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
runs reported success — and lan's own rustdoc had been asking for exactly that
mistake, so `dae4765` corrected three sites to describe what the turn could
actually do. A fact about the surface the acceptance criterion could not have
predicted, found by writing the example and paid for in doc corrections rather
than in API changes. Rev 6 is the second half of that story: the constraint
became a default rather than a law (`OutputSpec::with_tools()`, over mentra
`be65c00`), which is the outcome ADR-0005 is for. The examples still spend two
turns, deliberately — a fan-out wants each reviewer's reading in a context of
its own — so what changed is the ceremony's status, not this example's shape.

### Phase D — Bindings (evidence-gated) — **landed, less the item held**

1. ⏸ Declared subprocess tools: manifest discovery + stdio wrapper over
   `ExecutableTool`. **Held, not built.** No concrete use case exists for it on
   record, and the rule below is that a Phase D item ships only against one.
   Building it anyway would be the phase failing its own test.
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
   lan wrote and governs. Between rev 6 and that change the reason this row was
   tolerable was accounting: mentra `0436bae` made a delegated turn spend
   against the parent's bounds (footnote 10), and that half still holds on the
   new route — the half that does not is the child's usage reaching the
   parent's *stream*, which is one of ADR-0016's three new candidates in §2.
   Deciding `team_*`'s place is what remains, and it waits on a concrete use
   case like item 1.

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

Acceptance: met, with item 1 held rather than claimed. `cargo test --workspace`
was 625 passed, 0 failed at the time this was written, and it was that in both
directions — with `LAN_API_KEY`/`LAN_BASE_URL` exported and with them scrubbed,
which is the claim `f3529be` makes and the reason the `env -u` ritual is
retired. The data-directory probe is zero: across a full suite run, agent rows
in the machine-wide default database move by zero and no `runtime.sqlite` under
any of lan's four candidate paths changes mtime (footnote 6), and no temp
directory is left behind. `RUSTDOCFLAGS="-D warnings" cargo doc -p lan-core
--no-deps` is clean, which `f76617d` is the last commit of, and the `lan-core`
doctests pass under the scrubbed environment. Two hygiene notes belong with
that rather than in the win column: the phase adopted tests-in-their-own-file
at the 800-line ceiling and named the files then over it (footnote 19), and
mentra's `MockRuntime` left 58 stray SQLite files in the temp directory per
suite run until `aa206b7`, which takes that to zero (footnote 20).

### Rev 6 — the upstream wave (no phase) — **landed**

Not a phase and deliberately not numbered as one: nothing in ADR-0010…0015
called for this work. It is the tally in §2 being spent rather than filed. Each
of the five open upstream candidates was closed in mentra and met on lan's
side, and each footnote above now reads as a record of a fixed hole with its
original defect intact.

1. ✅ `RuntimeBuilder` re-exported where downstream code can name it. — mentra
   `c04986a` (footnotes 3, 7)
2. ✅ A run that ends on a bound records which one; lan maps it to
   `Bound::TokenBudget`, `lan run --token-budget` exits `3`, and `run_finished`
   carries `stopped_by`. — mentra `5a2a68e`, lan `8e35f3e`, `a2d170a`
   (footnote 11)
3. ✅ The Responses websocket transport behind `responses-websocket`;
   `cargo tree -p lan-core` is tungstenite-free and Phase B's last acceptance
   clause is met. — mentra `c30fa9c`, lan `27ab4c8` (footnote 1)
4. ✅ A typed turn can keep its tools, so read-then-shape is a choice rather
   than a constraint. — mentra `be65c00`, lan `b782e75` (footnote 12)
5. ✅ A remembered refusal carries its reason. — mentra `b895ea0` (footnote 5)

Riding along, because the ceiling footnote 19 named was the one piece of
housekeeping nothing else was going to do: `lan-acp/src/server.rs` (`89ccce4`),
`lan/src/main.rs` (`665ced6`) and `lan-acp/tests/acp.rs` (`e37f4f3`) split at
seams they already had, zero behavior change each. Also `ff5fc70`, which
re-exports `lan_core::async_trait` and closes footnote 16 — lan's own papercut
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
`lan-core/tests/hooks.rs`, on a five-second subprocess deadline that the same
target clears in a third of a second when run alone — the third such sighting,
recorded in footnote 20 rather than rerun until green and forgotten.

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
  `lan run` can do on a bare host. It lands with the docs rewrite in the same
  commit series, never separately, so no released state has the new default
  under the old README.
- **The crate split churns every import.** Accepted now, pre-publication,
  because it is the cheapest it will ever be (ADR-0011). *Landed in `fbcacb4`:
  the churn was most of the diff, and the suite stayed green through it.*
- **~~`run_to_output` is unproven in lan's flow.~~** *Retired in `07cf4d1`.*
  lan drives it now, through the session-level entry point mentra grew for it
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
  which mentra `5a2a68e` and lan `8e35f3e` fixed, so the bound is now reported
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
