//! Building the process-scoped substrate: everything that changes when the
//! host changes, and nothing that changes when a repository does.
//!
//! ADR-0018 moved these knobs off [`WorkspaceBuilder`](crate::WorkspaceBuilder):
//! the provider, the credential, the base URL, the history store policy, the
//! host's interceptors — plus the command environment, which the ADR's list
//! does not name but which is executor infrastructure and therefore fixed at
//! the same time everything else on a mentra runtime is. What stays on the
//! workspace is what the repository says.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use mentra::{
    BuiltinProvider, ModelSelector, ProviderId, RuntimePolicy,
    provider_core::{StaticCredentialSource, responses, responses::ResponsesProvider},
};

use crate::{
    approval::ApprovalGate,
    hooks::Interceptor,
    provider,
    run::RunError,
    shell::ShellAccess,
    store,
    tools::{
        SpawnTool,
        spawn::{LOCAL_TARGET, is_target_name},
    },
};

use super::{
    Runtime,
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
    model: ModelSelector,
    history: Option<History>,
    interceptors: Vec<Arc<dyn Interceptor>>,
    command_environment: BTreeMap<String, String>,
    /// The executors this runtime routes `!@<name>` commands to (ADR-0021).
    /// Names are validated at [`build`](Self::build) rather than here, which
    /// is where this builder answers every other piece of bad input.
    command_targets: CommandTargets,
    /// Registrars for host-supplied tools, applied in [`build_with`](Self::build_with)
    /// after basis's own `spawn`. A closure rather than a stored tool value
    /// because mentra's own `with_tool` is generic over the concrete tool type
    /// — nothing upstream implements `ExecutableTool` for `Box` or `Arc` (see
    /// `crate::tools`'s module doc) — so the concrete type has to be captured
    /// at the call site, in [`with_tool`](Self::with_tool), and erased behind
    /// `FnOnce` instead of behind the trait it can't yet be boxed as.
    host_tools:
        Vec<Box<dyn FnOnce(mentra::RuntimeBuilder) -> mentra::RuntimeBuilder + Send + Sync>>,
}

/// What a caller said about where this runtime's conversations go.
///
/// One field rather than a directory beside a flag, so that the two knobs which
/// set it cannot both be in force: whichever was called last is the one that is
/// read, and there is no state in which they disagree. `None` is *unsaid* —
/// mentra chooses, which is neither of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum History {
    /// [`RuntimeBuilder::with_store_dir`]: kept in this directory.
    Directory(PathBuf),
    /// [`RuntimeBuilder::with_ephemeral_history`]: kept in memory, and
    /// nowhere else.
    Ephemeral,
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
            model: ModelSelector::NewestAvailable,
            history: None,
            interceptors: Vec::new(),
            command_environment: BTreeMap::new(),
            command_targets: CommandTargets::new(),
            host_tools: Vec::new(),
        }
    }
}

impl RuntimeBuilder {
    pub fn with_provider(self, provider: BuiltinProvider) -> Self {
        Self {
            provider: Some(provider),
            ..self
        }
    }

    /// Points the runtime at an OpenAI-compatible endpoint. A trailing `/v1`
    /// is stripped during resolution — paste the URL a gateway publishes.
    /// Compatible endpoints use complete local replay rather than automatic
    /// `previous_response_id` chaining.
    pub fn with_base_url(self, base_url: impl Into<String>) -> Self {
        Self {
            base_url: Some(base_url.into()),
            ..self
        }
    }

    /// Supplies the provider credential directly, instead of having basis read it
    /// from the environment.
    ///
    /// A host whose key lives in a vault, a keychain, or a token it just
    /// exchanged should not have to export an environment variable for basis to
    /// find it again. Unset by default, which is the behavior every existing
    /// caller has: the key is looked up by the variable names the ecosystem
    /// already uses (see [`crate::provider`]).
    ///
    /// A key with no [`with_provider`](Self::with_provider) and no
    /// [`with_base_url`](Self::with_base_url) is refused rather than guessed
    /// at — with nothing to attribute it to, basis would be picking a service to
    /// send someone's credential to.
    pub fn with_api_key(self, api_key: impl Into<String>) -> Self {
        Self {
            api_key: Some(api_key.into()),
            ..self
        }
    }

    /// Sets the model resolution *policy*: what every workspace on this runtime
    /// resolves unless it overrides with
    /// [`WorkspaceBuilder::with_model`](crate::WorkspaceBuilder::with_model).
    ///
    /// A policy rather than a resolved model, because resolution needs the
    /// provider and may need the network, and both are workspace-open facts:
    /// the resolved id stays a [`Workspace`](crate::Workspace) fact (ADR-0018).
    pub fn with_model(self, model: ModelSelector) -> Self {
        Self { model, ..self }
    }

