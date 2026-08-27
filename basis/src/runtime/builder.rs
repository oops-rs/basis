//! Building the process-scoped substrate: everything that changes when the
//! host changes, and nothing that changes when a repository does.
//!
//! ADR-0018 moved these knobs off [`WorkspaceBuilder`](crate::WorkspaceBuilder):
//! the provider, the credential, the base URL, the history store policy, the
//! host's interceptors — plus the command environment, which the ADR's list
//! does not name but which is executor infrastructure and therefore fixed at
//! the same time everything else on a mentra runtime is. What stays on the
//! workspace is what the repository says.

// One responsibility each, so a reader looking for a knob knows which file
// answers: where commands run, where history goes, how a model is reached —
// and, beside the last, where the provider question is settled at build.
// What stays here is the builder itself: its fields, its defaults, the
// registration knobs with no better home, and `build`.
mod execution;
mod history;
mod provider;
mod provider_settlement;

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use mentra::{BuiltinProvider, ModelSelector, RuntimePolicy};

use crate::{
    approval::ApprovalGate,
    error::RunError,
    hooks::Interceptor,
    shell::ShellAccess,
    store,
    tools::{ChildContext, ChildSpec, SpawnTool, spawn::ChildPolicy},
};

use execution::{shared_policy, validate_target_names, with_command_patience, workspace_policy};
pub(crate) use history::History;
use provider_settlement::HostProvider;

use super::{
    FileToolProfile, ProviderRetry, ResponsesTransport, Runtime, RuntimeExecutor, ToolResultPolicy,
    Wire,
    dispatch::{DispatchHook, HookDispatch},
    executor::{CommandTargets, TargetedExecutor},
};

/// The persist tag a shared runtime's own conversations carry until mentra can
/// tag per session (see [`Runtime::mint`]). Never a workspace's tag — those
/// come from [`store::runtime_identifier`] — so shared-path rows written under
/// it stay out of every per-workspace list rather than leaking into one.
const SHARED_IDENTIFIER: &str = "basis:runtime";

