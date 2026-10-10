//! The Agent Card: what this server says it is, at
//! `/.well-known/agent-card.json`.
//!
//! The card advertises exactly what is served (ADR-0028 §5) — a caller
//! deciding whether to stream or register a webhook reads `capabilities`
//! and nothing else, so a capability claimed and not served is a bug a
//! caller only finds by failing.

use std::collections::HashMap;

use a2a::{
    AgentCapabilities, AgentCard, AgentInterface, AgentSkill, HttpAuthSecurityScheme,
    SecurityScheme, TRANSPORT_PROTOCOL_HTTP_JSON, TRANSPORT_PROTOCOL_JSONRPC,
};

use crate::{config::AgentIdentity, rest};

/// The well-known path the spec puts the card at.
pub const AGENT_CARD_PATH: &str = "/.well-known/agent-card.json";

/// The only media type served tasks take and give.
pub(crate) const TEXT: &str = "text/plain";

/// The scheme name the card's security requirement refers to.
const BEARER: &str = "bearer";

pub(crate) fn build(identity: &AgentIdentity, endpoint: &str, authenticated: bool) -> AgentCard {
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
            push_notifications: Some(false),
            extensions: None,
            extended_agent_card: Some(false),
        },
        default_input_modes: vec![TEXT.to_string()],
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_card_advertises_json_rpc_then_http_json_and_only_what_is_served() {
        let identity = AgentIdentity::new("helper", "Answers questions.");
        let card = build(&identity, "http://127.0.0.1:1/", false);
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
        let card = build(&identity, "http://127.0.0.1:1/", true);
        assert!(card.security_schemes.unwrap().contains_key(BEARER));
        assert!(card.security_requirements.unwrap()[0].contains_key(BEARER));
    }
}
