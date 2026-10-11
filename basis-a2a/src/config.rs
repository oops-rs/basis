//! What one served agent is: the workspace it runs in — or the host that
//! runs its tasks — what its card says about it, how its runs are bounded,
//! and who may call it.
//!
//! Everything here is decided once, before the first request, and none of it
//! changes while serving. [`InboundApproval`] has no `Prompt` variant
//! (ADR-0028 §4): there is no person at this server to put a question to.
//! What it has instead is [`InboundApproval::AskCaller`] (ADR-0030 §7), which
//! puts the question to the one party there is — whoever sent the task.

mod auth;
mod identity;

use std::{fmt, path::PathBuf, sync::Arc, time::Duration};

use basis::{Effort, SystemPrompt};
use basis_tasks::Approve;

use crate::host::TaskHost;

pub(crate) use auth::Principals;
pub use auth::{BearerToken, DEFAULT_PRINCIPAL, Principal, PrincipalError};
pub use identity::{AgentIdentity, ExtendedCard};

/// How a served task's consequential calls are answered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InboundApproval {
    /// Every consequential call is refused. The default: an agent reachable
    /// over a network reads and answers unless its operator says otherwise.
    #[default]
    Never,
    /// Every consequential call is allowed.
    Always,
    /// Every consequential call is put to the caller: the task moves to
    /// `INPUT_REQUIRED` with the call's id, tool and preview, and the call
    /// is made only if the caller answers `approve` within `deadline` — see
    /// `docs/a2a/approval-v1.md`. Silence past the deadline is `deny`.
    AskCaller { deadline: Duration },
}

impl From<InboundApproval> for Approve {
    fn from(approval: InboundApproval) -> Self {
        match approval {
            InboundApproval::Never => Approve::Never,
            InboundApproval::Always => Approve::Always,
            // `basis-tasks` asks whichever process drives the task, and this
            // server drives every task it accepts — through a host that puts
            // the question to the task's caller.
            InboundApproval::AskCaller { .. } => Approve::Prompt,
        }
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
    pub(crate) system_prompt: Option<SystemPrompt>,
}

impl RunDefaults {
    /// Replaces or appends to the workspace's own system prompt, as
    /// `--system-prompt` / `--append-system-prompt` do for `basis spawn`.
    #[must_use]
    pub fn with_system_prompt(self, system_prompt: SystemPrompt) -> Self {
        Self {
            system_prompt: Some(system_prompt),
            ..self
        }
    }

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

    /// Whether nothing is set: the defaults `basis-tasks` gives a task
    /// itself.
    pub(crate) fn is_unset(&self) -> bool {
        let Self {
            provider,
            base_url,
            model,
            effort,
            deadline,
            tool_budget,
            token_budget,
            system_prompt,
        } = self;
        provider.is_none()
            && base_url.is_none()
            && model.is_none()
            && effort.is_none()
            && deadline.is_none()
            && tool_budget.is_none()
            && token_budget.is_none()
            && system_prompt.is_none()
    }
}

/// Who runs the tasks a server serves.
#[derive(Clone)]
pub(crate) enum Source {
    /// `basis-tasks`, in this workspace.
    Workspace(PathBuf),
    /// A host, with the server's own files under `state_dir` (ADR-0033 §2).
    Hosted {
        state_dir: PathBuf,
        host: Arc<dyn TaskHost>,
    },
}

impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workspace(workspace) => f.debug_tuple("Workspace").field(workspace).finish(),
            Self::Hosted { state_dir, .. } => f
                .debug_struct("Hosted")
                .field("state_dir", state_dir)
                .finish_non_exhaustive(),
        }
    }
}

/// How many unsettled tasks one principal may have at once when the
/// operator names no other number. The workspace holds at most
/// `basis_tasks::MAX_TASKS` (1024), settled ones included; a quota per
/// caller keeps any one of them from filling it for the rest.
pub const DEFAULT_LIVE_TASK_QUOTA: usize = 32;

