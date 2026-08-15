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
| RPC / headless mode | `spawn --json` event stream (`run` is a compatibility alias) + **ACP** (standard, not bespoke) ✅; durable task control over a global data directory, with no resident process of any kind ✅ | built |
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
lan send <ID> "<message>"          # enqueue a follow-up turn and return its message ID
lan send <ID> "<message>" --await  # enqueue, then await that message's reply
lan ask <ID> "<question>"          # send and await the correlated reply
lan wait <ID>                      # repeatable terminal observation
lan wait <ID> --message <MID>       # await/retry one message's reply
lan cancel <ID>                    # downward cancellation request
lan watch <ID>                     # replayable progress observation
lan inbox [ID]                     # bounded message/reply summaries
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
the same ownership rules across CLI processes, and since
[ADR-0019](adr/0019-the-filesystem-is-the-coordination-surface.md) it adds them
on files rather than on a service. An agent is a directory under one global,
workspace-keyed data directory — `LAN_DATA_DIR`, else `XDG_DATA_HOME`, else the
platform data home — holding its metadata, its inbox, its event journal, and,
once it exists, its terminal record. `spawn` returns immediately with an opaque
task handle; `wait`/`watch`/`cancel`/`inbox` resolve that handle straight to
those files, so terminal results stay repeatable after the submitting process
exits.

The liveness contract is the part to read twice: **an agent advances only while
a process is attached to it.** Attaching is taking the agent's `fs2` lock — one
writer, ever — resuming the conversation from mentra's last committed turn, and
checkpointing at each turn boundary; `spawn --await`, `wait`, `ask` and
`send --await` all attach, and a contended lock means a live executor already
holds it, so the caller observes instead of racing it. The terminal record,
written atomically as the executor's last act, is the completion signal: an
agent is resumable iff that record does not exist. Nothing is resident, so
backgrounding belongs to the OS (`&`, `nohup`, tmux, `systemd-run`, CI),
cancellation is honored at the next turn boundary rather than instantly, and a
crash mid-turn loses the in-flight round — re-driving it may repeat that turn's
tool side effects, because a checkpoint restores state and never effects.

The semantics above that survive from ADR-0017 are unchanged by the substrate.
`send` appends an opaque message ID to the inbox file, consumed at the next turn
boundary; `send --await` and `ask` wait for the reply to that message, while
`wait --message` retries the same durable reply without rerunning the task.
Inbox bodies and replies are bounded summaries with truncation metadata.
Attached children inherit the narrower parent deadline and downward
cancellation, and a parent's executor may not write its terminal record while an
attached child lacks one — the scope rule as a single ordering constraint,
carried out by the attached process supervising exactly its own subtree, there
being no resident supervisor left to enforce it. Success settles children in
place; failure or
cancellation request them downward first. A finished worker accepts no new
messages and no new children. `--detached` creates a new root. `watch` tails the
event journal, which makes replay the default rather than a feature, while
terminal state is a separate file, so a slow watcher cannot strand completion.
All of this lives in the binary; `lan-core` remains protocol- and
transport-free.

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
refusal wins, modifications compose, nothing is smuggled past a later guard — and the one
hook lan registers with mentra dispatches each call to the calling workspace's own
`HookRunner`, keyed by the agent's base directory, since a shared runtime is built before
any workspace opens (ADR-0018). The ordering and the short-circuit are therefore lan's
rather than the runtime's. Participants speak in-process interceptors first (registration
order), then global hooks, then workspace hooks, on the rule that **the further a
participant is from the workspace's own data, the earlier it speaks**: a host's compiled
guard can then refuse before a program that arrived with a five-minute-old clone is spawned
at all. Anything that cannot answer denies.

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
    lrt["Runtime — one per process: provider · credential · history · host interceptors"]
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
  ws --> lrt
  lrt --> rt
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
  repository rather than to a prompt — context documents, the resolved model, skills,
  templates, hooks, MCP connections — is settled by `Workspace::open`, and `prepare` mints
  a run from it *synchronously*, because nothing is left to await. A twenty-way fan-out
  therefore reads `AGENTS.md` once. What a run carries of its own is the honestly per-run
  half: the prompt, the session name, the effort, and the bounds. The free functions
  (`run`, `prepare`, `resume`) are wrappers that open a workspace, mint one run, and drop
  it — one resolution path, not two.
