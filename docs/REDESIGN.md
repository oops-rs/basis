# lan — Redesign plan

> rev 5 · 2026-08-11 · The transition from the P0–P4 harness to the SDK-first
> shape decided in [ADR-0010](adr/0010-the-crate-is-the-workflow-surface.md)
> through [ADR-0015](adr/0015-cli-grammar.md). This document is the honest
> ledger of that transition: what exists, what is in between, what is not
> started. `README.md` and `ARCHITECTURE.md` describe the *shipped* state and
> are updated per phase as work lands — never ahead of it.
> **Phases A, B, C and D have landed** (rev 5), with one Phase D item — declared
> subprocess tools — deliberately **held** rather than built, because no
> concrete use case exists for it on record and the phase's own rule is that
> its items ship only against one (Bet 7).

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
(`with_ephemeral_history`), `f76617d` (the last rustdoc warning) — over two
mentra fixes, `0436bae` (delegated task accounting) and `b1a83de` (store
recovery deferred to build).

| Piece | Decided in | Status today |
|---|---|---|
| Shell default-on | 0013 | **Built** (`35c9ccb`) — `ShellAccess::Granted` is the default, `--no-shell` disables |
| `--allow-shell` / `LAN_ALLOW_SHELL` retirement | 0013 | **Built** (`35c9ccb`) — both refused with a migration message; the bare-host warning is gone |
| Docker image removal + containerization doc | 0013 | **Built** (`a246722`) — Dockerfile and `.dockerignore` deleted; `docs/containerization.md` written |
| `.git` carve-out kept as hygiene | 0013 | **Built** — no change |
| `watch` deletion | 0014 | **Built** (`4fbe1fd`) — `watch.rs`, `watch/`, `watch_cli.rs`, the subcommand, and its tests are gone |
| Bounds on `RunConfig` / `lan run` | 0014 | **Built** (`4fbe1fd`) — `with_deadline` / `with_tool_budget` / `with_token_budget` and the three `lan run` flags, all defaulting to unset |
| `Workspace::fingerprint()` + `lan fingerprint` | 0014 | **Built** (`4fbe1fd`, `8b52ebf`) — `fingerprint.rs` with ADR-0008's semantics intact, plus the subcommand; the method landed on `Workspace` itself once Phase C had one to put it on, reading the tree as it is now rather than as it was at open |
| Exit-code contract | 0015 | **Built** (`35c9ccb`) — 0 ok / 1 failed / 2 usage / 3 bound tripped, with `RunReport::stopped_by` carrying the same distinction in-process |
| `lan "<prompt>"` shorthand, `run -`, ACP first-line signpost | 0015 | **Built** (`35c9ccb`) — a positional naming no subcommand is a prompt, `--` escapes, `run -` reads stdin, and a first line that is not JSON-RPC exits with the signpost |
| Crate split (`lan-core` / `lan-acp` / binary) | 0011 | **Built** (`fbcacb4`) — three crates on one version; `agent-client-protocol` and `blocking` are out of `lan-core`'s graph, the bridge stays in the binary marked extractable [1] |
| MCP behind a feature | 0011/0012 | **Built** (`a4c259c`) — `mcp`, default-on; `default-features = false` compiles a `lan-core` with no MCP concept at all [2] |
| Approval enum → trait impls | 0010 | **Built** (`a4c259c`, `6192230`) — `ApprovalPolicy` is gone: `ApprovalGate` authorizes, `AllowAll` / `DenyAll` decide, the terminal approver is the binary's, and `lan_acp::ApprovalMode` holds the protocol's mode list. `--approve` is unchanged [3] [4] [5] |
| `Workspace` / run split | 0010 | **Built** (`8b52ebf`) — `Workspace::open` settles context, credential, model, skills, templates, hooks, MCP connections and the approval gate once; `prepare(RunSpec)` mints a run *synchronously*, which is the honest signal that nothing is left to await. `Workspace::fingerprint()` lands on the type its row above promised it to. The free functions stay, as wrappers over `RunConfig::split` [6] [7] [14] |
| `.output::<T>()` structured output | 0010 | **Built** (`07cf4d1`, over mentra `fce664a`; docs corrected in `dae4765`) — `PreparedRun::output::<T>()` and `output_with_options`, with `OutputSpec` / `OutputReport` lan's own and the schema the caller's to write. lan asks mentra for the raw `Value` and deserializes itself, which buys `RunError::OutputMismatch` [9] [12] |
| `BudgetPool` | 0010 | **Built** (`e21d632`) — the pool *is* mentra's shared `token_usage` counter, so `spent()` is the number the turns are stopped against rather than a tally reconciled later. `RunSpec::with_budget` / `TurnOptions::with_budget` attach one; an exhausted pool refuses the turn with `RunError::BudgetExhausted` before the prompt is sent [10] [11] |
| Tagged sinks / event fan-in | 0010 | **Built** (`07cf4d1`) — `EventFanIn` mints one `TaggedSink` per run and merges them into `MergedEvents`; the tag rides outside `Event`, so the versioned wire schema is untouched [13] |
| Cancellation on the public API | 0010 | **Built** (`07cf4d1`) — `TurnOptions::cancellable()` / `stoppable()` / `with_cancel` / `with_stop`, `execute_with_options` and its neighbours on every entry point, and `CancellationToken` re-exported under the rule the commit writes down: every mentra type lan's surface makes a caller *name*, lan re-exports [8] |
| Recipe + review-workflow examples | 0010/0014 | **Built** (`0ff745c`) — `examples/watch.rs` and `examples/review_workflow.rs`, public-API only, both run live [12] |
| Declared subprocess tools | 0012 | **Not started** — held for a concrete use case (Bet 7) |
| Hooks re-founded as authorizer binding | 0012 | **Built** (`e81e5d8`) — interception is one contract with two bindings: an in-process `Interceptor` trait and subprocess hooks, folded by one `Chain` so first-refusal-wins and composing modifications hold for both by construction. `Approver` stays a *sibling* seam rather than a parent — asking a person and rewriting a call are different questions, and mentra keeps them apart for the same reason [15] [16] |
| Subagents / teams surfaced | 0010 | **In between**, and for a smaller reason than before — `task` stays in the default reach and is now *accounted*: mentra `0436bae` runs the delegated subagent on the parent's `RunOptions::child()` and relays its usage, so delegated tokens reach both the parent's bounds and its stream. `team_*` is still reachable and still awaiting a concrete use case before lan surfaces it deliberately [10] |
| History location on `WorkspaceBuilder` | 0010 | **Built** (`397ca13`, `71cc59d`) — `with_store_dir(dir)` says where, `with_ephemeral_history()` says nowhere, and `store::list_in` reads back what the first wrote. One private field, so last call wins structurally. Closes the data-directory hole footnote 6 had been recording since Phase C [6] |
| `session/list` over ACP | 0007/0010 | **Built** (`e81e5d8`) — it had never worked: lan filtered on the workspace's runtime identifier while writing every agent under mentra's `"default"`. `WorkspaceBuilder::open` now tags what it persists. Forward-only, deliberately [17] |
| Credentials never printed | — | **Built** (`f3529be`) — `ProviderChoice` and `McpServer`'s stdio env hand-write `Debug`: names kept so a misconfiguration stays fixable, values redacted. Provider resolution reads the environment through a passed-in lookup, so the suite passes identically with `LAN_API_KEY`/`LAN_BASE_URL` set and unset [18] |

