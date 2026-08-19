# P0 Groundwork

> 2026-08-08 · outputs of ARCHITECTURE.md §6 P0: zentox feedback distilled, pi prior art, and the
> concrete mentra-vs-basis split. Supersedes the provisional table in ARCHITECTURE.md §1 where they
> disagree — this one is based on reading mentra's actual public API.

## 1. Requirements from zentox feedback (`mentra/docs/mentra-api-feedback.md`)

zentox was a real Mentra-based CLI agent. Its friction list, translated into domain-free
requirements for basis (and mentra):

| zentox friction | Requirement | Lands in |
|---|---|---|
| Context-dependent tool sets need hand-rolled `with_tool(...)` gating | First-class tool profiles / runtime presets | **mentra** (its #1 priority, verbatim) |
| App owns operational glue: path scoping, timeouts, output summarization | basis *is* that glue, packaged once — the "recommended CLI integration pattern" mentra wanted to document | **basis** |
| No integration-test story for runtime-plus-tools assemblies | Test harness support for full assemblies; basis's own tests will need it immediately | **mentra** (`test-utils` extension) |
| No realistic end-to-end example | basis is the realistic example — provider setup, policy, custom tools, composition, transcript inspection | **basis** (serves both repos) |

zentox validations to keep: policy roots (working/read/write) map cleanly to workspace scoping;
custom tool registration is already easy; structured transcript history is good for post-run
artifacts. basis's JSONL event stream should expose the same transcript walk zentox used for
`record.md`.

## 2. Prior art from pi (session-format.md, compaction.md)

Decisions worth adopting:

- **Entries form a tree via `id`/`parentId`** — branching is in-place, no file copying. The
  "leaf" is the current position. Branch = move leaf to an earlier entry.
- **Compaction is an entry in the tree**, not a mutation. Context building walks leaf→root and
  honors the newest compaction entry. Newer pi compactions carry `retainedTail` (materialized
  kept messages) so a compaction acts as a **self-contained checkpoint** — no walking past it.
- **Cut points**: only at user/assistant boundaries, never between a tool call and its result.
  A single over-budget turn becomes a "split turn" with a two-part summary (history + turn
  prefix).
- **Branch summarization**: on leaving a branch, summarize abandoned work from the common
  ancestor and inject it into the new branch. Same structured format as compaction.
- **Structured summary format**: Goal / Constraints / Progress (done, in progress, blocked) /
  Key Decisions / Next Steps / Critical Context + cumulative `<read-files>`/`<modified-files>`
  tracking that survives repeated compactions.
- **Serialization for summarization**: `[User]:`/`[Assistant]:` text transcript (prevents the
  model continuing the conversation), tool results truncated to 2k chars.
- **Settings**: `reserveTokens` (default 16384) triggers; `keepRecentTokens` (default 20k)
  decides the cut. Manual `/compact [instructions]` supported.
- **Version field + auto-migration** in the session header from day one.

Divergence: pi stores sessions as JSONL files; mentra persists to SQLite. Keep SQLite as the
store, but adopt the **tree-of-entries model and checkpoint semantics** on top of it.

## 3. Mentra reality check (read 2026-08-08, v0.11)

Mentra's public API already covers more than ARCHITECTURE.md §1 assumed:

| Capability | Status in mentra | Note |
|---|---|---|
| MCP client + tool bridge | **exists**: `McpManager`, `McpServerConfig`, stdio client, `McpBridgedTool` | ARCHITECTURE.md said "build" — wrong; basis only wires `.mcp.json` discovery to it |
| Compaction engine | **exists**: `CompactionEngine` trait, `StandardCompactionEngine`, modes, diagnostics | gap vs pi: checkpoint semantics (`retainedTail`), split turns, cumulative file tracking — verify, then extend in mentra |
| Skills loader | **exists but `pub(crate)`**: `runtime/skill.rs` (frontmatter, dedup, recursive discovery) | mentra gap: make public or expose via runtime builder config |
| Permissions | **exists**: `PermissionRequest/Decision`, `RuleStore`, `RememberedRule`, `SessionPermissionHandle` | maps directly onto ACP `session/request_permission` |
| Session event stream | **exists**: `SessionEvent` (token deltas, reasoning deltas, tool queued/started/…) via broadcast | near-1:1 with ACP `session/update` notifications; also the `run --json` payload |
| Structured transcript | **exists**: `AgentTranscript`, `TranscriptItem`, `CompactionSummary` | zentox's record-walking use case |
| Session branching / tree | **absent**: `Session`/`SessionMetadata` only; no fork/branch/snapshot API found in public surface | mentra gap (pi tree model, §2) |
| Round strategy / steering | **exists**: `RoundStrategy`, `SteeringHandle`, `QueueMode` | ACP `session/cancel` + prompt-during-turn map here |
| Teams, background tasks, memory | exists | not needed for basis v1; do not wire |

## 4. The split (decision)

Rule (from AGENTS.md): generic-for-any-harness → mentra; conventions and protocol → basis.

**mentra gaps to file/fix (issues even when fixed immediately):**
1. `feat(session)`: entry-tree branching — pi's `id`/`parentId` model over the SQLite store;
   branch summaries as first-class entries.
2. `feat(compaction)`: checkpoint semantics (`retainedTail` equivalent), split-turn handling,
   cumulative read/modified file tracking in summaries — gap analysis vs pi first.
3. `feat(runtime)`: public skills API (today `pub(crate)` in `runtime/skill.rs`).
4. `feat(runtime)`: tool profiles / runtime presets (zentox priority #1).
5. `feat(test)`: assembly-level test harness in `test-utils` (zentox priority #2).

### 4a. Re-verification against mentra 0.12 (2026-08-08, before filing)

§3 and the list above were written against mentra **0.11**. Re-reading the public API at
**0.12.0** before filing found two of the five already shipped and two narrower than
recorded. Filed three issues, not five.

| # | Verdict at 0.12 | Evidence | Issue |
|---|---|---|---|
| 1 Session branching | **Real, unchanged** — `session` exposes `Session`/`SessionId`/`SessionMetadata` and no fork, branch, or `parent_id` anywhere | — | [mentra#6](https://github.com/oops-rs/mentra/issues/6) |
| 2 Compaction checkpoints | **Mostly shipped; narrowed to two properties** | see below | [mentra#7](https://github.com/oops-rs/mentra/issues/7) |
| 3 Public skills API | **Partly shipped; narrowed** — `Runtime::register_skills_dir` is public (`runtime.rs:127`) | see below | [mentra#8](https://github.com/oops-rs/mentra/issues/8) |
| 4 Tool profiles | **Closed** — `mentra::agent::ToolProfile` is public with `all`/`only`/`hide`/`allows`, is an `AgentConfig` field (`agent/config.rs:257`), enforced at `agent.rs:529`; `examples/cli_runtime.rs` uses it exactly as zentox asked. Also `FileToolProfile` + `RuntimeBuilder::with_file_tools` | — | none |
| 5 Assembly test harness | **Closed** — `mentra::test::MockRuntime` behind the `test-utils` feature: scripted turns (`text`/`stream_text`/`tool_calls`/`failure`), `with_policy`, `with_store` | — | none |

**#2, what already exists** (so basis does not re-file it): `CompactionOutcome` returns a
*replacement* transcript with the tail materialized verbatim — that **is** the
`retainedTail` checkpoint property, plus a pre-compaction `.jsonl` snapshot at
`transcript_path` and a documented `details` preservation guarantee (mentra ADR-0001 §6).
Cut-point safety is also handled: `required_tail_start_for_continuation`
(`compaction.rs:609`) refuses to cut between an assistant tool call and its result. What
remains is (a) a single over-budget turn is uncompactable — `preserve_from == 0` returns
`Ok(None)`, so there is no split-turn path; (b) `extract_context`'s `files_touched` only
scans `TranscriptKind::ToolExchange` items and `CompactionSummary` has no field to hold
it (`transcript.rs:253`), so the list survives repeated compactions only as prose the
model chose to keep.

**#3, what remains:** `register_skill_loader` *replaces* rather than merges
(`runtime/handle/tooling.rs:103`), so a second `register_skills_dir` silently discards
the first root — basis needs workspace-over-global precedence; and loaded skills cannot be
enumerated (`SkillLoader`/`SkillEntry` are `pub(crate)`), which basis needs to surface
skills as ACP commands.

**Consequence for basis:** P1 no longer needs to hand-roll tool profiles — use
`agent::ToolProfile` directly — and basis's own assembly tests should build on
`mentra::test::MockRuntime` rather than a bespoke harness. §5's "profiles can be
hand-rolled then migrated" is obsolete.

### 4b. All four issues fixed and closed (2026-08-09, mentra 0.13.0)

Fixed upstream rather than worked around, per ADR-0005. basis now depends on
mentra 0.13 and carries no skills workaround.

| Issue | Landed as | What basis gained |
|---|---|---|
| [#9](https://github.com/oops-rs/mentra/issues/9) | `ToolNameIndex` correlates a call from queue/start to result | `tool_completed` names its tool; basis's test asserts it |
| [#8](https://github.com/oops-rs/mentra/issues/8) | `register_skills_dir` additive, `register_skills_dirs`, `Runtime::skills()`, `SkillLoadError` re-exported | Workspace **and** global skill roots both register with correct precedence; the header reports every skill; `RunError::Skills` is a typed `#[from]` again |
| [#7](https://github.com/oops-rs/mentra/issues/7) | `CompactionSummary::files_touched` accumulates; a tool-pinned turn is summarized as a unit | Long runs stop losing their file history |
| [#6](https://github.com/oops-rs/mentra/issues/6) | `EntryId`/`parent_id` on transcript entries, `Session::branch_from`/`children`, `SessionEvent::Branched` | P5 branching is unblocked; basis maps `Branched` onto its stream today |

**Deliberately not done upstream, recorded on the issues:** branch
summarization on leaving a branch (policy on top of the tree, wants the
compaction pipeline); a session-header `version` field (migration is currently
structural — missing parent links are inferred on load); and two-part
summarization of a long history whose final turn is separately over budget
(needs a total-budget signal in `CompactionRequest`).

**basis builds (harness-specific):**
1. AGENTS.md discovery: workspace + parent-dir walk + global; injection into system context.
2. Prompt templates: markdown + args → ACP commands.
3. ACP server: `agent-client-protocol` crate ↔ `SessionEvent`/`PermissionRequest` mapping.
4. `run --json`: JSONL rendering of the `SessionEvent` stream.
5. `watch`: interval scheduler, skip-if-unchanged. *(Built in P4, retired by
   ADR-0014 — the bounds moved onto `run`, the fingerprint became
   `basis fingerprint`, the interval went back to the host.)*
6. Subprocess hooks: exec-a-command JSON in/out, layered on mentra's `RuntimeHook`.
7. Docker packaging + `.git/hooks` write-deny policy preset. *(The carve-out is
   built and kept; the shipped image was withdrawn by ADR-0013 in favor of
   [`containerization.md`](containerization.md).)*
8. `.mcp.json` discovery → `McpManager` wiring.

## 5. P1 implications

- P1 (`basis run`) wants mentra gap #3 (skills enumeration + multi-root, [mentra#8]) for
  surfacing skills, but nothing blocks a first cut — skills can wait. Profiles are no
  longer a gap: use `agent::ToolProfile` directly (see §4a).

[mentra#8]: https://github.com/oops-rs/mentra/issues/8
- The `SessionEvent` broadcast channel is the single spine: `run --json`, ACP, and any future
  UI all consume the same stream. Design the JSONL schema once, version it in the first line
  (pi's header-version lesson).
- Branching (mentra gap #1) is P5-depth, not P1 — resume alone is enough to start.
