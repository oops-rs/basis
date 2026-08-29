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
basis = "0.8"
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

Three more verbs act on the conversation rather than on a turn. `run.set_name(…)` renames it
— mentra fixes a name at creation, which is before anyone knows what the conversation will
be about, so a host that mints one per topic would otherwise offer a list of identical
placeholders. `basis::store::list(&workspace)` is that list, most recently used first, each
entry carrying `created_at` and `updated_at` as epoch seconds. And
`basis::store::forget(agent_id)` removes one for good — the record and its memory both, so
nothing is left that `resume` would refuse. Deleting one that is not there is not an error;
deleting one a live `PreparedRun` still holds is, in effect, undone, because the run writes
its row back on its next persist.

`run.effort()` reads what the *session* is set to rather than what this handle was last
told, so a picker drawn from it shows the level a repository's `config.json` chose at mint
as readily as one `set_effort` did afterwards.

`workspace.skills()` reports what the four skill roots produced, after layering — see
[conventions.md](conventions.md) for which roots and in what order. Each entry carries
`model_invocable`, which is `false` when that `SKILL.md`'s frontmatter set
`disable-model-invocation`: the skill is left out of the list the model is shown and
`load_skill` refuses it, so it exists for a person to invoke. A host that offers skills in
its own UI is the only thing that can act on that distinction, which is why the report
carries it rather than quietly listing both kinds alike. basis does not itself route to
one — a skill is a body of instructions with no argument convention, and `/name args` is
already what `.basis/templates/` means.

When one prompt really is the whole job, the free functions are the same path with the
workspace opened and dropped around it — the binary is a thin shell over this:

```rust
let report = basis::run(
    "/repo",
    "summarize the recent changes",
    basis::CollectingSink::new(),
).await?;
```

A path and a prompt are all they take; anything more — a model, an endpoint, a bound — is
the same shape one call earlier, `Workspace::builder` and a `RunSpec`. The bounds are
builders on `RunSpec`, and `report.stopped_by` carries the distinction the exit code
makes: `Some(basis::Bound::Deadline)`, `Some(basis::Bound::ToolBudget)`,
`Some(basis::Bound::TokenBudget)`, or `None` when the work is what ended the run:

```rust
let spec = basis::RunSpec::new("bump the deps and fix the fallout")
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

Conversations on a shared runtime are still tagged per minted session with the workspace's
identifier. `store::list` — and ACP's `session/list` over it — therefore returns only that
workspace's conversations even though provider, store handle, and runtime are shared.

The knobs ADR-0018 moved are `RuntimeBuilder`'s now — `with_provider`, `with_base_url`,
`with_api_key`, `with_store_dir`, `with_ephemeral_history`, `with_interceptor`,
`with_command_environment` (whose pairs reach *every* process the runtime spawns — commands
through `spawn` and declared tools' programs alike) — joined by `with_command_target`, which
registers an executor a
command can name with `!@<target> <command>`
([ADR-0021](adr/0021-a-command-names-where-it-runs.md), [targets.md](targets.md)), and by the
three below that describe the provider connection itself. A
single-workspace host that wants one of them hands the recipe
to `WorkspaceBuilder::with_runtime_builder`, which configures the private runtime
`Workspace::open` would have built rather than switching to a shared one. Mentra's own
surface is still unhidden, under a name that now says whose it is:
`Runtime::mentra_runtime()`, and `Workspace::mentra_runtime()` for a host that has only the
workspace in hand.

## A host can pin the whole run contract

A strict embedding host can turn Basis's conventions into explicit inputs instead of
depending on ambient repository or home files
([ADR-0024](adr/0024-host-defined-runtime-contracts.md)). Basis 0.8 uses Mentra 0.23.3. For an
ordinary one-shot private runtime, the builder accepts a provider-core implementation directly;
a retained concrete Responses provider clone shares the registered session used for connection
prewarm. `ToolResultPolicy::unlimited()` separately pins unlimited bytes and physical lines with no
spill:

```rust
let runtime_recipe = basis::Runtime::builder()
    .with_registered_provider(provider.clone())
    .with_tool_result_policy(basis::ToolResultPolicy::unlimited())
    .with_ephemeral_history();
```

The workspace then takes an already-resolved model, disables every config/context/hook/tool/
memory/skill/template/MCP discovery lane as one posture, and can opt into the one-independent-mint
lifecycle:

```rust
let workspace = basis::Workspace::builder("/repo")
    .with_runtime_builder(runtime_recipe)
    .with_resolved_model(resolved_model)
    .without_discovery()
    .fresh_only()
    .open()
    .await?;
