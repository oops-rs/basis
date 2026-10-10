//! What a served agent's cards say it is: the public card's identity, and
//! the extended card an operator may add for authenticated callers.

use a2a::AgentSkill;

/// What the Agent Card says this agent is.
#[derive(Debug, Clone)]
pub struct AgentIdentity {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) version: String,
    pub(crate) skills: Vec<AgentSkill>,
}

impl AgentIdentity {
    /// A card names an agent and says what it does; both are required by the
    /// spec, and a caller choosing among agents reads nothing else first.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            skills: Vec::new(),
        }
    }

    /// The agent's own version, when it has one other than basis's.
    #[must_use]
    pub fn with_version(self, version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            ..self
        }
    }

    /// Adds a skill to the card. With none, the card advertises one general
    /// skill described by the agent's own description, because a card must
    /// list what the agent can be asked to do.
    #[must_use]
    pub fn with_skill(self, skill: AgentSkill) -> Self {
        let mut skills = self.skills;
        skills.push(skill);
        Self { skills, ..self }
    }
}

/// What an authenticated caller may additionally be told (ADR-0030 §5):
/// more skills, and a longer description. Served by `GetExtendedAgentCard`
/// to a caller that presented a token, and only declared on the public card
/// when configured.
///
/// Never a place for anything sensitive: an extended card is read by every
/// principal the server accepts, not by one of them.
#[derive(Debug, Clone, Default)]
pub struct ExtendedCard {
    pub(crate) description: Option<String>,
    pub(crate) skills: Vec<AgentSkill>,
}

impl ExtendedCard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the public card's description.
    #[must_use]
    pub fn with_description(self, description: impl Into<String>) -> Self {
        Self {
            description: Some(description.into()),
            ..self
        }
    }

    /// Adds a skill beyond the public card's.
    #[must_use]
    pub fn with_skill(self, skill: AgentSkill) -> Self {
        let mut skills = self.skills;
        skills.push(skill);
        Self { skills, ..self }
    }
}
