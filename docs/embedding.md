# Embedding basis — the `basis` SDK

> The reference for the surface the [README](../README.md) summarizes. Design rationale is in
> [`ARCHITECTURE.md`](ARCHITECTURE.md); the decisions are in [`adr/`](adr/); the ledger of the
> SDK-first transition is [`REDESIGN.md`](REDESIGN.md).

In-process, the harness is **`basis`** — the run lifecycle, workspace discovery, the
event stream, and the seams, with no protocol, no transport, and no terminal code in the
graph. `basis-acp` is the ACP adapter over it and the `basis` binary is the CLI over both, so
an embedding host compiles only what it runs
([ADR-0011](adr/0011-layered-crates.md)):

```toml
[dependencies]
basis = "0.1"   # unpublished so far — a git or path dependency until it isn't
```

MCP is a default-on `mcp` feature rather than a fixed part of the core:
`default-features = false` compiles a `basis` with no MCP concept at all — no `.mcp.json`
discovery, no servers registered ([ADR-0012](adr/0012-one-contract-many-bindings.md)).

## A workspace opens once and mints runs

Opening a workspace settles everything that belongs to the repository rather than to the
prompt — context documents, the resolved model, skills, templates, hooks, MCP connections.
Minting a run from it is then synchronous, because nothing is left to await
([ADR-0010](adr/0010-the-crate-is-the-workflow-surface.md)):

```rust
let workspace = basis::Workspace::open("/repo").await?;

let mut run = workspace.prepare("what does this repo do?")?;
let report = run.execute(basis::CollectingSink::default()).await?;
```

That is the shape to reach for whenever a host sends more than one prompt at a repository:
twenty runs read `AGENTS.md` once, resolve the model once, and share one set of MCP
connections. A `Workspace` is `Send + Sync`, so the runs can be spawned tasks.

For a conversation rather than a one-shot, keep the run and send again — the session
survives the turn, so the model sees everything said so far:

```rust
run.send("and which of those is riskiest?", sink, basis::AllowAll).await?;
```

`run.agent_id()` is the handle `Workspace::resume` takes, so a later process can pick the
same conversation back up.

When one prompt really is the whole job, the free functions are the same path with the
workspace opened and dropped around it — the binary is a thin shell over this:

```rust
let report = basis::run(
    basis::RunConfig::new("/repo", "summarize the recent changes"),
    basis::CollectingSink::new(),
).await?;
```

The bounds are builders on either shape — `RunConfig` for a one-shot, `RunSpec` for a run
minted from a workspace — and `report.stopped_by` carries the distinction the exit code
makes: `Some(basis::Bound::Deadline)`, `Some(basis::Bound::ToolBudget)`,
`Some(basis::Bound::TokenBudget)`, or `None` when the work is what ended the run:

```rust
let config = basis::RunConfig::new("/repo", "bump the deps and fix the fallout")
    .with_deadline(Duration::from_secs(600))
    .with_tool_budget(40)
    .with_token_budget(200_000);
```

## One runtime, many workspaces

Half of what opening used to settle was never the repository's: the provider and its
credential, where history is kept, the host's own interceptors. That half is a `Runtime`
([ADR-0018](adr/0018-the-runtime-owns-the-process.md)), and a host serving more than one
repository builds one and lends it out:

```rust
use std::sync::Arc;
use basis::{Runtime, Workspace};

let runtime = Arc::new(Runtime::builder().build()?);

let one = Workspace::builder("/repo/one").with_runtime(Arc::clone(&runtime)).open().await?;
let two = Workspace::builder("/repo/two").with_runtime(runtime).open().await?;
```

`build()` is synchronous and reaches no network — it resolves the provider and finds the
credential, and MCP servers are a workspace's business — so N repositories cost one provider
resolution and one history store rather than N. `Workspace::open("/repo")` is unchanged by
the split: it is the same call with a private runtime built behind it, bound to that path,
and a single-repository host never meets the type.

What sharing shares is the runtime's; the rest stays the workspace's. The model is a
*policy* on `RuntimeBuilder::with_model` that a workspace overrides with its own
`with_model`, and the resolved id is the workspace's fact either way. Skills land on the one
tool registry, so a skill one workspace registered is loadable by another's runs, while
`Workspace::skills` still reports only its own. MCP connections are workspace-owned — minted
from that repository's config, shut down when it drops — and every roster hides the `mcp__*`
tools of servers its workspace does not own, so a name two repositories both configure is
claimed once and suffixed for the second. Hooks, `ShellAccess`, and the `.git` carve-out
remain per workspace as well, enforced on a shared runtime by the one dispatch hook basis
registers.