Footnotes on the Phase B, C and D rows, because a ledger that records only
the wins is not a ledger:

1. `cargo tree -p lan-core` still shows `tokio-tungstenite`. It arrives
   through mentra-provider, which requires it unconditionally for the
   Responses websocket transport, so there is no lan-side gate to close. It is
   an upstream feature-gate candidate under ADR-0005 rather than something to
   paper over here, and Phase B's acceptance names it instead of quietly
   dropping the clause.
2. The `mcp` feature drops lan's half only. mentra has no `mcp` feature to
   forward — its client is unconditional — so the dependency graph does not
   shrink yet. What the feature delivers is the contract point of ADR-0012:
   one seam, one adapter, droppable at compile time. The day mentra grows a
   feature of its own, `lan-core`'s manifest is where it gets forwarded.
3. A mentra papercut the split surfaced: `RuntimeBuilder` is `pub` inside a
   *private* `mod builder`, re-exported neither by `mentra::runtime` nor at
   the crate root, so no downstream code can write its type at all.
   `lan_core::run::resolve` gets by on inference — `let builder =
   Runtime::builder()`, rebound as it goes — but a helper taking or returning
   one cannot be written. An upstream re-export is the fix; an issue under
   ADR-0005.
4. The deny-reason gap was fixed upstream rather than worked around. `a4c259c`
   knowingly lost lan's descriptive denial, because `PermissionDecision`
   carried no reason field; mentra `15fdcfe` added one, and `6192230` restored
   the wording through `ApprovalAnswer`. That ordering is ADR-0005 working as
   written — the gap went upstream and lan waited for it.