```

Both postures require a private runtime recipe. A borrowed `Arc<Runtime>` can be mutated or
minted through another holder, so it cannot prove either zero runtime-global skill leakage or
one independent mint. `fresh_only` consumes its claim on the first `prepare` or `resume`
attempt even if that attempt fails; follow-up turns on the returned `PreparedRun` remain
attached and allowed. Direct calls through `mentra_runtime()` are the raw escape hatch and
outside these Basis guarantees.

### Consume/rebuild for pooled checkouts

Safe reuse is a different construction path. `with_reusable_registered_provider(provider_id,
make, warm)` records a repeatable provider recipe. Each build calls `make` once, takes an ordinary
clone of the returned provider for `warm`, installs the other clone into Mentra, completes the
runtime build, and only then invokes and awaits `warm`. Basis verifies identity and this call order;
the host must make the provider generation fresh. A Responses factory should call
`fresh_session_scope()` for every generation, and its `warm` closure must actually prewarm the
session-sharing clone when connection prewarm is part of the host contract. The declared provider
id is checked against the resolved model before factory or warm activity and against every generated
provider before build:

```rust
let recipe = basis::Runtime::builder()
    .with_reusable_registered_provider(provider_id, make_provider, warm_provider)
    .with_tool_result_policy(basis::ToolResultPolicy::unlimited())
    .with_ephemeral_history()
    .into_reusable_recipe()?;

let workspace = basis::Workspace::builder("/repo")
    .with_runtime_recipe(recipe)
    .without_discovery()
    .fresh_only()
    .with_resolved_model(resolved_model)
    .with_tool_roster(basis::ToolRoster::only(["search", "finish"]))
    .open()
    .await?
    .bind_host_tools(checkout_tools)?;

// After the run, every observer guard and every event forwarder has exited:
let workspace = workspace.rebuild_for_reuse().await?;
let workspace = workspace.bind_host_tools(next_checkout_tools)?;
```

`into_reusable_recipe` requires explicit ephemeral history and refuses a one-shot registered or
higher-level provider and every `RuntimeBuilder::with_tool` value. Checkout-specific tools enter
only through the consuming `Workspace::bind_host_tools`; Basis preflights every supplied name and
collision before registration, and an explicitly empty vector is the binding for a tool-free
checkout. The host declares that vector complete. Basis does not infer semantic completeness or
validate that the bound names and the exact allow-list correspond. Because binding consumes the
workspace, any validation or registration failure returns no reusable entry. Every opened or
rebuilt generation starts unbound, binds once, and permits one independent `prepare` or `resume`
attempt; attached turns on that `PreparedRun` remain allowed.

`Workspace::rebuild_for_reuse(self)` is async and consuming. It seals the old generation, drops
workspace registrations and the uniquely owned runtime, calls the host factory, builds the
replacement, and invokes and awaits its warm step. A live run, `AgentEventTapGuard`, or detached
Basis event forwarder refuses rebuild and consumes the entry. Non-unique runtime ownership, provider
factory/build failure, and warm failure likewise return no reusable entry. Calling
`Workspace::mentra_runtime`, `PreparedRun::session`, `session_mut`, or `into_session` permanently
disables reuse for that generation because Basis can no longer count the escaped handles. Dropping
the workspace never invokes the recipe or builds a replacement.

This is deliberately narrower than Mentra's complete execution surface. Team, background, and
`spawn` execution are excluded, as is a custom tool that returns before its detached work finishes.
Basis does not automatically reject those execution names or detect detached custom work. A
reusable host omits those routes from its exact `ToolRoster::only` roster and makes every bound tool
await its effects before returning. The library proves lifecycle for Basis-attached runs, observer
guards, event forwarders, workspace registrations, the ephemeral store, and the provider factory /
warm sequence; the host supplies provider-session freshness and does not ask Basis to infer cleanup
for work it cannot track.

`RunProfile` states the per-mint half without changing the workspace defaults. Omitted fields
inherit; `with_max_output_tokens(None)`, `with_reasoning(None)`, and
`with_tool_result_paging(None)` are explicit clears:

```rust
let profile = basis::RunProfile::new()
    .with_resolved_model(gather_model)
    .with_tool_roster(basis::ToolRoster::only(["search", "finish"]))
    .with_provider_request_options(request_options)
    .with_reasoning(Some(reasoning))
    .with_max_output_tokens(Some(4_096))
    .with_compaction(compaction)
    .with_tool_result_paging(None)
    .with_system_prompt(basis::SystemPrompt::Replace(system_prompt));