/// One served agent.
#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub(crate) source: Source,
    pub(crate) identity: AgentIdentity,
    pub(crate) approval: InboundApproval,
    pub(crate) shell: bool,
    pub(crate) run: RunDefaults,
    pub(crate) tokens: Vec<(Principal, BearerToken)>,
    pub(crate) public_url: Option<String>,
    pub(crate) data_dir: Option<PathBuf>,
    pub(crate) extended_card: Option<ExtendedCard>,
    pub(crate) push_notifications: bool,
    pub(crate) private_webhooks: bool,
    pub(crate) hop_cap: u32,
    pub(crate) trust_anonymous: bool,
    pub(crate) live_task_quota: usize,
    pub(crate) stream_cap: usize,
}

impl ServeConfig {
    /// Serves the agent that runs in `workspace`: its instructions, skills,
    /// tools and MCP servers, discovered exactly as `basis spawn` there would
    /// discover them. No shell, no consequential calls, no token — each is
    /// something the operator turns on.
    pub fn new(workspace: impl Into<PathBuf>, identity: AgentIdentity) -> Self {
        Self::with_source(Source::Workspace(workspace.into()), identity)
    }

    /// Serves the tasks `host` runs (ADR-0033). The server's index, its push
    /// configs and every other file it keeps live under `state_dir`, which
    /// is created if missing; nothing is written to a workspace.
    ///
    /// Every option but the ones that configure a `basis-tasks` run applies
    /// unchanged. Those — [`with_shell`](Self::with_shell),
    /// [`with_run_defaults`](Self::with_run_defaults),
    /// [`with_data_dir`](Self::with_data_dir) and
    /// [`InboundApproval::AskCaller`] — would mean nothing here, and
    /// [`A2aService::open`](crate::A2aService::open) refuses each by name
    /// rather than ignore it.
    pub fn hosted(
        identity: AgentIdentity,
        state_dir: impl Into<PathBuf>,
        host: Arc<dyn TaskHost>,
    ) -> Self {
        Self::with_source(
            Source::Hosted {
                state_dir: state_dir.into(),
                host,
            },
            identity,
        )
    }

    fn with_source(source: Source, identity: AgentIdentity) -> Self {
        Self {
            source,
            identity,
            approval: InboundApproval::default(),
            shell: false,
            run: RunDefaults::default(),
            tokens: Vec::new(),
            public_url: None,
            data_dir: None,
            extended_card: None,
            push_notifications: false,
            private_webhooks: false,
            hop_cap: crate::DEFAULT_HOP_CAP,
            trust_anonymous: false,
            live_task_quota: DEFAULT_LIVE_TASK_QUOTA,
            stream_cap: crate::DEFAULT_STREAM_CAP,
        }
    }

    #[must_use]
    pub fn with_approval(self, approval: InboundApproval) -> Self {
        Self { approval, ..self }
    }

    /// Grants served tasks the shell. Off by default, unlike the CLI and ACP
    /// (ADR-0013): there the operator is the one asking for work, and here it
    /// is another agent (ADR-0028 §4).
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
    /// bearer scheme on the card. Its holder is the [`DEFAULT_PRINCIPAL`]:
    /// the one-caller server this was before principals existed.
    #[must_use]
    pub fn with_token(self, token: BearerToken) -> Self {
        let principal =
            Principal::new(DEFAULT_PRINCIPAL).expect("the default principal's name is plain");
        self.with_principal(principal, token)
    }

    /// Accepts `token` as `principal` (ADR-0030 §8). Every task a caller
    /// creates is recorded under its principal, and every task-scoped call
    /// sees only that principal's own. Names and tokens must each be unique;
    /// [`A2aService::open`](crate::A2aService::open) refuses a set that is not.
    #[must_use]
    pub fn with_principal(self, principal: Principal, token: BearerToken) -> Self {
        let mut tokens = self.tokens;
        tokens.push((principal, token));
        Self { tokens, ..self }
    }

