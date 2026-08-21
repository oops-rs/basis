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
| Skills | `.basis/skills/`, `.agents/skills/` | `<config dir>/skills/`, `$HOME/.agents/skills/` | all four layer, most specific first; a nearer root shadows a *name* |
| Prompt templates | `.basis/templates/*.md` | `<config dir>/templates/*.md` | workspace shadows global by name |
| Declared tools | `.basis/tools.json` | `tools.json` | workspace shadows global by tool name |
| Subprocess hooks | `.basis/hooks.json` | `hooks.json` | both run, global first; the first refusal wins |
| MCP servers | `.mcp.json` | `mcp.json` | client-supplied → workspace → global, by server name |

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
`--append-system-prompt` on `spawn` and `serve`.

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
file → global file → environment → basis's default.**

### `.basis/skills/`, `.agents/skills/`

`SKILL.md` per directory; loaded by name on demand, so only descriptions cost
context. The `.agents` spellings are what other harnesses read and are not
configurable — a fixed path is what makes a shared convention shared. Within a
scope the basis-specific root comes first.

### `.basis/templates/`

Markdown whose body is a prompt, with optional YAML frontmatter
(`description`, `argument-hint`). `$ARGUMENTS` and `$1`, `$2`… substitute. A
nested path is a namespace: `git/commit.md` is `git:commit`. ACP clients get
each as a command.

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

### `.basis/hooks.json`

```json
{
  "schema": 1,
  "hooks": [
    {"name": "guard", "command": ["./scripts/guard"], "tools": ["spawn"],
     "event": "pre_tool_use", "timeout_ms": 5000, "on_failure": "deny"}
  ]
}
```

`name` and `command` are required; `tools` absent means every tool, `event`
has one value today (`pre_tool_use`), `timeout_ms` defaults to five seconds,
and `on_failure` defaults to `deny` — a hook that cannot speak is a control
the operator believes is in place, so prefer the failure that announces
itself.

A hook receives JSON on stdin and answers `allow`, `deny` with a reason the
model sees, or `modify` with a replacement input. Any language. The chain is
host interceptors → global hooks → workspace hooks, and the first refusal
short-circuits. `tools` matches the exact tool name, so an entry naming a tool
that was renamed stops matching silently — `shell` became `spawn`, and `files`
became the split `read`/`write`/`edit`/`ls`/`grep`/`glob`.

### `.mcp.json`

The format other agents already read, so basis reads theirs rather than
inventing a spelling:

```json
{"mcpServers": {"fs": {"command": "npx", "args": ["-y", "…"], "env": {"T": "${TOKEN}"}}}}
```

`command` means stdio, `url` means the HTTP+SSE transport; `type` says which
when the shape is ambiguous. Streamable HTTP is refused by name. Unknown keys
are tolerated — the file is shared with other agents. `${VAR}` expands. A file
that exists but names no `mcpServers` is an error, because a typo would
otherwise disable every server silently. An ACP client's `session/new` servers
outrank both files.

## Directories basis writes

| What | Where |
| --- | --- |
| Tasks, conversations, event journals | `$BASIS_DATA_DIR`, else an absolute `$XDG_DATA_HOME`, else the platform data home — `0700` |

One agent is one directory under `<data root>/workspaces/<key>/agents/<id>`,
holding `meta.json`, `inbox.json`, `events.jsonl` and — written last —
`terminal.json`, whose existence is the completion signal. mentra's store lives
beside them at `<data root>/workspaces/<key>/store`, with compaction snapshots
in `transcripts/` under it
([ADR-0019](adr/0019-the-filesystem-is-the-coordination-surface.md)).

## Environment

| Variable | What it does |
| --- | --- |
| `BASIS_CONFIG_DIR` | The global config directory. Overrides `$XDG_CONFIG_HOME/basis` and `$HOME/.config/basis` |
| `BASIS_DATA_DIR` | Where tasks and conversations are kept. Overrides `$XDG_DATA_HOME` and the platform data home |
| `BASIS_BASE_URL` | An OpenAI-compatible endpoint. `OPENAI_BASE_URL` is read after it, because gateways already tell their users to set that one |
| `BASIS_API_KEY` | The key for that endpoint. `OPENAI_API_KEY` is read after it |
| `BASIS_TASK_ID` | Set by basis on every process a run spawns: which task this is. Read by a nested `basis` to route its own `spawn` |
| `BASIS_PARENT_TASK_ID` | Set by basis on the same processes when the task has a parent |

Provider credentials are read by the names the ecosystem already uses, in this
order when several are exported: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
`GEMINI_API_KEY`, `OPENROUTER_API_KEY`. A variable set to whitespace counts as
unset. A base URL — passed, configured, or exported — outranks auto-detection,
because pointing at an endpoint is always deliberate.

`XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `HOME` are consulted only as the
fallbacks named above.