5. A remembered refusal says why only once. The first denial carries the
   approver's reason; later calls never reach the approver — mentra's
   `RuleStore` answers them from a `RememberedRule` that keeps the verdict
   and its scope but no reason, so the model reads "blocked by remembered
   session rule". lan-acp masks it: `ModedApprover` remembers the "…for this
   session" answers itself and restates the reason each time, so the gap
   shows only for an embedding host whose `Approver` returns
   `DenyForSession` — the one path that reaches `deny_and_remember`.
   Threading a reason through `RememberedRule` was wider than `15fdcfe`
   needed to be; deliberately left as the third upstream candidate under
   ADR-0005.
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
7. `RuntimeBuilder`'s privacy (footnote 3) bit again, in the place the split
   made most visible. `WorkspaceBuilder::open` folds the discovered MCP
   servers into the builder inline (`lan-core/src/workspace/builder.rs`,
   the `servers.into_iter().fold(builder, …)` block) because a helper taking
   or returning a half-built `RuntimeBuilder` still cannot name its type —
   `mod builder` is private in `mentra/src/runtime.rs` and the type is
   imported there privately, never re-exported. Same candidate, second
   sighting.
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
11. **A crossed token budget is a silent success upstream**, which is why two
    lan decisions look the way they do. `mentra/src/agent/runner.rs` answers
    `options.token_budget_exceeded()` with a plain `return Ok(())` at the
    round boundary: the transcript is kept and the turn is over, but nothing
    typed says *why* it ended. lan cannot report a stop it cannot observe, so
    `Bound` has no `TokenBudget` variant and `--token-budget` cannot produce
    exit `3` — the exit-code table in `README.md` says so, and this footnote is
    the reason behind it rather than a preference. It also settles a tension
    Phase A left standing: ADR-0014 calls `--token-budget` a bound and ADR-0015
    promises "distinct nonzero codes for run failure and for a tripped bound",
    which read together would have exit `3` cover all three flags. It covers
    two, because the third is a bound lan cannot observe being tripped, and a
    code invented for it would be a guess dressed as a contract.
    The same softness makes `token_budget: Some(0)` a trap: mentra compares
    `reported >= budget`, so a zero budget is already crossed before the first
    round, the run does nothing, and the missing final message surfaces as
    `EmptyAssistantResponse` — a provider-shaped error for an accounting
    decision, with the prompt already committed to the transcript. Pinned by
    `lan-core/tests/budget.rs::a_zero_token_budget_is_what_refusing_avoids`,
    and the reason lan refuses with `BudgetExhausted` before the turn instead
    of passing the zero through.
12. **A typed turn is a shaping turn, not a working one.** While a run answers
    into a schema it holds exactly one tool: registering the generated terminal
    tool opens a gate on the agent (`mentra/src/agent/terminal_output.rs`), and
    while that gate is open `tools()` filters the whole toolset down to that
    one tool and `tool_choice()` forces it (`mentra/src/agent.rs`). It cannot
    read a file, run a command, or reach an MCP server on that turn — so asking
    a reviewer for findings in one `output` call returns an empty list from a
    model that opened nothing, *and returns it as a success*. That is not
    hypothetical: the first live fan-out did exactly that, and the wording that
    invited it was lan's own — the doctest asked a typed turn to "review the
    diff on this branch", and the guidance on `OutputSpec::description` held up
    "call this once you have reviewed every file" as the description to
    imitate. `dae4765` corrected three doc sites to say what the turn can
    actually do. Read-then-shape is two turns, which is what
    `examples/review_workflow.rs` documents at length and what every live run
    since has exercised. The mechanism is upstream's stated contract rather
    than an oversight — `run_to_output`'s own rustdoc says it "exposes only one
    forced terminal tool during this run" — so nothing here is a defect. It is
    still an ADR-0005 candidate, on ergonomics rather than truth: a mode that
    kept the ordinary toolset alongside the terminal tool would remove the
    two-turn ceremony, and a contract that needed three doc corrections in one
    commit to state plainly is one worth making harder to get wrong.
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
16. **Implementing a lan trait costs the host an `async-trait` dependency.**
    `Interceptor` and `Approver` are both `#[async_trait]`, and `lan-core` does
    not re-export the macro, so a host writing either impl adds
    `async-trait = "0.1"` to its own manifest to spell the attribute. A
    consistent papercut rather than a defect — mentra's own hook trait has the
    same shape and the reason is the same one (a participant that reads a file
    or takes a lock must not block a runtime worker) — but it is a line of
    someone else's `Cargo.toml` that lan's docs ask for without saying so.
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
    convention this phase rather than declared: `lan-core/src/hooks/runner.rs`
    and `lan-core/src/workspace/builder.rs` both ended `mod tests;` with the
    cases in `runner/tests.rs` and `builder/tests.rs`, which is what kept them
    under the limit while growing. Two pre-existing files are still over it and
    are not pretending otherwise — `lan-acp/src/server.rs` at 1,089 lines and
    `lan/src/main.rs` at 1,073. Neither was touched this phase; both are named
    here so the ceiling stays a real number rather than an aspiration.
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
    SQLite path kept as an explicit `with_store` — landed as mentra `aa206b7`,
    and with it the same before/after delta is zero.

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
`0436bae`, `b1a83de`, and `aa206b7`.

