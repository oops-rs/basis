# Embedding lan — the `lan-core` SDK

> The reference for the surface the [README](../README.md) summarizes. Design rationale is in
> [`ARCHITECTURE.md`](ARCHITECTURE.md); the decisions are in [`adr/`](adr/); the ledger of the
> SDK-first transition is [`REDESIGN.md`](REDESIGN.md).

In-process, the harness is **`lan-core`** — the run lifecycle, workspace discovery, the
event stream, and the seams, with no protocol, no transport, and no terminal code in the
graph. `lan-acp` is the ACP adapter over it and the `lan` binary is the CLI over both, so
an embedding host compiles only what it runs
([ADR-0011](adr/0011-layered-crates.md)):

```toml
[dependencies]
lan-core = "0.1"   # unpublished so far — a git or path dependency until it isn't
```

MCP is a default-on `mcp` feature rather than a fixed part of the core:
`default-features = false` compiles a `lan-core` with no MCP concept at all — no `.mcp.json`
discovery, no servers registered ([ADR-0012](adr/0012-one-contract-many-bindings.md)).

## A workspace opens once and mints runs

Opening a workspace settles everything that belongs to the repository rather than to the
prompt — context documents, the credential, the resolved model, skills, templates, hooks,
MCP connections. Minting a run from it is then synchronous, because nothing is left to
await ([ADR-0010](adr/0010-the-crate-is-the-workflow-surface.md)):

```rust
let workspace = lan_core::Workspace::open("/repo").await?;

let mut run = workspace.prepare("what does this repo do?")?;
let report = run.execute(lan_core::CollectingSink::default()).await?;
```

That is the shape to reach for whenever a host sends more than one prompt at a repository:
twenty runs read `AGENTS.md` once, resolve the model once, and share one set of MCP
connections. A `Workspace` is `Send + Sync`, so the runs can be spawned tasks.

For a conversation rather than a one-shot, keep the run and send again — the session
survives the turn, so the model sees everything said so far:

```rust
run.send("and which of those is riskiest?", sink, lan_core::AllowAll).await?;
```

`run.agent_id()` is the handle `Workspace::resume` takes, so a later process can pick the
same conversation back up.

When one prompt really is the whole job, the free functions are the same path with the
workspace opened and dropped around it — the binary is a thin shell over this:

```rust
let report = lan_core::run(
    lan_core::RunConfig::new("/repo", "summarize the recent changes"),
    lan_core::CollectingSink::new(),
).await?;
```

The bounds are builders on either shape — `RunConfig` for a one-shot, `RunSpec` for a run
minted from a workspace — and `report.stopped_by` carries the distinction the exit code
makes: `Some(lan_core::Bound::Deadline)`, `Some(lan_core::Bound::ToolBudget)`,
`Some(lan_core::Bound::TokenBudget)`, or `None` when the work is what ended the run:

```rust
let config = lan_core::RunConfig::new("/repo", "bump the deps and fix the fallout")
    .with_deadline(Duration::from_secs(600))
    .with_tool_budget(40)
    .with_token_budget(200_000);
```

## Answers you can branch on

A run that answers in prose composes with nothing, because the next step has to parse
English to find out what happened. `output::<T>()` asks for a declared shape instead:

```rust
let output = run
    .output::<Findings, _, _>(
        "submit what you found, one entry per problem",
        findings_spec(),          // name, description, and a JSON Schema you write
        sink,
        lan_core::AllowAll,
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
run.output::<Findings, _, _>(prompt, findings_spec().with_tools(), sink, lan_core::AllowAll)
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
let pool = lan_core::BudgetPool::new(500_000);
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
let fan = lan_core::EventFanIn::new();
let mut tests = workspace.prepare("review the tests")?;
let mut docs = workspace.prepare("review the docs")?;
let (a, b) = (fan.sink("tests"), fan.sink("docs"));
let mut merged = fan.into_events();          // minting closes here

let runs = async move {
    let (tests, docs) = tokio::join!(tests.execute(a), docs.execute(b));
    // Taking the answers out drops the reports, and their sinks with them —
    // which is what tells `merged` the stream is over.
    Ok::<_, lan_core::RunError>((tests?.final_message, docs?.final_message))
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
let (options, stop) = lan_core::TurnOptions::stoppable();
tokio::spawn(async move { on_stop_pressed().await; stop.cancel(); });

let report = run.execute_with_options(sink, options).await?;
```

One caveat worth stating: a graceful stop landing after a tool round comes back as a failed
turn even though nothing was discarded, because mentra still owes a final assistant message
and the last committed one was a tool result. The work is kept either way; the report is
what disagrees.

