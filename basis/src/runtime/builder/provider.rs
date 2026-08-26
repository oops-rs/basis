//! How this runtime reaches a model — the provider half of
//! [`RuntimeBuilder`](super::RuntimeBuilder).
//!
//! Every knob that answers *which service, at which endpoint, with which
//! credential, spoken to how*: the closed enum and the host-constructed
//! instance that replaces it, the base URL and the key, the model policy,
//! the retry schedule a failing provider is waited out on, and the two wire
//! questions (request format, and the transport the Responses format
//! streams over).
//!
//! Sits beside [`provider_settlement`](super::provider_settlement), which is
//! where these fields are *read*: the ambiguity rule that refuses an
//! instance next to a resolution knob, and the assembly that turns whichever
//! source won into a registered provider. Setting and settling are one
//! responsibility split at the build boundary, and this is its near half.

use mentra::{BuiltinProvider, ModelSelector, Provider};

use super::{HostProvider, ProviderRetry, ResponsesTransport, RuntimeBuilder, Wire};

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
    /// [`Runtime::provider`](crate::Runtime::provider) reports its id.
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
    /// host building a shared [`Runtime`](crate::Runtime) states its own process facts and
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
}
