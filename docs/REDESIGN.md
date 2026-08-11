# lan — Redesign plan

> rev 3 · 2026-08-11 · The transition from the P0–P4 harness to the SDK-first
> shape decided in [ADR-0010](adr/0010-the-crate-is-the-workflow-surface.md)
> through [ADR-0015](adr/0015-cli-grammar.md). This document is the honest
> ledger of that transition: what exists, what is in between, what is not
> started. `README.md` and `ARCHITECTURE.md` describe the *shipped* state and
> are updated per phase as work lands — never ahead of it.
> **Phases A and B have landed** (rev 3); C and D are open.

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
its reason again, over mentra `15fdcfe`) — and its rows carry those.

| Piece | Decided in | Status today |
|---|---|---|
| Shell default-on | 0013 | **Built** (`35c9ccb`) — `ShellAccess::Granted` is the default, `--no-shell` disables |
| `--allow-shell` / `LAN_ALLOW_SHELL` retirement | 0013 | **Built** (`35c9ccb`) — both refused with a migration message; the bare-host warning is gone |
| Docker image removal + containerization doc | 0013 | **Built** (`a246722`) — Dockerfile and `.dockerignore` deleted; `docs/containerization.md` written |
| `.git` carve-out kept as hygiene | 0013 | **Built** — no change |
| `watch` deletion | 0014 | **Built** (`4fbe1fd`) — `watch.rs`, `watch/`, `watch_cli.rs`, the subcommand, and its tests are gone |
| Bounds on `RunConfig` / `lan run` | 0014 | **Built** (`4fbe1fd`) — `with_deadline` / `with_tool_budget` / `with_token_budget` and the three `lan run` flags, all defaulting to unset |
| `Workspace::fingerprint()` + `lan fingerprint` | 0014 | **Built** (`4fbe1fd`) — `fingerprint.rs` with ADR-0008's semantics intact, plus the subcommand. Named `Workspace::fingerprint()` once Phase C mints a `Workspace` |
| Exit-code contract | 0015 | **Built** (`35c9ccb`) — 0 ok / 1 failed / 2 usage / 3 bound tripped, with `RunReport::stopped_by` carrying the same distinction in-process |
| `lan "<prompt>"` shorthand, `run -`, ACP first-line signpost | 0015 | **Built** (`35c9ccb`) — a positional naming no subcommand is a prompt, `--` escapes, `run -` reads stdin, and a first line that is not JSON-RPC exits with the signpost |
| Crate split (`lan-core` / `lan-acp` / binary) | 0011 | **Built** (`fbcacb4`) — three crates on one version; `agent-client-protocol` and `blocking` are out of `lan-core`'s graph, the bridge stays in the binary marked extractable [1] |
| MCP behind a feature | 0011/0012 | **Built** (`a4c259c`) — `mcp`, default-on; `default-features = false` compiles a `lan-core` with no MCP concept at all [2] |
| Approval enum → trait impls | 0010 | **Built** (`a4c259c`, `6192230`) — `ApprovalPolicy` is gone: `ApprovalGate` authorizes, `AllowAll` / `DenyAll` decide, the terminal approver is the binary's, and `lan_acp::ApprovalMode` holds the protocol's mode list. `--approve` is unchanged [3] [4] |
| `Workspace` / run split | 0010 | **Not started** — single `RunConfig`, `prepare()` re-discovers per run |
| `.output::<T>()` structured output | 0010 | **In between** — mentra ships `Agent::run_to_output` + `TerminalOutputSpec`; lan does not surface it |
| `BudgetPool` | 0010 | **Not started** (per-run bounds exist upstream; the shared pool does not) |
| Tagged sinks / event fan-in | 0010 | **Not started** |
| Cancellation on the public API | 0010 | **In between** — token exists in mentra options; not exposed on `RunConfig`/`PreparedRun` |
| Declared subprocess tools | 0012 | **Not started** |
| Hooks re-founded as authorizer binding | 0012 | **In between** — hooks work; unification with the `Approver`/`ToolAuthorizer` seam is structural, not behavioral |
| Subagents / teams surfaced | 0010 | **In between** — mentra ships `task` + `team_*`; decide their place in lan's default tool profile |
| Recipe + review-workflow examples | 0010/0014 | **Not started** — `lan-core/examples/embed.rs`, `conversation.rs` exist; the two acceptance examples do not |

Footnotes on the Phase B rows, because a ledger that records only the wins is
not a ledger:

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
   approver's reason; later calls are answered by mentra's `RuleStore`, whose
   "blocked by remembered session rule" has no reason field of its own.
   lan-acp masks it — `ModedApprover` remembers session answers itself — so
   it shows only for a host calling `deny_and_remember` directly. Threading a
   reason through `RememberedRule` was wider than `15fdcfe` needed to be;
   deliberately left as the third upstream candidate under ADR-0005.

Discoveries that shrank the plan: structured output and per-run bounds +
cancellation were assumed to be new mentra work; both already exist upstream
(`agent/terminal_output.rs`; `RunOptions`). Phase B corrected the other half
of this paragraph — a mentra change *was* required and was made rather than
worked around: `PermissionDecision` gained a reason field (mentra `15fdcfe`)
so a lan denial can say why it refused. Three further upstream candidates are
named in the footnotes above — tungstenite's unconditional transport,
`RuntimeBuilder`'s privacy, and `RememberedRule`'s reasonless denials — and
the co-evolution discipline (ADR-0005) applies to them as it does to anything
Phase C/D uncovers.

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

### Phase C — The SDK (the point of the exercise)

1. `Workspace` / run split: context, skills, templates, MCP connections, and
   provider setup prepared once; runs minted cheaply from it.
2. `.output::<T>()` over mentra's `run_to_output`.
3. Cancellation token on the run API.
4. `BudgetPool` shared across runs.
5. Tagged sinks with a fan-in helper.
6. The two acceptance examples, written against the public API only:
   `lan-core/examples/watch.rs` (interval + fingerprint + bounded run, ≲ 20
   lines of logic) and `lan-core/examples/review_workflow.rs` (fan-out with
   structured findings, shared budget, fan-in verification).

Acceptance: both examples compile and run; neither needs a private API. If
either fights the surface, the surface — not the example — is wrong.

### Phase D — Bindings (evidence-gated)

1. Declared subprocess tools: manifest discovery + stdio wrapper over
   `ExecutableTool`.
2. Hook/authorizer unification per ADR-0012 — structural refactor; behavior
   (fail-closed, allow/deny/modify) unchanged.
3. Surface mentra `task`/`team_*` in the default profile; agent definitions
   as workspace data if demanded.

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
- **`run_to_output` is unproven in lan's flow.** It exists upstream but lan
  has never driven it; Phase C item 2 starts with a spike, and friction goes
  upstream per ADR-0005.
- **Bridge limbo.** Neither core nor extracted; revisit when acp-ui usage is
  real or an upstream home appears.
