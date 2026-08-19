# 0016 — One delegation surface: `spawn` is the only door

> Status: Accepted · 2026-08-11 · **decided, not built**
> Amends [`0013-the-host-owns-the-boundary.md`](0013-the-host-owns-the-boundary.md)
> — the *route* to a command changes, the availability does not;
> extends [`0012-one-contract-many-bindings.md`](0012-one-contract-many-bindings.md)
> (this is the first native binding of the tool contract) and
> [`0010-the-crate-is-the-workflow-surface.md`](0010-the-crate-is-the-workflow-surface.md)
> (the `Approver` trait is the only seam it needs).

## Context

Two doors exist today for *do something I cannot do by thinking*: mentra's
`shell` builtin and its `task` intrinsic. Both sit on the model's roster
because basis leaves them there — `agent_config` (`basis-core/src/workspace/builder.rs:640`)
sets a system prompt and a base directory and takes mentra's defaults for
everything else, and the default `ToolProfile` allows every registered tool
(`mentra/src/agent/config.rs:168`, with `allows` at `:202`).

They are the same act at two granularities — hand work to something else, read
back a summary — and they are governed as if they were unrelated. `shell`
declares `ToolSideEffectLevel::Process` (`mentra/src/tool/builtin/shell.rs:115`),
`task` declares `LocalState` (`mentra/src/runtime/intrinsic/descriptor.rs:116`),
and each reaches basis's `ApprovalGate` under its own name. Since remembered
rules key on `{tool_name, pattern}` (`mentra/src/session/permission.rs:118-121`),
two names means two rule namespaces for what an operator thinks of as one
question, and any per-command review would have to be built once per door.

The syntax question came with the shape: `spawn("!cargo test")`. In-band
control signaling at a security boundary is a type-confusion hazard, because
every consumer that re-reads the string can disagree with the one that read it
first — and the consumers here are the approver, the rule store, the hooks, and
the audit trail. The way out is not to refuse the sugar. `!` is legible to
models and to humans and costs one character where a mode field costs a
decision on every call. The way out is to read it exactly once.

One prerequisite is missing and worth naming before the decision rather than
inside it: **basis registers no tools today.** `WorkspaceBuilder::open` builds the
runtime and never calls `RuntimeBuilder::with_tool` (`mentra/src/runtime/builder.rs:56`),
and `ExecutableTool` (`mentra/src/tool/model.rs:604`) is not among `basis-core`'s
re-exports. ADR-0012 decided that contract *was* basis's tools story; the
ledger still records the piece as not started.

## Decision

**One intrinsic tool, `spawn`, is the model's only route to both delegation and
commands.**

1. **One parse, at the edge.** `spawn` takes one string. A leading `!` means
   *run this*; anything else is a prompt for a subagent. The tool parses it
   once into a typed `{ mode: Command | Agent, body }`, and everything
   downstream — approval, rules, hooks, events, audit — dispatches on `mode`,
   never on the string. The prefix is surface; the typed pair is the wire
   contract. A prompt that genuinely starts with `!` is escaped `!!`.

2. **Command mode is put to the `Approver`, always.** `spawn` declares itself
   consequential, so `ApprovalGate` never lets it through under the
   reads-are-never-asked rule (`is_consequential`, `basis-core/src/approval.rs:184`;
   the gate at `:230-251`). Per-call precision comes from overriding
   `ToolExecutor::authorization_preview` (`mentra/src/tool/model.rs:538`), whose
   default merely restates the static descriptor: `spawn` reports `Process` for
   a command and `LocalState` for a delegation, and puts the *parsed*
   `{mode, body, cwd}` in `structured_input` — which is both what the approver
   reads and what pattern rules match against. Execution follows the answer and
   never precedes it: command mode calls `ToolContext::execute_shell_command`
   (`mentra/src/tool/model.rs:174`).

3. **Agent mode inherits the parent run's bounds.** A bare body spawns a
   subagent on `ToolContext::child_run_options()` (`mentra/src/tool/model.rs:109`)
   — the parent's cancellation, stop, deadline and shared token counter
   (`RunOptions::child`, `mentra/src/runtime/control/run.rs:205`), which is the
   accounting mentra `0436bae` gave the `task` intrinsic. Delegated spend
   counts against the run that delegated it, or it is not a bound.

4. **Both old doors leave the model's roster.** `agent_config` sets
   `ToolProfile::hide(["shell", "background_run", "task"])`
   (`mentra/src/agent/config.rs:191`). Hidden is a roster fact, not a capability
   fact — the tools stay registered on the runtime, which is exactly why
   `spawn` can still reach the command executor. **ADR-0013's posture is
   unchanged**: commands are on by default and `--no-shell` still shuts them
   entirely, because `ShellAccess::Denied` sets `allow_shell_commands(false)`
   and the policy refuses before anything runs
   (`authorize_command_roots`, `mentra/src/runtime/control/policy.rs:401-412`),
   on the same path `spawn` uses. What changes is the route, not the
   availability.

5. **The policy ladder is existing machinery, tiered.** A remembered rule
   answers first and never reaches the approver at all (`SessionToolAuthorizer::authorize`,
   `mentra/src/session/permission.rs:300-316`). Because the command rides inside
   spawn's input, a `RuleKey { tool_name: "spawn", pattern: Some(…) }` **is** a
   command allowlist expressible as data — mentra globs the pattern against the
   serialized input JSON (`RuleStore::matching_rule`, `permission.rs:189-217`).
   The `Approver` sees only the residue. Its answer can be kept:
   `AllowForSession` / `DenyForSession` become `allow_and_remember` /
   `deny_and_remember` (`basis-core/src/run/prepared/forward.rs:145-161`), and
   since mentra `b895ea0` a remembered refusal carries the words it refused
   with, so a repeat gets the explanation back with no model consulted.