What Phase C mostly discovered, though, is where the honest edges are — and
Phase D is where two of them stopped being edges. The running tally, because a
list of "candidates" that only grows says nothing about whether the ADR-0005
discipline works. **Nine named across Phases B–D. Three fixed upstream this
phase**: `task`-delegated accounting, which now shares the parent's handle and
relays its usage (mentra `0436bae`, footnote 10); the eager default-store
open, which now waits for the store the builder ends with (mentra `b1a83de`,
footnote 6); and `MockRuntime` defaulting to the volatile store, which took
the temp litter and a possible identifier collision with it (mentra `aa206b7`,
footnote 20). **Five still open**, none of them blocking: a crossed token
budget returns an untyped `Ok`, so lan cannot report a stop it cannot observe
(footnote 11); `RuntimeBuilder` is public inside a private module, so no
downstream code can name it (footnotes 3 and 7); a `RememberedRule` keeps a
verdict without its reason (footnote 5); `tokio-tungstenite` is unconditional
in mentra-provider, so there is no lan-side gate to close (footnote 1); and a
typed turn holds only its terminal tool, which is upstream's documented
contract and still costs every workflow a turn of reading before it can shape
(footnote 12). One edge on that Phase C list was lan's own rather than
mentra's — a store knob on `WorkspaceBuilder` — and `397ca13` plus `71cc59d`
built it (footnote 6).

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

Acceptance: met in substance, with the one clause it cannot literally satisfy
named rather than quietly dropped. `cargo tree -p lan-core` is free of
`agent-client-protocol` and of `blocking`; `tokio-tungstenite` is still in
there, reached through mentra-provider's unconditional Responses websocket
transport, which is an upstream gate to ask for and not a lan defect
(footnote 1). `cargo build -p lan-core --examples` compiles both embedder
examples against `lan-core` alone, and
`cargo check -p lan-core --no-default-features --all-targets` is clean — the
crate really does build with no MCP concept in it.

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
rather than in a footnote alone: a typed turn holds exactly one tool, so
reading and shaping are two turns (footnote 12). The first live fan-out is how
that was learned — reviewers submitted empty findings, having read nothing, and
the runs reported success — and lan's own rustdoc had been asking for exactly
that mistake, so `dae4765` corrected three sites to describe what the turn can
actually do. A fact about the surface the acceptance criterion could not have
predicted, found by writing the example and paid for in doc corrections rather
than in API changes.

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
   **Half done, and the half that was urgent.** `task` is still in the default
   reach — but the reason that mattered is gone: mentra `0436bae` makes a
   delegated turn spend against the parent's bounds and report onto its stream
   (footnote 10), so "reachable by default" no longer means "spends money
   nobody can see". Deciding `team_*`'s place is what remains, and it waits on
   a concrete use case like item 1.

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
is 625 passed, 0 failed, and it is that in both directions — with
`LAN_API_KEY`/`LAN_BASE_URL` exported and with them scrubbed, which is the
claim `f3529be` makes and the reason the `env -u` ritual is retired. The
data-directory probe is zero: across a full suite run, agent rows in the
machine-wide default database move by zero and no `runtime.sqlite` under any of
lan's four candidate paths changes mtime (footnote 6), and no temp directory is
left behind. `RUSTDOCFLAGS="-D warnings" cargo doc -p lan-core --no-deps` is
clean, which `f76617d` is the last commit of, and the ten `lan-core` doctests
pass under the scrubbed environment. Two hygiene notes belong with that rather
than in the win column: the phase adopted tests-in-their-own-file at the
800-line ceiling and named the two files still over it (footnote 19), and
mentra's `MockRuntime` left 58 stray SQLite files in the temp directory per
suite run until the fix now in flight, which takes that to zero (footnote 20).

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
  (`fce664a`). The spike found what a spike is for: a typed turn holds exactly
  one tool, so read-then-shape is two turns (footnote 12), and mentra reports
  "never called the terminal tool" with the same error as a malformed stream
  (footnote 9). Neither cost a surface change; the first cost three doc
  corrections (`dae4765`), which is the cheap way to find that out.
- **Token accounting is honest about less than it looks like** — narrower now
  than when this line was written. `RunUsage`, a `--token-budget` and a
  `BudgetPool` all count what providers *report*, and that caveat is
  permanent. What is no longer true is the second half: all three were blind to
  what a run delegated through `task`, and mentra `0436bae` closed that in both
  directions, accounting and event stream (footnote 10). The numbers are real;
  their scope is "what was reported for this run and everything it delegated",
  which is most of the way to "what this job cost", and the rustdoc on each says
  so.
- **Bridge limbo.** Neither core nor extracted; revisit when acp-ui usage is
  real or an upstream home appears.