- **A runtime is the process, and workspaces borrow it** (ADR-0018). The half of an open
  that was never about the repository — mentra's runtime, the provider and its credential,
  the model *policy*, where history is kept, the host's interceptors, the command
  environment, the approval gate that puts a consequential call to a run's `Approver` — is
  `Runtime`, built synchronously and shared through an `Arc` by every workspace opened on
  it, so N repositories cost one provider resolution rather than N. `Workspace::open(path)`
  is unchanged sugar over a private one bound to that path, so the one-repository host never
  meets the noun. What stays per workspace is what a repository says: hooks, `ShellAccess`,
  the `.git` carve-out — enforced on a shared runtime by the single dispatch hook lan
  registers, since mentra fixes hooks at build time and workspaces arrive later — and its
  MCP connections, which are minted from its own config and die with it while the tool
  registry underneath is the runtime's.
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
  protocol's session id, so `session/load` is mentra's `Runtime::resume_session` and lan
  stores no mapping of its own (ADR-0007). A session outlives a turn, which is what makes
  conversation and resume possible at all; compaction wires to context-pressure events. Which
  conversations belong to *this* workspace is a tag mentra keeps on each row, and
  `WorkspaceBuilder::open` is where lan sets it — until it did, `session/list` filtered on a
  tag lan never wrote and so returned nothing, whatever had been persisted. Since ADR-0018
  that holds for a workspace on its own private runtime, which is every `Workspace::open`
  and every path the binary takes; mentra fixes the tag per runtime at build time, so rows
  minted on a *shared* runtime carry `"lan:runtime"` and stay out of the per-workspace lists
  until a per-session override lands upstream. Where the rows live is the caller's to say,
  on the runtime that owns them: `RuntimeBuilder::with_store_dir` names a directory, and
  `with_ephemeral_history` says nowhere and takes an in-memory store instead.
- **The mentra/lan split** (same author owns both): anything a *different* harness could also
  want — session branching, compaction lifecycle, hook points, MCP client — belongs in mentra.
  lan keeps conventions and protocol: AGENTS.md/skills/template discovery, ACP mapping, the
  CLI grammar.
