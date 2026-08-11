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
//!   [`WorkspaceBuilder::with_interceptor`](crate::WorkspaceBuilder::with_interceptor).
//!   Its code, its process, its dependencies; the case a subprocess answers
//!   badly, because redacting a credential needs the vault handle the embedding
//!   program is already holding.
//! - **Subprocess** — a workspace declares a command in `.lan/hooks.json` and
//!   lan execs it with one JSON object on stdin, reading one JSON object back.
//!   Process isolation and any language; no scripting runtime is embedded
//!   (ADR-0001, and `docs/proposals/0001`, which stays deferred).
//!
//! Both answer in one vocabulary — allow, deny with a reason, modify with a
//! replacement input — and both go through one [`HookRunner`], so the ordering,
//! the short-circuit and the threading of a modification are decided once for
//! the pair rather than twice.
//!
//! The other seam is approval ([`crate::approval`]), and the two are siblings
//! rather than one thing: an [`Approver`](crate::approval::Approver) answers
//! *may this happen* to whoever is watching, and an [`Interceptor`] answers
//! *may this happen, in this form* as part of a composing chain. mentra keeps
//! `ToolAuthorizer` and `PreExecutionHook` apart for that reason and lan binds
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
//! # The seam underneath
//!
//! mentra's [`PreExecutionHook`](mentra::runtime::PreExecutionHook) fires after
//! authorization and before the tool runs. lan registers exactly one
//! implementation, [`HookRunner`], which fans out to every interceptor and every
//! configured command — not because it must (`with_pre_hook` appends) but
//! because lan wants to own the ordering and the short-circuit.
//!
//! # Configuration
//!
//! `.lan/hooks.json` in the workspace, and `hooks.json` in the global config
//! directory. JSON rather than TOML because the wire contract is already JSON
//! and lan already parses it; `.lan/` because that is where lan's other
//! workspace data lives (`.lan/skills`).
//!
//! ```json
//! {
//!   "schema": 1,
//!   "hooks": [
//!     {
//!       "name": "no-force-push",
//!       "command": ["./.lan/hooks/no-force-push.sh"],
//!       "tools": ["shell"],
//!       "timeout_ms": 5000,
//!       "on_failure": "deny"
//!     }
//!   ]
//! }
//! ```
//!
//! `command` is an argv array, never a shell string: lan execs the program
//! directly, so nothing in a tool's input can be reinterpreted as shell syntax.
//! A relative program path is resolved against the workspace root; a bare name
//! is left to `PATH`, which is what a person writing the file expects. Omitting
//! `tools` means every tool; listing them matches on the exact tool name.
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
//! printed something that is not a decision, asked for a rewrite lan cannot
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
//! An interceptor is bounded by nothing lan imposes: it is the host's own code
//! on the host's own runtime, and a deadline lan invented for it would be lan
//! guessing at a budget the host can state.
//!
//! # A hook is code from the workspace
//!
//! `.lan/hooks.json` is workspace data, so cloning a repository and running lan
//! on it can execute commands that repository chose, before any tool call. That
//! is the same exposure as [`crate::shell`] and is bounded the same way: by
//! whatever confines the process (ADR-0004), not by a check in here. An
//! interceptor carries no such exposure, which is the other half of why the
//! host's own code speaks first.

mod chain;
pub mod contract;
mod exec;
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
