//! Building the process-scoped substrate: everything that changes when the
//! host changes, and nothing that changes when a repository does.
//!
//! ADR-0018 moved these knobs off [`WorkspaceBuilder`](crate::WorkspaceBuilder):
//! the provider, the credential, the base URL, the history store policy, the
//! host's interceptors — plus the command environment, which the ADR's list
//! does not name but which is executor infrastructure and therefore fixed at
//! the same time everything else on a mentra runtime is. What stays on the
//! workspace is what the repository says.

mod provider_settlement;

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use mentra::{BuiltinProvider, ModelSelector, Provider, RuntimePolicy};

use crate::{
    approval::ApprovalGate,
    error::RunError,
    hooks::Interceptor,
    shell::ShellAccess,
    store,
    tools::{
        SpawnTool,
        spawn::{LOCAL_TARGET, is_target_name},
    },
};

use provider_settlement::HostProvider;

use super::{
    FileToolProfile, ProviderRetry, ResponsesTransport, Runtime, RuntimeExecutor, Wire,
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
    /// A provider the host constructed itself
    /// ([`with_provider_instance`](Self::with_provider_instance)). When set,
    /// the provider question is answered: resolution never runs, the
    /// environment is never read, and [`build`](Self::build) refuses the
    /// knobs resolution would have read beside it.
    host_provider: Option<HostProvider>,
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
            host_provider: None,
        }
    }
}

impl RuntimeBuilder {
    /// Names the provider basis resolves the credential and the models
    /// against — one of the three knobs [`crate::provider`]'s resolution
    /// reads, beside [`with_base_url`](Self::with_base_url) and
    /// [`with_api_key`](Self::with_api_key). Like them it cannot sit beside a
    /// [`with_provider_instance`](Self::with_provider_instance): the instance
    /// already answers what this chooses, so [`build`](Self::build) refuses
    /// the pair by name rather than ranking it.
    pub fn with_provider(self, provider: BuiltinProvider) -> Self {
        Self {
            provider: Some(provider),
            ..self
        }
    }

    /// Runs this runtime on a provider the *host* constructed, instead of one
    /// basis resolves.
    ///
    /// mentra's own seam, surfaced: an implementation of
    /// [`Provider`](crate::Provider) — a vendor SDK already living in the
    /// host's process, a gateway spoken to in a shape basis has no preset
    /// for, a scripted provider in a test — is registered under the id its
    /// own descriptor reports. Every workspace on this runtime resolves
    /// models against it and streams turns through it, and
    /// [`Runtime::provider`] reports its id.
    ///
    /// **An instance is an answer, not a preference.** With one supplied,
    /// [`crate::provider`]'s resolution never runs: no environment variable
    /// is read, no credential is looked up, and `build` stays as offline as
    /// ever. The knobs resolution reads therefore cannot sit beside one —
    /// [`with_provider`](Self::with_provider),
    /// [`with_base_url`](Self::with_base_url) and
    /// [`with_api_key`](Self::with_api_key) are each refused at
    /// [`build`](Self::build) with
    /// [`ProviderError::AmbiguousProviderSource`](crate::provider::ProviderError::AmbiguousProviderSource),
    /// whichever order they were called in — a named refusal, the same
    /// posture as the unattributed credential, because a silent priority is a
    /// knob that silently stopped working. A `config.json`'s `provider` and
    /// `base_url` yield instead ([`with_config`](Self::with_config) fills
    /// emptiness, and the question is no longer empty), and
    /// [`with_wire`](Self::with_wire) has nothing left to say: it is read
    /// only under a base URL, and the instance speaks whatever wire it
    /// implements.
    ///
    /// A later call replaces the earlier instance — the one-value rule every
    /// single-valued knob here follows.
    ///
    /// The trait is re-exported at the crate root, and everything an
    /// implementation touches as [`crate::runtime`]'s provider-authoring
    /// re-exports, so a host writes one against `basis` alone.
    #[must_use]
    pub fn with_provider_instance<P>(self, provider: P) -> Self
    where
        P: Provider + 'static,
    {
        let id = provider.descriptor().id;
        Self {
            host_provider: Some(HostProvider {
                id,
                install: Box::new(move |builder| builder.with_provider_instance(provider)),
            }),
            ..self
        }
    }

