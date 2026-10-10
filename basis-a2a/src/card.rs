//! The Agent Card: what this server says it is, at
//! `/.well-known/agent-card.json`.
//!
//! The card advertises exactly what is served (ADR-0028 §5) — a caller
//! deciding whether to stream or register a webhook reads `capabilities`
//! and nothing else, so a capability claimed and not served is a bug a
//! caller only finds by failing.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use a2a::{
    A2AError, AgentCapabilities, AgentCard, AgentExtension, AgentInterface, AgentSkill,
    HttpAuthSecurityScheme, SecurityScheme, TRANSPORT_PROTOCOL_HTTP_JSON,
    TRANSPORT_PROTOCOL_JSONRPC,
};

use crate::{
    config::{AgentIdentity, ExtendedCard, Principal},
    rest,
    service::{A2aService, APPROVAL_EXTENSION, HOPS_EXTENSION, INPUT_MODES},
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
    /// Consequential calls are put to the caller (the approval extension).
    pub(crate) ask_caller: bool,
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
            extensions: extensions(features),
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

/// The extensions this server speaks, each declared only when on.
fn extensions(features: Features) -> Option<Vec<AgentExtension>> {
    // Always served: every message is read for it, and a server that reads it
    // is one a caller can tell how deep its chain already is.
    let mut extensions = vec![AgentExtension {
        uri: HOPS_EXTENSION.to_string(),
        description: Some(
            "Message metadata may carry {hop, deadline}: a message past this agent's hop cap \
             is refused, and a task's deadline is clamped to its caller's."
                .to_string(),
        ),
        required: Some(false),
        params: None,
    }];
    if features.ask_caller {
        extensions.push(AgentExtension {
            uri: APPROVAL_EXTENSION.to_string(),
            description: Some(
                "A consequential tool call is put to the caller as INPUT_REQUIRED; answer with \
                 `approve <call id>` or `deny <call id>`."
                    .to_string(),
            ),
            required: Some(false),
            params: None,
        });
    }
    (!extensions.is_empty()).then_some(extensions)
}

/// `card` as it goes on the wire: the spec's v1.0 JSON, which is
/// `a2a-lf`'s serialization except for security requirements.
///
/// The v1.0 proto has `SecurityRequirement { map<string, StringList>
/// schemes }`, so a requirement is `{"schemes": {"bearer": {"list": []}}}`.
/// `a2a-lf` 0.4.1 models it as a bare map and writes `{"bearer": []}`,
/// which a strict client either drops — and then believes no credential is
/// needed — or refuses to parse. Every card this server answers with goes
/// through here, the card's own requirements and every skill's alike.
pub(crate) fn to_wire(card: &AgentCard) -> Value {
    let mut wire = serde_json::to_value(card).unwrap_or_default();
    wrap_requirements(&mut wire);
    if let Some(skills) = wire.get_mut("skills").and_then(Value::as_array_mut) {
        skills.iter_mut().for_each(wrap_requirements);
    }
    wire
}

fn wrap_requirements(holder: &mut Value) {
    let Some(requirements) = holder
        .get_mut("securityRequirements")
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for requirement in requirements {
        let Value::Object(schemes) = requirement else {
            continue;
        };
        if schemes.contains_key("schemes") {
            continue;
        }
        let wrapped: Map<String, Value> = std::mem::take(schemes)
            .into_iter()
            .map(|(scheme, scopes)| (scheme, json!({"list": scopes})))
            .collect();
        *requirement = json!({"schemes": wrapped});
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
    fn security_requirements_go_on_the_wire_in_the_v1_shape() {
        let skill = AgentSkill {
            id: "audit".into(),
            name: "Audit".into(),
            description: "Audits.".into(),
            tags: vec![],
            examples: None,
            input_modes: None,
            output_modes: None,
            security_requirements: Some(vec![HashMap::from([(
                BEARER.to_string(),
                vec!["audit".to_string()],
            )])]),
        };
        let identity = AgentIdentity::new("helper", "Answers questions.").with_skill(skill);
        let features = Features {
            authenticated: true,
            ..Features::default()
        };
        let wire = to_wire(&build(&identity, "http://127.0.0.1:1/", features));
        assert_eq!(
            wire["securityRequirements"],
            json!([{"schemes": {"bearer": {"list": []}}}])
        );
        assert_eq!(
            wire["skills"][0]["securityRequirements"],
            json!([{"schemes": {"bearer": {"list": ["audit"]}}}])
        );
        assert_eq!(
            wire["securitySchemes"]["bearer"]["httpAuthSecurityScheme"]["scheme"],
            "Bearer"
        );
        let back: AgentCard = serde_json::from_value(wire).expect("a2a-lf reads it back");
        assert!(back.security_requirements.unwrap()[0].contains_key(BEARER));
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
