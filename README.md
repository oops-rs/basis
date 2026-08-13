# lan

> **lan** — **L**ightweight **A**gent **N**ucleus.
> A LAN connects machines; lan connects agents to your codebase.

A full-functional, embeddable agent harness built on [Mentra](https://github.com/oops-rs/mentra).

Library first, binary second. No TUI — embedding is the front door:

1. **In-process** — depend on the `lan-core` crate (Rust hosts).
2. **ACP** — `lan serve --acp` serves the [Agent Client Protocol](https://agentclientprotocol.com)
   (JSON-RPC 2.0 over stdio) for editors (Zed, JetBrains) and web UIs
   ([acp-ui](https://github.com/formulahendry/acp-ui)).
3. **Subprocess** — `lan spawn` returns a durable task handle for scripts and
   agents; the compatibility `--json` spelling still streams JSONL.

There is one small command surface for work, communication, observation, and
explicit serving ([ADR-0017](docs/adr/0017-structured-agent-concurrency.md)):

```
lan "<prompt>"                     # shorthand: exactly `lan spawn "<prompt>"`
lan spawn "<prompt>"               # enqueue work and print a durable task handle
lan spawn "<prompt>" --await       # enqueue, then wait for the terminal result
lan send <ID> "<message>"          # enqueue a later turn and print its message ID
lan send <ID> "<message>" --await  # enqueue, then await that message's reply
lan ask <ID> "<question>"          # send with the correlated reply wait implied
lan wait <ID>                      # repeatably observe the task's terminal result
lan wait <ID> --message <MID>       # await/retry one message's correlated reply
lan cancel <ID>                    # request cancellation (attached descendants too)
lan watch <ID>                     # observe bounded/replayable progress
lan inbox [ID]                     # list bounded message/reply summaries
lan serve --acp                    # ACP server on stdio — what an editor spawns
lan serve --bridge                 # the same ACP server on a websocket, for a browser
lan fingerprint                    # the workspace's hash, for a loop you write yourself
```

A positional argument that names no subcommand is a prompt, so the human path carries no
ceremony. Bare `lan` is usage output, never an accidental server; `--` escapes a prompt
that collides with a subcommand name (`lan -- spawn`). The compatibility spelling
`lan run` remains an alias for `lan spawn`.

Human-readable outcomes finish with one `next:` line naming valid commands.
Lifecycle JSON includes the same `next` hint as metadata. A local task has a
finite 30-minute default deadline; `--deadline` narrows it, and `--detached`
starts an independent root rather than inheriting a parent.

`send` is enqueue-only by default and returns an opaque message ID. Blocking
lifecycle commands register a live wait edge: self, ancestor, and same-tree
peer edges are rejected, and an edge between independent ownership trees is
accepted only if it does not close a cycle. A task consumes accepted messages
at the next model-turn boundary. `send --await` waits for the reply produced by
that message's turn; `ask` is the explicit spelling that always waits. If the
client disconnects or the wait times out, the task and its message continue;
`wait <ID> --message <MID>` retries the same durable reply without rerunning
the task. `wait` without `--message` remains the repeatable terminal observation
and is finite (30 minutes by default).

The inbox is intentionally bounded: a task accepts at most 16 messages over
its lifetime, and human/JSON inbox replies are summaries capped at 4 KiB per
body and reply (with truncation metadata). A successful parent keeps attached children in its scope
until they settle; a failed or cancelled parent requests downward cancellation
and publishes its terminal state only after those children settle. Once a
worker has finished its own turn, it accepts no new messages or children.
`--detached` creates an independent root with its own finite deadline and
cancellation policy.

For backward compatibility, `--json` without lifecycle flags keeps the original
attended JSONL stream. Lifecycle commands return one bounded JSON object
(`watch --json` returns JSONL events), and the renderer does not change task
ownership.

The core has no opinions: task-specific behavior enters through data — the prompt, the
workspace (AGENTS.md, skills, prompt templates, `.mcp.json`), and config — never through code.

## Using it

Set a provider key — `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`, or
`OPENROUTER_API_KEY` — and run a prompt against a repository:

```sh
lan "summarize what changed in the last three commits"
lan spawn -C ../other-repo "find the slowest test and explain why"
```

`--effort` accepts exactly `low`, `medium`, `high`, `xhigh`, or `max`.
LAN keeps those values provider-neutral: Responses-family APIs receive
`reasoning.effort`, while Anthropic receives `output_config.effort` and enables
adaptive thinking only on models that support it. Provider/model combinations
without a requested tier fail explicitly instead of silently lowering it;
omitting the flag leaves the provider default unchanged.

The agent can read and write files in the workspace and **run commands**, and both of those
reach it through one tool. `spawn` takes a single string: one that starts with `!` is a
command (`!cargo test`), and anything else is a task handed to a subagent that works on it
and reports back — one door for *do something I cannot do by thinking*, so an operator
answers one question rather than two
([ADR-0016](docs/adr/0016-one-delegation-surface.md)). A task whose own text starts with `!`
is written `!!`. Commands are on by default because a harness that cannot run the test suite
does very little real work, and neither mode is ever waved through as a read: `spawn`
declares itself consequential, so every call is put to whoever answers approval for the run
before anything happens. `--no-shell` shuts commands off entirely for a run meant to read
and report:

```sh
lan --no-shell "explain how the event stream is assembled"
```

Be clear on what that flag is. A run holds whatever authority the user account that started
it holds; nothing inside the process narrows that, and lan claims no sandbox anywhere. The
path roots and the `.git` carve-out below are hygiene — they shut the route a model reaches
for first — and a shell redirect walks straight past them. `--no-shell` narrows what *this
run* does; it does not confine the process, because an in-process check cannot confine a
command once it has started ([ADR-0013](docs/adr/0013-the-host-owns-the-boundary.md)).
Isolation, where you want it, is the OS's job:
[docs/containerization.md](docs/containerization.md) has the read-only-root pattern, what
it protects, and what it does not.

Independently of that, you can decide when the agent has to ask:

```sh
lan --approve prompt "tidy up the imports"        # ask before each change
lan --approve never  "what does this crate do?"   # look, don't touch
```

Read-only calls are never queued for approval — prompting for them just trains you to
approve without reading. A command is never one of those, and neither is a delegation, so
both `spawn` modes reach you under `prompt` — and answering one of them "always allow"
covers the other, since a remembered answer is stored against the tool's name and `spawn`
is one name. Asking needs a terminal on stdin; without one a request is denied rather than
silently granted, so an unattended run fails visibly instead of quietly doing as it pleases.

A run nobody is watching should say so itself, with bounds — all unset by default, since an
attended run has a person who tells "thinking hard" from "stuck" in a way no timer can
([ADR-0014](docs/adr/0014-watch-retired-runs-are-boundable.md)):

```sh
lan spawn --deadline 10m --tool-budget 40 --token-budget 200000 "bump the deps and fix the fallout"
```

Every bound ends the run *gracefully* rather than discarding it: the event stream closes the
way it always does, and whatever the model committed is kept. `--token-budget` is soft by
construction — usage is only known once a round has streamed in full, so the round that
crosses the line finishes and the answer it produced is real. A run that was refused a
further round reports the bound that did it and exits `3`, so "the allowance ran out" never
looks like "the model was done"; a run whose model finished inside the crossing round exits
`0`, because nothing was refused it.

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

`--json` emits the event stream as newline-delimited JSON. The first line is always
`run_started` and carries the schema version; the last is always `run_finished`:

```jsonl
{"seq":0,"type":"run_started","schema":1,"lan":"0.1.0","workspace":"/repo","model":"…","provider":"anthropic","context_files":[{"path":"/repo/AGENTS.md","scope":"workspace"}]}
{"seq":1,"type":"assistant_delta","text":"Looking at "}
{"seq":2,"type":"run_finished","status":"ok"}
```

Exit codes are contract, so a caller branches without parsing anything:

| Code | Meaning |
|---|---|
| `0` | the run finished |
| `1` | the run failed, or lan could not start it |
| `2` | the invocation was wrong |
| `3` | a bound tripped (`--deadline`, `--tool-budget`, `--token-budget`); committed work was kept |

`3` is deliberately not `1`: "the model ran out of the time you gave it" and "the provider
refused the request" call for different reactions. `--token-budget` is the bound worth
reading twice, because it ends a run *gracefully* — the answer so far is real and goes to
stdout, and `3` is what says the allowance, not the model, is why there is not more of it.
A `--json` consumer reads the same fact off the stream instead: a bounded run's
`run_finished` line carries `"stopped_by":"deadline" | "tool_budget" | "token_budget"`,
and an unbounded one omits the key, so existing consumers see the lines they always saw.

In-process, the harness is **`lan-core`** — the run lifecycle, workspace discovery, the
event stream, and the seams, with no protocol, no transport, and no terminal code in the
graph. `lan-acp` is the ACP adapter over it and the `lan` binary is the CLI over both, so
an embedding host compiles only what it runs
([ADR-0011](docs/adr/0011-layered-crates.md)):

```toml
[dependencies]
lan-core = "0.1"   # unpublished so far — a git or path dependency until it isn't
```

MCP is a default-on `mcp` feature rather than a fixed part of the core:
`default-features = false` compiles a `lan-core` with no MCP concept at all — no `.mcp.json`
discovery, no servers registered ([ADR-0012](docs/adr/0012-one-contract-many-bindings.md)).

### A workspace opens once and mints runs

Opening a workspace settles everything that belongs to the repository rather than to the
prompt — context documents, the credential, the resolved model, skills, templates, hooks,
MCP connections. Minting a run from it is then synchronous, because nothing is left to
await ([ADR-0010](docs/adr/0010-the-crate-is-the-workflow-surface.md)):

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

### Answers you can branch on

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

### One allowance, many runs

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
when nothing was delegated; [docs/REDESIGN.md](docs/REDESIGN.md) carries the gap as an open
upstream candidate rather than as a fixed one.

### One stream for many runs

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

### Stopping a turn

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

### Getting a say over each tool call

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

### Where the history goes

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

See [`lan-core/examples/embed.rs`](lan-core/examples/embed.rs) for a host that reacts to
events as they arrive, [`conversation.rs`](lan-core/examples/conversation.rs) for the
two-turn version, [`watch.rs`](lan-core/examples/watch.rs) for the recurring-run loop,
[`review_workflow.rs`](lan-core/examples/review_workflow.rs) for the whole fan-out — one
workspace, one budget, typed findings, one merged stream, and a verdict folded out of them
— and [`reviewed_shell.rs`](lan-core/examples/reviewed_shell.rs) for an `Approver` that
reviews the agent's commands with a cheap typed turn of its own, with a remembered rule
answering the familiar ones before it is ever asked
(`cargo run -p lan-core --example embed -- "<prompt>"`, with a provider key set as above).


## ACP

`lan serve --acp` speaks the [Agent Client Protocol](https://agentclientprotocol.com)
on stdio, so any ACP client drives it with no lan-specific client code:

```sh
lan serve --acp                          # what an editor spawns
lan serve --acp --model gpt-5.6 --no-shell # same thing, configured
```

The client supplies the workspace (`cwd` on `session/new`) and the prompt, so the flags
here are only what a client cannot say. Sessions are mentra agents, which means
`session/load` resumes a conversation from a previous process, and `session/cancel` stops a
turn in flight. Permission requests become `session/request_permission`, so approval is the
client's UI rather than lan's ([ADR-0007](docs/adr/0007-acp-sessions-and-the-dispatch-loop.md)).

`session/list` works as of the interception wave, and had not before: lan filtered listings
by the workspace a conversation belongs to while filing every conversation under mentra's
`"default"` tag, so no list ever matched. Conversations from before the fix keep the old
tag and do not appear in a list — but none of them is stranded, because resuming looks a
conversation up by id and never by tag, and mentra re-files one under its workspace the
first time it is resumed and used.

An editor spawning lan and a shell pipe look identical from inside the process — both are a
non-TTY stdin with no arguments — so `cat prompt.txt | lan` cannot be detected as a prompt
without breaking every editor. Instead of waiting silently on prose, the server answers once
the input proves it was never a client:

```
lan: expected an ACP client on stdio
next: use `lan spawn -` for a prompt or `lan serve --acp` for ACP
```

[`scripts/acp-smoke.py`](scripts/acp-smoke.py) drives it by hand if you want to watch the
wire.

For a browser client, `lan serve --bridge` puts the same server behind a websocket — the transport
only; the UI is [acp-ui](https://github.com/formulahendry/acp-ui), adopted rather than
built ([ADR-0002](docs/adr/0002-acp-is-the-protocol.md)):

```sh
lan serve --bridge --allow-origin http://localhost:5173
```

It binds to loopback and serves **no page** until one is named. A websocket handshake is
exempt from the same-origin policy, so any page you visit could otherwise dial
`ws://127.0.0.1` and drive an agent with write access to your workspace; the `Origin`
allowlist is what stops that, and it starts empty.

## Recurring runs

lan ships no scheduler: an interval belongs to whatever already runs things on your machine
— cron, systemd, CI, a tokio task in your own binary. What lan ships instead are the two
pieces that are easy to get wrong, and the loop is composition
([ADR-0014](docs/adr/0014-watch-retired-runs-are-boundable.md)):

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

In-process the same loop is nine lines of host code against one long-lived `Workspace`, with
`Workspace::fingerprint()` in place of the subcommand:
[`lan-core/examples/watch.rs`](lan-core/examples/watch.rs) is it, kept in the tree as a
standing check that it stays that short. `fingerprint()` blocks — it spawns `git` and stats
every tracked file — so a host with a runtime to keep responsive hands it to
`tokio::task::spawn_blocking`, which needs `'static` and so an `Arc<Workspace>` to move in;
the example calls it inline because that loop has nothing else to do while it waits.

## What the workspace contributes

- **`AGENTS.md`** — discovered from a global config directory, then each ancestor of the
  workspace outermost-inward, then the workspace root. Later files are more specific and
  take precedence; all of them are named in the `run_started` event.
- **Skills** — `.lan/skills/` in the workspace, else `skills/` in the global config
  directory. The model loads one by name when it needs it, so only the descriptions cost
  context.
- **Prompt templates** — `.lan/templates/*.md`, markdown with a `description` and an
  optional `argument-hint` in frontmatter. `$ARGUMENTS` and `$1`, `$2`… are substituted; a
  nested path is a namespace, so `git/commit.md` is `git:commit`. ACP clients receive them
  as commands.
- **MCP servers** — `.mcp.json`, the same shape other agents read:
  `{"mcpServers": {"name": {"command": …, "args": […], "env": {…}}}}`. `${VAR}` expands from
  the environment, so a committed file need not carry a credential. An ACP client can also
  send servers with `session/new`, and both sets are honored.
- **Hooks** — `.lan/hooks.json` lists commands that get a JSON object on stdin and answer
  with one on stdout: `allow`, `deny` with a reason the model sees, or `modify` with a
  replacement input. Any language, process-isolated. A hook that breaks denies by default,
  because a guard that fails open is a guard nobody knows is gone. This is the subprocess
  binding of the interception contract; an embedding host's own `Interceptor` is the other,
  and both are folded by one chain (see above). **Migration:** an entry scoped
  `"tools": ["shell"]` no longer fires. Nothing errors — the name the model calls is now
  `spawn`, and a `tools` list matches on the exact name, so the hook simply stops running.
  Match `spawn` instead; a hook that wants commands and not delegations reads the call's own
  input, where `input` is the string the model wrote and a single leading `!` (never `!!`)
  is what makes it a command.
- **Approval** — `--approve prompt` puts every consequential call to you first, with the
  command or the changed keys shown; `always` (the default on the CLI) and `never` are the
  other two settings. Over ACP the default is `prompt`, since there is a client to ask.
  In-process there is no policy setting to pass: `Approver` is the seam, `AllowAll` (what a
  run with no approver gets) and `DenyAll` ship in `lan-core`, and everything between them —
  allow edits but deny the network, ask over Slack with a timeout, escalate after the third
  refusal — is an impl ([ADR-0010](docs/adr/0010-the-crate-is-the-workflow-surface.md)). A
  refusal names its reason, since that reason is what the model reads as the call's result.
- **`.git` carve-out** — `.git/hooks` and `.git/config` are denied to the file tools by
  default: a file written there runs on the next commit, which turns an edit into code
  execution outside anything approval covers. The rest of `.git` stays writable, since git
  needs it. This binds the file tools, **not the shell** — a redirect inside `sh -c` still
  lands, because nothing parses shell. Hygiene, not a boundary.
- **Confinement** — the agent is scoped to the workspace; a write above it is refused by the
  file tools. This is hygiene, not a boundary
  ([ADR-0004](docs/adr/0004-kernel-enforced-confinement.md),
  [ADR-0013](docs/adr/0013-the-host-owns-the-boundary.md)). The boundary, if you want one,
  is the OS's — see [docs/containerization.md](docs/containerization.md).

## Status

**P0–P4 complete.** Everything the original plan called a phase is built: the ACP server on
stdio via the explicit `lan serve --acp` adapter with modes, session listing — which only began returning anything
in Phase D — and history replay; multi-turn
conversation and resume; durable local `spawn`/`send`/`ask`/`wait`/`cancel`/`watch`/`inbox`
tasks; one-shot `lan spawn` in prose or JSONL (`lan run` remains a compatibility alias); MCP servers from `.mcp.json`
and from the client; prompt templates surfaced as commands; subprocess hooks that allow,
deny, or rewrite a tool call; a websocket bridge for
[acp-ui](https://github.com/formulahendry/acp-ui); and branching. All with AGENTS.md
discovery, skills, and approval.

**The redesign is underway.** [ADR-0010](docs/adr/0010-the-crate-is-the-workflow-surface.md)
through [ADR-0017](docs/adr/0017-structured-agent-concurrency.md) point lan at an SDK-first shape: the crate
is the workflow surface, the host owns the boundary, and the binary keeps only the grammar
above. [docs/REDESIGN.md](docs/REDESIGN.md) is the honest ledger of that transition.
**Phase A has landed** — `watch` retired with its bounds moved onto `RunConfig` and
`lan spawn`, the fingerprint kept as a utility, shell on by default, the shipped container
replaced by documented patterns, and the CLI grammar with its exit codes. **Phase B has
landed** too — the split into `lan-core`, `lan-acp`, and the binary, MCP behind a feature,
and approval as the `Approver` trait alone. **So has Phase C, the SDK proper** — the
`Workspace` / run split, typed output, cancellation on every entry point, the shared
`BudgetPool`, tagged sinks with a fan-in, and the two acceptance examples that were its
criterion. **And Phase D, the bindings** — interception as one contract with two bindings,
the two history knobs, `session/list` working for the first time, and credentials kept out
of every `Debug`. One Phase D item is deliberately held rather than built: declared
subprocess tools, because no concrete use case for them exists on record and the phase's
rule is that its items ship only against one. This README describes only what is built.

Still open, and named honestly: compaction tuning, the packages convention, and provider
OAuth remain, and **nobody has driven this from Zed or JetBrains yet** — it is verified
against the protocol and its official client library, not against the ecosystem. The source
repository is public; the runtime dependency is published, while LAN crates will follow a
coordinated release order. CI runs `cargo fmt --all --check`, clippy at `-D warnings`, and the full
suite on Linux, macOS, and Windows, plus an MSRV job. See
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) §6.

Docs follow the nous layout: [docs/PROPOSAL.md](docs/PROPOSAL.md) (why) ·
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) (how) · [docs/REDESIGN.md](docs/REDESIGN.md)
(the transition) · [docs/adr/](docs/adr/) (locked decisions) ·
[docs/proposals/](docs/proposals/) (deferred ideas).

## License

MIT. See [LICENSE](LICENSE).