/// How a runtime is built.
///
/// Named a builder because it is one: filled in, then consumed by
/// [`build`](Self::build) — or embedded in a
/// [`WorkspaceBuilder`](crate::WorkspaceBuilder) via
/// [`with_runtime_builder`](crate::WorkspaceBuilder::with_runtime_builder),
/// where [`Workspace::open`](crate::Workspace::open) builds it bound to the
/// workspace's own path. Fields are private because one of them is a
/// credential. `with_*` returns a new value, so a host can keep a
/// half-configured builder and finish it differently per runtime.
pub struct RuntimeBuilder {
    command_timeout: Option<std::time::Duration>,
    provider: Option<BuiltinProvider>,
    base_url: Option<String>,
    api_key: Option<String>,
    /// The model *policy*; `None` is unsaid, which is what lets
    /// [`with_config`](Self::with_config) fill it from a file without
    /// outranking a host that named one. Resolved to
    /// [`ModelSelector::NewestAvailable`] at [`build`](Self::build), which is
    /// what an unsaid policy has always meant.
    model: Option<ModelSelector>,
    history: Option<History>,
    interceptors: Vec<Arc<dyn Interceptor>>,
    command_environment: BTreeMap<String, String>,
    /// How patiently a run waits out a failing provider
    /// ([`with_provider_retry`](Self::with_provider_retry)).
    ///
    /// A plain value rather than an `Option`, unlike [`History`] below: *unset*
    /// and *mentra's default* are the same schedule here, so there is nothing
    /// for a `None` to mean that `ProviderRetry::default()` does not already
    /// say. Applying it unconditionally therefore leaves a builder nobody
    /// touched building exactly the `RunOptions` mentra would have.
    provider_retry: ProviderRetry,
    /// How many attempts that schedule gets
    /// ([`with_provider_retry_budget`](Self::with_provider_retry_budget)).
    ///
    /// Separate from the field above because mentra keeps the two apart —
    /// `RunOptions::retry_budget` is a bare count beside the typed schedule —
    /// and seeded from mentra's own default for the same reason that one is.
    provider_retry_budget: usize,
    /// Which transport mentra streams the Responses wire format over
    /// ([`with_responses_transport`](Self::with_responses_transport)).
    ///
    /// An `Option` precisely because the reasoning above does not apply: the
    /// default is mentra's to state, basis has no business restating it, and
    /// `None` here means the builder chain never mentions transport at all.
    responses_transport: Option<ResponsesTransport>,
    /// Which request format a custom endpoint is spoken to in
    /// ([`with_wire`](Self::with_wire)).
    ///
    /// A plain value rather than an `Option`, unlike
    /// [`responses_transport`](Self::responses_transport) directly above: this
    /// default is basis's own to state and not mentra's, because mentra has no
    /// view on what is behind somebody's base URL and basis does — almost
    /// always `chat/completions`. Read only when
    /// [`base_url`](Self::base_url) is set; a provider preset carries its own
    /// wire and this cannot override it.
    wire: Wire,
    /// Which builtin file tools the model is offered
    /// ([`with_file_tools`](Self::with_file_tools)).
    ///
    /// A plain value rather than an `Option` — the opposite ruling to
    /// [`responses_transport`](Self::responses_transport) above, because basis
    /// has an opinion here that it does not have there: the default is
    /// [`FileToolProfile::Split`], not mentra's `Batched`, so *unsaid* and
    /// *mentra's default* are different answers and there is nothing for a
    /// `None` to mean. The chain states it unconditionally.
    file_tools: FileToolProfile,
    /// The executors this runtime routes `!@<name>` commands to (ADR-0021).
    /// Names are validated at [`build`](Self::build) rather than here, which
    /// is where this builder answers every other piece of bad input.
    command_targets: CommandTargets,
    /// Host-supplied tools, claimed by name in
    /// [`build_with`](Self::build_with) after the runtime is built and
    /// already carries basis's own registrations — see the claim loop there
    /// for why a name collision refuses rather than replacing (decision
    /// D5d). Stored as what they are: mentra implements the tool traits for
    /// `Box<T: ?Sized>` (mentra#22), so a boxed `dyn ExecutableTool` is
    /// itself an `ExecutableTool` and mentra's by-value `try_register_tool`
    /// takes it whole. `Send + Sync` are not restated on the box:
    /// `ToolDefinition` itself requires both.
    host_tools: Vec<Box<dyn mentra::tool::ExecutableTool>>,
    /// How many levels of delegation `spawn` will start before refusing
    /// ([`with_delegation_depth`](Self::with_delegation_depth), decision D9).
    delegation_depth: usize,
    /// Who a delegated child is ([`with_child_policy`](Self::with_child_policy),
    /// decision D4). `None` — the default — is inherit-everything, on the code
    /// path every runtime has always used.
    child_policy: Option<ChildPolicy>,
    /// How completed tool results are bounded before the next provider
    /// request. `None` preserves the Mentra policy derived for this runtime;
    /// `Some` overlays only bytes, physical lines, and spill posture.
    tool_result_policy: Option<ToolResultPolicy>,
    /// A provider the host constructed itself, through either the runtime-level
    /// [`with_provider_instance`](Self::with_provider_instance) seam or the
    /// provider-core [`with_registered_provider`](Self::with_registered_provider)
    /// seam. When set, the provider question is answered: resolution never
    /// runs, the environment is never read, and [`build`](Self::build) refuses
    /// the knobs resolution would have read beside it.
    host_provider: Option<HostProvider>,
}

