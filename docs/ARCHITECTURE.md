# lan — Architecture

> rev 11 · 2026-08-11 · **lan** — **L**ightweight **A**gent **N**ucleus
> The *how*. For the *why* — problem, idea, bets — see [`PROPOSAL.md`](PROPOSAL.md);
> locked decisions live in [`adr/`](adr/); deferred ideas in [`proposals/`](proposals/);
> research grounding in [`p0-groundwork.md`](p0-groundwork.md).
> **Note (2026-08-11):** ADR-0010…0015 redirect the design toward an SDK-first
> shape. **Phases A, B, C and D of that transition have landed** — watch retired,
> bounds moved onto runs, shell default flipped, no shipped container, the CLI
> grammar of ADR-0015; the split into `lan-core` / `lan-acp` / the binary with
> MCP behind a feature and approval as a trait; the SDK proper — a
> `Workspace` opened once that mints runs, typed output, cancellation, a shared
> budget, and tagged event fan-in; and the bindings — interception as one
> contract with two bindings, and the workspace's history put where the caller
> says. §2, §3 and §4 below describe the state after them. One Phase D item is
> held rather than built (declared subprocess tools, for want of a concrete use
> case); where this document still describes the P0–P4 shape it says so.
> A later wave belonging to no phase closed the last five upstream candidates,
> which is what rev 11 records: a typed turn can now keep its tools, a run
> names the bound that ended it, and `lan-core`'s graph carries no websocket
> stack. The ledger and phases are in [`REDESIGN.md`](REDESIGN.md).
> Reference bar: [pi](https://github.com/earendil-works/pi) (earendil-works) — minimal core, complete harness.
> General-purpose: no domain assumptions. Periodic bug-checking is one use case, never a design input.

lan is a coding-agent harness in Rust, built on [Mentra](https://github.com/oops-rs/mentra), with
the shape pi proved out: a small but **complete** core — sessions, compaction, multi-provider,
context conventions, extension points — everything else arriving as data or plugins. No TUI.
Embedding is the front door: ACP for editors and web UIs, a JSONL event stream for scripts, the
crate itself as the SDK.

## 1. The bar: what "full-functional" means

pi's thesis: *stay small at the core while being extended through extensions, skills, prompt
templates, and packages.* Its core is nonetheless complete — sessions with branching and tree
navigation, context compaction, unified multi-provider access, RPC headless mode, an SDK,
hot-reloadable extensions. Two pi decisions independently validate ours:

- **No built-in permission system.** pi ships none and says "containerize or sandbox pi" —
  the posture we arrived at independently and adopted knowingly in ADR-0013: the boundary is
  the OS's, documented rather than shipped.
- **Embedding via protocol.** pi's RPC mode is a bespoke JSONL protocol over stdio. We take the
  same architecture but adopt the *standard* — ACP — so every existing client works without a
  custom client library.

| Capability (pi has it) | Ours | Source |
|---|---|---|
| Agent loop + tool calling | Mentra runtime, async tool traits | mentra |
| Multi-provider LLM API | mentra-provider: OpenAI, Anthropic, Gemini, OpenRouter, Ollama, LM Studio | mentra |
| Session persistence + resume | SQLite-backed sessions, snapshots | mentra |
| Compaction | Memory compaction exists in mentra; wire to session lifecycle | mentra + glue |
| Session branching / tree | mentra's transcript tree, exposed on `PreparedRun` ✅ | built |
| Builtin tools (shell, files, background exec, tasks) | Mentra builtins | mentra |
| Context files (AGENTS.md) | Loader: workspace + global, parent-dir walk | build |
| Skills (on-demand) | SKILL.md discovery, description-first loading | build |
| Prompt templates (/commands) | Markdown templates with args, exposed over ACP as commands ✅ | built |
| Extensions (custom tools, event interception) | MCP servers + interception with two bindings — in-process `Interceptor`, subprocess hooks — allow/deny/modify (§3) ✅ | built |
| Packages (shareable bundles) | Directory convention over skills/templates/hooks/MCP — defer | later |
| RPC / headless mode | `spawn --json` event stream (`run` is a compatibility alias) + **ACP** (standard, not bespoke) ✅; local lifecycle daemon for durable task control ✅ | built |
| SDK | `lan-core`: a `Workspace` opened once, runs minted from it with typed output, bounds, cancellation ✅ — other languages use ACP | built |
| TUI / themes / keybindings | Out of scope by design — ACP clients own presentation | — |
| Provider OAuth login flows | API-key auth first; OAuth per provider later | later |

## 2. Principle: the core has no opinions

Task-specific behavior enters through data, never code: the **prompt**, the **workspace** (its
AGENTS.md, skills, templates, `.mcp.json`), and **config**. A periodic code-health check, a
nightly dependency bump, an interactive refactor are all the same to the binary. If a use case
seems to need core changes, close the gap generically or push it to an extension point.

```
lan "<prompt>"                     # shorthand: exactly `lan spawn "<prompt>"`
lan spawn "<prompt>"               # enqueue work and return a durable handle
lan send <ID> "<message>"          # enqueue a follow-up turn
lan wait <ID>                      # repeatable terminal observation
lan cancel <ID>                    # downward cancellation request
lan watch <ID>                     # replayable progress observation
lan inbox [ID]                     # accepted-message listing
lan serve --acp                    # ACP server on stdio (explicit)
lan serve --bridge                 # the same server on a websocket, for a browser
lan fingerprint                    # the workspace's hash, for a caller's own loop
```

The grammar is ADR-0017's and includes the local lifecycle verbs above. Bare `lan` returns
usage rather than starting a long-lived server. Recurrence is not in it: an interval is the host's (cron,
systemd, CI, a tokio task), and the two pieces that are easy to get wrong — the
fingerprint and per-run bounds — are a subcommand and three flags on `spawn` (ADR-0014). In
process they are `Workspace::fingerprint()` and the bounds on a `RunSpec`, which is the same
loop without the subprocess: [`lan-core/examples/watch.rs`](../lan-core/examples/watch.rs).
A run that a bound ended says which one, both as `RunReport::stopped_by` in process and as
`run_finished`'s `stopped_by` on the stream, and the CLI exits `3` for all three of them —
the exit-code contract of ADR-0015 is answerable without parsing prose.

In-process concurrent work is owned by `lan_core::Supervisor`. The binary adds
the same ownership rules across CLI processes: a per-workspace hidden daemon
owns a loopback TCP endpoint, a private capability descriptor, and an atomic
JSON journal. `spawn` returns immediately with an opaque instance/task handle;
`wait`/`watch`/`cancel` reconnect through that descriptor, and terminal results
remain repeatable after the submitting process exits. Attached children inherit
the narrower parent deadline and downward cancellation; `--detached` creates a
new root. Progress is bounded and advisory, while terminal state is persisted
on a separate control path, so a slow watcher cannot strand completion. The
transport lives in the binary; `lan-core` remains protocol- and transport-free.

## 3. Extension model (without embedding a scripting language)

pi's extensions are TypeScript modules loaded into a TS host — free for them, expensive for a
Rust binary. Equivalent coverage, Rust-native:

| pi extension capability | lan mechanism |
|---|---|
| Custom tools for the LLM | **MCP servers** (`rmcp`): any language, process-isolated, ecosystem standard |
| Event interception (block/modify tool calls) | **One contract, two bindings** (ADR-0012): an in-process `Interceptor` a host implements, and subprocess hooks a workspace declares — same request, same allow/deny/modify vocabulary, one chain |
| Custom commands | Prompt templates, surfaced as ACP commands |
| Custom UI | ACP client's job (permission requests, input prompts are protocol messages) |
| In-process extension with full API access | The `lan-core` crate: the harness is a library first, binary second |

Interception is not a subsystem parallel to anything. `hooks::contract` holds the request
and outcome types both bindings speak, one `Chain` decides what an answer *means* — first
refusal wins, modifications compose, nothing is smuggled past a later guard — and a single
`HookRunner` is the one hook lan registers with mentra, so the ordering and the
short-circuit are lan's rather than the runtime's. Participants speak in-process
interceptors first (registration order), then global hooks, then workspace hooks, on the
rule that **the further a participant is from the workspace's own data, the earlier it
speaks**: a host's compiled guard can then refuse before a program that arrived with a
five-minute-old clone is spawned at all. Anything that cannot answer denies.

`Approver` is a *sibling* seam, not a parent and not a child. It answers *may this happen*
and its answer feeds the permission machinery a person drives; an interceptor answers *may
this happen, in this form*. mentra keeps the two apart for the same reason, and merging
them would trade two honest contracts for one vague one.

> If subprocess hooks + MCP prove too coarse, an embedded scripting layer (wasm or rhai) is the
> escalation path — decided by evidence, not up front.

## 4. Architecture

```mermaid
flowchart LR
  subgraph clients["ACP clients (adopted)"]
    zed["Zed · JetBrains"]
    web["acp-ui (web)"]
  end
  subgraph bin["lan — the binary"]
    entry["CLI grammar · terminal approver"]
    br["ws bridge (extractable)"]
  end
  subgraph adapter["lan-acp — the ACP adapter"]
    srv["server · session mapping · modes"]
  end
  subgraph lib["lan-core — the SDK"]
    ws["Workspace — opened once: context · model · MCP · seams"]
    ctx["context: AGENTS.md · skills · templates"]
    ext["interception (2 bindings) · MCP client (mcp feature)"]
    runs["runs — minted cheaply: typed output · bounds · cancel · fan-in"]
    sess["sessions · branching · compaction"]
    rt["Mentra runtime"]
  end
  subgraph box["host OS — isolation, if any, is the operator's"]
    wsp[("workspace  rw")]
  end
  llm[("providers")]
  host["a Rust host, in-process"]
  zed -- stdio --> entry
  web -- ws --> br
  br --> srv
  entry --> srv
  entry --> lib
  host --> lib
  srv --> lib
  rt --> wsp
  rt --> llm
  ctx --> ws
  ext --> ws
  ws --> runs
  ws --> rt
  runs --> sess
  sess --> rt
```

- **Crate layering mirrors pi's package layering**: mentra-provider ≈ pi-ai, mentra ≈
  pi-agent-core, lan ≈ pi-coding-agent minus TUI. Since ADR-0011 lan is itself three crates,
  split by dependency weight rather than by release schedule (they share one version):
  **`lan-core`** is the in-process SDK and carries no protocol, no transport, and no TTY
  code; **`lan-acp`** is the ACP adapter over its event stream and seams, opt-in by
  dependency; **`lan`** is the binary over both, and the explicit `lan serve --acp` command
  is what an editor spawns. MCP is a
  default-on `mcp` feature of `lan-core`, so an embedder can compile a core that has never
  heard of it (ADR-0012). The websocket bridge stays in the binary, marked extractable: it
  is ACP-ecosystem tooling with no lan-specific knowledge, and never an identity argument
  for lan.
- **ACP is explicit** — `lan serve --acp` serves the protocol on stdio and
  `lan serve --bridge` serves it over a websocket. Bare `lan` prints usage; making a
  long-lived server an explicit command keeps a prompt invocation from accidentally
  becoming a server (ADR-0017).
- **A workspace is opened once and mints runs** (ADR-0010). Everything that belongs to a
  repository rather than to a prompt — context documents, the credential and resolved
  model, skills, templates, hooks, MCP connections, the approval gate — is settled by
  `Workspace::open`, and `prepare` mints a run from it *synchronously*, because nothing is
  left to await. A twenty-way fan-out therefore reads `AGENTS.md` once. What a run carries
  of its own is the honestly per-run half: the prompt, the session name, the effort, and
  the bounds. The free functions (`run`, `prepare`, `resume`) are wrappers that open a
  workspace, mint one run, and drop it — one resolution path, not two.
- **A run answers with a value when asked.** `PreparedRun::output::<T>()` runs a turn that
  must answer through a generated terminal tool whose input *is* the answer, which is what
  makes a workflow composable in host Rust rather than in prose-parsing. The stream is
  unchanged; only the return value differs. That turn *shapes* by default: it holds the
  answering tool alone, so reading and shaping are two turns, and asked to do both at once
  it answers in shape having opened nothing, with the run reporting success.
  `OutputSpec::with_tools` is the other mode — the ordinary toolset stays on the turn, one
  call reads and then answers, and what it trades away is the forcing, since nothing makes
  a working turn stop and answer. Neither is right for the other's job, so the choice sits
  on the spec rather than in a default.
- **Sessions**: an ACP session *is* a mentra agent — lan uses the persisted agent id as the
  protocol's session id, so `session/load` is `Runtime::resume_session` and lan stores no
  mapping of its own (ADR-0007). A session outlives a turn, which is what makes conversation
  and resume possible at all; compaction wires to context-pressure events. Which
  conversations belong to *this* workspace is a tag mentra keeps on each row, and
  `WorkspaceBuilder::open` is where lan sets it — until it did, `session/list` filtered on a
  tag lan never wrote and so returned nothing, whatever had been persisted. Where those rows
  live is the caller's to say: `with_store_dir` names a directory, `with_ephemeral_history`
  says nowhere and takes an in-memory store instead.
- **The mentra/lan split** (same author owns both): anything a *different* harness could also
  want — session branching, compaction lifecycle, hook points, MCP client — belongs in mentra.
  lan keeps conventions and protocol: AGENTS.md/skills/template discovery, ACP mapping, the
  CLI grammar.
- **Confinement**: the boundary is the OS's and lan ships no instance of it (ADR-0013,
  amending ADR-0004). Shell and background execution are on by default; a run holds the
  authority of the account that starts it, and lan never claims otherwise. In-process there
  is hygiene only — workspace path roots, and a policy that keeps `.git/hooks` and agent
  config read-only to the *file tools* (codex's anti-escape carve-out), which a shell
  redirect walks past. [`containerization.md`](containerization.md) documents the
  read-only-root pattern lan used to ship; a native per-command sandbox (Seatbelt on macOS,
  bubblewrap+seccomp on Linux, codex's `workspace-write` design) stays parked in
  [`proposals/0002`](proposals/0002-native-sandbox.md) as an *optional* later layer, not a
  return to denying commands by default.

## 5. Research notes (2026-08-08)

- **ACP (Agent Client Protocol)** is the standard: JSON-RPC 2.0 over stdio, LSP-style; v1
  stable; adopted by Zed, JetBrains, Copilot, Gemini CLI, 25+ agents; official Rust crate
  (`agent-client-protocol`). Web/mobile clients exist: acp-ui, acp-mobile — only a small
  WebSocket↔stdio bridge to write, no frontend to build.
- **codex** (OpenAI) is the counter-example on protocol — no ACP; a proprietary
  not-quite-JSON-RPC app-server that even its own SDKs bypass — but the reference on
  sandboxing: per-command wrapping (`codex-rs/sandboxing/src/manager.rs`), `workspace-write`
  policy with `.git`/agent-config kept read-only *inside* the workspace, network default-deny
  in three layers (seccomp, netns unshare, SBPL omission).
- **pi**: its public coding-agent session and compaction documentation informed
  the prior-art notes in P0; no local checkout is required.
- **zentox**: `mentra/docs/mentra-api-feedback.md` describes a prior Mentra-based agent and
  catalogs its API friction — requirements input for lan's core, whatever its domain was.

## 6. Plan

| Phase | Scope | Estimate |
|---|---|---|
| P0 Groundwork | Mine `mentra/docs/mentra-api-feedback.md`; read pi session-format + compaction docs; decide mentra-vs-lan split per capability | done |
| P1 Crate + `run` | Mentra wiring, AGENTS.md loader, skills discovery, worktree hygiene, JSONL event stream. Acceptance: arbitrary prompts on arbitrary repos, in-process and as subprocess | done |
| **P2 ACP server** ✅ | `agent-client-protocol` crate; session mapping, permission surfacing, modes, listing, history replay. Sessions survive turns, so conversation and resume work independent of protocol | done |
| **P3 Extension points** ✅ | MCP client honoring `.mcp.json` *and* the servers an ACP client sends; subprocess hooks (allow/deny/modify); prompt templates surfaced as ACP commands; ws↔stdio bridge for acp-ui | done |
| **P4 Loop + Docker** ✅ | `watch` scheduler with skip-if-unchanged — retired by ADR-0014, its bounds and fingerprint kept; Dockerfile, state volume, shell grant — withdrawn by ADR-0013 for [`containerization.md`](containerization.md) | done |
| P5 Depth | Branching ✅ — two-way since mentra 0.16, so an abandoned line of work can be returned to; compaction tuning, packages convention, provider OAuth remain | ongoing |

This table is the record of how lan was built, not the current plan. What follows P5 is the
SDK-first transition of ADR-0010…0015, phased in [`REDESIGN.md`](REDESIGN.md) §3: Phase A
(posture and pruning), Phase B (structure — the crate split, the `mcp` feature, approval
as a trait), Phase C (the SDK — the `Workspace` / run split, typed output, cancellation,
the shared budget, event fan-in) and Phase D (bindings — interception's second binding,
the history knobs, `session/list`, credential redaction) have landed, with Phase D's
declared-subprocess-tools item held for want of a concrete use case.

Validation stays deliberately varied — a refactor, a doc task, a test-writing task, *and* a
periodic check — so no single use case bends the API toward itself.

## 7. Risks and open questions

- **Scope honesty.** pi-class is a real harness, not a demo: sessions + compaction + extensions
  + protocol is weeks, not days, to polish. The phase order front-loads the embeddable core.
- **Extension expressiveness.** Narrower than it was: an embedding Rust host now writes an
  `Interceptor` in its own process with its own types, which is the case subprocess hooks
  were worst at. What remains untested is the other audience — a *repository* whose guard
  has to be a program, in any language, with JSON on stdin. If that proves coarser than
  pi's in-process TS extensions, the escalation path (wasm/rhai) is named but deferred
  until friction is shown.
- **ACP crate maturity.** Official but young; budget for permission-flow gaps; acp-ui's traffic
  monitor is the debugger.
- **Mentra co-evolution.** Same author on both sides: gaps lan hits become mentra changes, not
  workarounds. The discipline is direction, not permission — capabilities generic enough for
  any harness land in mentra; lan keeps only harness-specific glue. Track each gap as a mentra
  issue even when fixing it immediately, so the API story stays legible to other mentra users.
  Nine stand named in [`REDESIGN.md`](REDESIGN.md) §2's footnotes across Phases B–D, and as
  of the wave after Phase D **all nine are closed** — eight fixed upstream, one built in lan
  where it belonged. That is the first clean tally the ledger has had, and it measures the
  discipline rather than mentra's completeness: three further candidates were named on the
  way through and none is built, and footnote 8 remains open.
- **Compaction quality.** Mentra has the primitive; behavior under long sessions is unproven.
  pi's compaction doc is the reference to study in P0.
- **Name collision.** `lan` collides with the networking acronym; searchability will be poor.
  Accepted trade-off — the expansion (Lightweight Agent Nucleus) leans into it rather than
  fighting it.
