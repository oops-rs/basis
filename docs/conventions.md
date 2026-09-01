# Conventions

Every file, directory, and environment variable basis reads, in one page. A
reference: what each one is, where it lives, and which wins when two say the
same thing. The reasoning is in the module docs and the ADRs; this is the map.

Two scopes recur throughout:

- **workspace** — the directory a run was opened on (`-C`, ACP's `cwd`, or the
  path a Rust host passed).
- **global config directory** — `$BASIS_CONFIG_DIR`, else
  `$XDG_CONFIG_HOME/basis`, else `$HOME/.config/basis`. Files there are the
  *user's*, not any repository's, and are the weaker of the two.

Inside the config directory the names are undotted (`config.json`, not
`.basis/config.json`): a hidden file inside a directory that exists to hold
configuration would be hiding it from the person who put it there.

## Files

| What | Workspace | Global | Precedence |
| --- | --- | --- | --- |
| Instructions | `AGENTS.md`, else `CLAUDE.md` | `AGENTS.md`, else `CLAUDE.md` | global → each ancestor outermost-inward → workspace root; **all are used**, later is more specific |
| Model choice | `.basis/config.json` | `config.json` | workspace over global, key by key |
| Skills | `.basis/skills/`, `.agents/skills/` | `<config dir>/skills/`, `$HOME/.agents/skills/` | all four layer, most specific first; a nearer root shadows a *name*; each disables independently on `SkillsConfig` |
| Prompt templates | `.basis/templates/*.md` | `<config dir>/templates/*.md` | workspace shadows global by name |
| Declared tools | `.basis/tools.json` | `tools.json` | host-supplied → workspace → global by tool name |
| Subprocess hooks | `.basis/hooks.json` | `hooks.json` | runtime interceptors → host-supplied → global → workspace; all matching entries run until the first refusal |
| MCP servers | `.mcp.json` | `mcp.json` | client-supplied → workspace → global, by server name |
| Memories | `memory/` beside the runtime's store dir | `<config dir>/memory/` | workspace shadows global by memory name |

A missing file is never an error. A file that exists and cannot be read or
parsed always is: the operator wrote it meaning something, and a silently
skipped file is a capability the model's instructions assume and will not find.

There is **no parent walk** for `.mcp.json`, `.basis/tools.json`,
`.basis/hooks.json` or `.basis/config.json`. Instructions are prose and a
monorepo's house rules should reach every crate inside it; these four name
programs to run and credentials to run them with, and inheriting one from a
directory nobody pointed basis at means running a program nobody chose.

### `AGENTS.md` / `CLAUDE.md`

The whole file, rendered into the system prompt weakest-first. `CLAUDE.md` is
read only in a directory that has no `AGENTS.md` — *present* decides, not
*non-empty*, so which file is in effect never depends on its contents. Named in
`run_started`. A host can replace or append to the rendered result with
`WorkspaceBuilder::with_system_prompt`, or with `--system-prompt` /
`--append-system-prompt` on `spawn` and `serve`. `ContextConfig::none()` turns
discovery off entirely — neither name is read, in the workspace, an ancestor,
or the global directory — while workspace path validation still runs.

### `.basis/config.json`

What this repository says about which model runs in it.

```json
{
  "schema": 1,
  "provider": "anthropic",
  "model": "claude-sonnet-4-5-20250929",
  "effort": "high"
}
```

| Key | Value | Notes |
| --- | --- | --- |
| `schema` | `1` | required |
| `provider` | `anthropic`, `openai`, `gemini`, `openrouter`, `ollama`, `lmstudio` | selects the preset endpoint and the key variable |
| `model` | a model id | `--model`'s value |
| `effort` | `low`, `medium`, `high`, `xhigh`, `max` | the default when a run asks for none |
| `base_url` | an OpenAI-compatible endpoint | **global file only** |

`${VAR}` and `${VAR:-default}` expand in every string value. Unknown keys are
an error. There is no `api_key` key: a credential belongs to the environment.