/// Hand-written so a supplied credential cannot reach a log through a
/// `{:?}`. Everything else is printed as it is; the command environment names
/// its variables and redacts their values.
impl std::fmt::Debug for RuntimeBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeBuilder")
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field(
                "provider_instance",
                &self.host_provider.as_ref().map(|host| host.id.as_str()),
            )
            .field("model", &self.model)
            .field("history", &self.history)
            .field(
                "interceptors",
                &self
                    .interceptors
                    .iter()
                    .map(|interceptor| interceptor.name())
                    .collect::<Vec<_>>(),
            )
            .field("provider_retry", &self.provider_retry)
            .field("provider_retry_budget", &self.provider_retry_budget)
            .field("responses_transport", &self.responses_transport)
            .field("wire", &self.wire)
            .field("file_tools", &self.file_tools)
            .field("delegation_depth", &self.delegation_depth)
            .field("tool_result_policy", &self.tool_result_policy)
            // Presence is all a `dyn` policy can honestly print.
            .field(
                "child_policy",
                &self.child_policy.as_ref().map(|_| "<child policy>"),
            )
            .field(
                "command_environment",
                &self.command_environment.keys().collect::<Vec<_>>(),
            )
            // Names, never executors: a host's executor closes over whatever
            // reaches its machine — a key path, a token, a connection — and
            // none of that belongs in a log line.
            .field(
                "command_targets",
                &self.command_targets.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Default for RuntimeBuilder {
    fn default() -> Self {
        Self {
            command_timeout: None,
            provider: None,
            base_url: None,
            api_key: None,
            model: None,
            history: None,
            interceptors: Vec::new(),
            provider_retry: ProviderRetry::default(),
            provider_retry_budget: mentra::runtime::RunOptions::default().retry_budget,
            responses_transport: None,
            wire: Wire::ChatCompletions,
            file_tools: FileToolProfile::Split,
            command_environment: BTreeMap::new(),
            command_targets: CommandTargets::new(),
            host_tools: Vec::new(),
            delegation_depth: crate::tools::DEFAULT_DELEGATION_DEPTH,
            child_policy: None,
            tool_result_policy: None,
            host_provider: None,
        }
    }
}

impl RuntimeBuilder {
    /// Which builtin file tools this runtime offers the model.
    /// [`FileToolProfile::Split`] by default, which is not mentra's default.
    ///
    /// **The roster is the model's API, and this is the one place basis writes
    /// it.** mentra's `Batched` profile registers a single `files` tool whose
    /// input is an `operations` array over nine variants —
    /// `read`/`list`/`search`/`create`/`set`/`replace`/`insert`/`move`/`delete`
    /// — so reading one file means picking a branch out of a nine-way `oneOf`
    /// and nesting the path inside an array of objects. `Split` registers the
    /// six names every model in this class was trained on: `read`, `ls`,
    /// `grep`, `glob`, `write`, `edit`. Same workspace engine underneath, same
    /// policy, same hook points — a different surface presented to the one
    /// consumer that cannot be given a migration note.
    ///
    /// Two of the differences are capability rather than shape.
    /// mentra's `grep` carries `glob`, `ignore_case`, `literal`, `context` and
    /// `multiline`; the batched `search` op hardcodes all five to their
    /// defaults, so a case-insensitive search scoped to `*.rs` is not
    /// expressible through `files` at all. And `glob` — find files whose path
    /// matches a pattern — has **no** batched equivalent, so under `Batched` a
    /// model that wants one reaches for a shell command instead, which is a
    /// tool call that goes to the approver in place of a read that would not
    /// have.
    ///
    /// **Who wants `Batched` back.** A host whose `.basis/hooks.json` matchers
    /// or whose operators' remembered rules name `files`: both key on the
    /// exact tool name, so under `Split` a `"tools": ["files"]` entry stops
    /// matching and nothing errors — the same silent-stop ADR-0016's
    /// `shell` → `spawn` note describes. Choosing `Batched` keeps the roster
    /// those were written against, unchanged, for as long as the host needs to
    /// rewrite them. That is a migration path rather than an opinion; the
    /// opinion is the default. `Both` exists too, and costs the model both
    /// surfaces in its context for one engine.
    ///
    /// Runtime-scoped (ADR-0018) because the roster is a property of the
    /// mentra runtime's registry, which is fixed at build: every workspace on
    /// this runtime, and every subagent, is offered the same set.
    #[must_use]
    pub fn with_file_tools(self, file_tools: FileToolProfile) -> Self {
        Self { file_tools, ..self }
    }

    /// Sets the limits applied to completed tool results before the next
    /// provider request.
    ///
    /// This is intentionally narrower than Mentra's [`RuntimePolicy`]: Basis
    /// continues to derive filesystem, command, timeout, and process posture.
    /// Only the result byte limit, physical-line limit, and spill posture from
    /// `tool_result_policy` replace that derived policy's corresponding
    /// values. If this method is never called, existing Mentra defaults remain
    /// untouched.
    #[must_use]
    pub fn with_tool_result_policy(self, tool_result_policy: ToolResultPolicy) -> Self {
        Self {
            tool_result_policy: Some(tool_result_policy),
            ..self
        }
    }

    /// Gives the host's own code a say over each tool call, on every workspace
    /// this runtime carries.
    ///
    /// The in-process binding of ADR-0012's interception contract, and the
    /// sibling of [`WorkspaceBuilder::with_hooks`](crate::WorkspaceBuilder::with_hooks):
    /// same vocabulary — allow, deny with a reason, modify with a replacement
    /// input — and the same chain. What it buys is the case a subprocess
    /// answers badly, because the judgement needs something the embedding
    /// program is already holding: the vault handle, the token it just
    /// exchanged, the policy it parsed at startup. Redacting a credential out
    /// of a tool's input is the worked example.
    ///
    /// Runtime-scoped because host scope *is* runtime scope (ADR-0018): the
    /// chain has always run host interceptors → global hooks → workspace
    /// hooks, and this is the registration point that matches the first slot.
    /// Appends, so a host may register several; they are consulted in the
    /// order registered, and **before** any subprocess hook. The rule is that
    /// the further a participant is from the workspace's own data, the earlier
    /// it speaks — an interceptor is compiled into this program, while
    /// `.basis/hooks.json` came with a repository — and since the first refusal
    /// short-circuits, that is what lets the host's own guard stop a
    /// repository's program from being spawned at all. It is not a claim of
    /// precedence: a hook still sees, and can still refuse, whatever an
    /// interceptor rewrote.
    ///
    /// Fail-closed carries over unchanged: an interceptor that returns an error
    /// or panics denies the call, and says which one it was.
    pub fn with_interceptor(self, interceptor: impl Interceptor + 'static) -> Self {
        Self {
            interceptors: {
                let mut interceptors = self.interceptors;
                interceptors.push(Arc::new(interceptor));
                interceptors
            },
            ..self
        }
    }

    /// Registers a tool the *host* implements, in the embedding program's own
    /// process — mentra's `ExecutableTool`, not a [`crate::tools::declared`]
    /// manifest entry.
    ///
    /// The gap this closes: `.basis/tools.json` gives a workspace's own repo a
    /// tool, wrapping a subprocess that speaks JSON over stdio and sees
    /// nothing beyond that JSON — no session, no caller identity, nothing the
    /// host knows about the call it is answering. A host tool runs in the same
    /// process as the code that is driving the run, so it can close over
    /// whatever context that code already has (a client handle, a connection,
    /// which conversation this is) instead of receiving it, or failing to.
    ///
    /// Registered on the runtime (ADR-0018's host scope), so — like `spawn` —
    /// it is visible to every workspace and every subagent this runtime opens,
    /// not to one session. A host that wants a tool visible to only *some*
    /// workspaces still needs one runtime per audience; there is no per-
    /// workspace host-tool registration yet.
    ///
    /// **A name basis or an earlier host tool already answers to is refused,
    /// not replaced** (decision D5d). [`build`](Self::build) claims host
    /// tools only after `spawn`, the builtins, and everything else basis
    /// registers unconditionally already exist on the runtime, with mentra's
    /// `try_register_tool` rather than its plain `with_tool` — a host tool
    /// named `spawn` fails the build naming the collision
    /// ([`RunError::HostTool`](crate::RunError::HostTool)) instead of quietly
    /// taking over the name and every rule an operator ever wrote about
    /// commands and delegation.
    pub fn with_tool<T>(self, tool: T) -> Self
    where
        T: mentra::tool::ExecutableTool + 'static,
    {
        Self {
            host_tools: {
                let mut host_tools = self.host_tools;
                host_tools.push(Box::new(tool));
                host_tools
            },
            ..self
        }
    }

    /// How many levels of delegation `spawn` will start before refusing, on
    /// every workspace this runtime carries (decision D9).
    ///
    /// [`crate::tools::DEFAULT_DELEGATION_DEPTH`] (two) unless a caller says
    /// otherwise here — the smallest bound that leaves delegation
    /// compositional (a subagent may split its own work once) while keeping
    /// runaway recursion structurally impossible rather than merely unlikely.
    /// The root run is depth 0, so the deepest agent that can still delegate
    /// is one less than this value.
    ///
    /// The guard's shape does not move with the number: it is still basis's
    /// own ledger (mentra's floor is name-specific and does not fire for a
    /// registered tool), and it still refuses *in the preview*, so a
    /// remembered allow-rule cannot lift whatever floor is set here.
    #[must_use]
    pub fn with_delegation_depth(self, depth: usize) -> Self {
        Self {
            delegation_depth: depth,
            ..self
        }
    }

    /// Decides who a delegated child is, per delegation (decision D4).
    ///
    /// `spawn` has always minted a subagent as an exact clone of its parent —
    /// same roster, same model, same system prompt — and unset, it still
    /// does, byte for byte. A policy makes the clone a *default* instead of
    /// the only shape: consulted with what `spawn` knows about the delegation
    /// ([`ChildContext`] — the child's prompt, the parent's agent id, the
    /// workspace directory), it answers which of those three inherited facts
    /// to override ([`ChildSpec`]), and [`ChildSpec::inherit`] is today's
    /// behavior exactly. Cheap triage beside a full fixer is the shape this
    /// exists for: a prompt-prefix convention routed to a narrowed roster and
    /// a cheaper model, with everything else inherited —
    /// `examples/child_policy.rs` runs it.
    ///
    /// Runtime-scoped, like the depth floor above and for the same reason:
    /// `spawn` is registered on the runtime, every workspace and every
    /// subagent on it shares the one instance, so the policy is consulted at
    /// every depth — a child's own delegations answer to it too, which is how
    /// a host confines a whole chain rather than one generation.
    ///
    /// Three facts and no more travel through a spec, deliberately. Bounds
    /// stay on the run options a child already inherits (deadline, budgets,
    /// cancellation, the shared token counter) — a second spelling here would
    /// be a second bounds system. The depth floor is checked before the
    /// policy runs, so no override lifts it. And the approver sees what the
    /// policy decided: a delegation with overrides carries an additive
    /// `child` key in its preview, so a remembered rule can match on what the
    /// child will be, while an inherit answer leaves the preview byte-
    /// identical to a policy-free runtime's. [`ChildSpec`]'s module docs
    /// carry the rest, including why a system-prompt override is
    /// replace-wholesale with no append.
    ///
    /// The policy should be a pure function of its context: it is consulted
    /// once for the preview and once at execution, and one that answers
    /// differently between the two shows the approver a child it will not
    /// spawn.
    #[must_use]
    pub fn with_child_policy<F>(self, policy: F) -> Self
    where
        F: Fn(&ChildContext<'_>) -> ChildSpec + Send + Sync + 'static,
    {
        Self {
            child_policy: Some(Arc::new(policy)),
            ..self
        }
    }

    /// Builds the workspace-agnostic runtime: the substrate an N-repository
    /// host hands to every [`WorkspaceBuilder::with_runtime`](crate::WorkspaceBuilder::with_runtime).
    ///
    /// Synchronous, and deliberately so: nothing here needs the network. The
    /// provider is resolved (credential lookup, no request), the mentra
    /// runtime is assembled, and that is all — MCP servers are a workspace
    /// concern and are connected by [`Workspace::open`](crate::Workspace::open),
    /// never here.
    ///
    /// Per-workspace file confinement needs no policy roots: mentra's builtin
    /// file tools always allow paths under the calling agent's own `base_dir`,
    /// which basis sets per workspace. What this policy grants is command
    /// execution — shell and background on, workspace-bounded's timeouts — and
    /// a workspace that says [`ShellAccess::Denied`] is enforced per-workspace
    /// by the runtime's hook dispatcher instead of by this shared policy.
    pub fn build(self) -> Result<Runtime, RunError> {
        let policy = with_command_patience(shared_policy(), self.command_timeout);
        self.build_with(SHARED_IDENTIFIER.to_string(), policy)
    }

    /// The sugar path: the same build, bound to one workspace.
    ///
    /// What `Workspace::open(path)` has always done, byte for byte — the
    /// per-path persist identifier, `git_protected(workspace_bounded(path))`,
    /// the caller's shell posture baked into policy as a second belt beside
    /// the dispatcher's guard.
    pub(crate) fn build_for(
        self,
        workspace: &Path,
        shell: ShellAccess,
        memory_roots: &[PathBuf],
    ) -> Result<Runtime, RunError> {
        let policy = with_command_patience(
            workspace_policy(workspace, shell, memory_roots),
            self.command_timeout,
        );

        self.build_with(store::runtime_identifier(workspace), policy)
    }

    fn build_with(self, identifier: String, policy: RuntimePolicy) -> Result<Runtime, RunError> {
        let policy = match self.tool_result_policy {
            Some(tool_result_policy) => tool_result_policy.apply_to(policy),
            None => policy,
        };

        // First, before even the credential is looked up, because opening the
        // store is what refuses a directory still holding a basis ≤0.6
        // database (ADR-0023's no-migration ruling) and that is the most
        // fundamental fact an upgrade can trip over: a missing key is fixable
        // in the environment, this needs a decision about the data.
        // `History::open` has the rest.
        let history = History::open(self.history.as_ref())?;

        // Before anything is resolved or assembled, because a name that cannot
        // be routed on is a configuration mistake and not a runtime condition.
        validate_target_names(&self.command_targets)?;
        let target_names = self.command_targets.keys().cloned().collect::<Vec<_>>();
        // Behind an `Arc` from here on, because two things read it: the
        // executor every `spawn` command goes through, and — since these pairs
        // are the host's statement about *every* process this runtime spawns —
        // each declared tool's subprocess (`crate::tools::declared`).
        let command_environment = Arc::new(self.command_environment);
        // Taken out here and claimed after the runtime is built (decision
        // D5d) — see the loop beside `Ok(Runtime { .. })` below for why.
        let host_tools = self.host_tools;

        // Which provider this runtime runs on, settled before assembly —
        // `provider_settlement::settle`'s own docs have the ambiguity rule.
        let source = provider_settlement::settle(
            self.host_provider,
            self.provider,
            self.base_url,
            self.api_key,
        )?;

        let dispatch = Arc::new(HookDispatch::new(self.interceptors));

        let builder = mentra::Runtime::builder()
            // Which conversations belong where, which is the only question
            // `session/list` can honestly answer (see `crate::store`). Unset,
            // mentra tags every agent `"default"` and basis's own listing —
            // which filters on this — finds nothing, whatever was persisted.
            .with_runtime_identifier(identifier)
            .with_policy(policy)
            // Which file tools the model is offered. Stated unconditionally
            // because basis's default differs from mentra's: `Split` puts the
            // six names models are trained on where a nine-variant `oneOf`
            // used to be, and is the only profile carrying `glob` and `grep`'s
            // own knobs (see `with_file_tools`). A host that needs the old
            // roster back says so and gets it here.
            .with_file_tools(self.file_tools)
            // Without an authorizer mentra allows every call unconditionally,
            // and no permission request can ever be raised — so the gate goes
            // on even for a runtime whose runs approve everything (see
            // `crate::approval`).
            .with_tool_authorizer(ApprovalGate::new())
            // The one tool basis registers (ADR-0016). It has to be on the
            // runtime rather than on a session, because a subagent shares its
            // parent's runtime registry and `spawn` must reach the model at
            // every depth — the uniformity the ADR calls recursive, and the
            // reason the child policy below reaches every depth too (D4).
            //
            // Told the target names, and only the names: the tool needs them to
            // teach the `!@` prefix and to refuse one nothing registered, while
            // *which executor* a name resolves to stays the runtime's business
            // (ADR-0021). With none registered, the default depth and no child
            // policy this is `SpawnTool::new()` in every observable respect,
            // including that the model is never told the prefix exists.
            .with_tool(spawn_tool(
                target_names,
                self.delegation_depth,
                self.child_policy,
                Arc::clone(&dispatch),
            ))
            // The one pre-hook basis registers, always: mentra takes hooks at
            // build time only, and workspaces arrive later, through the
            // dispatcher (see `runtime::dispatch`).
            .with_pre_hook(DispatchHook(Arc::clone(&dispatch)))
            // And the one post-hook, for the same reason and the same
            // dispatcher — a second handle on it, because mentra's two seams
            // are two registrations. Always, again: whether a workspace will
            // declare a `post_tool_use` hook is not knowable from here.
            .with_post_hook(DispatchHook(Arc::clone(&dispatch)));

        // Installed whenever either half has something to say. With both
        // empty, mentra keeps its own local executor and basis adds no layer
        // at all — the runtime a host that asked for neither has always had.
        let builder = if command_environment.is_empty() && self.command_targets.is_empty() {
            builder
        } else {
            builder.with_executor(TargetedExecutor::new(
                Arc::clone(&command_environment),
                self.command_targets,
            ))
        };

        // Only when the host named one. mentra owns both transports and the
        // default between them, and restating that default here would be a
        // second opinion to keep in step with upstream's first.
        let builder = match self.responses_transport {
            Some(transport) => builder.with_responses_transport(transport),
            None => builder,
        };

        // Left alone unless the caller said something, because mentra's default
        // is a real store a host may already have history in — moving it, or
        // dropping it on the floor, is a thing to be asked for and never a
        // thing to happen by upgrade. `history` is the store opened above,
        // already past the legacy-database refusal.
        let builder = match (history, &self.history) {
            (Some(store), _) => builder.with_store(store),
            (None, Some(History::Ephemeral)) => builder.with_store(store::volatile()),
            (None, _) => builder,
        };

        // The same posture applied to the other thing mentra writes about a
        // conversation, derived beside the store it belongs next to so
        // `with_store_dir` moves both or neither — `History::transcripts`.
        let transcripts = History::transcripts(self.history.as_ref());

        // A base URL is the only place the wire is a question: a preset
        // carries the one its vendor speaks, and an instance *is* its wire —
        // `provider_settlement::assemble`'s own docs have the rest.
        let (mentra, provider) = provider_settlement::assemble(source, builder, self.wire)?;

        // Claimed one at a time, now that the runtime exists and already
        // carries every registration basis makes unconditionally — `spawn`,
        // mentra's own builtins, whatever a host-supplied provider's
        // `install` added (decision D5d). mentra's plain `with_tool` on the
        // builder chain above *replaces* on a name collision, which is the
        // right behavior for deliberately overriding a builtin and the wrong
        // one for a host tool that did not mean to shadow `spawn` and inherit
        // every rule an operator ever wrote about commands and delegation.
        // `try_register_tool` refuses instead, and does it against the live
        // registry, so the second host tool sharing an earlier one's name
        // collides too — the same claim posture
        // `Runtime::claim_declared_tool` holds for a declared tool naming
        // one basis or a workspace already answers to.
        for tool in host_tools {
            mentra.try_register_tool(tool)?;
        }

        Ok(Runtime {
            mentra,
            command_environment,
            provider,
            // Unsaid is the newest the provider offers, which is what this
            // builder has always resolved for a caller that named no model.
            model: self.model.unwrap_or(ModelSelector::NewestAvailable),
            provider_retry: self.provider_retry,
            provider_retry_budget: self.provider_retry_budget,
            transcripts,
            dispatch,
            #[cfg(feature = "mcp")]
            mcp_claims: Mutex::new(HashMap::new()),
            declared_claims: Mutex::new(HashMap::new()),
        })
    }
}

/// The one tool basis registers, assembled once.
///
/// Built here rather than inline so the two conditional facts — whether a
/// child policy was set, and the workspace registry the roster guard reads —
/// are attached to *one* construction. With no policy this is
/// `SpawnTool::with_targets_and_depth` plus a registry handle that only a
/// roster override ever reads, which is `SpawnTool::new()` in every
/// observable respect for a runtime that registered no targets and kept the
/// default depth.
fn spawn_tool(
    targets: Vec<String>,
    delegation_depth: usize,
    child_policy: Option<ChildPolicy>,
    workspaces: Arc<HookDispatch>,
) -> SpawnTool {
    let tool = SpawnTool::with_targets_and_depth(targets, delegation_depth).with_workspaces(
        // Read for one question — what is this delegation's workspace denied
        // — so a narrowed child cannot be handed a sibling's tools (D4, R1).
        workspaces,
    );

    match child_policy {
        // The stored `Arc` goes straight through: re-wrapping it in a closure
        // and a second `Arc` would add an indirection per delegation for
        // nothing.
        Some(policy) => tool.with_child_policy_arc(policy),
        None => tool,
    }
}

#[cfg(test)]
mod tests;
