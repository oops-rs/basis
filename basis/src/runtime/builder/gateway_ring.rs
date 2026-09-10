//! Several gateways to one model, stated as one builder answer.
//!
//! [`RuntimeBuilder::with_gateway_ring`] is [`with_base_url`] said more than
//! once, in preference order: every member is built exactly the way a lone
//! base URL would be — the same URL normalization, the same wire, the same
//! provider id — and the finished members are handed to mentra's
//! [`GatewayRing`], which basis then registers through its own
//! [`with_registered_provider`] seam. The rotation itself lives in mentra
//! (ADR-0027): basis states which gateways exist and how patient to be, and
//! never touches the provider after `build`.
//!
//! [`with_base_url`]: RuntimeBuilder::with_base_url
//! [`with_registered_provider`]: RuntimeBuilder::with_registered_provider

use std::sync::Arc;

use mentra::{
    BuiltinProvider, ProviderId,
    provider_core::{
        AuthScheme, Provider, chat_completions,
        gateway_ring::{GatewayRing, GatewayRingEvent, GatewayRingPolicy},
    },
};

use crate::{error::RunError, provider, runtime::credential::Credential};

use super::{RuntimeBuilder, Wire, provider_settlement::responses_provider};

/// One gateway in a ring: where it is, and the key it takes.
///
/// A member is a URL *and* a credential because two gateways in front of the
/// same model rarely accept the same key — the one measured case answered
/// `401` to its neighbour's. Passing the runtime's single
/// [`with_api_key`](RuntimeBuilder::with_api_key) to every member would be
/// a ring that only ever works on one of them.
///
/// Deliberately not `Debug`-transparent: the key is redacted.
#[derive(Clone)]
pub struct GatewayMember {
    base_url: String,
    api_key: Option<String>,
}

impl GatewayMember {
    /// A gateway at `base_url`, spoken to without a credential until
    /// [`with_api_key`](Self::with_api_key) says otherwise. The URL is taken
    /// as published, `/v1` and all, the way [`RuntimeBuilder::with_base_url`]
    /// takes one.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: None,
        }
    }

    /// The key this member's requests carry as a bearer token.
    #[must_use]
    pub fn with_api_key(self, api_key: impl Into<String>) -> Self {
        Self {
            api_key: Some(api_key.into()),
            ..self
        }
    }

    /// Where this member is, as given.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl std::fmt::Debug for GatewayMember {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayMember")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

type Observer = Arc<dyn Fn(GatewayRingEvent) + Send + Sync>;

/// What the builder holds between [`RuntimeBuilder::with_gateway_ring`] and
/// [`assemble`]: the members, and the two optional knobs beside them.
///
/// `members` is `None` until `with_gateway_ring` is called, so a policy or an
/// observer stated on their own do not make a ring — they wait for one. An
/// empty `Some` is a ring that was stated with nobody in it, and is refused
/// at build.
#[derive(Clone, Default)]
pub(in crate::runtime) struct GatewayRingSpec {
    pub(super) members: Option<Vec<GatewayMember>>,
    pub(super) policy: GatewayRingPolicy,
    pub(super) observer: Option<Observer>,
}

impl GatewayRingSpec {
    /// Whether [`RuntimeBuilder::with_gateway_ring`] was ever called.
    pub(in crate::runtime) fn is_stated(&self) -> bool {
        self.members.is_some()
    }
}

impl std::fmt::Debug for GatewayRingSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayRingSpec")
            .field("members", &self.members)
            .field("policy", &self.policy)
            .field("observer", &self.observer.as_ref().map(|_| "<observer>"))
            .finish()
    }
}

impl RuntimeBuilder {
    /// Reaches the model through several gateways, preferring the first and
    /// rotating to the next when the current one keeps failing.
    ///
    /// This is [`with_base_url`](Self::with_base_url) said more than once.
    /// Each member is built exactly as a lone base URL would be — normalized
    /// the same way, spoken to in the wire [`with_wire`](Self::with_wire)
    /// names, filed under the id [`with_provider`](Self::with_provider)
    /// names — and the ring of them is registered as this runtime's one
    /// provider. Nothing above the provider knows there is more than one
    /// gateway: the runtime's retry schedule keeps pacing the attempts, and
    /// once the current member's streak reaches the policy's threshold the
    /// ring finishes that attempt on the next member.
    ///
    /// **Members must front the same upstream serving the same model.** A
    /// rotation replays the conversation to the new member, and a transcript
    /// carrying one vendor's reasoning items is refused by another vendor's
    /// endpoint. Two relays in front of one model are a ring; two vendors are
    /// not, and basis cannot tell them apart for you.
    ///
    /// How patient the ring is, and whether it drifts back to the preferred
    /// member, is [`with_gateway_ring_policy`](Self::with_gateway_ring_policy).
    /// What it did is [`with_gateway_ring_observer`](Self::with_gateway_ring_observer).
    ///
    /// Beside [`with_base_url`](Self::with_base_url),
    /// [`with_api_key`](Self::with_api_key), or either host-provider seam this
    /// is refused at [`build`](Self::build): a ring answers the endpoint
    /// question in full, so a second answer next to it has nowhere to point.
    /// A ring with no members is refused there too. A later call replaces the
    /// earlier members.
    #[must_use]
    pub fn with_gateway_ring(self, members: impl IntoIterator<Item = GatewayMember>) -> Self {
        let spec = GatewayRingSpec {
            members: Some(members.into_iter().collect()),
            ..self.gateway_ring.clone().unwrap_or_default()
        };
        Self {
            gateway_ring: Some(spec),
            ..self
        }
    }