One thing does not work yet on a shared runtime: mentra fixes the persistence tag per
*runtime* at build time, so conversations minted there are filed under `"basis:runtime"`
instead of under their workspace, and `store::list` — ACP's `session/list` — does not find
them for it. Nothing is stranded, because resume takes an agent id and never a tag, and a
row re-files itself the next time it persists under a runtime that knows its workspace. The
private path is unaffected: every `Workspace::open` tags exactly as it always did.

The knobs ADR-0018 moved are `RuntimeBuilder`'s now — `with_provider`, `with_base_url`,
`with_api_key`, `with_store_dir`, `with_ephemeral_history`, `with_interceptor`,
`with_command_environment` (whose pairs reach *every* process the runtime spawns — commands
through `spawn` and declared tools' programs alike) — joined by `with_command_target`, which
registers an executor a
command can name with `!@<target> <command>`
([ADR-0021](adr/0021-a-command-names-where-it-runs.md), [targets.md](targets.md)), and by the
two below that describe the provider connection itself. A
single-workspace host that wants one of them hands the recipe
to `WorkspaceBuilder::with_runtime_builder`, which configures the private runtime
`Workspace::open` would have built rather than switching to a shared one. Mentra's own
surface is still unhidden, under a name that now says whose it is:
`Runtime::mentra_runtime()`, and `Workspace::mentra_runtime()` for a host that has only the
workspace in hand.

## What the repository says about its model

`Workspace::open("/repo")` reads `.basis/config.json` and the global
`config.json` itself, because opening a path is what reads a repository's
conventions — the same reason it reads `AGENTS.md` and `.mcp.json` without
being asked to. Everything a host states explicitly still wins:
`WorkspaceBuilder::with_model` and every `RuntimeBuilder` knob layer *above*
the file, and the file layers above the environment.

A host building a `Runtime` of its own gets nothing automatically, and that is
deliberate: a shared runtime's provider, credential and endpoint were settled
before any workspace existed, so a repository's file has nothing to reach
there. Apply it yourself if you want one to speak for the process:

```rust
use std::path::Path;
use basis::{Config, Runtime};

// `None` reads no global file; `Config::discover_default` finds the user's.
let config = Config::discover(Path::new("/repo"), None)?;
let runtime = Runtime::builder().with_config(&config).build()?;
```

`with_config` fills the provider, the endpoint and the model policy **only
where the builder was told nothing** — order does not matter, because what it
reads is emptiness rather than who spoke last. On a shared runtime the file's
`model` still applies per workspace (ADR-0018 already makes that an override)
and its `effort` becomes the default for a `RunSpec` that asked for none, so
one server over many repositories gives each the model it chose.

`Config::default()` says nothing, which is how
`WorkspaceBuilder::with_config(Config::default())` turns discovery off for a
host whose own configuration is the only configuration. `Workspace::config()`
and `Workspace::config_files()` report what took effect and which file said so.
There is no `api_key` key and there will not be one: a credential belongs to
the environment, which is the same ruling `RunConfig` makes.
[conventions.md](conventions.md) has the keys.

## What the host says on top of the workspace

basis ships no system prompt: unset, the prompt is the discovered context files and nothing
else. That is deliberate and it stays — but it left an embedding host with no way to give
its product a voice, or to say *for my runs, answer in Chinese*, short of writing into the
user's repository's `AGENTS.md`, which is the one file that is not the host's to edit.
`with_system_prompt` is the seam; the text is still the host's, so the core gains no opinion:

```rust
use basis::{SystemPrompt, Workspace};

// After the repository's own instructions, as the most specific block.
let workspace = Workspace::builder("/repo")
    .with_system_prompt(SystemPrompt::Append("Answer in Chinese.".to_string()))
    .open()
    .await?;

// Or instead of them, discovery left out of the prompt entirely.
let bare = Workspace::builder("/repo")
    .with_system_prompt(SystemPrompt::Replace("You are Acme's release reviewer.".to_string()))
    .open()
    .await?;
```

`Append` goes **last**, where the rendered context block's own preamble says the most
specific statement goes: a repository cannot know which product is running it, and a knob a
repository could override by writing a file is not a knob. The weakest end of that scale was
already covered — the global `AGENTS.md` is a personal append below every workspace file.

`Replace` drops the context from the prompt, including the global file, but not from the
report: `run_started` still names what discovery found, because *which context files does
this workspace have* has one true answer and the host that replaced the prompt already knows
it did. `Replace("")` is how to ask for no system prompt at all; `Append("")` is a no-op.
Neither variant touches the skills block — mentra appends that itself, after whatever basis
hands it.

One enum rather than two methods, because the two are alternatives and not layers: one
field, last call wins, and *both at once* is unspellable. And it is a **workspace** knob, so
a host serving many repositories off one shared `Runtime` can give each its own voice.

`RunConfig::with_system_prompt` is the same seam for a one-prompt caller, carried through
`split` to exactly that call — which is how `basis spawn --system-prompt` /
`--append-system-prompt`, `basis serve --acp --append-system-prompt`, and `ServeConfig`'s
template all reach it without a second implementation.

## How patiently a failing provider is waited out

mentra retries a transient provider error on a doubling backoff and gives up when the budget
runs out. Its default — five attempts, from 500ms, capped at 5s — spends about **twelve and
a half seconds**, which is shaped for a blip: a connection reset, a tunnel restart, a 502
from a proxy already coming back. A rate limit is a different failure. It lasts as long as
the window it belongs to, routinely a minute, so the whole default schedule elapses inside a
limit that was never going to lift and the caller reads a provider failure where the honest
answer was *wait*.

```rust
use std::time::Duration;
use basis::runtime::ProviderRetry;

let runtime = basis::Runtime::builder()
    .with_provider_retry(ProviderRetry {
        base_delay: Duration::from_secs(1),
        max_delay: Duration::from_secs(30),
        ..ProviderRetry::default()
    })
    .with_provider_retry_budget(8)
    .build()?;
```

Two knobs because mentra keeps the two questions apart, and both are usually needed: widening
the schedule without raising the count still gives up after five tries, and raising the count
against the default 5s ceiling reaches only about 27 seconds in total, short of the minute a
rate-limit window wants. Do the arithmetic before choosing.

What a host knows that basis cannot is how long its own caller will hold still. An editor
session should fail fast, because somebody is watching a cursor blink; a chat bot whose turn
already takes eight minutes can afford one of them waiting, and would far rather do that than
hand back an error the user has to re-ask. That judgement is why the number is the host's.

The scope is the runtime's (ADR-0018): this describes the connection to the provider, the
same kind of fact as the credential beside it, not something one prompt decides. Every run
minted on the runtime carries it, and so does every subagent a run delegates to through
`spawn` — a delegated run that reset to the default would be quietly less patient than the
run that delegated it, against the same gateway. `ProviderRetry` is mentra's own type,
re-exported as `basis::runtime::ProviderRetry`, and `retry_after_cap` on it bounds how long a
server's own `Retry-After` may make this process wait. None of it is a deadline:
`TurnOptions::with_deadline` still bounds the whole turn, and a generous schedule inside a
short deadline is bounded by the deadline.

## Which wire the model's requests go over

mentra streams the Responses wire format over HTTP+SSE or over a websocket. Unset, it picks,
and what it picks is HTTP+SSE — what every basis run has ever used. A host driving basis
against an endpoint where the websocket transport is the point says so:

```rust
use basis::runtime::ResponsesTransport;

let runtime = basis::Runtime::builder()
    .with_responses_transport(ResponsesTransport::WebSocket)
    .build()?;
```

**This needs the `responses-websocket` feature**, which is off by default so that an embedder
streaming over HTTP+SSE does not carry a websocket stack for it. Without the feature the
choice is accepted and then **fails at request time** rather than falling back to HTTP+SSE:
a host that asked for a transport should learn it did not get one, not discover later that
its traffic went the other way. A provider that does not serve websockets — Anthropic and
Gemini report that they do not — refuses an explicit choice at its first request, naming
itself, for the same reason. `Runtime::mentra_runtime().responses_transport()` reads back
what a runtime chose.

## Answers you can branch on

A run that answers in prose composes with nothing, because the next step has to parse
English to find out what happened. `output::<T>()` asks for a declared shape instead:

```rust
let output = run
    .output::<Findings, _, _>(
        "submit what you found, one entry per problem",
        findings_spec(),          // name, description, and a JSON Schema you write
        sink,
        basis::AllowAll,
    )
    .await?;

for finding in output.value.findings.iter().filter(|f| f.blocking) { … }
```

The schema is yours to write rather than derived from the type, because its field
descriptions are a *prompt* — they are what the model reads to decide what belongs in each
field.

By default a typed turn *shapes* rather than works: the answering tool is the only tool it
holds, and it is required to call it, so the turn can answer only from what earlier turns
on the same run already gathered. Ask for both at once and it does not fail loudly — it
returns a well-formed answer from a model that opened nothing, reported as a success. So
either do the reading on an ordinary turn and ask for the shape on the next, or hand the
turn its tools back:

```rust
run.output::<Findings, _, _>(prompt, findings_spec().with_tools(), sink, basis::AllowAll)
```

`with_tools()` keeps the ordinary toolset beside the answering tool, so one call reads and
then answers. What it gives up is the forcing: nothing makes a working turn stop and
answer, so it can reply in prose or run out of budget mid-gather, and on those paths there
is no value and `output` returns `Err`. Put the stopping condition in the description —
"call this once you have read every changed file" — which is exactly the wording the
default mode warns you against.

## One allowance, many runs

Dividing a limit across a fan-out starves the runs with something to say; granting it per
run multiplies the bill by N. A `BudgetPool` is the single figure in between — attach it and
every drawing run reports into one counter:

```rust
let pool = basis::BudgetPool::new(500_000);
let mut reviewer = workspace.prepare(pool.spec("review the tests"))?;
```

It is soft, and honestly so: usage is known only once a round has streamed, so the round
that crosses the line finishes, and a job lands at up to the limit plus one in-flight round
per concurrent run. `pool.spent()` reads the live number the turns are actually stopped
against. A turn drawing on a spent pool is refused with `RunError::BudgetExhausted` before
its prompt is sent — a decision with its own name, so a fan-out stops minting on it instead
of retrying it like a provider error.

Both the pool and `RunReport::usage` count what providers *report*. One that reports
nothing spends nothing as far as either is concerned. Work a run delegates through `spawn`
is inside the **bound**: the subagent runs on the parent's accounting handle, so what it
spends is what a `BudgetPool` meters and what a `--token-budget` stops the parent on. It is
outside the **tally**: the relay that puts a child's usage on the parent's event stream is
internal to mentra's own delegation intrinsic and a registered tool cannot reach it, so
`RunReport::usage` reports what the parent's own rounds cost and under-reports any run that
delegated. The number that stops a run and the number it says it spent are the same only
when nothing was delegated; [REDESIGN.md](REDESIGN.md) carries the gap as an open
upstream candidate rather than as a fixed one.

## One stream for many runs

Each run wants a sink of its own; a host wants one view of all of them without losing which
run said what. `EventFanIn` mints one tagged sink per run and merges them:

```rust
let fan = basis::EventFanIn::new();
let mut tests = workspace.prepare("review the tests")?;
let mut docs = workspace.prepare("review the docs")?;
let (a, b) = (fan.sink("tests"), fan.sink("docs"));
let mut merged = fan.into_events();          // minting closes here

let runs = async move {
    let (tests, docs) = tokio::join!(tests.execute(a), docs.execute(b));
    // Taking the answers out drops the reports, and their sinks with them —
    // which is what tells `merged` the stream is over.
    Ok::<_, basis::RunError>((tests?.final_message, docs?.final_message))
};
let watch = async {
    while let Some(tagged) = merged.recv().await {
        println!("[{}] {:?}", tagged.tag, tagged.event);
    }
};
let (answers, ()) = tokio::join!(runs, watch);
```

The tag rides outside `Event`, so the versioned wire schema stays exactly what its version
number promises. The stream ends when the last sink is dropped — and a finished run hands
its sink back inside its report, so a report held past the join is a branch of the stream
held open, and the join would wait on a stream waiting on the join. That is the one sharp
edge in the design, and the comment above is how to stay on the right side of it.

## Stopping a turn

Two signals, and they differ in what happens to the work. `cancellable()` abandons the turn
and rolls it back, which is what a client's stop button means; `stoppable()` ends it at the
next round boundary and keeps everything the model committed:

```rust
let (options, stop) = basis::TurnOptions::stoppable();
tokio::spawn(async move { on_stop_pressed().await; stop.cancel(); });

let report = run.execute_with_options(sink, options).await?;
```

One caveat worth stating: a graceful stop landing after a tool round comes back as a failed
turn even though nothing was discarded, because mentra still owes a final assistant message
and the last committed one was a tool result. The work is kept either way; the report is
what disagrees.

In-process concurrent work is owned by `basis::Supervisor`, whose ownership rules —
attached versus detached, downward cancellation, repeatable terminal observation, and a
wait graph that rejects cycles — are [ADR-0017](adr/0017-structured-agent-concurrency.md)'s
and are the same rules the CLI's durable handles obey across processes.

## Getting a say over each tool call

Interception is one contract with two bindings. A repository declares a subprocess in
`.basis/hooks.json`; an embedding host implements `Interceptor` and its own compiled code
gets the say, which is what you want when the guard needs a vault handle, a token you just
minted, or a regex that lives in a config struct:

```rust
#[basis::async_trait]
impl basis::Interceptor for Redact {
    fn name(&self) -> &str { "redact" }

    async fn intercept(&self, call: &basis::HookRequest)
        -> Result<basis::HookOutcome, basis::InterceptorError>
    {
        let Some(command) = call.input.get("command").and_then(|v| v.as_str()) else {
            return Ok(basis::HookOutcome::Allow);
        };
        if !command.contains("--token") {
            return Ok(basis::HookOutcome::Allow);
        }
        Ok(basis::HookOutcome::Modify {
            input: serde_json::json!({"command": "deploy --token REDACTED"}),
            reason: Some("stripped a credential".to_string()),
        })
    }
}

// Host scope is runtime scope (ADR-0018): the guard registers on the runtime —
// the shared one every workspace borrows, or the private one this open builds.
let workspace = basis::Workspace::builder("/repo")
    .with_runtime_builder(basis::Runtime::builder().with_interceptor(Redact))
    .open()
    .await?;
```

Both bindings speak the same vocabulary and are folded by the same chain, so allow, deny,
and modify mean one thing whichever side said it. They are consulted interceptors first in
registration order, then global hooks, then workspace hooks — the further a participant is
from the workspace's own data, the earlier it speaks — and since the first refusal
short-circuits, that is what lets your own guard refuse before a repository's program is
spawned at all. A participant that errors or panics **denies**. The trait is `async`, and
`basis::async_trait` is the attribute to spell it with — re-exported, so implementing a
basis trait costs your manifest nothing.

The other seam is `Approver`, and the two are deliberately not merged: an approver answers
*may this happen* and feeds the permission machinery a person drives, while an interceptor
answers *may this happen, in this form* and composes with everything else on the chain.
`AllowAll` (what a run with no approver gets) and `DenyAll` ship in `basis`, and
everything between them — allow edits but deny the network, ask over Slack with a timeout,
escalate after the third refusal — is an impl
([ADR-0010](adr/0010-the-crate-is-the-workflow-surface.md)). A refusal names its reason,
since that reason is what the model reads as the call's result.

## Where the history goes

Conversations are persisted by mentra, and unset, it picks a database keyed by the
*process's* current directory — not by the workspace you opened. Two knobs say otherwise,
and the last one called wins:

```rust
basis::Runtime::builder().with_store_dir("/var/lib/myapp/history")  // there
basis::Runtime::builder().with_ephemeral_history()                  // nowhere
```

The knobs are the runtime's (ADR-0018): history is a process fact, so a host sharing one
`Runtime` across workspaces sets it once, and a single-workspace host hands the recipe to
`WorkspaceBuilder::with_runtime_builder`.

`with_store_dir` keeps this runtime's conversations in a directory you name, and
`basis::store::list_in` reads them back from it. `with_ephemeral_history` uses an
in-memory store: resume works for as long as the `Runtime` holding it lives, and nothing
survives the process — no file, no export, no way to make one durable afterwards, so a host
that might want that later wants `with_store_dir` now.

Compaction snapshots follow the same answer: `<store_dir>/transcripts`, which is mentra's
own layout, so relocating the store relocates them. An ephemeral runtime files them under
the OS temp directory instead — mentra writes a snapshot before it summarizes without
asking the store, so *nowhere* is not available for that one file.

## Compaction

Two unrelated things shorten a history, and only one of them is the one you would guess.

**Every provider request** passes through micro-compaction, which blanks the content of
older tool results — no token budget in the decision, no event when it fires, on the fourth
tool call as readily as the four-hundredth. mentra's own default keeps them all, and basis
agrees: a harness that silently blanks the file the model just read is worse at the job, and
the tokens are ones you can already see and price.

**A long conversation** gets summarized: the transcript is snapshotted to disk, an older
prefix is replaced by a model-written summary, and the recent tail is preserved. It fires
three ways, and it announces itself (`Event::CompactionStarted` /
`Event::CompactionCompleted`) every time — including the third, unconditional one:

```rust
let workspace = basis::Workspace::builder("/repo")
    .with_compaction(
        basis::Compaction::default()
            .with_keep_recent_tool_results(Some(5))       // elide older ones; default None keeps all
            .with_auto_threshold_tokens(Some(400_000))    // default Some(50_000); None never triggers this way
            .with_auto_threshold_percent(Some(80))        // default Some(75); wins when the window is known
            .with_preserve_recent_user_tokens(20_000),
    )
    .open()
    .await?;
```

The knob is the workspace's, not the runtime's: these numbers live on mentra's agent
config, one is built per workspace, and every session and subagent that workspace mints
carries it.

`auto_threshold_tokens` is the fallback for a model whose context window basis does not
know. `auto_threshold_percent` is the one that did not exist until mentra could ask the
model itself how big it is, and it wins whenever the window *is* known — 50,000 tokens is
most of a small model's window and a rounding error in a 1M-token one, so no single constant
was ever going to be right for both. A run reads the same two figures a host would need to
decide this for itself:

```rust
if let Some(window) = run.context_window() {
    println!("{}/{window} tokens", run.estimated_context_tokens());
}
```

The window is known only when the workspace's model was resolved from a provider's listing
— `ModelSelector::NewestAvailable`, what a workspace resolves to when nothing named a model
— and only if that listing reports one: Gemini's does (`inputTokenLimit`); Anthropic's and
the Responses transport's do not. Naming a model explicitly — `--model`, a repository's
`config.json`, `RunConfig::with_model(ModelSelector::Id(_))` — resolves without a listing at
all, so the window is unknown for it regardless of provider. `estimated_context_tokens` is a
floor even when the window is known: it covers the history and the system prompt basis
configured, but not the task-reminder banner or skill-description block mentra may add to
the *effective* prompt, which nothing outside mentra can read.

Third, independent of both thresholds: a provider that refuses a request as too long
(`ProviderError::ContextLengthExceeded`) gets exactly one compaction and one retry, even with
`auto_threshold_tokens` cleared — a second overflow after that is not retried again. So
turning the first trigger off means basis never compacts *ahead of* running out of room, not
that an oversized conversation is guaranteed to fail outright.

Where the snapshots go is not a knob here: it follows the store (above).

## The fingerprint, in process

In-process a recurring-run loop is nine lines of host code against one long-lived
`Workspace`, with `Workspace::fingerprint()` in place of the subcommand:
[`basis/examples/watch.rs`](../basis/examples/watch.rs) is it, kept in the tree as a
standing check that it stays that short. `fingerprint()` blocks — it spawns `git` and stats
every tracked file — so a host with a runtime to keep responsive hands it to
`tokio::task::spawn_blocking`, which needs `'static` and so an `Arc<Workspace>` to move in;
the example calls it inline because that loop has nothing else to do while it waits.

## Examples

See [`basis/examples/embed.rs`](../basis/examples/embed.rs) for a host that reacts to
events as they arrive, [`conversation.rs`](../basis/examples/conversation.rs) for the
two-turn version, [`watch.rs`](../basis/examples/watch.rs) for the recurring-run loop,
[`review_workflow.rs`](../basis/examples/review_workflow.rs) for the whole fan-out — one
workspace, one budget, typed findings, one merged stream, and a verdict folded out of them
— and [`reviewed_shell.rs`](../basis/examples/reviewed_shell.rs) for an `Approver` that
reviews the agent's commands with a cheap typed turn of its own, with a remembered rule
answering the familiar ones before it is ever asked
(`cargo run -p basis --example embed -- "<prompt>"`, with a provider key set).
