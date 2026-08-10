# Handoff: lan — Lightweight Agent Nucleus

You are picking up an in-progress project. Read this fully, then start with "Where to pick up" at the bottom.

## What lan is

A general-purpose, embeddable coding-agent harness in Rust, built on **Mentra** (the user's own agent runtime, `oops-rs/mentra`, v0.11). Library first, binary second. No TUI — clients own presentation. Three embedding surfaces:

1. In-process via the `lan` crate (Rust hosts)
2. **ACP** (Agent Client Protocol, JSON-RPC 2.0 over stdio) — `lan` with no subcommand serves it; existing clients (Zed, JetBrains, acp-ui web client) work without lan shipping any UI
3. `lan run "<prompt>" --json` — headless one-shot, JSONL event stream; `lan watch "<prompt>" --every 30m` — recurring runs (P4, not built)

Name: **lan = Lightweight Agent Nucleus** ("A LAN connects machines; lan connects agents to your codebase"). Do not mention any other name origin.

## Repo state

`/Users/wendell/developer/oops-rs/lan` — git on `main`, clean and pushed to `origin`.
`cargo test -p lan --all-features` = 93 passing; `cargo clippy --all-targets
--all-features -- -D warnings` clean. On **mentra 0.13** (published to crates.io) /
mentra-provider 0.4 — no path dependency, no sibling checkout needed.

```
Cargo.toml            # workspace: edition 2024, MSRV 1.85
lan/
  src/lib.rs          # re-exports; the SDK surface
  src/context.rs      # AGENTS.md discovery: global -> ancestors -> workspace
  src/context/        #   discovery.rs (the walk), render.rs (system-prompt block)
  src/event.rs        # the JSONL wire contract (schema v1) + EventLine envelope
  src/event/          #   mapping.rs (SessionEvent -> Event), jsonl.rs (writer)
  src/provider.rs     # provider choice, credential from env, base-URL normalizing
  src/skills.rs       # skills dir discovery (.lan/skills, then global skills/)
  src/shell.rs        # ShellAccess: command execution needs an explicit grant
  src/approval.rs     # ApprovalPolicy + Approver; PolicyAuthorizer on the runtime
  src/approval/       #   terminal.rs (asks a person on stderr/stdin)
  src/run.rs          # RunConfig/run/prepare/prepare_with_session
  src/run/            #   prepared.rs (drives a session), sink.rs (EventSink impls)
  src/main.rs         # thin CLI: `lan run [--json] [--allow-shell] [--approve] ...`
  examples/embed.rs   # in-process host reacting to events
  tests/run_stream.rs # assembly tests over mentra::test::MockRuntime (no network)
  tests/approval.rs   # approval loop over a local ScriptedProvider, each run timed out
Dockerfile            # read-only root, workspace sole rw mount, shell granted inside
.dockerignore
AGENTS.md             # mentra's conventions + lan-specific rules (READ THIS FIRST)
README.md
docs/
  PROPOSAL.md         # the why: problem, one idea, 7 bets (believe/buys/refuse)
  ARCHITECTURE.md     # the how: capability table, extension model, phases, risks
  p0-groundwork.md    # research + §4a: the gap list re-verified against 0.12
  adr/0001..0006      # locked decisions (0006 = shell needs a grant)
  proposals/0001..0003 # deferred ideas with triggers
```

**Do not commit this file** (user's instruction); it stays untracked.

Docs follow the **nous workflow** (modeled on `/Users/wendell/developer/oops-rs/nous/docs/`): PROPOSAL = why, ARCHITECTURE = how, ADRs = locked (Context/Decision/Consequences), proposals/ = deferred ideas with Status line + trigger + invariants. New significant decision → ADR. Deferred idea → proposal, never a TODO.

## Locked decisions (ADRs — do not relitigate)

1. **Mentra is the runtime** — lan re-implements nothing mentra has. Layering: mentra-provider ≈ pi-ai, mentra ≈ pi-agent-core, lan ≈ pi-coding-agent minus TUI.
2. **ACP is the only wire protocol** — official `agent-client-protocol` Rust crate. `run --json` is an output format, not a protocol. Web UI = adopt acp-ui + thin ws↔stdio bridge; never build a frontend.
3. **Library first, no TUI ever** — binary is a thin shell over the crate.
4. **Confinement is kernel-enforced** — v1 = Docker, `--read-only`, workspace as sole rw mount. In-process checks (`.git/hooks` write-deny via mentra policy hooks) are hygiene, never claimed as the boundary. v2 = codex-style per-command native sandbox (see proposals/0002, carries the codex design notes).
5. **Mentra co-evolution discipline** — same author owns both repos (user is mentra's author; updating mentra is allowed and expected). Test: "would a different harness want it?" → yes = land in mentra, AND file a mentra issue even when fixing immediately. lan-side workarounds only with a linked issue + removal note.

Core principle (Bet 4): **the core has no opinions** — no task types, pipelines, or domain vocabulary. Missions arrive as prompt + workspace data (AGENTS.md, skills, `.mcp.json`) + config. (Project origin was a bug-fix agent; that got explicitly generalized away — bug-fixing is just one prompt.)

## Key research facts (verified, don't re-research)

- **Mentra v0.11 public API already has**: MCP client (`McpManager`, `McpServerConfig`, stdio client, tool bridge), compaction (`CompactionEngine` trait, `StandardCompactionEngine`), permissions (`PermissionRequest/Decision`, `RuleStore`) mapping ~1:1 onto ACP `session/request_permission`, `SessionEvent` broadcast stream (token deltas, tool lifecycle) mapping ~1:1 onto ACP `session/update`, structured transcript (`AgentTranscript`), steering (`SteeringHandle`, `RoundStrategy`). **Missing**: session branching/tree (absent), skills loader exists but is `pub(crate)` (`mentra/src/runtime/skill.rs`).
- ~~**Five mentra gaps queued**~~ — **corrected 2026-08-08, see p0-groundwork §4a.** Re-verified against mentra 0.12 before filing: gaps #4 (tool profiles — `mentra::agent::ToolProfile` is public and on `AgentConfig`) and #5 (assembly test harness — `mentra::test::MockRuntime` behind `test-utils`) had **already shipped**; #2 and #3 were narrower than recorded. Four issues filed, all open: [mentra#6](https://github.com/oops-rs/mentra/issues/6) session branching · [mentra#7](https://github.com/oops-rs/mentra/issues/7) split turns + cumulative file tracking · [mentra#8](https://github.com/oops-rs/mentra/issues/8) skills multi-root + enumeration · [mentra#9](https://github.com/oops-rs/mentra/issues/9) `ToolCompleted` always has an empty `tool_name` (found while building P1; lan passes it through faithfully rather than working around it, per ADR-0005 — the CLI falls back to `tool_call_id`).
- **Eight lan builds**: AGENTS.md discovery (workspace + parent walk + global); prompt templates → ACP commands; ACP server mapping; `run --json` JSONL renderer; `watch` scheduler (skip-if-unchanged); subprocess hooks (JSON in/out over mentra's `RuntimeHook`); Docker packaging + `.git/hooks` deny preset; `.mcp.json` discovery → `McpManager`.
- **pi prior art** (`/Users/wendell/developer/WeNext/ai/pi/packages/coding-agent/docs/`, session-format.md + compaction.md, distilled in p0-groundwork §2): entry tree via id/parentId, compaction-as-entry with retainedTail checkpoints, cut only at turn boundaries (never between tool call and result), split-turn double summary, structured summary format, serialize-to-text before summarizing, version field + migration from day one.
- **codex** (`/Users/wendell/developer/WeNext/ai/codex`): no ACP (proprietary app-server, cautionary tale); its sandbox is the v2 reference (per-command wrapping, workspace-write, `.git`/agent-config read-only INSIDE workspace, network default-deny in layers; details preserved in proposals/0002).
- **zentox**: a prior mentra-based agent, not on disk; its API friction is cataloged in `mentra/docs/mentra-api-feedback.md` and distilled in p0-groundwork §1.

## Conventions (from AGENTS.md — binding)

- First-principles reasoning; verify against code, not assumptions.
- **Commit each completed step** before the next; Conventional Commits `<type>(<scope>): <summary>`, narrow concrete scopes, imperative mood.
- Rust: edition 2024 idioms, `foo.rs` + `foo/` (never `foo/mod.rs`), `cargo fmt` after edits, `cargo clippy --all-targets --all-features -- -D warnings`, focused modules.
- End commit messages with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.

## P1 design decisions (made while building; not yet ADRs)

- **lan owns the wire schema.** `Event` is lan's own enum, not a re-export of `SessionEvent`, so mentra's internals can move without changing lan's output. `EVENT_SCHEMA_VERSION = 1` rides on the `run_started` line.
- **The mapping is exhaustive with no wildcard** (`event/mapping.rs`). A new mentra event breaks the build instead of vanishing from the stream — that is deliberate; when bumping mentra, expect to map the new variant.
- **Deltas carry only the delta.** `SessionEvent::AssistantTokenDelta` also has `full_text`; forwarding it would make the stream quadratic.
- **Tool input and permission previews are parsed** from mentra's JSON-encoded strings into real JSON.
- **`prepare_with_session` is the seam.** `run()` builds a runtime; `prepare_with_session()` takes one the caller already has. That is what makes the pipeline testable against `MockRuntime` — and it is the API a Rust host with custom tools will want.
- **Discovery canonicalizes the workspace** (needed for a correct parent walk and dedup), so `WorkspaceContext::root()` exists and the header reports the *resolved* root. A test caught `workspace` and `context_files` disagreeing on macOS, where `/var` is a symlink.
- **Errors vs outcomes:** setup failure (no credential, bad model, missing workspace) is `Err`; a failure *during* the turn is `RunOutcome::Error` on a stream that still closes properly.
- **Base URLs are normalized.** mentra's Responses transport appends `v1/responses` itself, but gateways publish their URL *with* `/v1`; lan strips a trailing `/v1` so a pasted URL works. `--base-url`, else `LAN_BASE_URL`, else `OPENAI_BASE_URL`; key from `LAN_API_KEY` or `OPENAI_API_KEY`. A base URL beats provider auto-detection — pointing somewhere specific is always deliberate.
- **One skills directory, deliberately.** `register_skill_loader` replaces rather than merges (mentra#8), so lan registers the most specific and names the ignored ones on stderr, instead of merging directories itself (which would mean reimplementing mentra's frontmatter parsing — ADR-0005).

## Where things stand

**Done: P0, P1, and two pieces of P4** (the container, and the shell grant it makes
sound). Phase plan is ARCHITECTURE.md §6: P2 = ACP server; P3 = extension points;
P4 = `watch`; P5 = depth.

93 tests green (80 unit + 13 integration across `run_stream.rs` and `approval.rs`),
clippy clean at `-D warnings`. mentra is 721 green. Both repos pushed.

### What works, verified against a live model

One prompt against a workspace — prose or versioned JSONL, in-process or as a subprocess
— with AGENTS.md precedence, layered skills, command execution, approval, and
kernel-enforced confinement in the image. Specifically proven end to end:

- **Skills layering** — workspace `.lan/skills` and global `skills/` both registered;
  `review` resolved to the *project* body while `deploy` still came from the global root.
  Before mentra#8 only one root registered at all.
- **Confinement** — in-process, a write to `../outside.txt` is denied. In the container,
  a command reaching `/etc` is refused by the kernel with `Read-only file system`. That
  is the ADR-0004 boundary actually delivered rather than asserted.
- **Approval** — `--approve never` refuses a write and the model reports it accurately;
  `--approve prompt` with stdin closed denies in seconds rather than hanging.

### What is missing (the README synopsis still names some of it)

- **ACP server** — `lan` with no subcommand prints "not implemented". This is the
  *default* mode and the primary embedding surface per Bet 2 / ADR-0002. **P2.**
- **Multi-turn and resume** — `PreparedRun::execute` consumes the session, so every run
  is one prompt and mentra's `resume_session` is never called. Blocks ACP, and is the one
  real refactor P2 needs.
- **`lan watch`** — no subcommand at all. P4.
- **MCP `.mcp.json`, prompt templates, subprocess hooks** — P3.
- **Branching** — mentra has it since 0.13 (`Session::branch_from`, `children`); lan maps
  `SessionEvent::Branched` onto its stream but exposes nothing. P5.
- **No CI**, and lan itself is unpublished.

### Starting P2

`Event` is already the normalized spine, so the server maps `Event` → `session/update`
and never touches `SessionEvent` again. `PermissionRequested`/`PermissionResolved` map
onto `session/request_permission`, and the approval plumbing underneath is done and
tested — P2 supplies an `Approver` that asks the client and returns its answer, and gets
the rest free.

The refactor to do first: keep the session alive across turns instead of consuming it.
That unlocks conversation and resume regardless of protocol, so it is worth doing on its
own terms before any ACP code is written.

Use the official `agent-client-protocol` crate (ADR-0002). Do not invent an RPC, and do
not build a web frontend — adopt acp-ui behind a thin ws↔stdio bridge.

### Testing without burning tokens

`lan/tests/approval.rs` defines a minimal `ScriptedProvider` (~40 lines) and builds a
real `Runtime` around it; `run_stream.rs` uses `mentra::test::MockRuntime`. Between them
the whole pipeline is exercised with no network. Prefer these to live runs while
iterating. Note `Runtime::builder()` registers builtin tools and `empty_builder()` does
not — a scripted `files` call silently does nothing under the latter.

For live checks the user runs a local OpenAI-compatible gateway on
`http://127.0.0.1:3455/v1` (Responses API, Codex/ChatGPT-backed, SSH-tunnel only — ask
for the key, do not assume one is in the environment). Two quirks:

- **Pass `--model gpt-5.6-sol`.** `/v1/models` advertises ten but the proxy refuses most,
  including `gpt-4o`, which is what `NewestAvailable` picks.
- **It drops streams roughly one run in three** — transport errors and "Upstream response
  stream closed before response.completed". Not a lan bug; retry before debugging.
