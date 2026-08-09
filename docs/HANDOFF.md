# Handoff: lan — Lightweight Agent Nucleus

You are picking up an in-progress project. Read this fully, then start with "Where to pick up" at the bottom.

## What lan is

A general-purpose, embeddable coding-agent harness in Rust, built on **Mentra** (the user's own agent runtime, `oops-rs/mentra`, v0.11). Library first, binary second. No TUI — clients own presentation. Three embedding surfaces:

1. In-process via the `lan` crate (Rust hosts)
2. **ACP** (Agent Client Protocol, JSON-RPC 2.0 over stdio) — `lan` with no subcommand serves it; existing clients (Zed, JetBrains, acp-ui web client) work without lan shipping any UI
3. `lan run "<prompt>" --json` — headless one-shot, JSONL event stream; `lan watch "<prompt>" --every 30m` — recurring runs (P4, not built)

Name: **lan = Lightweight Agent Nucleus** ("A LAN connects machines; lan connects agents to your codebase"). Do not mention any other name origin.

## Repo state

`/Users/wendell/developer/oops-rs/lan` — git on `main`, 11 commits, pushed to
`origin`. **P1 is built and verified live**: `cargo test -p lan --all-features` = 71
passing; `cargo clippy --all-targets --all-features -- -D warnings` clean. Now on
**mentra 0.12** / mentra-provider 0.4.

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
  src/run.rs          # RunConfig/run/prepare/prepare_with_session
  src/run/            #   prepared.rs (drives a session), sink.rs (EventSink impls)
  src/main.rs         # thin CLI: `lan run [--json] [--base-url] [--model]`
  examples/embed.rs   # in-process host reacting to events
  tests/run_stream.rs # assembly tests over mentra::test::MockRuntime (no network)
AGENTS.md             # mentra's conventions + lan-specific rules (READ THIS FIRST)
README.md
docs/
  PROPOSAL.md         # the why: problem, one idea, 7 bets (believe/buys/refuse)
  ARCHITECTURE.md     # the how: capability table, extension model, phases, risks
  p0-groundwork.md    # research + §4a: the gap list re-verified against 0.12
  adr/0001..0005      # locked decisions
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

## Where to pick up

Completed: **P0** (research, groundwork, doc structure) and **P1** (crate + `lan run`).
Phase plan is ARCHITECTURE.md §6: P2 = ACP server; P3 = extension points; P4 = watch +
Docker; P5 = depth (branching, compaction).

**P1 is verified live, and all four mentra gaps are fixed upstream.**

The keys in `mentra/.env` are expired. The user supplied a local OpenAI-compatible
gateway instead: `http://127.0.0.1:3455/v1`, Responses API, backed by a Codex/ChatGPT
account. Two quirks worth knowing before using it again:

- **Model choice is not free.** `/v1/models` advertises ten, but the proxy refuses most
  — `gpt-4o`, `gpt-5.6`, and every `*-codex` return "not supported when using Codex with
  a ChatGPT account". **`gpt-5.6-sol` works** (`-terra`/`-luna` likely too). Since
  `NewestAvailable` picks `gpt-4o` off that list, always pass `--model gpt-5.6-sol`.
- **It drops streams intermittently.** Transport errors and "Upstream response stream
  closed before response.completed" appear perhaps one run in three, and succeed on
  retry. Not a lan bug — lan reports them correctly and exits 1. Retry before debugging.

Verified end to end against it:

- prompt answered from injected `AGENTS.md`; real tool use on a *different* repo
  (`-C ../mentra` → correct answer, twice after one transient failure);
- **layered skills** — workspace `.lan/skills` and global `skills/` both registered;
  `review` resolved to the *project* body (`PROJECT-8891`, not the personal
  `PERSONAL-0000`) while `deploy` still came from the global root (`GLOBAL-4417`). That
  is mentra#8 proven through the model: before the fix only one root registered at all;
- **`tool_completed` names its tool** (`load_skill`) — mentra#9 proven live;
- confinement: a write to `../outside.txt` denied, file unchanged;
- in-process embedding via `lan/examples/embed.rs`;
- prose mode pipes cleanly — stdout carries only the answer.

71 lan tests green (63 unit + 8 assembly); mentra 721 green; clippy clean at
`-D warnings` in both.

**mentra 0.13.0 is published** (crates.io, 2026-08-09). lan depends on it from the
registry — no path dependency, no sibling checkout needed. The layered-skills run above
was re-verified against the published crate and gave the identical result.

**Shell and the container landed after that** (ADR-0006, `docs/adr/0006-*`). P1 could not
run a single command — `RuntimePolicy::workspace_bounded` leaves mentra's
`allow_shell_commands` false, correctly, so `lan run` could read and write files but not
run `cargo test` or `git`. Now:

- `--allow-shell` / `LAN_ALLOW_SHELL=1` grants it; the crate defaults to `Denied` so no
  embedder inherits command execution. `RunConfig` does not read the env itself.
- lan never *infers* the boundary. `detect_environment()` can tell it is inside a
  container but not that the container was run with constrained mounts, so detection only
  warns (granting on a bare `Host` prints what authority was handed over).
- The **Dockerfile** is where the grant is sound and is on by default. Verified live:
  `git log` works inside; writes to `/etc` and `/usr/local/bin` are refused by the kernel
  with `Read-only file system`, not by a lan check. That is ADR-0004 finally delivered.
- Two gotchas the image had to solve: mentra's SQLite store defaults under `$HOME`
  (unwritable under `--read-only`) so `XDG_DATA_HOME=/state`; and `Cargo.lock` is now
  committed, needed for `--locked` and standard for a crate shipping a binary.

Still **not** built, despite the README synopsis naming them: the ACP server (default
`lan`, P2) and `lan watch` (P4). Runs are single-turn — no conversation, no resume,
`execute` consumes the session and mentra's `resume_session` is never called. No MCP
wiring, templates, or hooks (P3). No CI. lan is unpublished.

**The permission gate is now closed** (`lan/src/approval.rs`). lan installs a
`PolicyAuthorizer`, so `permission_requested` can actually fire, and the event forwarder
answers it through an `Approver` — previously nothing resolved, which would have hung the
turn, since mentra blocks on a oneshot. `--approve always|prompt|never`; read-only calls
are never queued (prompting for reads trains people to approve blind). `TerminalApprover`
asks on stderr and reads stdin, denying when there is no TTY so an unattended run fails
visibly. `lan/tests/approval.rs` wraps every case in a 10s timeout — those tests fail by
hanging, which is the regression worth catching. **P2 supplies an ACP `Approver` and gets
the whole flow for free.**

Then **P2 (ACP)**: `Event` is already the normalized spine, so the server maps `Event` →
`session/update` rather than touching `SessionEvent` again. `PermissionRequested` /
`PermissionResolved` map onto `session/request_permission`, and mentra's
`SessionPermissionHandle` is the resolution side. `Session::subscribe` +
`PreparedRun::execute` generalize to multi-turn by keeping the session alive instead of
consuming it — that is the one refactor P2 will want.
