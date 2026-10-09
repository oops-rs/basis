//! What one served agent is: the workspace it runs in, what its card says
//! about it, how its runs are bounded, and who may call it.
//!
//! Everything here is decided once, before the first request, and none of it
//! changes while serving. That is what lets [`InboundApproval`] have no
//! `Prompt` variant at all (ADR-0028 §4): a served agent's callers are other
//! agents, so a question about a tool call has nobody to put it to, and a
//! policy that would ask is refused by the type rather than discovered on
//! the first consequential call.

use std::{fmt, path::PathBuf, time::Duration};

use a2a::AgentSkill;
use basis::Effort;
use basis_tasks::Approve;

/// How a served task's consequential calls are answered. There is no third
/// option: see the module docs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InboundApproval {
    /// Every consequential call is refused. The default: an agent reachable
    /// over a network reads and answers unless its operator says otherwise.
    #[default]
    Never,
    /// Every consequential call is allowed.
    Always,
}

impl From<InboundApproval> for Approve {
    fn from(approval: InboundApproval) -> Self {
        match approval {
            InboundApproval::Never => Approve::Never,
            InboundApproval::Always => Approve::Always,
        }
    }
}

/// The bearer token every call but the public card must carry.
///
/// Held privately and never printed: `Debug` redacts it, and nothing in this
/// crate formats it into an error.
#[derive(Clone)]
pub struct BearerToken(String);

impl BearerToken {
    /// `None` for an empty or all-whitespace token, which would authenticate
    /// a request carrying `Authorization: Bearer ` and nothing else.
    pub fn new(token: impl Into<String>) -> Option<Self> {
        let token = token.into();
        (!token.trim().is_empty()).then_some(Self(token))
    }

    /// Whether `presented` is this token, compared in time that depends only
    /// on the lengths — the bytes of a wrong guess do not change how long it
    /// takes to refuse.
    pub(crate) fn matches(&self, presented: &str) -> bool {
        let expected = self.0.as_bytes();
        let presented = presented.as_bytes();
        if expected.len() != presented.len() {
            return false;
        }
        expected
            .iter()
            .zip(presented)
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BearerToken(<redacted>)")
    }
}

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

/// The per-run settings every task this server spawns is given — the same
/// knobs `basis spawn` takes, fixed by the operator rather than chosen by a
/// caller.
#[derive(Debug, Clone, Default)]
pub struct RunDefaults {
    pub(crate) provider: Option<String>,
    pub(crate) base_url: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<Effort>,
    pub(crate) deadline: Option<Duration>,
    pub(crate) tool_budget: Option<usize>,
    pub(crate) token_budget: Option<u64>,
}

impl RunDefaults {
    #[must_use]
    pub fn with_provider(self, provider: impl Into<String>) -> Self {
        Self {
            provider: Some(provider.into()),
            ..self
        }
    }

    #[must_use]
    pub fn with_base_url(self, base_url: impl Into<String>) -> Self {
        Self {
            base_url: Some(base_url.into()),
            ..self
        }
    }

    #[must_use]
    pub fn with_model(self, model: impl Into<String>) -> Self {
        Self {
            model: Some(model.into()),
            ..self
        }
    }

    #[must_use]
    pub fn with_effort(self, effort: Effort) -> Self {
        Self {
            effort: Some(effort),
            ..self
        }
    }

    /// Narrows every task's deadline; without one, `basis-tasks` gives each
    /// its own finite default.
    #[must_use]
    pub fn with_deadline(self, deadline: Duration) -> Self {
        Self {
            deadline: Some(deadline),
            ..self
        }
    }

    #[must_use]
    pub fn with_tool_budget(self, tool_budget: usize) -> Self {
        Self {
            tool_budget: Some(tool_budget),
            ..self
        }
    }

    #[must_use]
    pub fn with_token_budget(self, token_budget: u64) -> Self {
        Self {
            token_budget: Some(token_budget),
            ..self
        }
    }
}

/// One served agent.
#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub(crate) workspace: PathBuf,
    pub(crate) identity: AgentIdentity,
    pub(crate) approval: InboundApproval,
    pub(crate) shell: bool,
    pub(crate) run: RunDefaults,
    pub(crate) token: Option<BearerToken>,
    pub(crate) public_url: Option<String>,
    pub(crate) data_dir: Option<PathBuf>,
}

impl ServeConfig {
    /// Serves the agent that runs in `workspace`: its instructions, skills,
    /// tools and MCP servers, discovered exactly as `basis spawn` there would
    /// discover them. No shell, no consequential calls, no token — each is
    /// something the operator turns on.
    pub fn new(workspace: impl Into<PathBuf>, identity: AgentIdentity) -> Self {
        Self {
            workspace: workspace.into(),
            identity,
            approval: InboundApproval::default(),
            shell: false,
            run: RunDefaults::default(),
            token: None,
            public_url: None,
            data_dir: None,
        }
    }

    #[must_use]
    pub fn with_approval(self, approval: InboundApproval) -> Self {
        Self { approval, ..self }
    }

    /// Grants served tasks the shell (ADR-0006: an explicit grant, never a
    /// default).
    #[must_use]
    pub fn with_shell(self) -> Self {
        Self {
            shell: true,
            ..self
        }
    }

    #[must_use]
    pub fn with_run_defaults(self, run: RunDefaults) -> Self {
        Self { run, ..self }
    }

    /// Requires `token` on every call but the public card, and declares the
    /// bearer scheme on the card.
    #[must_use]
    pub fn with_token(self, token: BearerToken) -> Self {
        Self {
            token: Some(token),
            ..self
        }
    }

    /// The URL callers reach the JSON-RPC endpoint at, as advertised on the
    /// card — what a reverse proxy terminating TLS in front of this server
    /// exposes. Defaults to `http://<bound address>/`.
    #[must_use]
    pub fn with_public_url(self, public_url: impl Into<String>) -> Self {
        Self {
            public_url: Some(public_url.into()),
            ..self
        }
    }

    /// Keeps tasks under an explicit data root rather than the one
    /// `BASIS_DATA_DIR`/XDG discovery finds — what `basis_tasks::Tasks::open_at`
    /// is for.
    #[must_use]
    pub fn with_data_dir(self, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: Some(data_dir.into()),
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_token_is_no_token() {
        assert!(BearerToken::new("").is_none());
        assert!(BearerToken::new("  \t").is_none());
        assert!(BearerToken::new("s3cret").is_some());
    }

    #[test]
    fn a_token_matches_only_itself() {
        let token = BearerToken::new("s3cret").unwrap();
        assert!(token.matches("s3cret"));
        assert!(!token.matches("s3cre"));
        assert!(!token.matches("s3crex"));
        assert!(!token.matches(""));
    }

    #[test]
    fn a_token_never_prints() {
        let token = BearerToken::new("s3cret").unwrap();
        assert!(!format!("{token:?}").contains("s3cret"));
    }

    #[test]
    fn inbound_approval_maps_onto_the_two_policies_that_ask_nobody() {
        assert_eq!(Approve::from(InboundApproval::Never), Approve::Never);
        assert_eq!(Approve::from(InboundApproval::Always), Approve::Always);
        assert_eq!(InboundApproval::default(), InboundApproval::Never);
    }
}
