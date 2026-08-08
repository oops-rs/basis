# P0 Groundwork

> 2026-08-08 · outputs of ARCHITECTURE.md §6 P0: zentox feedback distilled, pi prior art, and the
> concrete mentra-vs-lan split. Supersedes the provisional table in ARCHITECTURE.md §1 where they
> disagree — this one is based on reading mentra's actual public API.

## 1. Requirements from zentox feedback (`mentra/docs/mentra-api-feedback.md`)

zentox was a real Mentra-based CLI agent. Its friction list, translated into domain-free
requirements for lan (and mentra):

| zentox friction | Requirement | Lands in |
|---|---|---|
| Context-dependent tool sets need hand-rolled `with_tool(...)` gating | First-class tool profiles / runtime presets | **mentra** (its #1 priority, verbatim) |
| App owns operational glue: path scoping, timeouts, output summarization | lan *is* that glue, packaged once — the "recommended CLI integration pattern" mentra wanted to document | **lan** |
| No integration-test story for runtime-plus-tools assemblies | Test harness support for full assemblies; lan's own tests will need it immediately | **mentra** (`test-utils` extension) |
| No realistic end-to-end example | lan is the realistic example — provider setup, policy, custom tools, composition, transcript inspection | **lan** (serves both repos) |

zentox validations to keep: policy roots (working/read/write) map cleanly to workspace scoping;
custom tool registration is already easy; structured transcript history is good for post-run
artifacts. lan's JSONL event stream should expose the same transcript walk zentox used for
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
| MCP client + tool bridge | **exists**: `McpManager`, `McpServerConfig`, stdio client, `McpBridgedTool` | ARCHITECTURE.md said "build" — wrong; lan only wires `.mcp.json` discovery to it |
| Compaction engine | **exists**: `CompactionEngine` trait, `StandardCompactionEngine`, modes, diagnostics | gap vs pi: checkpoint semantics (`retainedTail`), split turns, cumulative file tracking — verify, then extend in mentra |
| Skills loader | **exists but `pub(crate)`**: `runtime/skill.rs` (frontmatter, dedup, recursive discovery) | mentra gap: make public or expose via runtime builder config |
| Permissions | **exists**: `PermissionRequest/Decision`, `RuleStore`, `RememberedRule`, `SessionPermissionHandle` | maps directly onto ACP `session/request_permission` |
| Session event stream | **exists**: `SessionEvent` (token deltas, reasoning deltas, tool queued/started/…) via broadcast | near-1:1 with ACP `session/update` notifications; also the `run --json` payload |
| Structured transcript | **exists**: `AgentTranscript`, `TranscriptItem`, `CompactionSummary` | zentox's record-walking use case |
| Session branching / tree | **absent**: `Session`/`SessionMetadata` only; no fork/branch/snapshot API found in public surface | mentra gap (pi tree model, §2) |
| Round strategy / steering | **exists**: `RoundStrategy`, `SteeringHandle`, `QueueMode` | ACP `session/cancel` + prompt-during-turn map here |
| Teams, background tasks, memory | exists | not needed for lan v1; do not wire |

## 4. The split (decision)

Rule (from AGENTS.md): generic-for-any-harness → mentra; conventions and protocol → lan.

**mentra gaps to file/fix (issues even when fixed immediately):**
1. `feat(session)`: entry-tree branching — pi's `id`/`parentId` model over the SQLite store;
   branch summaries as first-class entries.
2. `feat(compaction)`: checkpoint semantics (`retainedTail` equivalent), split-turn handling,
   cumulative read/modified file tracking in summaries — gap analysis vs pi first.
3. `feat(runtime)`: public skills API (today `pub(crate)` in `runtime/skill.rs`).
4. `feat(runtime)`: tool profiles / runtime presets (zentox priority #1).
5. `feat(test)`: assembly-level test harness in `test-utils` (zentox priority #2).

**lan builds (harness-specific):**
1. AGENTS.md discovery: workspace + parent-dir walk + global; injection into system context.
2. Prompt templates: markdown + args → ACP commands.
3. ACP server: `agent-client-protocol` crate ↔ `SessionEvent`/`PermissionRequest` mapping.
4. `run --json`: JSONL rendering of the `SessionEvent` stream.
5. `watch`: interval scheduler, skip-if-unchanged.
6. Subprocess hooks: exec-a-command JSON in/out, layered on mentra's `RuntimeHook`.
7. Docker packaging + `.git/hooks` write-deny policy preset.
8. `.mcp.json` discovery → `McpManager` wiring.

## 5. P1 implications

- P1 (`lan run`) needs mentra gaps #3 (skills API) and benefits from #4 (profiles); neither
  blocks a first cut — skills can wait, profiles can be hand-rolled then migrated.
- The `SessionEvent` broadcast channel is the single spine: `run --json`, ACP, and any future
  UI all consume the same stream. Design the JSONL schema once, version it in the first line
  (pi's header-version lesson).
- Branching (mentra gap #1) is P5-depth, not P1 — resume alone is enough to start.