let mut run = workspace.prepare(
    basis::RunSpec::new("gather the evidence").with_profile(profile),
)?;
```

Complete request options and the dedicated reasoning override follow ordinary builder order:
whichever is called last decides reasoning. Nonempty `session.extra_headers` are accepted only
on an explicitly ephemeral runtime, because Mentra persists its agent config and a durable
store must never receive request credentials.

One attached conversation can switch phases without losing its committed transcript:
`set_resolved_model` preserves the new context window, and `set_reasoning` preserves every
non-reasoning request option. Legacy `set_model` and `set_effort` remain intentionally lossy
wrappers. When a turn fails, `RunReport::failure` retains the original typed Mentra variant and
recoverability category before `RunOutcome` projects it to display/wire text; callers do not
parse that text to decide whether to retry.

### Lossless in-process observation

The summary-oriented `basis::Event` and JSONL surfaces intentionally omit complete tool bodies. A
host that needs an evidence-grade stream registers a synchronous tap on the prepared run:

```rust
let guard: basis::AgentEventTapGuard = run.register_agent_event_tap(
    |event: &basis::AgentEvent| persist_complete_event(event),
);
```

The callback receives Mentra's provider-neutral `AgentEvent` values unchanged, synchronously and in
occurrence order before the bounded broadcast stream. Tool inputs, structured results, error
payloads, and the terminal cancellation event remain complete. Registration does not replay earlier
events. The callback runs inline with the emitting operation, so it must return promptly and must
not block or panic. It must not re-enter an event-emitting operation or drop a tap guard. The
returned Basis-owned guard is opaque; keep it alive for the whole observation window. Dropping it
waits for any invocation already in flight and then unregisters, so do not drop it while holding a
lock or other resource that callback needs. On a reusable workspace the guard also holds a lifecycle
lease, so rebuild cannot race a still-registered observer.

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
the environment, which is the same ruling the rest of the surface makes.
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

`basis spawn --system-prompt` / `--append-system-prompt`,
`basis serve --acp --append-system-prompt`, and `SessionTemplate::with_system_prompt` on
`ServeConfig`'s template all reach exactly this call — one seam, no second implementation.

## How patiently a failing provider is waited out

mentra retries a transient provider error on a doubling backoff and gives up when the budget
runs out. Its default — five retries after the initial call, from 500ms, capped at 5s —
permits six calls and waits about **twelve and a half seconds**, which is shaped for a blip:
a connection reset, a tunnel restart, a 502 from a proxy already coming back. A rate limit
is a different failure. It lasts as long as
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

Those are defaults, not a ceiling on an individual call. A latency-sensitive
turn can override either half independently. `model_budget` separately caps
all main-model calls in that turn: the initial call, retries, and later rounds.

```rust
use std::time::Duration;
use basis::{CollectingSink, TurnOptions};
use basis::runtime::ProviderRetry;

let options = TurnOptions::default()
    .with_provider_retry(ProviderRetry {
        base_delay: Duration::from_millis(100),
        max_delay: Duration::from_secs(1),
        ..ProviderRetry::default()
    })
    .with_retry_budget(2)
    .with_model_budget(3);
let report = run.execute_with_options(CollectingSink::default(), options).await?;
```

Two knobs because mentra keeps the two questions apart, and both are usually needed: widening
the schedule without raising the count still gives up after five retries (six calls), and
raising the count against the default 5s ceiling reaches only about 27 seconds in total,
short of the minute a rate-limit window wants. Do the arithmetic before choosing.

What a host knows that basis cannot is how long its own caller will hold still. An editor
session should fail fast, because somebody is watching a cursor blink; a chat bot whose turn
already takes eight minutes can afford one of them waiting, and would far rather do that than
hand back an error the user has to re-ask. That judgement is why the number is the host's.

The default scope is the runtime's (ADR-0018): this describes the connection to the provider,
the same kind of fact as the credential beside it. Every run minted on the runtime carries
it, and so does every subagent a run delegates to through `spawn` — a delegated run that
reset to Mentra's default would be quietly less patient than the run that delegated it,
against the same gateway. `TurnOptions` is the explicit exception for one call.
`ProviderRetry` is mentra's own type,
re-exported as `basis::runtime::ProviderRetry`, and `retry_after_cap` on it bounds how long a
server's own `Retry-After` may make this process wait. None of it is a deadline:
`TurnOptions::with_deadline` still bounds the whole turn, and a generous schedule inside a
short deadline is bounded by the deadline.

## Which wire a custom endpoint is spoken to in

Two request formats answer to the name "OpenAI-compatible" and they agree on almost nothing:
a flat `messages` array against typed input items, tool arguments as a JSON string against a
value, `max_tokens` against `max_output_tokens` — and, the difference an operator meets
first, `v1/chat/completions` against `v1/responses`. Speaking the wrong one is a 404 on the
very first turn, worded like a mistyped URL.

`with_base_url` gets `chat/completions`, because that is what the name means in the wild:
Ollama, LM Studio, vLLM, llama.cpp, DeepSeek, Groq, Together, OpenRouter, and the gateways in
front of them serve it and nothing else. OpenAI's own `v1/responses` is served by OpenAI —
where `with_provider(BuiltinProvider::OpenAI)` reaches it with no base URL at all — and by a
few proxies that forward to it. Those proxies say so:

```rust
use basis::runtime::Wire;