    /// Points the runtime at an OpenAI-compatible endpoint.
    ///
    /// Paste the URL the server publishes. A trailing `/v1` is stripped during
    /// resolution, because every gateway advertises itself with one — that is
    /// the form the OpenAI SDKs take — and mentra's transports append their
    /// own `v1/…`; without the strip the published URL would produce
    /// `/v1/v1/…` and a 404 that names nothing.
    ///
    /// **The endpoint is spoken to in `chat/completions`**, which is what
    /// "OpenAI-compatible" means in the wild: Ollama, LM Studio, vLLM,
    /// llama.cpp, and the gateways in front of them serve that wire and not
    /// OpenAI's own `v1/responses`. A proxy that does serve Responses is
    /// reached by saying [`with_wire(Wire::Responses)`](Self::with_wire), and
    /// such an endpoint then uses complete local replay rather than automatic
    /// `previous_response_id` chaining.
    ///
    /// Beside a [`with_provider_instance`](Self::with_provider_instance) this
    /// is refused at [`build`](Self::build): an instance reaches its endpoint
    /// itself, so a base URL next to one has nowhere left to point.
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
    ///
    /// Beside a [`with_provider_instance`](Self::with_provider_instance) this
    /// is refused at [`build`](Self::build) for the same reason: an instance
    /// authenticates itself, so a key basis cannot hand it is a credential on
    /// its way to being ignored.
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
        Self {
            model: Some(model),
            ..self
        }
    }

    /// Fills in what a `config.json` said, wherever this builder has not been
    /// told otherwise.
    ///
    /// The provider, the endpoint and the model policy are this builder's
    /// three answers that a [`Config`](crate::Config) can also give, and it
    /// gives them from a file rather than from the process's arguments — so
    /// they go *below* every `with_*` call above and *above* the environment,
    /// which [`build`](Self::build) consults only for what nothing has
    /// answered. A host calling `with_provider` and then this keeps its
    /// provider; a host calling them the other way round keeps it too, because
    /// what this reads is emptiness rather than order.
    ///
    /// `effort` is not here because a runtime has no effort: it is a per-turn
    /// request, and [`Workspace`](crate::Workspace) applies the file's answer
    /// as the default for a [`RunSpec`](crate::workspace::RunSpec) that asked
    /// for none.
    ///
    /// **A workspace file cannot reach `base_url`.** [`Config`](crate::Config) refuses to
    /// carry one from a repository at all (see [`crate::config`]), so what
    /// arrives here is always the user's own — this method needs no rule of
    /// its own to keep that true.
    ///
    /// [`Workspace::open`](crate::Workspace::open) calls this for the private
    /// runtime it builds, so the one-repository host gets it without asking. A
    /// host building a shared [`Runtime`] states its own process facts and
    /// calls this itself if it wants a file to speak for them.
    ///
    /// **A provider instance leaves the file's `provider` and `base_url`
    /// unread.** [`with_provider_instance`](Self::with_provider_instance)
    /// answers the question those keys answer, and this method only ever
    /// fills emptiness — so they yield silently where an explicit builder
    /// call is refused by name. The model policy still arrives: which model
    /// is asked for is orthogonal to who answers.
    #[must_use]
    pub fn with_config(self, config: &crate::Config) -> Self {
        // See the doc above: with an instance supplied the provider question
        // is not empty, and emptiness is all a file may fill.
        let provider_unanswered = self.host_provider.is_none();
        Self {
            provider: self.provider.or_else(|| {
                config
                    .provider
                    .as_ref()
                    .filter(|_| provider_unanswered)
                    .map(|provider| provider.value)
            }),
            base_url: self.base_url.or_else(|| {
                config
                    .base_url
                    .as_ref()
                    .filter(|_| provider_unanswered)
                    .map(|url| url.value.clone())
            }),
            model: self.model.or_else(|| config.model_selector()),
            ..self
        }
    }

    /// How patiently every run minted on this runtime waits out a provider
    /// that is failing transiently.
    ///
    /// mentra retries a transient provider error on a doubling backoff and
    /// gives up when the budget runs out. Its default — five attempts, from
    /// 500ms, capped at 5s — spends about **12.5 seconds** before the run
    /// fails, which is tuned for a provider that hiccups and not for one that
    /// is rate-limiting you: a gateway's 429 routinely names a window longer
    /// than that, so the whole schedule elapses inside a limit that was never
    /// going to lift, and the caller reads a provider failure where the honest
    /// answer was *wait*.
    ///
    /// What a host knows that basis cannot is how long its own caller will
    /// hold still. An interactive editor session should fail fast, because
    /// somebody is watching a cursor blink; a chat bot whose turn already
    /// takes eight minutes can afford to spend one of them waiting, and would
    /// far rather do that than hand back an error the user has to re-ask. That
    /// is the judgement this knob is for, and it is why the number is the
    /// host's rather than a constant here.
    ///
    /// Runtime-scoped (ADR-0018) because it describes the *connection to the
    /// provider* — the same kind of fact as the credential and the base URL
    /// beside it, and not the kind of fact one prompt decides. Every run
    /// [`Workspace`](crate::Workspace) mints on this runtime carries it, and
    /// so does every subagent a run delegates to through
    /// [`spawn`](crate::tools::spawn): a child that reset to the default would
    /// be a delegated run quietly less patient than the run that delegated it,
    /// against the same rate limit.
    ///
    /// Unset is exactly mentra's default, so a host that never calls this gets
    /// the behavior it has always had. Takes mentra's own
    /// [`ProviderRetry`] rather than a basis type — see the re-export in
    /// [`crate::runtime`] for why there is only one spelling of this policy.
    ///
    /// **Not a deadline.** [`TurnOptions::with_deadline`](crate::TurnOptions::with_deadline)
    /// still bounds the whole turn, and a generous schedule inside a short
    /// deadline is bounded by the deadline. Set both, and set them knowingly.
    ///
    /// **Sets the waits, not the count.** mentra keeps *how many* attempts a
    /// run gets on its own `RunOptions::retry_budget`, so widening the
    /// schedule alone still gives up after five tries —
    /// [`with_provider_retry_budget`](Self::with_provider_retry_budget) is the
    /// other half, and the rate-limit case above needs both.
    #[must_use]
    pub fn with_provider_retry(self, provider_retry: ProviderRetry) -> Self {
        Self {
            provider_retry,
            ..self
        }
    }

    /// How many times a run minted here retries a transient provider error
    /// before giving up. Five by default.
    ///
    /// The count half of [`with_provider_retry`](Self::with_provider_retry),
    /// separate because mentra keeps the two apart: the schedule is a value
    /// with a type, the count is a bare number on each run's options. They are
    /// two knobs here rather than one because they are genuinely two questions
    /// — *how long between tries* and *how many tries* — and because the
    /// commonest adjustment is this one alone, which should not require
    /// constructing a [`ProviderRetry`] to express.
    ///
    /// Worth doing the arithmetic before choosing: with the default schedule
    /// the waits double from 500ms to a 5s ceiling, so raising the count from
    /// five to eight reaches about 27 seconds in total — still short of the
    /// minute a rate-limit window usually wants. Widening the schedule is what
    /// makes a larger count worth having.
    ///
    /// Runtime-scoped and inherited by delegated runs, exactly as the schedule
    /// is; see [`with_provider_retry`](Self::with_provider_retry) for why that
    /// scope is the right one.
    #[must_use]
    pub fn with_provider_retry_budget(self, budget: usize) -> Self {
        Self {
            provider_retry_budget: budget,
            ..self
        }
    }

    /// Which transport mentra streams the Responses wire format over.
    ///
    /// Passed straight through to mentra, which owns both transports and the
    /// choice between them. Unset, mentra picks, and what it picks is HTTP+SSE
    /// — the transport every basis run has ever used.
    ///
    /// Who asks for it: a host driving basis against an endpoint where the
    /// websocket transport is the point rather than an option — lower
    /// per-turn setup on a long conversation, or a gateway that only offers
    /// it. Nothing else in basis selects a transport, so before this method a
    /// host that wanted one had to build the mentra runtime itself and give up
    /// basis's own surface to get it.
    ///
    /// **Two ways this can disappoint, and neither is basis's to soften.**
    /// Selecting [`ResponsesTransport::WebSocket`] needs basis's
    /// `responses-websocket` feature, which forwards to mentra's, which
    /// forwards to mentra-provider's and compiles the websocket client back
    /// in. It is off by default — the default build links no websocket stack
    /// — and without it the choice is accepted here and **fails at request
    /// time**, loudly, which is mentra's stance rather than a silent fallback
    /// to HTTP+SSE: a host that asked for a transport should learn it did not
    /// get one, not discover later that its traffic went the other way. The
    /// second is the provider: not every one serves websockets — Anthropic and
    /// Gemini report that they do not — and such a provider refuses an
    /// explicit `WebSocket` at its first request, naming itself, for the same
    /// reason and with the same loudness.
    ///
    /// Read back through `Runtime::mentra_runtime().responses_transport()`,
    /// for a host that reports its own configuration.
    #[must_use]
    pub fn with_responses_transport(self, transport: ResponsesTransport) -> Self {
        Self {
            responses_transport: Some(transport),
            ..self
        }
    }

    /// Which request format the endpoint behind
    /// [`with_base_url`](Self::with_base_url) is spoken to in.
    ///
    /// [`Wire::ChatCompletions`] by default, and that default is the point: an
    /// operator who pastes a base URL has pasted Ollama, LM Studio, vLLM,
    /// llama.cpp, or a gateway in front of one of them, and every one of those
    /// serves `chat/completions` alone. OpenAI's own `v1/responses` is served
    /// by OpenAI — reached through the `openai` preset with no base URL at all
    /// — and by a handful of proxies that forward to it.
    ///
    /// So this exists for those proxies, and for nothing else. Without it the
    /// new default would not be a default but a removal: a Responses-speaking
    /// gateway was reachable by base URL before, and one word here keeps it
    /// reachable rather than sending its operator off to build a mentra
    /// runtime by hand. Choosing wrong is not subtle — the wrong wire is a 404
    /// on the first turn.
    ///
    /// **Read only when a base URL is set.** A provider preset carries the
    /// wire its vendor speaks, so calling this without
    /// [`with_base_url`](Self::with_base_url) says nothing: basis will not
    /// talk `chat/completions` to Anthropic because a builder asked.
    #[must_use]
    pub fn with_wire(self, wire: Wire) -> Self {
        Self { wire, ..self }
    }

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
    /// Deliberately not a per-run knob: a run describes an invocation, and
    /// where a machine keeps its history is not something an invocation
    /// decides. A one-shot caller that needs it opens the
    /// [`Workspace`](crate::Workspace) itself and hands
    /// [`WorkspaceBuilder::with_runtime_builder`](crate::WorkspaceBuilder::with_runtime_builder)
    /// a recipe, which is the documented migration path.
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
    /// no database file is opened, no tool output is spilled, no directory is
    /// created, and dropping the [`Runtime`] takes the history with it.
    ///
    /// One file is still written, and only if a conversation gets long enough
    /// to be summarized: mentra persists a compaction snapshot before it
    /// replaces a prefix of the transcript, and does that without consulting
    /// the store. basis files those under the operating system's temp
    /// directory, unique per runtime — never the user's data directory and
    /// never the workspace.
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

    /// The history directory this recipe names, if any — what
    /// [`Workspace::open`](crate::Workspace::open) derives the workspace
    /// memory root beside ([`crate::memory`]), read here because the private
    /// path resolves memory before the runtime exists.
    pub(crate) fn named_store_dir(&self) -> Option<&Path> {
        match &self.history {
            Some(History::Directory(dir)) => Some(dir),
            _ => None,
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

    /// Adds one fixed environment value to every process this runtime spawns.
    ///
    /// Mentra clears the ambient environment before running a model command, so
    /// a host must state execution context explicitly. A later call with the
    /// same name replaces the earlier value. Debug output names variables but
    /// redacts values.
    ///
    /// **Every process** is meant literally, and it did not used to be: a
    /// command through [`spawn`](crate::tools::spawn) received these pairs and
    /// a declared tool's program did not, so a host that had told the runtime
    /// where its service lived watched `.basis/tools.json` tools fail at the
    /// far end asking for a variable the runtime was holding. Both get them
    /// now. A declared tool's own `env` block still wins for a name they share,
    /// because that is the tool's own statement about itself
    /// ([`crate::tools::declared`]).
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
    /// The trait and everything an implementation of it names are re-exported
    /// as [`crate::runtime`]'s executor types, so a host writes one against
    /// `basis` alone and never adds mentra to its own manifest.
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
        executor: impl RuntimeExecutor + 'static,
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
        memory_roots: &[PathBuf],
    ) -> Result<Runtime, RunError> {
        let policy = with_command_patience(
            workspace_policy(workspace, shell, memory_roots),
            self.command_timeout,
        );

        self.build_with(store::runtime_identifier(workspace), policy)
    }

    fn build_with(self, identifier: String, policy: RuntimePolicy) -> Result<Runtime, RunError> {
        // First, before even the credential is looked up: a store directory
        // that still holds a basis ≤0.6 SQLite database is refused in basis's
        // words (ADR-0023's no-migration ruling), never quietly shadowed with
        // an empty file store that would read as every conversation being
        // lost. First because it is the most fundamental fact an upgrade can
        // trip over — a missing key is fixable in the environment, this needs
        // a decision about the data — and checked by basis rather than left
        // to mentra because mentra's own detection is swallowed by its
        // best-effort recovery path and worded for a mentra embedder (see
        // `store::refuse_legacy_store`). The default arm checks the directory
        // mentra will choose, which is where a 0.6 host that never named one
        // kept its history.
        match &self.history {
            Some(History::Directory(dir)) => store::refuse_legacy_store(dir)?,
            Some(History::Ephemeral) => {}
            None => store::refuse_legacy_store(&store::default_directory())?,
        }

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
            // every depth — the uniformity the ADR calls recursive.
            //
            // Told the target names, and only the names: the tool needs them to
            // teach the `!@` prefix and to refuse one nothing registered, while
            // *which executor* a name resolves to stays the runtime's business
            // (ADR-0021). With none registered and the default depth this is
            // `SpawnTool::new()` in every observable respect, including that
            // the model is never told the prefix exists.
            .with_tool(SpawnTool::with_targets_and_depth(
                target_names,
                self.delegation_depth,
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
        // thing to happen by upgrade.
        let builder = match &self.history {
            Some(History::Directory(dir)) => builder.with_store(store::store_in(dir)),
            Some(History::Ephemeral) => builder.with_store(store::volatile()),
            None => builder,
        };

        // The same answer applied to the other file mentra writes about a
        // conversation. Compaction persists a verbatim snapshot before it
        // summarizes, and mentra takes the directory for it on the *agent*
        // config — where a workspace would otherwise inherit a default keyed by
        // the process's cwd, which is the hazard `with_store_dir` was added for.
        // Derived here, once, so `with_store_dir` moves both files or neither.
        let transcripts = match &self.history {
            Some(History::Directory(dir)) => store::transcripts_in(dir),
            Some(History::Ephemeral) => store::volatile_transcripts(),
            None => store::default_transcripts(),
        };

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

/// The policy a private runtime bakes for one workspace:
/// `git_protected(workspace_bounded(path))`, the caller's shell posture as a
/// second belt beside the dispatcher's guard, and the memory roots.
///
/// Path roots are hygiene, not a boundary: per ADR-0004 that is the kernel's
/// job, and per ADR-0013 basis ships no instance of one. What the caller said
/// about commands is passed through as written.
///
/// The memory roots ([`crate::memory`]) sit outside the workspace — that is
/// what makes them memory rather than working files — so recall (`read`,
/// `grep`) and writing a memory (`write`, `edit`) need them stated here, on
/// both the read and the write lists. Stated whether or not a directory
/// exists yet: the first memory is written by exactly the run that finds none
/// to read. The shared policy deliberately gets none of this — it is fixed
/// before any workspace exists and a per-workspace root added there could not
/// be unsaid — so on a shared runtime the index renders and these writes are
/// refused, a recorded cost of sharing beside the others.
pub(crate) fn workspace_policy(
    workspace: &Path,
    shell: ShellAccess,
    memory_roots: &[PathBuf],
) -> RuntimePolicy {
    let policy = git_protected(RuntimePolicy::workspace_bounded(workspace), workspace)
        .allow_shell_commands(shell.is_granted())
        .allow_background_commands(shell.is_granted());

    memory_roots.iter().fold(policy, |policy, root| {
        policy
            .with_allowed_read_root(root.clone())
            .with_allowed_write_root(root.clone())
    })
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

#[cfg(test)]
mod tests;