`base_url` in a workspace file is **refused by name**, not ignored. `.mcp.json`
and `.basis/hooks.json` name programs to run, and a program is bounded by
whatever confines the process; a `base_url` redirects the traffic carrying the
credential basis just read out of the environment, and a leaked secret is
bounded by nothing.

Precedence, strongest first: **CLI flag or explicit builder call → workspace
file → global file → environment → basis's default.** An ACP client's
`session/set_config_option` sits above all of it, because it changes a live
session after this ladder has already settled what it opened with.

### `.basis/skills/`, `.agents/skills/`

`SKILL.md` per directory; loaded by name on demand, so only descriptions cost
context. The `.agents` spellings are what other harnesses read and their
*path* is not configurable — a fixed path is what makes a shared convention
shared — but every one of the four roots switches off independently on
`SkillsConfig`: `workspace_subdir` and `global_dir` also say *where* (`None`
disables), `shared_workspace_dir` and `shared_home_dir` are on/off only. Within
a scope the basis-specific root comes first.

Frontmatter `disable-model-invocation: true` (or `disable_model_invocation`)
keeps a skill out of the list the model is shown and makes `load_skill` refuse
it. basis still reports it, marked `model_invocable: false`, so a host can
offer it to a person. basis does not turn one into a `/name` command — that is
what `.basis/templates/` is for.

### `.basis/templates/`

Markdown whose body is a prompt, with optional YAML frontmatter
(`description`, `argument-hint`). `$ARGUMENTS` and `$1`, `$2`… substitute. A
nested path is a namespace: `git/commit.md` is `git:commit`. ACP clients get
each as a command.

A prompt is read as an invocation when its **first token** is `/` plus a name —
`/git:commit the parser fix`. Only the first token, and a name never contains
`/`, so `basis "/usr/bin/x crashes on startup"` is a bug report and passes
through untouched. At a shell a name that matches nothing is refused rather
than sent, with the names that exist; `basis spawn -` reads a prompt beginning
with a literal `/` from stdin.

### Commands basis answers itself

One name is basis's rather than the workspace's, and it is offered to every ACP
client whatever the repository holds, because it acts on the conversation
rather than on the workspace:

| Command | What it does |
| --- | --- |
| `/compact [what to keep]` | Summarizes the conversation so far and continues from the summary. The argument is *added* to the standing continuity requirements, not substituted for them. |

**A built-in wins the name.** A `.basis/templates/compact.md` still loads and is
still discovered, but it is not offered as a command and `/compact` reaches
basis. The rule has to point one way — two commands with one name is a coin
flip the client makes — and this is the direction whose loss is recoverable:
the template's author can rename the file, where a person whose only way to
compact a conversation had been silently replaced by somebody else's prompt
could do nothing at all.

### `.basis/tools.json`

```json
{
  "schema": 1,
  "tools": {
    "deploy": {
      "description": "...",
      "input_schema": {"type": "object"},
      "command": ["./scripts/deploy"],
      "cwd": ".",
      "env": {"TOKEN": "${DEPLOY_TOKEN}"},
      "timeout_ms": 120000,
      "side_effect": "process"
    }
  }
}
```

The program is exec'd directly — no shell — with the tool's JSON input on
stdin. `side_effect` is `process` (default) or `external`; there is no
read-only value, so every declared tool reaches the approver.

An embedding host may put final typed declarations in
`ToolsConfig::with_supplied`. They outrank file declarations by name and are
validated by the same rules, but are not `${VAR}`-expanded and are not reported
as files. `without_discovery()` retains this list while reading neither tool
manifest.

`input_schema` is checked against each call before the approver is asked and
before the program starts: a missing `required` field, a wrong scalar type, a
value outside an `enum`, or a property the schema never named when it sets
`additionalProperties: false`. The check is partial by design — it ignores
keywords it does not implement rather than refusing a call it cannot judge —
so a program that depends on a constraint beyond those still checks its own
stdin.

### `.basis/hooks.json`

