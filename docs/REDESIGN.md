# lan — Redesign plan

> rev 2 · 2026-08-11 · The transition from the P0–P4 harness to the SDK-first
> shape decided in [ADR-0010](adr/0010-the-crate-is-the-workflow-surface.md)
> through [ADR-0015](adr/0015-cli-grammar.md). This document is the honest
> ledger of that transition: what exists, what is in between, what is not
> started. `README.md` and `ARCHITECTURE.md` describe the *shipped* state and
> are updated per phase as work lands — never ahead of it.
> **Phase A has landed** (rev 2); B, C, and D are open.

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
| Crate split (`lan-core` / `lan-acp` / binary) | 0011 | **Not started** — one crate |
| MCP behind a feature | 0011/0012 | **Not started** — always compiled |
| Approval enum → trait impls | 0010 | **In between** — both `ApprovalPolicy` and `Approver` exist; enum must dissolve, terminal approver moves to binary |
| `Workspace` / run split | 0010 | **Not started** — single `RunConfig`, `prepare()` re-discovers per run |
| `.output::<T>()` structured output | 0010 | **In between** — mentra ships `Agent::run_to_output` + `TerminalOutputSpec`; lan does not surface it |
| `BudgetPool` | 0010 | **Not started** (per-run bounds exist upstream; the shared pool does not) |
| Tagged sinks / event fan-in | 0010 | **Not started** |
| Cancellation on the public API | 0010 | **In between** — token exists in mentra options; not exposed on `RunConfig`/`PreparedRun` |
| Declared subprocess tools | 0012 | **Not started** |
| Hooks re-founded as authorizer binding | 0012 | **In between** — hooks work; unification with the `Approver`/`ToolAuthorizer` seam is structural, not behavioral |
| Subagents / teams surfaced | 0010 | **In between** — mentra ships `task` + `team_*`; decide their place in lan's default tool profile |
| Recipe + review-workflow examples | 0010/0014 | **Not started** — `examples/embed.rs`, `conversation.rs` exist; the two acceptance examples do not |

Discoveries that shrank the plan: structured output and per-run bounds +
cancellation were assumed to be new mentra work; both already exist upstream
(`agent/terminal_output.rs`; `RunOptions`). No mentra issue is currently
required — the co-evolution discipline (ADR-0005) still applies to anything
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

### Phase B — Structure

1. Workspace split: `lan-core`, `lan-acp`, `lan` binary (ADR-0011). Bridge
   stays in the binary, marked extractable.
2. MCP behind a `mcp` feature in `lan-core`, default-on in the binary.
3. Dissolve `ApprovalPolicy`: `AllowAll` (default) and `DenyAll` in core,
   `TerminalApprover` + `--approve` flag wiring in the binary. Document the
   fail-closed rule on the trait.

Acceptance: `cargo tree -p lan-core` shows no `agent-client-protocol`, no
`tokio-tungstenite`; an embedder example compiles against `lan-core` alone.

### Phase C — The SDK (the point of the exercise)

1. `Workspace` / run split: context, skills, templates, MCP connections, and
   provider setup prepared once; runs minted cheaply from it.
2. `.output::<T>()` over mentra's `run_to_output`.
3. Cancellation token on the run API.
4. `BudgetPool` shared across runs.
5. Tagged sinks with a fan-in helper.
6. The two acceptance examples, written against the public API only:
   `examples/watch.rs` (interval + fingerprint + bounded run, ≲ 20 lines of
   logic) and `examples/review_workflow.rs` (fan-out with structured
   findings, shared budget, fan-in verification).

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
  because it is the cheapest it will ever be (ADR-0011).
- **`run_to_output` is unproven in lan's flow.** It exists upstream but lan
  has never driven it; Phase C item 2 starts with a spike, and friction goes
  upstream per ADR-0005.
- **Bridge limbo.** Neither core nor extracted; revisit when acp-ui usage is
  real or an upstream home appears.
