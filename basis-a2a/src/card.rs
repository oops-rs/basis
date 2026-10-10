//! The Agent Card: what this server says it is, at
//! `/.well-known/agent-card.json`.
//!
//! The card advertises exactly what is served (ADR-0028 §5) — a caller
//! deciding whether to stream or register a webhook reads `capabilities`
//! and nothing else, so a capability claimed and not served is a bug a
//! caller only finds by failing.

use std::collections::HashMap;

use a2a::{
    A2AError, AgentCapabilities, AgentCard, AgentInterface, AgentSkill, HttpAuthSecurityScheme,
    SecurityScheme, TRANSPORT_PROTOCOL_HTTP_JSON, TRANSPORT_PROTOCOL_JSONRPC,
};

use crate::{
    config::{AgentIdentity, ExtendedCard, Principal},
    rest,
    service::{A2aService, INPUT_MODES},
};

/// The well-known path the spec puts the card at.
pub const AGENT_CARD_PATH: &str = "/.well-known/agent-card.json";

/// The only media type served tasks give: an answer is text.
pub(crate) const TEXT: &str = "text/plain";

/// The scheme name the card's security requirement refers to.
const BEARER: &str = "bearer";

/// Which optional features a service serves — each is claimed on the card
/// when, and only when, it is on (ADR-0030 §10).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Features {
    /// At least one token is configured.
    pub(crate) authenticated: bool,
    pub(crate) extended_card: bool,
    pub(crate) push_notifications: bool,
}

/// The public card, at the well-known path.
pub(crate) fn build(identity: &AgentIdentity, endpoint: &str, features: Features) -> AgentCard {
    let authenticated = features.authenticated;
    let skills = if identity.skills.is_empty() {
        vec![AgentSkill {
            id: "general".to_string(),
            name: identity.name.clone(),
            description: identity.description.clone(),
            tags: vec!["basis".to_string()],
            examples: None,
            input_modes: None,
            output_modes: None,
            security_requirements: None,
        }]
    } else {
        identity.skills.clone()
    };
    let (security_schemes, security_requirements) = if authenticated {
        let scheme = SecurityScheme::HttpAuth(HttpAuthSecurityScheme {
            scheme: "Bearer".to_string(),
            description: Some("The token the operator configured for this server.".to_string()),
            bearer_format: None,
        });
        (
            Some(HashMap::from([(BEARER.to_string(), scheme)])),
            Some(vec![HashMap::from([(BEARER.to_string(), Vec::new())])]),
        )
    } else {
        (None, None)
    };
    AgentCard {
        name: identity.name.clone(),
        description: identity.description.clone(),
        version: identity.version.clone(),
        // JSON-RPC first: a client takes the first interface it speaks
        // (ADR-0030 §1), and JSON-RPC is the binding basis has served longest.
        supported_interfaces: vec![
            AgentInterface::new(endpoint, TRANSPORT_PROTOCOL_JSONRPC),
            AgentInterface::new(rest::interface_url(endpoint), TRANSPORT_PROTOCOL_HTTP_JSON),
        ],
        capabilities: AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(features.push_notifications),
            extensions: None,
            extended_agent_card: Some(features.extended_card),
        },
        default_input_modes: INPUT_MODES.iter().map(|mode| mode.to_string()).collect(),
        default_output_modes: vec![TEXT.to_string()],
        skills,
        provider: None,
        documentation_url: None,
        icon_url: None,
        security_schemes,
        security_requirements,
        signatures: None,
    }
}

/// Both cards one server serves, built once from its service and endpoint.
#[derive(Debug, Clone)]
pub(crate) struct Cards {
    pub(crate) public: AgentCard,
    pub(crate) extended: Option<AgentCard>,
}

impl Cards {
    pub(crate) fn of(service: &A2aService, endpoint: &str) -> Self {
        let public = build(service.identity(), endpoint, service.features());
        let extended = service
            .extended_card()
            .map(|extended| extend(&public, extended));
        Self { public, extended }
    }

    /// `GetExtendedAgentCard`, for `caller`. A card that declares none answers
    /// `UnsupportedOperationError`, as the spec requires (§13.3); one that
    /// does serves it to an authenticated caller only — and a server with an
    /// extended card refuses every unauthenticated request before it gets
    /// here, so the second refusal is for a host calling the service with an
    /// anonymous principal of its own.
    pub(crate) fn extended_for(&self, caller: &Principal) -> Result<&AgentCard, A2AError> {
        let extended = self
            .extended
            .as_ref()
            .ok_or_else(|| A2AError::unsupported_operation("this agent has no extended card"))?;
        if caller.is_authenticated() {
            Ok(extended)
        } else {
            Err(A2AError::unsupported_operation(
                "the extended card is served to authenticated callers only",
            ))
        }
    }
}

/// The extended card: the public one, with the operator's longer
/// description and the skills it adds.
pub(crate) fn extend(public: &AgentCard, extended: &ExtendedCard) -> AgentCard {
    let mut skills = public.skills.clone();
    skills.extend(extended.skills.iter().cloned());
    AgentCard {
        description: extended
            .description
            .clone()
            .unwrap_or_else(|| public.description.clone()),
        skills,
        ..public.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_card_advertises_json_rpc_then_http_json_and_only_what_is_served() {
        let identity = AgentIdentity::new("helper", "Answers questions.");
        let card = build(&identity, "http://127.0.0.1:1/", Features::default());
        assert_eq!(card.supported_interfaces[0].url, "http://127.0.0.1:1/");
        assert_eq!(
            card.supported_interfaces[0].protocol_binding,
            TRANSPORT_PROTOCOL_JSONRPC
        );
        assert_eq!(card.supported_interfaces[0].protocol_version, "1.0");
        assert_eq!(card.supported_interfaces[1].url, "http://127.0.0.1:1/json");
        assert_eq!(
            card.supported_interfaces[1].protocol_binding,
            TRANSPORT_PROTOCOL_HTTP_JSON
        );
        assert_eq!(card.supported_interfaces[1].protocol_version, "1.0");
        assert_eq!(card.capabilities.streaming, Some(true));
        assert_eq!(card.capabilities.push_notifications, Some(false));
        assert_eq!(card.capabilities.extended_agent_card, Some(false));
        assert!(card.security_schemes.is_none());
        assert_eq!(card.skills.len(), 1, "a card always lists a skill");
    }

    #[test]
    fn a_token_puts_the_bearer_scheme_on_the_card() {
        let identity = AgentIdentity::new("helper", "Answers questions.");
        let features = Features {
            authenticated: true,
            ..Features::default()
        };
        let card = build(&identity, "http://127.0.0.1:1/", features);
        assert!(card.security_schemes.unwrap().contains_key(BEARER));
        assert!(card.security_requirements.unwrap()[0].contains_key(BEARER));
    }
}