    /// Keeps this runtime's conversations in `dir` rather than in the
    /// machine-wide default.
    ///
    /// Unset, mentra chooses, and what it chooses is keyed by the **process's
    /// current directory** rather than by any workspace basis opened — so a host
    /// that opens two workspaces from one place writes both histories to one
    /// file, and a test suite writes to a real database under the user's data
    /// directory whatever temp directory it opened. Two callers want to say
    /// otherwise: a host that keeps basis's history inside its own application
    /// data, and a test that wants no persistent side effect at all. Both are
    /// asking the same question — *where* — so that is what this takes.
    /// [`with_ephemeral_history`](Self::with_ephemeral_history) answers it with
    /// *nowhere*, and is the last word between the two: whichever was called
    /// last decides.
    ///
    /// Not the store itself, though mentra's `RuntimeBuilder::with_store` would
    /// take one. `RuntimeStore` is a composition of nine traits, and under the
    /// rule written on [`CancellationToken`](crate::CancellationToken) — every
    /// mentra type basis's surface makes a caller *name*, basis re-exports — that
    /// shape would cost the re-export of all nine plus the record types they
    /// pass. What it would buy is reachable without it: mentra ships two
    /// stores, a SQLite file and an in-memory one, and between this and
    /// [`with_ephemeral_history`](Self::with_ephemeral_history) a caller
    /// already picks either without naming a mentra type. A caller that
    /// genuinely wants its own backend still has one, on
    /// [`Runtime::mentra_runtime`]'s side of the bargain: build the mentra
    /// runtime and drive it directly.
    ///
    /// The directory is created on first write, and basis names the file inside
    /// it — [`store::list_in`](crate::store::list_in) is how the same
    /// conversations are read back, and it has to be able to find them.
    /// Pointing this at [`store::default_directory`](crate::store::default_directory)
    /// is exactly the default.
    ///
    /// Deliberately absent from [`RunConfig`](crate::RunConfig), for the reason
    /// its `api_key` is: a one-prompt config describes an invocation, and where
    /// a machine keeps its history is not something an invocation decides. A
    /// one-shot caller that needs it hands
    /// [`RunConfig::split`](crate::RunConfig::split)'s builder a runtime recipe
    /// through [`WorkspaceBuilder::with_runtime_builder`](crate::WorkspaceBuilder::with_runtime_builder),
    /// which is the documented migration path.
    pub fn with_store_dir(self, dir: impl Into<PathBuf>) -> Self {
        Self {
            history: Some(History::Directory(dir.into())),
            ..self
        }
    }

