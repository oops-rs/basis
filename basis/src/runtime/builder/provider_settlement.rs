//! Where this runtime's provider comes from, and what mentra's builder chain
//! does with the answer.
//!
//! Settled once, early in `build_with`, because everything downstream — which
//! mentra door is called, which id models resolve under — follows from it.
//! Split out of `builder.rs` only for that function's size (whole-wave
//! review, G6): the *knobs* that feed this ([`RuntimeBuilder::with_provider`],
//! [`RuntimeBuilder::with_provider_instance`],
//! [`RuntimeBuilder::with_registered_provider`],
//! [`RuntimeBuilder::with_base_url`], [`RuntimeBuilder::with_api_key`]) stay
//! there, because they are its public surface; what moved is the settling
//! ([`settle`]) and the assembly ([`assemble`]), which are `build_with`'s own
//! machinery and touch nothing else in the builder.
//!
//! [`RuntimeBuilder::with_provider`]: super::RuntimeBuilder::with_provider
//! [`RuntimeBuilder::with_provider_instance`]: super::RuntimeBuilder::with_provider_instance
//! [`RuntimeBuilder::with_registered_provider`]: super::RuntimeBuilder::with_registered_provider
//! [`RuntimeBuilder::with_base_url`]: super::RuntimeBuilder::with_base_url
//! [`RuntimeBuilder::with_api_key`]: super::RuntimeBuilder::with_api_key

use mentra::{
    BuiltinProvider, ProviderId,
    provider_core::{AuthScheme, responses, responses::ResponsesProvider},
};

use crate::{error::RunError, provider, runtime::credential::Credential};

use super::Wire;

/// A provider instance the host built, held until [`assemble`] hands it to the
/// matching mentra registration seam.
///
/// Two parts because they are needed at two times. The `id` is read out of
/// the instance's descriptor at either host-provider call: it is what
/// `Runtime::provider` reports and what models resolve under, and holding it
/// here is what lets `RuntimeBuilder`'s `Debug` and the ambiguity refusal name
/// the instance without asking it again. The installer preserves which of
/// mentra's two provider abstractions the host supplied: the runtime-level
/// trait goes through `with_provider_instance`, while the provider-core trait
/// goes through `with_registered_provider`. It is `FnOnce` because mentra
/// takes either instance by value, and boxing that move is what keeps
/// `RuntimeBuilder` free of a generic parameter a half-configured one would
/// otherwise have to carry.
pub(super) struct HostProvider {
    pub(super) id: ProviderId,
    pub(super) install:
        Box<dyn FnOnce(mentra::RuntimeBuilder) -> mentra::RuntimeBuilder + Send + Sync>,
}

/// Where the provider a runtime runs on came from: an instance the host
/// constructed, or basis's resolution over the enum, the base URL and the
/// environment. [`settle`] decides which; [`assemble`] is what each answer
/// does to mentra's builder chain.
pub(super) enum ProviderSource {
    Host(HostProvider),
    Resolved(provider::ProviderChoice),
}

/// Settles which provider this runtime runs on, refusing an ambiguous
/// statement before anything is resolved or assembled.
///
/// A host-supplied instance, at either provider abstraction level, is an answer
/// rather than a preference: with one present, resolution — and with it the
/// environment — is skipped entirely, and `provider`/`base_url`/`api_key` set
/// beside it are each refused by name with
/// [`provider::ProviderError::AmbiguousProviderSource`], whichever was set. A
/// silent priority here would be a `with_provider` that silently stopped
/// meaning anything, so this checks all three before choosing either path
/// rather than letting one win quietly.
pub(super) fn settle(
    host_provider: Option<HostProvider>,
    provider: Option<BuiltinProvider>,
    base_url: Option<String>,
    api_key: Option<String>,
) -> Result<ProviderSource, RunError> {
    match host_provider {
        Some(host) => {
            for (also_set, knob) in [
                (provider.is_some(), "with_provider"),
                (base_url.is_some(), "with_base_url"),
                (api_key.is_some(), "with_api_key"),
            ] {
                if also_set {
                    return Err(provider::ProviderError::AmbiguousProviderSource { knob }.into());
                }
            }
            Ok(ProviderSource::Host(host))
        }
        None => Ok(ProviderSource::Resolved(provider::resolve_with(
            provider,
            base_url.as_deref(),
            api_key.as_deref(),
        )?)),
    }
}

/// Builds the mentra runtime on whichever provider `source` names: the host's
/// matching door for an instance, registered under the id its descriptor
/// reports, or basis's resolved choice, dispatched on `wire` the way
/// [`RuntimeBuilder::with_wire`](super::RuntimeBuilder::with_wire) documents.
///
/// `build`, not `build_async`: no MCP server is ever registered at the
/// runtime level, so there is nothing for the async constructor to connect.
/// Workspace-owned connections arrive post-build (ADR-0018).
pub(super) fn assemble(
    source: ProviderSource,
    builder: mentra::RuntimeBuilder,
    wire: Wire,
) -> Result<(mentra::Runtime, ProviderId), RunError> {
    match source {
        ProviderSource::Host(host) => Ok(((host.install)(builder).build()?, host.id)),
        ProviderSource::Resolved(choice) => {
            let assembled = match (&choice.base_url, wire) {
                // mentra's own door for a compatible endpoint, keyed or not;
                // filed under the resolved id so the model lookup finds it.
                (Some(base_url), Wire::ChatCompletions) => builder.with_openai_compatible(
                    ProviderId::from(choice.provider),
                    base_url,
                    choice.api_key.clone(),
                ),
                (Some(base_url), Wire::Responses) => {
                    builder.with_registered_provider(responses_provider(
                        choice.provider,
                        base_url,
                        Credential::new(choice.api_key.as_deref()),
                    ))
                }
                // A preset takes a `String`; resolution hands one back for
                // every keyed preset, and the two local ones ignore what they
                // are given.
                (None, _) => builder
                    .with_provider(choice.provider, choice.api_key.clone().unwrap_or_default()),
            };
            Ok((assembled.build()?, ProviderId::from(choice.provider)))
        }
    }
}

/// Builds a provider aimed at a base URL that serves OpenAI's own Responses
/// wire — [`RuntimeBuilder::with_wire`](super::RuntimeBuilder::with_wire)'s
/// other answer.
///
/// mentra's OpenAI preset is the right shape — the Responses wire format and
/// bearer auth — so basis takes that definition, swaps the base URL, and disables
/// automatic Hybrid HTTP state chaining. Building on the preset avoids
/// describing a provider from scratch and drifting from whatever mentra learns
/// next.
///
/// The preset's own id is `openai`, which is right only when nothing named
/// another: it is filed under the resolved id so that `--provider …
/// --base-url …` finds its model rather than failing at the first turn under
/// a name nobody registered.
pub(super) fn responses_provider(
    provider: BuiltinProvider,
    base_url: &str,
    credential: Credential,
) -> ResponsesProvider<Credential> {
    let mut definition = responses::openai_definition();
    definition.base_url = Some(base_url.to_string());
    definition.descriptor.id = ProviderId::from(provider);
    definition.descriptor.display_name = Some(format!("OpenAI-compatible ({base_url})"));
    if !credential.is_some() {
        definition.auth_scheme = AuthScheme::None;
    }

    // A compatible endpoint promises the Responses wire shape, not every
    // optional OpenAI extension. basis already replays the complete local
    // transcript, so do not probe `previous_response_id` support with a
    // request that may fail; native provider presets retain Hybrid chaining.
    ResponsesProvider::new(definition, credential).without_hybrid_http_previous_response_id()
}