let runtime = basis::Runtime::builder()
    .with_base_url("https://gateway.internal/v1")
    .with_wire(Wire::Responses)
    .build()?;
```

Paste the URL the server publishes on either wire: a trailing `/v1` is stripped during
resolution, because both transports append their own `v1/…` and the published form would
otherwise produce `/v1/v1/…`.

A key is optional on either wire. `with_base_url` alone, with nothing in `BASIS_API_KEY` or
`OPENAI_API_KEY`, builds a provider that sends no `Authorization` header — what a local
vLLM, llama.cpp or Ollama expects — and `with_provider(BuiltinProvider::Ollama)` or
`LmStudio` needs no key by construction. `ProviderChoice::api_key` is `Option<String>` for
exactly this reason; a server that wanted a key says so with a 401, which reaches the host
as the provider's own error rather than a guess basis made about the endpoint.

Builder-only, and that is deliberate. `.basis/config.json` carries `provider`, `model`,
`effort` and — global file only — `base_url`, but not this: a wire is not a fact a repository
has about itself, and the host that needs the other one is embedding basis rather than typing
at it. Nothing reads this without a base URL, either: a provider preset carries the wire its
vendor speaks, and basis will not talk `chat/completions` to Anthropic because a builder asked.

## Which transport a Responses stream goes over

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
what a runtime chose. An endpoint on `Wire::ChatCompletions` is unaffected either way: that
wire is HTTP+SSE and has no websocket to ask for.

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

That `Err` is an `OutputFailure`, not a bare error, because the turn still happened. Its
`error` field is the same `RunError` as ever — `OutputMismatch` for an answer the type
refused, `Runtime` for a turn that failed or never answered — and its `report` is the
`RunReport` the turn earned: what it spent, which bound stopped it, and the sink it wrote
to, all of which a fan-out charging one allowance needs whether or not a value came out.
`.map_err(RunError::from)` if you only want the error:

```rust
match run.output::<Findings, _, _>(prompt, findings_spec().with_tools(), sink, basis::AllowAll).await {
    Ok(output) => …,
    Err(failure) => {
        // A budget, not a broken provider — and what it cost to find out.
        let report = failure.report.expect("the turn ran");
        eprintln!("{}: stopped by {:?} after {} tokens",
            failure.error, report.stopped_by, report.usage.total_tokens());
    }
}
```

`report` is `None` only when there was no turn to report on: an empty prompt, an option set
that cannot be drawn, or a sink that refused a write.

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

For nested work, `pool.with_token_allowance(n)` shares the same counter but
stops at the smaller of the parent limit and the current spend plus `n`.
Sibling usage therefore consumes the nested allowance instead of creating a
second budget.

Both the pool and `RunReport::usage` count what providers *report*. One that reports
nothing spends nothing as far as either is concerned. Work a run delegates through `spawn`
is inside the **bound** and inside the **tally** alike: the subagent runs on the parent's
accounting handle, so what it spends is what a `BudgetPool` meters and what a
`--token-budget` stops the parent on — and since mentra `5f303b8` and basis `e22aa63` the
child's usage is relayed onto the parent's stream too, so `RunReport::usage` agrees with
the figure the bound stops on. [REDESIGN.md](REDESIGN.md) records the gap as closed.

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

In-process concurrent work is the host's own tokio: fan out on a
`tokio::task::JoinSet`, wire the stop button through the `CancellationToken` a
`TurnOptions` hands back, and let the bounds — deadline, tool budget, token budget — keep
an unattended branch finite. `examples/review_workflow.rs` runs that shape live and is the
reference. basis schedules nothing in process;
[ADR-0017](adr/0017-structured-agent-concurrency.md)'s ownership rules are the CLI's
durable-task contract across processes, where a handle is something any process can name.

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

### And a say over each result

Some questions have no answer before the call. Whether a grep pulled a credential out of a
file nobody meant to expose is not knowable from its pattern. `Interceptor::review` is the
same seam after the tool has run, and a workspace reaches it with `"event":
"post_tool_use"` in `.basis/hooks.json`:

```rust
#[basis::async_trait]
impl basis::Interceptor for Redact {
    fn name(&self) -> &str { "redact" }

