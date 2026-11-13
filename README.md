# basis

**An embeddable AI agent harness for Rust applications, multi-protocol serving, and developer workflows.**

[![CI](https://github.com/oops-rs/basis/actions/workflows/ci.yml/badge.svg)](https://github.com/oops-rs/basis/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/basis.svg)](https://crates.io/crates/basis)
[![docs.rs](https://docs.rs/basis/badge.svg)](https://docs.rs/basis)
[![MSRV: 1.95+](https://img.shields.io/badge/rust-1.95+-orange.svg)](https://blog.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

---

Basis bridges the gap between raw LLM agent runtimes and production applications. Built on [Mentra](https://github.com/oops-rs/mentra), Basis packages the generic harness glue—workspace discovery, context conventions, multi-protocol serving, durable execution, and safety boundaries—without imposing opinionated presentation, custom TUIs, or fixed domain pipelines.

> **Rent the loop, own the glue.** The core has no opinions: task-specific behavior enters via prompts, templates, and workspace conventions, never via harness code.

---

## Highlights

- 🧩 **Embeddable Rust SDK** — Library-first design (`basis`) with no transport, protocol, or terminal dependencies in the core. Open a `Workspace`, mint runs, stream typed events, and set execution bounds.
- 🔌 **Standard Protocol Serving** — Native support for the **Agent Client Protocol (ACP)** over stdio and WebSockets (for Zed, JetBrains, and Web UIs) and the **Agent-to-Agent (A2A v1.0)** protocol for multi-agent delegation.
- 📁 **Durable Filesystem-Coordinated Tasks** — Headless, asynchronous execution with persistent task handles, inboxes, and message correlation. Advances only while a process is attached (`fs2` file locks)—no background daemons or resident schedulers required.
- 🧭 **Workspace Conventions & Discovery** — Automatically discovers `AGENTS.md` (and `CLAUDE.md`), hierarchical skills, prompt templates (`.basis/templates/`), declared tools, and Model Context Protocol (`.mcp.json`) servers.
- 🛡️ **Execution Bounds & Approvals** — Precise constraints on wall-clock deadlines, tool invocation counts, and token budgets. Customizable approval policies (`always`, `never`, `prompt`, or custom trait implementations).
- ⚡ **Clean Layered Architecture** — Six focused crates split by dependency weight, allowing applications to compile only what they actually use.

---

## Crate Architecture

Basis is organized into six crates split by dependency weight ([ADR-0011](docs/adr/0011-layered-crates.md)):

| Crate | Purpose | Key Responsibilities |
|---|---|---|
| [`basis`](basis/) | Core SDK | Workspace discovery, run lifecycle, event streaming, bounds, and execution seams. No network, transport, or terminal dependencies. |
| [`basis-host`](basis-host/) | Host Kit | Adapter-neutral policy, approval gates, session management, and runtime pools ([ADR-0025](docs/adr/0025-basis-host.md)). |
| [`basis-tasks`](basis-tasks/) | Durable Tasks | Filesystem-coordinated task lifecycle, locks (`fs2`), inboxes, journals, and crash recovery ([ADR-0017](docs/adr/0017-structured-agent-concurrency.md), [ADR-0019](docs/adr/0019-the-filesystem-is-the-coordination-surface.md)). |
| [`basis-acp`](basis-acp/) | ACP Adapter | Serves the Agent Client Protocol over stdio or WebSocket bridge for editors and UIs. |
| [`basis-a2a`](basis-a2a/) | A2A Server | Serves workspaces to remote agents over HTTP/JSON and JSON-RPC with workflows and file exchange ([ADR-0028](docs/adr/0028-a2a-is-the-agent-to-agent-protocol.md), [ADR-0050](docs/adr/0050-a-served-agent-may-be-held-to-workflows.md)). |
| [`basis-cli`](basis-cli/) | CLI Binary | Unified command-line interface (`basis`) publishing all features for interactive and automated use. |

---

## Quick Start

### 1. Install the CLI

Requires **Rust 1.95 or newer**:

```sh
cargo install basis-cli --locked    # installs the `basis` binary
```

### 2. Configure a Provider

Set the API key for your preferred provider:

```sh
export ANTHROPIC_API_KEY="your-api-key"
# or:
export OPENAI_API_KEY="your-api-key"
export GEMINI_API_KEY="your-api-key"
export OPENROUTER_API_KEY="your-api-key"
```

For local or custom OpenAI-compatible endpoints (such as Ollama, vLLM, or LM Studio):

```sh
export BASIS_BASE_URL="http://127.0.0.1:11434/v1"
# optional: export BASIS_API_KEY="your-key"
```

### 3. Run a Prompt

Navigate to any repository and run:

```sh
cd /path/to/your/repo
basis spawn --approve prompt "Explain the project structure"
```

> [!NOTE]
> `basis "<prompt>"` is shorthand for `basis spawn "<prompt>"`. Answers are sent to `stdout` and streaming progress goes to `stderr`.

---

## CLI & Durable Tasks

The CLI provides both direct prompt execution and asynchronous task management.

### Prompts & Conversations

```sh
# Shorthand prompt execution
basis "Explain this repository"

# Target a different workspace directory
basis spawn -C ../other-repo "Review recent commits"

# Read prompt from stdin
cat prompt.txt | basis spawn -

# Continue the conversation last worked on in this workspace
basis list
basis spawn --continue "Which parts need more unit tests?"

# Continue a specific historical session
basis spawn --session <TASK_ID> "Explain that finding in detail"
```

### Durable Task Lifecycle

At a shell, `basis spawn` drives the agent to completion. With `--resumable`, it creates a persistent task and prints its handle immediately. **An agent advances only while a process is attached to it**—there are no background daemons:

```sh
# Create a durable task without driving it
basis spawn --resumable "Run comprehensive benchmark suite"   # prints <TASK_ID>

# Attach to drive or observe its terminal result
basis wait <TASK_ID>

# Follow progress without driving
basis watch <TASK_ID>

# Enqueue follow-up messages into the task inbox
basis send <TASK_ID> "Also analyze memory overhead"
basis ask <TASK_ID> "Are the benchmarks finished?"           # enqueues and awaits reply

# Cancel a running task and its attached descendants
basis cancel <TASK_ID>
```

### Bounds & Exit Codes

Enforce resource bounds across runs:

```sh
basis spawn --deadline 10m --tool-budget 40 --token-budget 100000 "Refactor the parser"
basis spawn --json "Summarize architecture" > events.jsonl
```

| Exit Code | Meaning |
|:---:|---|
| `0` | Operation finished successfully. |
| `1` | Run failed or could not start. |
| `2` | Invalid CLI invocation. |
| `3` | A bound tripped (`--deadline`, `--tool-budget`, `--token-budget`); committed work is retained. |

See the [CLI Guide](docs/cli.md) for full argument details and output specifications.

---

## Protocol Servers

Basis can serve any workspace over standard protocols:

```sh
# Serve ACP over stdio (for Zed, JetBrains, or editors launching the process)
basis serve --acp

# Serve ACP over a local WebSocket bridge (for browser-based UIs)
basis serve --bridge --allow-origin https://your-client.example

# Serve A2A (Agent-to-Agent v1.0) over HTTP/JSON and JSON-RPC
basis serve --a2a 127.0.0.1:8080
```

- **ACP** defaults to asking the client for permission approvals before consequential actions.
- **A2A** supports multi-agent collaboration, delegation (`@<agent> <prompt>`), task push notifications, file exchanges, and fine-grained workflow constraints ([ADR-0050](docs/adr/0050-a-served-agent-may-be-held-to-workflows.md)).

---

## Embed in Rust

Add `basis` to your application's `Cargo.toml`:

```toml
[dependencies]
basis = "0.25.0"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

### Minimal Execution Example

```rust
use basis::{AllowAll, NullSink, Workspace};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open the workspace once: settles context documents, skills, and tools
    let workspace = Workspace::open(std::env::current_dir()?).await?;

    // Prepare a run and execute it
    let mut run = workspace.prepare("What does this project do?")?;
    let report = run.execute_with_approver(NullSink, AllowAll).await?;

    if !report.succeeded() {
        return Err(format!("Run failed: {:?}", report.outcome).into());
    }

    println!("{}", report.final_message.unwrap_or_default());
    Ok(())
}
```

### Multi-Turn Conversations & Event Streaming

A `PreparedRun` maintains its session across turns:

```rust
use basis::{AllowAll, CollectingSink, TurnOptions};

// Execute follow-up turn
let second_turn = run
    .send_with_options(
        "Which module has the most dependencies?",
        CollectingSink::new(),
        AllowAll,
        TurnOptions::default(),
    )
    .await?;

println!("{}", second_turn.final_message.unwrap_or_default());
```

### Feature Flags

- **`mcp`** (*default*): Model Context Protocol support (`.mcp.json` discovery and client connections). Disable via `default-features = false` for environments requiring no MCP footprint.
- **`a2a`** (*default*): Agent-to-Agent client support (`.basis/a2a.json` discovery and remote delegation).

See the [Embedding Guide](docs/embedding.md) and [Runnable Examples](basis/examples/) for advanced patterns including custom approvers, event taps, and shared runtime pools.

---

## Workspace Conventions

Basis loads configuration and context files directly from the workspace:

| Path | Purpose |
|---|---|
| `AGENTS.md` / `CLAUDE.md` | Workspace instructions and system prompt context, discovered hierarchically up the directory tree. |
| `.basis/config.json` | Workspace configuration (pins provider, model, and effort). |
| `.basis/skills/`<br>`.agents/skills/` | On-demand skill definitions (`SKILL.md`) discovered and layered across workspace and user roots. |
| `.basis/templates/` | Reusable prompt templates, invokable via `/name <args>` or exposed as ACP commands. |
| `.basis/tools.json` | Subprocess declared tools executing local commands with JSON input/output schemas. |
| `.basis/a2a.json` | Remote agent registry for delegation targets (`@<agent> <prompt>`). |
| `.mcp.json` | MCP server definitions (stdio, SSE, Streamable HTTP). |

### Precedence

Settings are resolved in strict order:
**CLI Flags / Builder Options** > **Workspace Config** (`.basis/config.json`) > **Global Config** (`~/.config/basis/config.json`) > **Environment Variables**.

### Important Environment Variables

- `BASIS_BASE_URL` — Custom endpoint URL for OpenAI-compatible providers.
- `BASIS_API_KEY` — API key for custom endpoint configurations.
- `BASIS_CONFIG_DIR` — Overrides the global configuration directory.
- `BASIS_DATA_DIR` — Overrides where durable tasks and conversation states are stored.

See the [Configuration Reference](docs/conventions.md) for complete details.

---

## Security & Isolation

> [!WARNING]
> **Basis is an agent harness, not an OS sandbox.** Basis runs with the full OS permissions of the host process or user account. While file tools enforce workspace scoping and deny sensitive paths (such as `.git/hooks` and `.git/config`), shell commands can bypass these checks. For untrusted code or tasks, always run Basis inside a container or VM boundary.

### Approval Policies

Consequential tool calls (shell execution, file writes, delegation) are governed by approval modes:
- `always` (*CLI default*) — Consequential calls are executed automatically without prompts.
- `prompt` — Prompts interactively at the terminal (`tty`) before executing consequential calls.
- `never` — Automatically denies all consequential tool calls (read-only execution).

Use `--no-shell` to disable command execution while still permitting file writes. Refer to the [Isolation Guide](docs/containerization.md) for containerized and rootless deployment configurations.

---

## Documentation

- 📖 [CLI Guide](docs/cli.md) — Comprehensive command syntax, lifecycle verbs, and JSON output specs.
- 🛠️ [Embedding Guide](docs/embedding.md) — Complete Rust SDK integration guide and patterns.
- ⚙️ [Configuration Reference](docs/conventions.md) — Directory conventions, schemas, and precedence.
- 🏗️ [Architecture Overview](docs/ARCHITECTURE.md) — Structural design, invariants, and component boundaries.
- 📋 [Architecture Decision Records (ADRs)](docs/adr/) — Numbered ledger of all accepted design decisions.
- 💡 [Proposal](docs/PROPOSAL.md) — The founding thesis, problems, and core architectural bets.
- 🔒 [Containerization Guide](docs/containerization.md) — OS sandboxing and production isolation patterns.
- 📝 [Changelog](CHANGELOG.md) — Release notes and upgrade guides.

---

## Contributing

We welcome contributions! Please see [CONTRIBUTING.md](CONTRIBUTING.md) for development workflows and testing requirements. All participants are expected to adhere to the [Code of Conduct](CODE_OF_CONDUCT.md).

For security disclosures, please consult [SECURITY.md](SECURITY.md).

---

## License

This project is licensed under the [MIT License](LICENSE).