- **Confinement**: the boundary is the OS's and lan ships no instance of it (ADR-0013,
  amending ADR-0004). Shell and background execution are on by default; a run holds the
  authority of the account that starts it, and lan never claims otherwise. In-process there
  is hygiene only — each agent bounded to its own workspace directory, and a rule that keeps
  `.git/hooks` and `.git/config` read-only to the *file tools* (codex's anti-escape
  carve-out), which a shell redirect walks past. [`containerization.md`](containerization.md)
  documents the read-only-root pattern lan used to ship; a native per-command sandbox
  (Seatbelt on macOS, bubblewrap+seccomp on Linux, codex's `workspace-write` design) stays
  parked in [`proposals/0002`](proposals/0002-native-sandbox.md) as an *optional* later
  layer, not a return to denying commands by default.

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

## 8. Operator notes

Behavior a caller can observe but the sections above do not describe. The embedding
counterpart is [`embedding.md`](embedding.md).

### Where task state lives, and what E2 does not migrate

One root holds everything durable about local tasks: `LAN_DATA_DIR` if set, else
`XDG_DATA_HOME/lan` when that is absolute, else the platform data home
(`~/Library/Application Support/lan`, `~/.local/share/lan`, `%APPDATA%\lan`). It is created
private — `0700` where the platform has file modes — and under it each workspace gets a
directory keyed by a digest of its canonical path:

```
<root>/workspaces/<key>/store          mentra's conversations for that workspace
<root>/workspaces/<key>/agents/<task>  meta.json · inbox.json · events.jsonl · terminal.json …
```

Keying on a digest rather than on the path text is what keeps path-length limits out of the
correctness story, and each `spawn` reads back the workspace path recorded beside the digest,
so a collision is an error naming the key and both paths rather than two repositories quietly
sharing agents. Nothing under the root holds a credential: the executor is whichever process
attached, carrying that shell's environment.

The registry the daemon kept is gone with it, and **none of it is migrated**. `LAN_REGISTRY_DIR`
no longer exists, pre-E2 task handles do not resolve, and the conversations that daemon
persisted are not recovered — it filed them beside its registry under `XDG_RUNTIME_DIR` (or the
temp directory), which the platform may erase between boots. A container or CI runner that
should resume yesterday's agents therefore mounts the data root, not just the workspace:
[`containerization.md`](containerization.md) has the volume.

### Effort, providers, and custom endpoints

`--effort` accepts exactly `low`, `medium`, `high`, `xhigh`, or `max`.
LAN keeps those values provider-neutral: Responses-family APIs receive
`reasoning.effort`, while Anthropic receives `output_config.effort` and enables
adaptive thinking only on models that support it. Provider/model combinations
without a requested tier fail explicitly instead of silently lowering it;
omitting the flag leaves the provider default unchanged.

Any OpenAI-compatible endpoint works too — a gateway, a proxy, or a local
server. Paste the URL as published; the trailing `/v1` is handled:

```sh
export LAN_BASE_URL=http://127.0.0.1:3455/v1
export LAN_API_KEY=…
lan spawn --model gpt-5.6 "explain the module layout"
```

Custom endpoints use complete local transcript replay and do not automatically
send `previous_response_id`. That optional extension is not part of LAN's
compatibility assumption; native provider presets retain Mentra's Hybrid state
chaining.

### The two meanings of stdin, and `session/list`

An editor spawning lan and a shell pipe look identical from inside the process — both are a
non-TTY stdin with no arguments — so `cat prompt.txt | lan` cannot be detected as a prompt
without breaking every editor. Instead of waiting silently on prose, the server answers once
the input proves it was never a client:

```
lan: expected an ACP client on stdio
next: use `lan spawn -` for a prompt or `lan serve --acp` for ACP
```

`session/list` works as of the interception wave, and had not before: lan filtered listings
by the workspace a conversation belongs to while filing every conversation under mentra's
`"default"` tag, so no list ever matched. Conversations from before the fix keep the old
tag and do not appear in a list — but none of them is stranded, because resuming looks a
conversation up by id and never by tag, and mentra re-files one under its workspace the
first time it is resumed and used.

### Hooks: the `shell` → `spawn` migration

An entry scoped `"tools": ["shell"]` no longer fires. Nothing errors — the name the model
calls is now `spawn`, and a `tools` list matches on the exact name, so the hook simply stops
running. Match `spawn` instead; a hook that wants commands and not delegations reads the
call's own input, where `input` is the string the model wrote and a single leading `!`
(never `!!`) is what makes it a command (ADR-0016).

### The recurring-run loop, written out

lan ships no scheduler: an interval belongs to whatever already runs things on your machine
— cron, systemd, CI, a tokio task in your own binary. What lan ships instead are the two
pieces that are easy to get wrong, and the loop is composition
([ADR-0014](adr/0014-watch-retired-runs-are-boundable.md)):

```sh
last=""
while :; do
  now=$(lan fingerprint)
  if [ "$now" != "$last" ]; then
    lan spawn --json --deadline 10m --tool-budget 40 \
        "check for newly introduced TODOs and summarize them" > run.jsonl
    case $? in
      0) last=$now ;;                          # only a clean run moves the baseline
      3) echo "bound tripped; retry next tick" >&2 ;;
      *) echo "run failed" >&2 ;;
    esac
  fi
  sleep 1800
done
```

`lan fingerprint` prints a digest over `git ls-files` — path, length, mtime, plus `HEAD` —
so `.gitignore` is honored and `.git`'s own churn is ignored:

```
$ lan fingerprint
cea476f305ecf3f5
```

Every uncertain case reports *changed* rather than unchanged: a false "changed" costs tokens,
while a false "unchanged" would silently stop the loop doing anything at all. Recording the
baseline only after a run you consider successful is the caller's policy, because the caller
is where the definition of "successful" lives — above, that is the `0` arm.