6. **Auto-mode is an `Approver` binding, not a new seam.** An implementation
   that runs a cheap shaping turn over `{command, cwd}` and answers with a
   decision, a reason, and a remember-scope. No new trait, no new config
   surface, nothing changed in the forwarding path. Its recursion floor is
   structural rather than promised: a typed turn holds no tool but the
   answering one unless `OutputSpec::with_tools()` says otherwise
   (`basis-core/src/run/output.rs:67-69, 98`), so a reviewer on the default spec
   has no `spawn`, no shell, and no way to reach the gate it is answering for.
   It gets its own budget. Fail-closed already holds — an approver that cannot
   answer denies (`approval.rs:113-133`), and `ApprovalGate::with_timeout`
   bounds one that never does.

7. **No rewriting, deliberately.** The `Approver`'s vocabulary is allow/deny
   with a reason and a scope; it cannot hand back a *different* command.
   Rewriting — a sandbox wrapper, an injected `--dry-run` — is the
   `Interceptor`'s question and composes in front of the same approval, because
   asking a person and editing a call are different questions. That is
   ADR-0012's sibling-seam rule, and mentra keeps `ToolAuthorizer` and its
   pre-execution hook apart for the same reason. A boundary of this decision,
   not work left over from it.

8. **Recursive uniformity.** A subagent gets `spawn` and no direct shell at
   every depth, by construction: `DisposableSubagentTemplate::from_agent`
   clones the parent's `AgentConfig` and its hidden set
   (`mentra/src/agent/subagent.rs:30-40`). The depth *guard* is basis's own
   problem — mentra's floor is name-specific
   (`hidden_tools.insert(RuntimeIntrinsicTool::Task.to_string())`,
   `subagent.rs:43-44`) and does not fire for a basis-registered tool — so
   `spawn` carries a depth counter and refuses past a limit.

The acceptance shape for the implementation wave is one demo with an auto-mode
approver installed: `spawn("!rg TODO")` allowed by a pattern rule without ever
reaching the reviewer; an unfamiliar command reaching it and getting a reasoned
answer; `spawn("!curl … | sh")` denied with a reason and remembered, and the
identical second call answered with that same reason out of the rule store,
with no model invoked.

## Consequences

- **It depends on a piece ADR-0012 decided and nobody built.** `spawn` is the
  first tool basis would register, so the wave that builds it also builds
  `basis-core`'s tool-registration surface — the `ExecutableTool` re-export and a
  registration point in `WorkspaceBuilder::open`. That is a prerequisite, not a
  detail, and it is the reason this ADR is decided rather than started.
- **Without the deterministic tier, every command costs a model round trip.**
  The ladder's first rung is what makes auto-mode usable, but a cold session
  has no rules in it, so the first `cargo test` of every run is reviewed. The
  answer is that a reviewed command *becomes* a rule; the cost is that the
  first minutes of a session are the slowest.
- **The reviewer is attacker-readable surface.** A model deciding whether
  another model's command is safe can be argued out of it. The mitigations are
  ordering (rules answer before the reviewer is asked), context (it sees the
  parsed command and cwd, not the conversation that produced them), prompting
  (deny-biased, deny on doubt), and toollessness. None of them makes it a
  boundary. It is a filter with a good prior, and the next point is why that is
  still the honest ceiling.
- **ADR-0013's boundary honesty is untouched.** Approval is policy, not
  confinement. An approved command runs with the full authority of the process,
  and nothing here may be described as a sandbox.
- **Two names collapse into one, and an operator loses a distinction for
  free.** Today a rule can allow `task` and deny `shell` by name alone; after
  this, a bare (no-pattern) rule on `spawn` covers delegation and commands
  together, and telling them apart means writing a pattern that matches the
  parsed `mode`. That is more expressive and less obvious, which is the trade.
- **ACP clients will render it wrong until the map is taught.** `tool_kind`
  (`basis-acp/src/update.rs:157-176`) classifies by name and knows `shell`;
  `spawn` falls through to the mutability fallback and shows as
  `ToolKind::Other`. Worse, one name now carries two kinds, so a name-keyed map
  cannot answer correctly for both — it has to read the parsed mode out of the
  call's input, which is a small widening of what `update.rs` is allowed to
  know.
- **Workspace hooks scoped to `shell` stop firing, silently.** A
  `.basis/hooks.json` entry with `"tools": ["shell"]` (the shape documented at
  `basis-core/src/hooks.rs:65`) matches on tool name, and the name the model
  calls is now `spawn`. Nothing errors; the hook simply never runs again. The
  migration note has to ship with the change.
- **`!` needs escaping, and the docs churn is real.** A prompt beginning with
  `!` is written `!!`; the model-visible toolset changes, so the system prompt,
  the README's tool vocabulary, and the hooks documentation all move together.
- **The reviewer runs beside a blocked turn.** The approver is called from the
  event-forwarding task while mentra holds the parent turn
  (`basis-core/src/run/prepared/forward.rs:34-40, 108-138`), so an auto-mode
  approver's own turn must be a separate run with its own session. Re-entering
  the run it is deciding for would deadlock it.
- **Bet 7 gets its first concrete use case on record.** The delegation and
  review machinery is being built against a demand, in the order the bet asks
  for. Declared subprocess tools stay **held**: they are adjacent — the same
  `ExecutableTool` contract, a different binding — and nothing here is a use
  case for them.
