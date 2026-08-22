//! Interception: a say over tool calls, from the host's code or from the
//! workspace's.
//!
//! ARCHITECTURE.md §3 lists "event interception (block/modify tool calls)" as
//! something pi gets from in-process TypeScript extensions. ADR-0012's answer is
//! that the contract, not the binding, is the design: **one contract per seam,
//! and transports are adapters.** So this module is one interception contract
//! with two ways to speak it.
//!
//! - **In-process** — a host implements [`Interceptor`] and registers it with
//!   [`RuntimeBuilder::with_interceptor`](crate::RuntimeBuilder::with_interceptor) —
//!   host scope is runtime scope (ADR-0018).
//!   Its code, its process, its dependencies; the case a subprocess answers
//!   badly, because redacting a credential needs the vault handle the embedding
//!   program is already holding.
//! - **Subprocess** — a workspace declares a command in `.basis/hooks.json` and
//!   basis execs it with one JSON object on stdin, reading one JSON object back.
//!   Process isolation and any language; no scripting runtime is embedded
//!   (ADR-0001, and `docs/proposals/0001`, which stays deferred).
//!
//! Both answer in one vocabulary — allow, deny with a reason, modify with a
//! replacement input, replace with a different result — and both go through one
//! [`HookRunner`], so the ordering, the short-circuit and the threading of a
//! rewrite are decided once for the pair rather than twice.
//!
//! Both are asked at two moments, too. Before a call, the question is whether
//! it should happen and in what form; after it, the tool has run and the only
//! thing left to decide is what the model is shown — which is where some
//! questions can first be answered at all, because a command's output is not
//! knowable from its arguments. [`HookEvent`] names the two, a hook declares
//! which it wants, and an [`Interceptor`] implements one method or both.
//!
//! The other seam is approval ([`crate::approval`]), and the two are siblings
//! rather than one thing: an [`Approver`](crate::approval::Approver) answers
//! *may this happen* to whoever is watching, and an [`Interceptor`] answers
//! *may this happen, in this form* as part of a composing chain. mentra keeps
//! `ToolAuthorizer` and `PreExecutionHook` apart for that reason and basis binds
//! each of them once per binding, which is what ADR-0012's "hooks re-founded as
//! a binding of the authorizer seam" honestly amounts to.
//!
//! # Where the pieces are
//!
//! | what | where |
//! |---|---|
//! | what is asked, and what an answer may say | [`contract`] — both bindings |
//! | the in-process binding | [`Interceptor`] |
//! | the subprocess binding's JSON encoding | [`wire`] |
//! | how a subprocess is declared and found | [`HookSpec`], [`HooksConfig`] |
//! | the one chain both arrive at | [`HookRunner`] |
//!
//! # The seams underneath
//!
//! mentra's [`PreExecutionHook`](mentra::runtime::PreExecutionHook) fires after
//! authorization and before the tool runs;
//! [`PostExecutionHook`](mentra::runtime::PostExecutionHook) fires after it and
//! before the result reaches the model — before mentra's own pager, so a
//! participant sees the whole output rather than its first window, and without
//! touching `AgentEvent::ToolExecutionFinished`, so the audit trail keeps what
//! actually happened whatever the model is shown.
//!
//! basis registers exactly one implementation on each, [`HookRunner`], which
//! fans out to every interceptor and every configured command — not because it
//! must (`with_pre_hook` and `with_post_hook` both append) but because basis
//! wants to own the ordering and the short-circuit.
//!
//! What a post hook *cannot* do is un-run anything. By the time it speaks the
//! side effects have happened, which is why the two events are not
//! interchangeable and why a guard that must stop something belongs before the
//! call.
//!
//! # Configuration
//!
//! `.basis/hooks.json` in the workspace, and `hooks.json` in the global config
//! directory. JSON rather than TOML because the wire contract is already JSON
//! and basis already parses it; `.basis/` because that is where basis's other
//! workspace data lives (`.basis/skills`).
//!
//! ```json
//! {
//!   "schema": 1,
//!   "hooks": [
//!     {
//!       "name": "no-force-push",
//!       "command": ["./.basis/hooks/no-force-push.sh"],
//!       "tools": ["spawn"],
//!       "event": "pre_tool_use",
//!       "timeout_ms": 5000,
//!       "on_failure": "deny"
//!     },
//!     {
//!       "name": "no-secrets",
//!       "command": ["./.basis/hooks/no-secrets.sh"],
//!       "event": "post_tool_use"
//!     }
//!   ]
//! }
//! ```
//!
//! `command` is an argv array, never a shell string: basis execs the program
//! directly, so nothing in a tool's input can be reinterpreted as shell syntax.
//! A relative program path is resolved against the workspace root; a bare name
//! is left to `PATH`, which is what a person writing the file expects. Omitting
//! `tools` means every tool; listing them matches on the exact tool name.
//! Omitting `event` means before the call, which is what every hooks file
//! written before there was a second event meant — one entry is asked at one
//! event, and a guard that wants both sides writes two.
//!
//! There is nothing equivalent for the in-process binding, because there is
//! nothing to discover: an interceptor is registered as a value, by code that
//! already exists.
//!
//! # Who speaks first
//!
//! Interceptors, in registration order; then global hooks; then workspace ones.
//! The rule is that the further a participant is from the workspace's own data,
//! the earlier it speaks — so the host's own guard, and then the operator's,
//! can refuse before a program a repository chose is ever spawned. Since any
//! deny short-circuits, that ordering is the whole of what ordering decides.
//! [`HookRunner`] carries the argument in full.
//!
//! # When a participant breaks
//!
//! Every hook has a deadline ([`DEFAULT_HOOK_TIMEOUT`]) and is killed at it, so
//! a hanging hook costs a turn its budget rather than the turn. Past that,
//! a hook that cannot answer — killed, exited non-zero, printed nothing,
//! printed something that is not a decision, asked for a rewrite basis cannot
//! use, could not be started at all — **denies the call by default**, and says
//! so on stderr either way. An interceptor that returns an error or panics
//! denies on the same terms.
//!
//! That default is the one real judgement call in this module, and the
//! reasoning is on [`OnFailure`]. In short: a participant's power is over
//! whether the call happens, so a configured one is by construction something
//! whose opinion the operator wanted, and the two ways of being wrong are not
//! symmetric — failing open on a broken guard silently removes a control
//! someone believes is in place, while failing closed on a broken observer is
//! loud and gets fixed. A hook that would rather be ignored says
//! `"on_failure": "allow"`; an interceptor that would rather be ignored returns
//! `Allow` in code it already owns.
//!
//! After the call, denying is the same ruling with the only power still
//! available: the model is shown the failure in place of the output, marked as
//! an error. A guard that broke while checking an output for credentials has
//! not established that there were none in it — and the stream still carries
//! what the tool really returned, so nothing is lost to whoever is auditing.
//!
//! # A hook takes as long as it takes
//!
//! Consulting a hook means spawning a process and waiting for it, so it is
//! genuinely blocking work. mentra's hook trait is async (since 0.16), so the
//! wait goes to `spawn_blocking` — a thread meant for it — rather than onto a
//! runtime worker. That holds on every runtime flavor, including
//! `current_thread`, which is what an embedder inside an editor or a
//! single-threaded server is likely to have.
//!
//! This used to require branching on `Handle::runtime_flavor()` and calling
//! `block_in_place`, which panics on `current_thread` and otherwise stalled
//! that runtime for the hook's whole timeout. The trait's shape was the reason
//! ([oops-rs/mentra#16](https://github.com/oops-rs/mentra/issues/16), fixed in
//! 0.16), and it mattered most for ACP, where ADR-0007 makes "the dispatch loop
//! is never blocked" an invariant.
//!
//! [`DEFAULT_HOOK_TIMEOUT`] still bounds how long any one hook can hold up the
//! turn it is vetting, which is a different question from which thread waits.
//! An interceptor is bounded by nothing basis imposes: it is the host's own code
//! on the host's own runtime, and a deadline basis invented for it would be basis
//! guessing at a budget the host can state.
//!
//! # A hook is code from the workspace
//!
//! `.basis/hooks.json` is workspace data, so cloning a repository and running basis
//! on it can execute commands that repository chose, before any tool call. That
//! is the same exposure as [`crate::shell`] and is bounded the same way: by
//! whatever confines the process (ADR-0004), not by a check in here. An
//! interceptor carries no such exposure, which is the other half of why the
//! host's own code speaks first.

mod chain;
pub mod contract;
mod interceptor;
mod runner;
pub mod wire;

mod config;

pub use config::{
    DEFAULT_GLOBAL_HOOKS_FILE, DEFAULT_HOOK_TIMEOUT, DEFAULT_WORKSPACE_HOOKS_FILE, HookConfigError,
    HookSpec, HooksConfig, HooksFile, HooksSource, OnFailure, discover, load,
};
pub use contract::{HookCall, HookEvent, HookOutcome, HookRequest};
pub use interceptor::{Interceptor, InterceptorError};
pub use runner::HookRunner;
pub use wire::{HOOK_SCHEMA_VERSION, HookResponse};