```json
{
  "schema": 1,
  "hooks": [
    {"name": "guard", "command": ["./scripts/guard"], "tools": ["spawn"],
     "event": "pre_tool_use", "timeout_ms": 5000, "on_failure": "deny"},
    {"name": "no-secrets", "command": ["./scripts/no-secrets"],
     "event": "post_tool_use"}
  ]
}
```

An embedding host may put typed entries in `HooksConfig::with_supplied`. They
run before global and workspace file hooks; same-name entries do not shadow,
because hooks compose. Runtime interceptors still speak first.

`name` and `command` are required; `tools` absent means every tool, `event` is
`pre_tool_use` (the default) or `post_tool_use`, `timeout_ms` defaults to five
seconds, and `on_failure` defaults to `deny` — a hook that cannot speak is a
control the operator believes is in place, so prefer the failure that announces
itself.

A hook receives JSON on stdin and answers on stdout. Any language. The chain is
host interceptors → global hooks → workspace hooks, and the first refusal
short-circuits. `tools` matches the exact tool name, so an entry naming a tool
that was renamed stops matching silently — `shell` became `spawn`, and `files`
became the split `read`/`write`/`edit`/`ls`/`grep`/`glob`. One entry is asked at
one event; a guard that wants a say on both sides of a call writes two.

