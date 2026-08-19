# 0010 — The crate is the workflow surface

> Status: Accepted · 2026-08-11
> Extends [`0003-library-first-no-tui.md`](0003-library-first-no-tui.md);
> rejects [`../proposals/0001-embedded-scripting-extensions.md`](../proposals/0001-embedded-scripting-extensions.md).

## Context

Claude Code ships a workflow feature as a JavaScript DSL (`agent()`,
`pipeline()`, `parallel()`, budgets) interpreted inside the product, because
the product is closed: orchestration logic has to travel *to* the harness as a
script. basis's founding bet is the inverse — the harness travels to the host as
a crate. For a Rust host, `pipeline` is a loop with `.await`, `parallel` is
`join_all`, and a judge panel is a `Vec` of futures, with real types and a real
debugger.

Proposal 0001 (embedded scripting: wasm or rhai) was written for extension
authors, deferred until friction showed. The friction that actually showed
points the other way: what people want is to *call basis from code they already
write*, not to write code that basis interprets.

The same review found the approval surface carrying a redundant entity:
`ApprovalPolicy::{Always, Prompt, Never}` is an enum the core interprets,
duplicating what the `Approver` trait already expresses — the enum's three
values are three trait impls, and the enum can express nothing else, while the
trait can express anything (allow edits but deny network, ask a human over
Slack with a timeout).

## Decision

**Orchestration is host-language code against the crate. basis ships primitives,
never a DSL, and no embedded scripting layer.** Day 1 is Rust-only: other
languages are not a design input until real friction is recorded.

The SDK surface this commits to:

- **`Workspace` / run split.** What is per-workspace (context discovery,
  skills, templates, MCP connections, provider setup) is prepared once; runs
  are minted from it cheaply. A 20-agent fan-out discovers `AGENTS.md` once,
  not 20 times. Today's `RunConfig` conflates the two.
- **Structured output.** `.output::<T>()` on a run, surfacing mentra's
  existing `Agent::run_to_output` (schema-forced terminal tool, typed
  `FinalOutput<T>`). Prose return values kill programmatic composition; this
  is the primitive workflows live on. **Already built in mentra** — basis only
  exposes it.
- **Shared budgets.** A cloneable `BudgetPool` that concurrent runs draw
  from, on top of the per-run bounds of
  [`0014`](0014-watch-retired-runs-are-boundable.md), so "this whole review
  costs ≤ 500k tokens" is one line.
- **Cancellation.** A run accepts an abort signal. Mentra's run options
  already carry a cancellation token; basis exposes it.
- **Event fan-in.** Sinks taggable with a run identity so N concurrent runs
  merge into one observable stream.
- **Approval is the trait alone.** `ApprovalPolicy` is deleted. `Approver` is
  the seam; `AllowAll` (the default) and `DenyAll` ship in the core; the
  terminal prompter moves to the binary, where TTYs live —
  `basis spawn --approve prompt` behaves exactly as before by installing it. The
  trait contract inherits the fail-closed rule: an approver that cannot
  answer (no TTY, timeout, broken channel) denies.

## Consequences

- Subagents and teams inside one run come from mentra's builtin `task` and
  `team_*` tools — basis adds convention, not machinery.
- Proposal 0001 moves from Deferred to **Rejected**: the case it anticipated
  is served by the SDK, and its trigger (an extension inexpressible through
  hooks + MCP) never fired.
- No per-language client SDKs. The versioned JSONL stream and ACP remain what
  they are — a CLI convenience and the interactive-client door — not
  orchestration APIs. Building "basis-sdk-python" is the client-per-integrator
  trap of [`0002`](0002-acp-is-the-protocol.md), declined in advance.
- Pre-1.0 and Rust-only, the crate API is the sole compatibility surface, so
  the `Workspace`/run reshaping can proceed without protocol or schema debt.
- The acceptance test for "done" is dogfood: the retired `watch` reproduced
  as a short example on this API, and a fan-out review workflow example in
  plain Rust.