    /// The URL callers reach the JSON-RPC endpoint at, as advertised on the
    /// card — what a reverse proxy terminating TLS in front of this server
    /// exposes. Defaults to `http://<bound address>/`. The HTTP+JSON interface
    /// is advertised under it, at [`HTTP_JSON_PATH`](crate::HTTP_JSON_PATH).
    #[must_use]
    pub fn with_public_url(self, public_url: impl Into<String>) -> Self {
        Self {
            public_url: Some(public_url.into()),
            ..self
        }
    }

    /// Serves `extended` to authenticated callers and declares it on the
    /// public card (ADR-0030 §5). Needs at least one token:
    /// [`A2aService::open`](crate::A2aService::open) refuses an extended card
    /// on a server nobody authenticates to, since nobody could be served it.
    #[must_use]
    pub fn with_extended_card(self, extended: ExtendedCard) -> Self {
        Self {
            extended_card: Some(extended),
            ..self
        }
    }

    /// Serves push notifications (ADR-0030 §4): callers may register
    /// webhooks, which this server then calls on every status change of
    /// their task. Off by default, because it makes this server send
    /// requests to URLs its callers choose; every webhook is checked against
    /// loopback, private and link-local addresses unless
    /// [`with_private_webhooks`](Self::with_private_webhooks) says otherwise.
    #[must_use]
    pub fn with_push_notifications(self) -> Self {
        Self {
            push_notifications: true,
            ..self
        }
    }

    /// Lets webhooks point at loopback, private and link-local addresses —
    /// for a deployment whose callers live on the same private network. It
    /// still never sends anywhere but `http` and `https`.
    #[must_use]
    pub fn with_private_webhooks(self) -> Self {
        Self {
            private_webhooks: true,
            ..self
        }
    }

    /// Refuses a message more than `hop_cap` agent-to-agent hops from its
    /// top-level caller (ADR-0030 §9, `docs/a2a/hops-v1.md`). Defaults to
    /// [`DEFAULT_HOP_CAP`](crate::DEFAULT_HOP_CAP).
    #[must_use]
    pub fn with_hop_cap(self, hop_cap: u32) -> Self {
        Self { hop_cap, ..self }
    }

    /// Lets a server with no token serve push notifications and ask its
    /// caller anyway. Without a token every local process is the one
    /// anonymous caller, so any of them could register a webhook this server
    /// then calls, or answer its questions; [`A2aService::open`] refuses
    /// either feature on such a server unless the operator says, with this,
    /// that every local process is trusted.
    ///
    /// [`A2aService::open`]: crate::A2aService::open
    #[must_use]
    pub fn with_trusted_anonymous_callers(self) -> Self {
        Self {
            trust_anonymous: true,
            ..self
        }
    }

    /// Lets each principal have at most `quota` tasks that have not
    /// settled; a send past it is refused until one does. Defaults to
    /// [`DEFAULT_LIVE_TASK_QUOTA`].
    #[must_use]
    pub fn with_live_task_quota(self, quota: usize) -> Self {
        Self {
            live_task_quota: quota,
            ..self
        }
    }

    /// Lets each principal hold at most `cap` streams open at once —
    /// `SendStreamingMessage` and `SubscribeToTask` alike, on either binding.
    /// Defaults to [`DEFAULT_STREAM_CAP`](crate::DEFAULT_STREAM_CAP).
    #[must_use]
    pub fn with_stream_cap(self, cap: usize) -> Self {
        Self {
            stream_cap: cap,
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
    fn inbound_approval_maps_onto_the_task_layers_policies() {
        assert_eq!(Approve::from(InboundApproval::Never), Approve::Never);
        assert_eq!(Approve::from(InboundApproval::Always), Approve::Always);
        assert_eq!(
            Approve::from(InboundApproval::AskCaller {
                deadline: Duration::from_secs(60)
            }),
            Approve::Prompt
        );
        assert_eq!(InboundApproval::default(), InboundApproval::Never);
    }
}