**`pre_tool_use`** — asked before the call. The request carries `hook_schema`,
`event`, `workspace`, `agent_id`, `tool_call_id`, `tool_name` and `input`
(parsed when the tool's input is JSON, the raw string when it is not). The
answers are `allow`, `deny` with a reason the model reads as the call's error,
and `modify` with a replacement `input`.

**`post_tool_use`** — asked after the call, before the model is shown what it
returned. The same request with two more fields: `output` (a structured result
as itself, a text result as a JSON string) and `is_error`; `input` is what the
tool actually *ran* with, after any `modify`. The answers are `allow` — keep the
result as it is — `replace` with an `output` and optionally an `is_error` (say
nothing about it and the tool's own verdict stands), and `deny`, which shows the
model the reason in place of the output, marked as an error.

Nothing at `post_tool_use` can stop anything: the tool has run, and the event
stream already carried its real result to every subscriber, unmodified. What
this event decides is what the *model* reads — which is where a question like
"did that command print a credential" can first be answered at all, since the
output is not knowable from the arguments. A guard that must stop something
belongs before the call.

Both events share one `hook_schema` and one envelope. A hook that only declared
`pre_tool_use` sees byte-identical requests to the ones it always did.

### `.mcp.json`

The format other agents already read, so basis reads theirs rather than
inventing a spelling:

```json
{"mcpServers": {"fs": {"command": "npx", "args": ["-y", "…"], "env": {"T": "${TOKEN}"}}}}
```

`command` means stdio, `url` means the HTTP+SSE transport; `type` says which
when the shape is ambiguous, and `"http"` (or `"streamable-http"`) selects
Streamable HTTP. A bare `url` with no `type` still means SSE, deliberately: a
file written before the third transport existed keeps its meaning. Unknown
keys are tolerated — the file is shared with other agents. `${VAR}` expands. A file
that exists but names no `mcpServers` is an error, because a typo would
otherwise disable every server silently. An ACP client's `session/new` servers
outrank both files. A server name may not contain `__` or end in `_`: mentra
namespaces a bridged tool as `mcp__{server}__{tool}` and recovers the split
on the first `__` it finds, so a name like `evil__foo` would be parsed back
as server `evil`, and a name like `evil_` joins its trailing `_` to a tool's
leading `_` the same way — `evil_` with tool `_thing` encodes identically to
`evil` with tool `__thing`. The rule applies wherever a server name comes
from, not just this file.

### Memory files

Memory is files, not a subsystem: one `.md` per memory, YAML frontmatter
naming `name`, a one-line `description`, and `type` (`user`, `feedback`,
`project`, or `reference`), body free-form. Two roots: `memory/` in the global
config directory, and — when the runtime is bound to this one workspace
(`Workspace::open`'s private path) and keeps its history in a named directory
(`RuntimeBuilder::with_store_dir`, which the CLI always does) — the sibling
`memory/` beside that store, so the CLI's memories live at
`<data root>/workspaces/<key>/memory`. Ephemeral or default history names no
directory, so there is no per-workspace root then. **On a shared runtime the
derived root is always absent, whatever its store dir is** — a store dir there
is one runtime-wide fact, not any one workspace's, and deriving from it would
hand every workspace borrowing that runtime the same directory, each reading
the others' memory index into its own prompt. A `WorkspaceMemoryRoot::Dir`
named explicitly is unaffected: naming a path is the host's own
responsibility, shared runtime or not.
`WorkspaceBuilder::with_memory` overrides either root or disables discovery.

At `Workspace::open` each file's frontmatter is read — never the body — and an
index (name, one line, path) is appended to the system prompt after the
context documents; `SystemPrompt::Replace` removes it with everything else,
and zero memories render no block at all. There is no memory tool and no
database: recall is `read`, search is `grep`, writing or revising a memory is
`write` and `edit`, and on a private runtime both roots join the file tools'
allowed read and write roots so those calls reach them (a shared runtime's
policy is fixed at build and cannot carry them, so writes there are refused).
A memory file that exists and cannot be parsed fails the open, naming the
file. Memories are not named in `run_started` — the index is prompt, not
schema.

## Directories basis writes

| What | Where |
| --- | --- |
| Tasks, conversations, event journals | `$BASIS_DATA_DIR`, else an absolute `$XDG_DATA_HOME`, else the platform data home — `0700` |

One agent is one directory under `<data root>/workspaces/<key>/agents/<id>`,
holding `meta.json`, `inbox.json`, `events.jsonl` and — written last —
`terminal.json`, whose existence is the completion signal. mentra's store lives
beside them at `<data root>/workspaces/<key>/store`, with compaction snapshots
in `transcripts/` under it
([ADR-0019](adr/0019-the-filesystem-is-the-coordination-surface.md)). Since
0.7 that store is plain files too — `agents/`, `rules.json`, `runs.jsonl`
under the store directory, mentra's file-backed layout
([ADR-0023](adr/0023-basis-persists-to-files.md)) — and a `runtime.sqlite`
found there is a basis ≤0.6 store this build refuses by name rather than
reads, migrates, or shadows.

## Environment

| Variable | What it does |
| --- | --- |
| `BASIS_CONFIG_DIR` | The global config directory. Overrides `$XDG_CONFIG_HOME/basis` and `$HOME/.config/basis` |
| `BASIS_DATA_DIR` | Where tasks and conversations are kept. Overrides `$XDG_DATA_HOME` and the platform data home. A relative path is resolved once, against the directory current when the root is opened, and passed on to spawned commands absolute — so a process that changes directory, and a nested `basis` that inherits the variable, read one directory rather than one each |
| `BASIS_BASE_URL` | An OpenAI-compatible endpoint. `OPENAI_BASE_URL` is read after it, because gateways already tell their users to set that one |
| `BASIS_API_KEY` | The key for that endpoint. `OPENAI_API_KEY` is read after it |
| `BASIS_TASK_ID` | Set by basis on every process a run spawns: which task this is. Read by a nested `basis` to route its own `spawn` |
| `BASIS_PARENT_TASK_ID` | Set by basis on the same processes when the task has a parent |

Provider credentials are read by the names the ecosystem already uses, in this
order when several are exported: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
`GEMINI_API_KEY`, `OPENROUTER_API_KEY`. A variable set to whitespace counts as
unset. A base URL — passed, configured, or exported — outranks auto-detection,
because pointing at an endpoint is always deliberate. A host-supplied provider
instance (`RuntimeBuilder::with_provider_instance`) reads none of them:
resolution is skipped whole, and a provider, base URL or key named beside the
instance is refused rather than outranked.

`XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `HOME` are consulted only as the
fallbacks named above.
