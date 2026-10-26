# basis

Basis is an embeddable AI agent harness for Rust applications and developer
workflows. Open a workspace, run a prompt against it, and handle the result or
stream events into your application. The `basis` library builds on Mentra;
separate crates provide the CLI, durable tasks, and protocol adapters.

- **Run and resume conversations**, with typed output and a shared event stream.
- **Bound work** by time, tool calls, or tokens, including shared budgets across runs.
- **Extend the agent** with workspace instructions, skills, custom tools, MCP
  servers, hooks, and your own approval policy.
- **Use the same harness** from Rust, the command line, ACP clients, or other agents
  over A2A.

## Quick start

Requires **Rust 1.95 or newer** and either a supported provider's API key or a
running OpenAI-compatible endpoint. Install the CLI, then run it in your repository:

```sh
cargo install basis-cli --locked    # installs the `basis` binary
export ANTHROPIC_API_KEY="your-api-key"
cd /path/to/your/repo
basis spawn --approve prompt "Explain the project structure"
```

This example asks at the terminal before consequential tool calls. **Without
`--approve prompt`, CLI runs default to allowing commands and file edits.** Basis
runs with your user account's permissions and **does not provide a sandbox**.
Use `--approve never` to deny consequential calls; `--no-shell` alone still permits
file writes. See [isolation guidance](docs/containerization.md) for an OS boundary.

`OPENAI_API_KEY`, `GEMINI_API_KEY`, and `OPENROUTER_API_KEY` are also supported.
Use `--provider` to choose explicitly when several keys are set, and `--model` to
select a model available from that provider. For a local Ollama server with a
model already installed:

```sh
basis spawn --provider ollama --model qwen3 --approve prompt "Explain the module layout"
```

At a shell, `basis "<prompt>"` is shorthand for `basis spawn "<prompt>"`. Answers go
to stdout and progress goes to stderr. Continue a conversation or limit a run with:

```sh
basis list
basis spawn --continue --approve prompt "Which parts need more tests?"
basis spawn --deadline 10m --tool-budget 40 --approve prompt "Review the tests"
```

Tasks persist on disk, but advance only while a process is attached; there is no
background daemon. See the [CLI guide](docs/cli.md) for task handles, JSON output,
exit codes, and serving ACP or A2A. `basis --help` and `basis spawn --help` list options.

## Embed in Rust

Add to your application's `Cargo.toml`:

```toml
[dependencies]
basis = "0.22.2"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

With a provider configured as above, this complete example opens the current
directory and prints the final answer:

```rust
use basis::{AllowAll, NullSink, Workspace};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = Workspace::open(std::env::current_dir()?).await?;
    let mut run = workspace.prepare("What does this project do?")?;
    let report = run.execute_with_approver(NullSink, AllowAll).await?;

    if !report.succeeded() {
        return Err(format!("Run failed: {:?}", report.outcome).into());
    }
    println!("{}", report.final_message.unwrap_or_default());
    Ok(())
}
```

`AllowAll` permits consequential tool calls. Implement `Approver` for your host's
policy, and replace `NullSink` with an event sink to stream progress.
The [embedding guide](docs/embedding.md) covers conversations, typed results,
bounds, cancellation, tools, and shared runtimes. See the
[runnable examples](basis/examples/) for complete integrations.

The `basis` crate has no server or terminal UI. Add `basis-tasks` for durable task
handles, `basis-host` for shared host policy and session management, or `basis-acp`
and `basis-a2a` for protocol adapters. MCP and the A2A client are default features
that embedders can disable.

## Configuration

Basis discovers `AGENTS.md` (or `CLAUDE.md`), skills, and other workspace files when
it opens a repository. To pin a repository's provider and model for the `basis` CLI,
create `.basis/config.json` (as an SDK, basis reads it only when you pass a `Config`
or a path — see [configuration](docs/conventions.md)):

```json
{
  "schema": 1,
  "provider": "anthropic",
  "model": "claude-sonnet-4-5-20250929"
}
```

Choose a model available to your account. Explicit CLI flags or builder settings
win over workspace configuration, then global configuration, then the environment.
Without a model setting, Basis selects the provider's newest available model.

- Keep credentials in environment variables. Configuration files have no `api_key` field.
- For a custom endpoint, set `BASIS_BASE_URL` and, if needed, `BASIS_API_KEY`.
  For several gateways in front of one model, tried in order, set `gateways` in the
  global `config.json` or `BASIS_GATEWAYS` (see the [configuration reference](docs/conventions.md)).
  Custom URLs use OpenAI's `chat/completions` wire by default; the `openai` provider
  uses Responses. See [endpoint configuration](docs/ARCHITECTURE.md#effort-providers-and-custom-endpoints).
- `BASIS_CONFIG_DIR` overrides the global configuration directory.
  `BASIS_DATA_DIR` overrides where CLI tasks and conversations are stored.
- A `base_url` may appear in the global `config.json`, but is **rejected in workspace
  configuration** so a repository cannot redirect traffic carrying your credentials.

The [configuration reference](docs/conventions.md) lists all files, variables,
default paths, and precedence rules, including tools, hooks, MCP, and remote agents.

## Documentation and contributing

- [CLI guide](docs/cli.md) — commands, task lifecycle, output, and protocol servers.
- [Embedding guide](docs/embedding.md) — Rust SDK and examples.
- [Configuration reference](docs/conventions.md) — workspace and global conventions.
- [Architecture](docs/ARCHITECTURE.md) and [design decisions](docs/adr/) — structure and rationale.
- [Changelog](CHANGELOG.md) — releases and breaking changes. For older installations,
  read the [storage migration notes](docs/ARCHITECTURE.md#where-task-state-lives-and-what-e2-does-not-migrate):
  pre-0.7 conversation databases and daemon-era task handles are not migrated.

ACP is tested against the protocol and its official client library; integration
with Zed and JetBrains has not yet been verified.

See [CONTRIBUTING.md](CONTRIBUTING.md) for development checks and
[SECURITY.md](SECURITY.md) for reporting vulnerabilities. Contributions follow the
[Code of Conduct](CODE_OF_CONDUCT.md).

## License

[MIT](LICENSE).
