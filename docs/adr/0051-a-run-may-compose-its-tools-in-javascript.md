# ADR-0051 — A run may compose its tools in JavaScript

## Context

When the second tool needs a value from the first, a direct caller usually sends
the first result to the model to construct the next call. For many searches and
lookups the model can express that decision before the first result arrives.
[ADR-0010](0010-the-crate-is-the-workflow-surface.md) gives a Rust host the
primitives to compose runs. Here the author is the model working inside one run.

## Decision

A host may offer a run a tool named `code`. Its input is the body of an async
JavaScript function. The script calls `await tools[name](args)` with the tool's
existing name and JSON input, and returns the value the model should read. A
fresh evaluation serves each call. Text stays text, including text that happens
to parse as JSON. Ordinary tool calls remain available.

Basis owns this spelling and an optional QuickJS-NG binding through `rquickjs`.
Both the Cargo feature and the run option default to off. Direct and scripted
calls use the turn's effective tool roster. The evaluator exposes ECMAScript
and those tools. Host filesystem, network, process and module loading enter
only through a tool that the run can already call. Scripts cannot call `code`
recursively.

Mentra owns nested dispatch, including the checks on the actual computed input,
approval, result review, budgets, events and interruption. Each inner call is
observable. Only the script's public result goes back to the model. A stopped
turn stops the evaluation. A resumed turn must never replay an interrupted
script's effects. See [mentra#72](https://github.com/oops-rs/mentra/issues/72).

## Consequences

The same interface applies to native, declared and MCP tools and acquires new
tools from the run. A host still writes its own workflows in Rust. This amends
ADR-0010 only for model-authored computation during a turn.

JavaScript's JSON values and async functions fit the tool contract. We expect
model familiarity to help first-attempt success. That is a hypothesis to measure.
A macOS arm64 release probe chaining two yielding Rust futures added 0.70 MiB
for QuickJS-NG, 0.31 MiB for Lua and 2.04 MiB for Rhai. These are standalone
embedding measurements. Production acceptance compares direct and scripted calls
on the same workload for effects, model rounds and model-visible results. The
feature-off build should carry zero JavaScript engine dependencies.

Amended 2026-11-19: the evaluator's 120-second clock counted time inside child
calls, so one slow delegation expired the script and the expiry cancelled the
remote task in flight. A child call already has the bounds of a direct call.
The clock now counts only time with no child call outstanding, and expiry can
no longer drop a call in progress.

Amended 2026-11-19: child calls ran one at a time, so `Promise.all` over
read-only tools was slower than the same calls written directly in one
response. Calls a script starts in one synchronous stretch now reach mentra as
one batch (`ToolContext::invoke_children`), which gives them the lanes of a
response's calls. Exclusive tools, blocking delegation among them, still run
one at a time. A run may also set its own script time.

Amended 2026-11-23: `code` was registered on the run's own agent only, so a
delegated subagent or teammate could never compose its tools, even when the
policy that narrowed it named `code`. An agent `spawn` starts from a run that
offers `code` now gets `code` too, with that run's script time, and so does
any agent that child delegates to in turn. Whether the child's model sees
`code` is still up to its roster, under the existing rule that a roster
narrows a child and never widens it. An inherited roster keeps it, and an
override such as `ToolRoster::only` offers it only when it names `code`, and
only if the parent's own roster allowed it. A child's script runs on the
child's turn, so it composes the child's tools under the child's approvals.