    /// Keeps this runtime's conversations in memory, and nowhere else.
    ///
    /// The sibling of [`with_store_dir`](Self::with_store_dir), for the caller
    /// whose answer to *where* is *nowhere*. mentra's in-memory store backs it:
    /// no database file is opened, no transcript snapshot is written, no
    /// directory is created, and dropping the [`Runtime`] takes the history
    /// with it.
    ///
    /// **Nothing survives the process.** While the runtime lives a conversation
    /// behaves as it always does — [`Workspace::resume`](crate::Workspace::resume)
    /// finds an agent this runtime minted, because the store lives exactly as
    /// long as the runtime does. Past that edge there is nothing to find: a
    /// later process cannot resume one of these by agent id, a second runtime
    /// gets its own empty store, and
    /// [`store::list_in`](crate::store::list_in) has no file to read whichever
    /// directory it is pointed at, so `session/list` over ACP reports nothing.
    /// There is no flush and no export — a host that might want a transcript
    /// later wants [`with_store_dir`](Self::with_store_dir) now.
    ///
    /// Who asks for it. A test suite, which otherwise writes to the real
    /// database under the user's data directory. And a host whose conversations
    /// are genuinely disposable — a request-scoped run inside a server, a
    /// one-shot classifier — where keeping a transcript is a cost and a
    /// disclosure rather than a feature.
    ///
    /// Setting this and [`with_store_dir`](Self::with_store_dir) is not an
    /// error: they write one field, so the last call wins — the same rule as
    /// every single-valued knob on this builder, and what makes the
    /// half-configured builder this type advertises usable.
    pub fn with_ephemeral_history(self) -> Self {
        Self {
            history: Some(History::Ephemeral),
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
    pub fn with_tool<T>(self, tool: T) -> Self
    where
        T: mentra::tool::ExecutableTool + 'static,
    {
        Self {
            host_tools: {
                let mut host_tools = self.host_tools;
                host_tools.push(Box::new(move |builder| builder.with_tool(tool)));
                host_tools
            },
            ..self
        }
    }

    /// How long a command may run before it is killed.
    ///
    /// Two minutes by default, which suits the commands a harness usually runs
    /// and does not suit the ones that build software. A host whose agent runs
    /// container builds, test suites, or archives needs to say so: past the
    /// limit the process is killed mid-stream, and what reaches the caller is
    /// truncated output with no error in it — a build that looks like it
    /// failed silently rather than one that was stopped.
    ///
    /// Clamped by mentra's ceiling for the runtime's policy; asking for longer
    /// than that grants the ceiling rather than failing, because a host that
    /// asked for patience should not get less than the default for asking.
    #[must_use]
    pub fn with_command_timeout(self, timeout: std::time::Duration) -> Self {
        Self {
            command_timeout: Some(timeout),
            ..self
        }
    }

    /// Adds one fixed environment value to every command this runtime runs.
    ///
    /// Mentra clears the ambient environment, so a host must state execution
    /// context explicitly. A later call with the same name replaces the
    /// earlier value. Debug output names variables but redacts values.
    ///
    /// Runtime-scoped, so on a shared runtime every workspace's commands see
    /// the same pairs. A host that wants two concurrently driven workspaces to
    /// carry different identities gives each its own runtime through
    /// [`WorkspaceBuilder::with_runtime_builder`](crate::WorkspaceBuilder::with_runtime_builder),
    /// which is what the local task service does.
    pub fn with_command_environment(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.command_environment.insert(name.into(), value.into());
        self
    }

    /// Registers an executor this runtime's commands can be routed to by name.
    ///
    /// ADR-0021. `spawn` is still the model's one door, and *where a command
    /// runs* is a dimension of a call through it rather than a second tool:
    /// `!@<name> <command>` reaches the executor registered here under `name`,
    /// and a command with no `@` reaches the local one exactly as before. The
    /// case this exists for is basis running inside a Linux container on a
    /// macOS build machine, where `cargo test` belongs in the container and
    /// `xcodebuild` does not exist there at all.
    ///
    /// **basis ships no executors and claims nothing about what one reaches.**
    /// The host writes it — SSH to a forced command, `docker exec`, an agent
    /// on a build box — and a target is exactly as trusted as that code.
    /// `docs/targets.md` has the worked pattern, what the executor receives,
    /// and the honesty this cannot be written without: routing a command
    /// elsewhere is not confinement, and nothing here may be described as a
    /// sandbox (ADR-0013).
    ///
    /// What the executor is handed is a `CommandRequest` with this runtime's
    /// fixed command environment already merged, a timeout mentra has already
    /// clamped, and the `target` name still on it, so one executor registered
    /// under two names can tell which it was called as. The `cwd` is
    /// **advisory**: it is a path in *this* process's filesystem, and what it
    /// means on the far side is the executor's to decide.
    ///
    /// A later call with the same name replaces the earlier one, the same rule
    /// [`with_command_environment`](Self::with_command_environment) follows.
    /// Names are `[A-Za-z0-9_-]+` and may not be `local`, which is the wire
    /// word for *here*; a name that breaks either rule is a
    /// [`RunError::CommandTarget`] from [`build`](Self::build) rather than a
    /// panic here, because a host reading its targets out of its own
    /// configuration should be able to report a bad one the way it reports
    /// every other bad setting.
    ///
    /// Runtime-scoped, for ADR-0018's reason and one of its own: a target that
    /// changed per repository would be a different machine per repository,
    /// which is not a thing a repository knows.
    pub fn with_command_target(
        mut self,
        name: impl Into<String>,
        executor: impl mentra::runtime::RuntimeExecutor + 'static,
    ) -> Self {
        self.command_targets.insert(name.into(), Arc::new(executor));
        self
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
    ) -> Result<Runtime, RunError> {
        // Path roots are hygiene, not a boundary: per ADR-0004 that is the
        // kernel's job, and per ADR-0013 basis ships no instance of one. What
        // the caller said about commands is passed through as written.
        let policy = git_protected(RuntimePolicy::workspace_bounded(workspace), workspace)
            .allow_shell_commands(shell.is_granted())
            .allow_background_commands(shell.is_granted());
        let policy = with_command_patience(policy, self.command_timeout);

        self.build_with(store::runtime_identifier(workspace), policy)
    }

    fn build_with(self, identifier: String, policy: RuntimePolicy) -> Result<Runtime, RunError> {
        // Before anything is resolved or assembled, because a name that cannot
        // be routed on is a configuration mistake and not a runtime condition.
        validate_target_names(&self.command_targets)?;
        let target_names = self.command_targets.keys().cloned().collect::<Vec<_>>();

        let choice = provider::resolve_with(
            self.provider,
            self.base_url.as_deref(),
            self.api_key.as_deref(),
        )?;

        let dispatch = Arc::new(HookDispatch::new(self.interceptors));

        // Cloned into mentra below rather than moved, because the gate is the
        // only thing that sees a call's side-effect level and mentra never
        // hands an authorizer back. The kept half is what puts that level on
        // the `ApprovalRequest` an approver reads — interim, and mentra#21 is
        // where it ends (`crate::approval::SideEffectLevels`).
        let gate = ApprovalGate::new();
        let levels = gate.levels();

        let builder = mentra::Runtime::builder()
            // Which conversations belong where, which is the only question
            // `session/list` can honestly answer (see `crate::store`). Unset,
            // mentra tags every agent `"default"` and basis's own listing —
            // which filters on this — finds nothing, whatever was persisted.
            .with_runtime_identifier(identifier)
            .with_policy(policy)
            // Without an authorizer mentra allows every call unconditionally,
            // and no permission request can ever be raised — so the gate goes
            // on even for a runtime whose runs approve everything (see
            // `crate::approval`).
            .with_tool_authorizer(gate)
            // The one tool basis registers (ADR-0016). It has to be on the
            // runtime rather than on a session, because a subagent shares its
            // parent's runtime registry and `spawn` must reach the model at
            // every depth — the uniformity the ADR calls recursive.
            //
            // Told the target names, and only the names: the tool needs them to
            // teach the `!@` prefix and to refuse one nothing registered, while
            // *which executor* a name resolves to stays the runtime's business
            // (ADR-0021). With none registered this is `SpawnTool::new()` in
            // every observable respect, including that the model is never told
            // the prefix exists.
            .with_tool(SpawnTool::with_targets(target_names))
            // The one pre-hook basis registers, always: mentra takes hooks at
            // build time only, and workspaces arrive later, through the
            // dispatcher (see `runtime::dispatch`).
            .with_pre_hook(DispatchHook(Arc::clone(&dispatch)));

        // Host tools registered via `with_tool`, applied after basis's own
        // `spawn` — order that matches every other builder chain above, where
        // basis's fixed registrations run first and a caller's own choices
        // build on top of them.
        let builder = self
            .host_tools
            .into_iter()
            .fold(builder, |builder, register| register(builder));

        // Installed whenever either half has something to say. With both
        // empty, mentra keeps its own local executor and basis adds no layer
        // at all — the runtime a host that asked for neither has always had.
        let builder = if self.command_environment.is_empty() && self.command_targets.is_empty() {
            builder
        } else {
            builder.with_executor(TargetedExecutor::new(
                self.command_environment,
                self.command_targets,
            ))
        };

        // Left alone unless the caller said something, because mentra's default
        // is a real database a host may already have history in — moving it, or
        // dropping it on the floor, is a thing to be asked for and never a
        // thing to happen by upgrade.
        let builder = match &self.history {
            Some(History::Directory(dir)) => builder.with_store(store::store_in(dir)),
            Some(History::Ephemeral) => builder.with_store(store::volatile()),
            None => builder,
        };

        // `build`, not `build_async`: no MCP server is ever registered at the
        // runtime level, so there is nothing for the async constructor to
        // connect. Workspace-owned connections arrive post-build (ADR-0018).
        let mentra = match &choice.base_url {
            Some(base_url) => {
                builder.with_registered_provider(compatible_provider(base_url, &choice.api_key))
            }
            None => builder.with_provider(choice.provider, choice.api_key.clone()),
        }
        .build()?;

        Ok(Runtime {
            mentra,
            provider: choice.provider,
            provider_label: ProviderId::from(choice.provider).to_string(),
            model: self.model,
            dispatch,
            levels,
            #[cfg(feature = "mcp")]
            mcp_claims: Mutex::new(HashMap::new()),
            declared_claims: Mutex::new(HashMap::new()),
        })
    }
}

/// Refuses a target name basis cannot route on, before a runtime is built
/// around it.
///
/// Two rules, and both are about what the name has to survive downstream. It
/// is glob-matched inside a serialized rule pattern and printed into refusals
/// the model reads, so a name carrying a quote, a slash or a space would mean
/// one thing to the operator who wrote the rule and another to the matcher
/// reading it — hence the charset, which is the same predicate the `!@` parser
/// applies, from the same function, so the two can never disagree about which
/// names exist. And `local` is the wire word for *here*
/// ([`LOCAL_TARGET`]), so a target answering to it would make
/// `"target":"local"` mean two things in one field.
fn validate_target_names(targets: &CommandTargets) -> Result<(), RunError> {
    for name in targets.keys() {
        if !is_target_name(name) {
            return Err(RunError::CommandTarget {
                name: name.clone(),
                reason: "a target name is one or more of letters, digits, `_` and `-`".to_string(),
            });
        }

        if name == LOCAL_TARGET {
            return Err(RunError::CommandTarget {
                name: name.clone(),
                reason: format!(
                    "`{LOCAL_TARGET}` is what the wire contract calls a command that names no \
                     target, so nothing may be registered under it"
                ),
            });
        }
    }

    Ok(())
}

/// The command posture a shared runtime grants: shell and background on, with
/// `workspace_bounded`'s timeouts, and no path roots of its own.
///
/// Commands are on because ADR-0013 grants them by default and a shared policy
/// cannot say otherwise per workspace — the dispatcher's guard is where a
/// `ShellAccess::Denied` workspace is enforced. No roots, because mentra's
/// file bounding always allows under the calling agent's `base_dir`: with the
/// list empty, each workspace's agents are confined to their own directory and
/// no workspace's root widens another's.
pub(crate) fn shared_policy() -> RuntimePolicy {
    RuntimePolicy::default()
        .allow_shell_commands(true)
        .allow_background_commands(true)
        // workspace_bounded's numbers, restated because that constructor also
        // sets roots this policy must not have. A drift here would give shared
        // and private runtimes different command patience.
        .with_default_command_timeout(std::time::Duration::from_secs(120))
        .with_max_command_timeout(std::time::Duration::from_secs(600))
}

/// Applies a host's chosen command timeout, raising the ceiling to match.
///
/// The ceiling moves with the default because the two mean different things to
/// mentra — one is what a command gets when it asks for nothing, the other is
/// the most it may ask for — and a host setting the first past the second
/// would otherwise be silently clamped back to a number it did not choose.
fn with_command_patience(
    policy: RuntimePolicy,
    timeout: Option<std::time::Duration>,
) -> RuntimePolicy {
    match timeout {
        None => policy,
        Some(timeout) => policy
            .with_default_command_timeout(timeout)
            .with_max_command_timeout(timeout),
    }
}

/// Keeps the parts of `.git` that decide what *runs* out of reach.
///
/// `.git/hooks` holds programs git executes on ordinary operations, and
/// `.git/config` can name more of them (`core.hooksPath`, and the `filter`/
/// `diff` drivers that run on checkout). Writing either turns a file edit into
/// code execution outside anything basis's policy or approval covers, which is
/// why they are singled out rather than denying `.git` wholesale — an agent
/// legitimately reads `.git`, and `git` itself must keep writing objects and
/// refs underneath it.
///
/// **This binds the builtin file tools, not the shell.** A command like
/// `sh -c 'echo … > .git/hooks/pre-commit'` still reaches the path, because
/// nothing here parses shell. It closes the route a model actually takes and
/// remains hygiene; per ADR-0004 and ADR-0013 the boundary is the OS's, and
/// basis does not ship one. On shared runtimes the same rule is enforced by the
/// hook dispatcher, which knows which workspace a call belongs to; the private
/// path keeps this policy baking as a second belt.
fn git_protected(policy: RuntimePolicy, workspace: &Path) -> RuntimePolicy {
    let git = workspace.join(".git");
    policy
        .with_denied_write_root(git.join("hooks"))
        .with_denied_write_root(git.join("config"))
}

/// Builds a provider aimed at an OpenAI-compatible endpoint.
///
/// mentra's OpenAI preset is the right shape — the Responses wire format and
/// bearer auth — so basis takes that definition, swaps the base URL, and disables
/// automatic Hybrid HTTP state chaining. Building on the preset avoids
/// describing a provider from scratch and drifting from whatever mentra learns
/// next.
fn compatible_provider(base_url: &str, api_key: &str) -> ResponsesProvider<StaticCredentialSource> {
    let mut definition = responses::openai_definition();
    definition.base_url = Some(base_url.to_string());
    definition.descriptor.display_name = Some(format!("OpenAI-compatible ({base_url})"));

    // A compatible endpoint promises the Responses wire shape, not every
    // optional OpenAI extension. basis already replays the complete local
    // transcript, so do not probe `previous_response_id` support with a
    // request that may fail; native provider presets retain Hybrid chaining.
    ResponsesProvider::new(definition, StaticCredentialSource::new(api_key))
        .without_hybrid_http_previous_response_id()
}

#[cfg(test)]
mod tests;