    /// When the ring leaves a member and whether it comes back — see
    /// [`GatewayRingPolicy`]. The default rotates after five consecutive
    /// failures and returns to the preferred member once it has rested a
    /// minute; [`GatewayRingPolicy::sticky`] never returns on its own.
    ///
    /// Read only beside [`with_gateway_ring`](Self::with_gateway_ring); alone
    /// it says nothing.
    #[must_use]
    pub fn with_gateway_ring_policy(self, policy: GatewayRingPolicy) -> Self {
        let spec = GatewayRingSpec {
            policy,
            ..self.gateway_ring.clone().unwrap_or_default()
        };
        Self {
            gateway_ring: Some(spec),
            ..self
        }
    }

    /// Hears every counted failure, rotation, return, and recovery the ring
    /// goes through, as [`GatewayRingEvent`]s. basis has no logger of its own
    /// and a rotation is otherwise invisible — a turn that was answered by
    /// the fallback gateway reads exactly like one answered by the preferred
    /// one — so a host that will ever need to know which gateway answered
    /// wires this to its logs.
    ///
    /// Read only beside [`with_gateway_ring`](Self::with_gateway_ring).
    #[must_use]
    pub fn with_gateway_ring_observer(
        self,
        observer: impl Fn(GatewayRingEvent) + Send + Sync + 'static,
    ) -> Self {
        let spec = GatewayRingSpec {
            observer: Some(Arc::new(observer)),
            ..self.gateway_ring.clone().unwrap_or_default()
        };
        Self {
            gateway_ring: Some(spec),
            ..self
        }
    }
}

/// Builds the ring the way [`assemble`](super::provider_settlement::assemble)
/// builds a lone base URL: one member per gateway in the wire named, filed
/// under `provider`'s id.
pub(super) fn assemble(
    spec: GatewayRingSpec,
    provider: BuiltinProvider,
    wire: Wire,
) -> Result<GatewayRing, RunError> {
    let members = spec
        .members
        .unwrap_or_default()
        .iter()
        .map(|member| build_member(member, provider, wire))
        .collect::<Result<Vec<_>, _>>()?;
    let ring = GatewayRing::new(members, spec.policy)
        .map_err(|_| provider::ProviderError::EmptyGatewayRing)?;
    Ok(match spec.observer {
        Some(observer) => ring.with_observer(move |event| observer(event)),
        None => ring,
    })
}

fn build_member(
    member: &GatewayMember,
    provider: BuiltinProvider,
    wire: Wire,
) -> Result<Arc<dyn Provider>, RunError> {
    let base_url = provider::normalize_base_url(&member.base_url)?;
    let credential = Credential::new(member.api_key.as_deref());
    Ok(match wire {
        Wire::Responses => Arc::new(responses_provider(provider, &base_url, credential)),
        Wire::ChatCompletions => {
            Arc::new(chat_completions_provider(provider, &base_url, credential))
        }
    })
}

/// The `chat/completions` counterpart of
/// [`responses_provider`](super::provider_settlement::responses_provider):
/// mentra's definition for that wire, aimed at `base_url`, filed under
/// `provider`'s id, with no `Authorization` header at all when there is no
/// key rather than an empty bearer.
///
/// A lone base URL on this wire goes through mentra's own
/// `with_openai_compatible` door, which builds the same definition behind
/// mentra's runtime-level `Provider`; a ring member has to be a provider-core
/// instance, so the definition is built here at that level instead.
fn chat_completions_provider(
    provider: BuiltinProvider,
    base_url: &str,
    credential: Credential,
) -> chat_completions::ChatCompletionsProvider<Credential> {
    let mut definition = chat_completions::definition(ProviderId::from(provider), base_url);
    definition.descriptor.display_name = Some(format!("OpenAI-compatible ({base_url})"));
    if !credential.is_some() {
        definition.auth_scheme = AuthScheme::None;
    }
    chat_completions::ChatCompletionsProvider::new(definition, credential)
}