    async fn intercept(&self, _call: &basis::HookRequest)
        -> Result<basis::HookOutcome, basis::InterceptorError>
    {
        Ok(basis::HookOutcome::Allow)
    }

    // Defaulted to Allow — keep — so an interceptor that only guards calls
    // says nothing here and is not made to.
    async fn review(&self, result: &basis::HookRequest)
        -> Result<basis::HookOutcome, basis::InterceptorError>
    {
        let Some(output) = result.output.as_ref().and_then(|o| o.as_str()) else {
            return Ok(basis::HookOutcome::Allow);
        };
        if !output.contains("AKIA") {
            return Ok(basis::HookOutcome::Allow);
        }
        Ok(basis::HookOutcome::Replace {
            output: serde_json::json!(output.replace("AKIA0123", "[redacted]")),
            // The tool's own verdict, unless you mean to overturn it.
            is_error: result.is_error.unwrap_or(false),
            reason: Some("a key".to_string()),
        })
    }
}
```

`result` is the same `HookRequest` with `output` and `is_error` on it, and `input` holding
what the tool actually *ran* with rather than what the model asked for. `Allow` keeps the
result; `Replace` shows the model something else; `Deny` shows it the reason instead,
marked as an error — which is what a refusal can still mean once a tool has run, and what a
broken guard falls back to.

**A post hook cannot un-run anything.** The side effects have happened, and the event stream
already carried the real result to every subscriber: `Event::ToolCompleted` reports what the
tool returned whatever the model is shown. That split is the point — the stream is the
record, this seam is the model's view — and it is why a guard that must *stop* something
belongs before the call.

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

## Durable, resumable tasks (`basis-tasks`)

Everything above is one run: a `Workspace`, a `RunSpec`, a turn that ends when the model
stops. `basis-tasks` is a sibling crate, over `basis` alone, for the other shape — a task
that outlives the process that started it, resumable from a durable handle, driven by
whichever process next attaches ([ADR-0017](adr/0017-structured-agent-concurrency.md),
[ADR-0019](adr/0019-the-filesystem-is-the-coordination-surface.md),
[ADR-0022](adr/0022-the-task-layer-is-a-crate.md)). It is what the `basis` binary's
`spawn`/`send`/`ask`/`wait`/`cancel`/`watch`/`list` verbs are built on, reachable from Rust
directly:

```toml
[dependencies]
basis-tasks = "0.8"
```

```rust
let tasks = basis_tasks::Tasks::open(&workspace)?;
let handle = tasks.spawn(basis_tasks::RunSpec::new(prompt))?;
let reply = tasks.ask(&handle, None, "and now?", std::time::Duration::from_secs(60)).await?;
for task in tasks.list()? { /* … */ }
```

`Tasks::open` resolves the same data directory the CLI does (`BASIS_DATA_DIR`, else an
absolute `XDG_DATA_HOME`, else the platform data home); `Tasks::open_at` takes an explicit
root instead, for a host — or a test — that wants no dependency on the process environment.
Every cap ADR-0017 set is unchanged here: 16 messages per inbox, 4 KiB bounded summaries, a
finite default deadline on every unattended task, downward-only cancellation, and the
wait-edge policy admitting a descendant or an independent root and refusing an ancestor or a
peer. `Approve::Prompt` needs a `PromptHost` supplied — a library has no terminal to ask at
any more than `basis` itself does — and showing a task's progress live is a `LiveSink` a
caller plugs in per call rather than something the crate decides for you.

## Compaction

Two unrelated things shorten a history, and only one of them is the one you would guess.

**Every provider request** passes through micro-compaction, which can blank the content of
older tool results — no token budget in the decision, on the fourth tool call as readily as
the four-hundredth. Mentra 0.23 reports each changed projection through
`Event::RequestToolResultsElided`; the canonical transcript remains intact, but the model no
longer sees those bodies. mentra's own default keeps them all, and basis agrees: a harness
that blanks the file the model just read is worse at the job, and the tokens are ones you can
already see and price.

**A long conversation** gets summarized: the transcript is snapshotted to disk, an older
prefix is replaced by a model-written summary, and the recent tail is preserved. It fires
four ways, and it announces itself (`Event::CompactionStarted` /
`Event::CompactionCompleted`) every time — including the third, unconditional one:

```rust
let workspace = basis::Workspace::builder("/repo")
    .with_compaction(
        basis::Compaction::default()
            .with_keep_recent_tool_results(Some(5))       // elide older ones; default None keeps all
            .with_auto_threshold_tokens(Some(400_000))    // default Some(50_000); None leaves the share alone in charge
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
was ever going to be right for both.

The two are one setting, though, not two independent triggers: mentra resolves them
together, and the pair also says *which* posture you mean.

| `auto_threshold_tokens` | `auto_threshold_percent` | what fires |
| --- | --- | --- |
| set | either | the percentage of a known window, else the absolute number |
| cleared | set | the percentage of a known window, and nothing when the window is unknown |
| cleared | cleared | nothing, at any window |

The middle row is the one to reach for when you do not want to name a token count you
cannot justify: an absolute fallback goes live on exactly the models whose window nobody
reports, which is where a wrong guess costs the most. Before mentra 0.24 it had no
spelling — a cleared `auto_threshold_tokens` was the off switch for the whole feature, so
wanting the window share meant leaving a large absolute number in place. It no longer does.

A run reads the same two figures a host would need to decide this for itself:

```rust
if let Some(window) = run.context_window() {
    println!("{}/{window} tokens", run.estimated_context_tokens());
}
```

The window is known when the provider's listing reports one, and mentra consults the listing
for a named model as well as for `NewestAvailable` (a pinned id that the listing does not
name still resolves — the id is the caller's intent, not a claim the listing must confirm).
Gemini's listing reports a window (`inputTokenLimit`); Anthropic's and the OpenAI wires' do
not, and a server that cannot list reports nothing. The value is read from the live session,
so it is exactly what mentra is compacting against. `estimated_context_tokens` is a
floor even when the window is known: it covers the history and the system prompt basis
configured, but not the task-reminder banner or skill-description block mentra may add to
the *effective* prompt, which nothing outside mentra can read.

Third, independent of both thresholds: a provider that refuses a request as too long
(`ProviderError::ContextLengthExceeded`) gets exactly one compaction and one retry, even with
both thresholds cleared — a second overflow after that is not retried again. So
turning the first trigger off means basis never compacts *ahead of* running out of room, not
that an oversized conversation is guaranteed to fail outright.

Fourth, and the only one a *person* can ask for: `run.compact(instructions, &mut sink)`
runs the pass now, whatever the thresholds say.

```rust
if let Some(compacted) = run.compact(Some("keep the migration plan"), &mut sink).await? {
    println!("{} items replaced by a summary", compacted.replaced_items);
}
```

A pass that *fails* puts an `Event::Error` on the sink before returning the error, so a
client watching the stream is told why a conversation it expected to shrink did not. The
transcript is untouched and the next turn goes out on the unshortened history.

Two limits worth knowing, both upstream's and neither worked around here. A summarizing
call is billed by the provider but does not appear in `RunReport::usage` and is not
charged against a token budget or a `BudgetPool`: mentra reports no usage for it, and
basis tallies only what is reported. And an *automatic* pass that fails is retried and
then dropped silently — the run carries on with an unshortened history, but nothing on
the stream says so. The pass a host asks for itself is the one whose failure is visible.

The instructions are **added** to mentra's standing continuity requirements rather than
substituted for them, so asking for one extra thing cannot cost the file paths and command
outcomes every summary needs; `None` asks for the standing ones alone. `Ok(None)` means
there was nothing to compact — the last turn is always preserved whole, exactly as it is
for the model's own `compact` intrinsic, so a conversation with only that has no older
prefix to summarize — and nothing is emitted in that case either.

It is a model call: the summary is written by the same provider the conversation runs on,
and it is billed and can fail like any other request. It is not a *turn*, though: no prompt
is committed, the transcript gains no exchange, and nothing is sent afterwards. The sink is
borrowed rather than taken, because there is no report to hand it back in — two events and
a value are the whole of what happened.

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
