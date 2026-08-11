# lan — Redesign plan

> rev 4 · 2026-08-11 · The transition from the P0–P4 harness to the SDK-first
> shape decided in [ADR-0010](adr/0010-the-crate-is-the-workflow-surface.md)
> through [ADR-0015](adr/0015-cli-grammar.md). This document is the honest
> ledger of that transition: what exists, what is in between, what is not
> started. `README.md` and `ARCHITECTURE.md` describe the *shipped* state and
> are updated per phase as work lands — never ahead of it.
> **Phases A, B and C have landed** (rev 4); D is open.

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
`BudgetPool`), `0ff745c` (the two acceptance examples).

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
| Declared subprocess tools | 0012 | **Not started** |
| Hooks re-founded as authorizer binding | 0012 | **In between** — hooks work; unification with the `Approver`/`ToolAuthorizer` seam is structural, not behavioral |
| Subagents / teams surfaced | 0010 | **In between** — mentra ships `task` + `team_*` and lan sets no tool profile, so `task` is already reachable from every run. What Phase C added is the reason the decision matters: a delegated turn's tokens reach neither `RunUsage` nor any bound [10] |

Footnotes on the Phase B and Phase C rows, because a ledger that records only
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
6. **The suite writes a real database under the user's data directory**, and
   Phase C is where that finally got written down rather than where it
   started. A runtime with no store configured takes mentra's default —
   `~/Library/Application Support/mentra/workspaces/<hash>/runtime.sqlite` on
   macOS, `data_local_dir()` elsewhere (`mentra/src/default_paths.rs`) — and
   lan has been building real `Runtime`s in tests since P1 (`af04f9d`):
   `cargo test -p lan-core --test approval` alone touches that file, no
   `Workspace` involved. What Phase C changed is how ordinary it is, since
   `Workspace::open` makes "this test drives a real runtime" the default shape.
   Two things follow that are worth knowing before someone rediscovers them.
   The knob is *not* missing upstream — `RuntimeBuilder::with_store` takes any
   `RuntimeStore` and `SqliteRuntimeStore::new` is public — so this is lan's
   gap and not mentra's: `WorkspaceBuilder` exposes no `with_store`, and
   adding one is a lan-side builder method, not an ADR-0005 candidate. And the
   default path is keyed by the *process's* current directory rather than by
   the workspace path lan opened, so every test binary in one `cargo test`
   shares one file whatever temp directory it opened — verified by running two
   test binaries and watching one `runtime.sqlite` change under both.
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
10. **Delegated tokens escape every bound lan can set.** mentra's `task`
    intrinsic spawns a subagent and drives it with `child.send(…)`
    (`mentra/src/runtime/intrinsic/execute.rs`), and `send` is
    `run(content, RunOptions::default())` (`mentra/src/agent/lifecycle.rs`) —
    a fresh, zeroed counter and no bound at all. So a delegating run's
    subagent tokens reach neither `RunUsage` nor `TurnOptions::token_budget`
    nor a `BudgetPool`. `RunOptions::child()` exists and documents itself as
    the way to share exactly that accounting, and mentra's own path does not
    use it; lan sets no `tool_profile`, so `task` is available to every run by
    default. The clearest upstream candidate Phase C found (ADR-0005) — lan
    cannot infer a subagent's spending from outside. One live observation
    belongs on the same pile without being a claim about either crate: on the
    gateway the acceptance runs used, one model failed *every* reading turn
    with `malformed provider event: assistant turn ended before MessageStopped`
    where two others on the same gateway did the same work, and what set it
    apart is that it reached for `task` where they read files directly. One
    gateway, one model, no reduction — recorded because the delegating path is
    already where the unknowns are.
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

Discoveries that shrank the plan: structured output and per-run bounds +
cancellation were assumed to be new mentra work; both already exist upstream
(`agent/terminal_output.rs`; `RunOptions`). Phase B corrected the other half
of this paragraph — a mentra change *was* required and was made rather than
worked around: `PermissionDecision` gained a reason field (mentra `15fdcfe`)
so a lan denial can say why it refused. Phase C made a second one, for the
same reason: the typed path wanted a *session*-level entry point so a typed
turn would emit the same events as any other, and mentra grew
`Session::append_turn_to_output` (`fce664a`) rather than lan reaching past the
session to the agent.

What Phase C mostly discovered, though, is where the honest edges are — and
they are cheaper to name than to close. Three candidates join ADR-0005's list.
Two are about accounting: a `task`-delegated subagent runs on default options,
so its tokens are invisible to every bound lan can set (footnote 10), and a
crossed token budget returns an untyped `Ok`, so lan cannot report a stop it
cannot observe (footnote 11). The third is about ergonomics rather than truth:
a typed turn holds only its terminal tool, which is upstream's documented
contract and still costs every workflow a turn of reading before it can shape
(footnote 12). One more edge is lan's own to fix rather than mentra's — a store
knob on `WorkspaceBuilder` (footnote 6). Together with Phase B's three, that is
six upstream candidates named and none of them blocking.

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

### Phase D — Bindings (evidence-gated)

1. Declared subprocess tools: manifest discovery + stdio wrapper over
   `ExecutableTool`.
2. Hook/authorizer unification per ADR-0012 — structural refactor; behavior
   (fail-closed, allow/deny/modify) unchanged.
3. Surface mentra `task`/`team_*` deliberately rather than by default; agent
   definitions as workspace data if demanded. Phase C sharpened this one: lan
   sets no tool profile, so `task` is already reachable, and a delegated turn
   spends tokens no bound of lan's can see (footnote 10). "Decide their place"
   now means deciding that too.

Each Phase D item ships only against a concrete use case, per Bet 7. Gaps
found here are filed as mentra issues even when fixed immediately (ADR-0005).

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
- **Token accounting is honest about less than it looks like.** `RunUsage`, a
  `--token-budget` and a `BudgetPool` all count what providers *report*, and
  all three are blind to what a run delegates through `task` (footnote 10).
  The numbers are real; their scope is narrower than "what this job cost", and
  the rustdoc on each says so rather than the docs implying otherwise.
- **Bridge limbo.** Neither core nor extracted; revisit when acp-ui usage is
  real or an upstream home appears.