In-process concurrent work is owned by `lan_core::Supervisor`, whose ownership rules —
attached versus detached, downward cancellation, repeatable terminal observation, and a
wait graph that rejects cycles — are [ADR-0017](adr/0017-structured-agent-concurrency.md)'s
and are the same rules the CLI's durable handles obey across processes.

## Getting a say over each tool call

Interception is one contract with two bindings. A repository declares a subprocess in
`.lan/hooks.json`; an embedding host implements `Interceptor` and its own compiled code
gets the say, which is what you want when the guard needs a vault handle, a token you just
minted, or a regex that lives in a config struct:

```rust
#[lan_core::async_trait]
impl lan_core::Interceptor for Redact {
    fn name(&self) -> &str { "redact" }

    async fn intercept(&self, call: &lan_core::HookRequest)
        -> Result<lan_core::HookOutcome, lan_core::InterceptorError>
    {
        let Some(command) = call.input.get("command").and_then(|v| v.as_str()) else {
            return Ok(lan_core::HookOutcome::Allow);
        };
        if !command.contains("--token") {
            return Ok(lan_core::HookOutcome::Allow);
        }
        Ok(lan_core::HookOutcome::Modify {
            input: serde_json::json!({"command": "deploy --token REDACTED"}),
            reason: Some("stripped a credential".to_string()),
        })
    }
}

let workspace = lan_core::Workspace::builder("/repo").with_interceptor(Redact).open().await?;
```

Both bindings speak the same vocabulary and are folded by the same chain, so allow, deny,
and modify mean one thing whichever side said it. They are consulted interceptors first in
registration order, then global hooks, then workspace hooks — the further a participant is
from the workspace's own data, the earlier it speaks — and since the first refusal
short-circuits, that is what lets your own guard refuse before a repository's program is
spawned at all. A participant that errors or panics **denies**. The trait is `async`, and
`lan_core::async_trait` is the attribute to spell it with — re-exported, so implementing a
lan trait costs your manifest nothing.

The other seam is `Approver`, and the two are deliberately not merged: an approver answers
*may this happen* and feeds the permission machinery a person drives, while an interceptor
answers *may this happen, in this form* and composes with everything else on the chain.
`AllowAll` (what a run with no approver gets) and `DenyAll` ship in `lan-core`, and
everything between them — allow edits but deny the network, ask over Slack with a timeout,
escalate after the third refusal — is an impl
([ADR-0010](adr/0010-the-crate-is-the-workflow-surface.md)). A refusal names its reason,
since that reason is what the model reads as the call's result.

## Where the history goes

Conversations are persisted by mentra, and unset, it picks a database keyed by the
*process's* current directory — not by the workspace you opened. Two knobs say otherwise,
and the last one called wins:

```rust
lan_core::Workspace::builder("/repo").with_store_dir("/var/lib/myapp/history")  // there
lan_core::Workspace::builder("/repo").with_ephemeral_history()                  // nowhere
```

`with_store_dir` keeps this workspace's conversations in a directory you name, and
`lan_core::store::list_in` reads them back from it. `with_ephemeral_history` uses an
in-memory store: resume works inside the workspace's lifetime and nothing survives the
process — no file, no export, no way to make one durable afterwards, so a host that might
want that later wants `with_store_dir` now.

## The fingerprint, in process

In-process a recurring-run loop is nine lines of host code against one long-lived
`Workspace`, with `Workspace::fingerprint()` in place of the subcommand:
[`lan-core/examples/watch.rs`](../lan-core/examples/watch.rs) is it, kept in the tree as a
standing check that it stays that short. `fingerprint()` blocks — it spawns `git` and stats
every tracked file — so a host with a runtime to keep responsive hands it to
`tokio::task::spawn_blocking`, which needs `'static` and so an `Arc<Workspace>` to move in;
the example calls it inline because that loop has nothing else to do while it waits.

## Examples

See [`lan-core/examples/embed.rs`](../lan-core/examples/embed.rs) for a host that reacts to
events as they arrive, [`conversation.rs`](../lan-core/examples/conversation.rs) for the
two-turn version, [`watch.rs`](../lan-core/examples/watch.rs) for the recurring-run loop,
[`review_workflow.rs`](../lan-core/examples/review_workflow.rs) for the whole fan-out — one
workspace, one budget, typed findings, one merged stream, and a verdict folded out of them
— and [`reviewed_shell.rs`](../lan-core/examples/reviewed_shell.rs) for an `Approver` that
reviews the agent's commands with a cheap typed turn of its own, with a remembered rule
answering the familiar ones before it is ever asked
(`cargo run -p lan-core --example embed -- "<prompt>"`, with a provider key set).
