//! The complete host-owned contract for one minted run.
//!
//! A workspace is still the source of defaults. Every `None` at the outer
//! level here means “inherit the workspace”; the nested options on values
//! whose upstream setting is itself optional distinguish inheritance from an
//! explicit clear. Applying a profile therefore never has to guess whether a
//! host omitted a value or deliberately removed one.

use mentra::{ModelInfo, ProviderRequestOptions, ToolResultPagingConfig};

use crate::{compaction::Compaction, context::SystemPrompt};

use super::ToolRoster;

/// Immutable overrides for one run minted from a [`Workspace`](super::Workspace).
///
/// A default profile says nothing and preserves every workspace default.
/// Builder methods return a new value, so a host may keep a common profile and
/// derive narrower variants without shared mutation.
///
/// Reasoning is decided by
/// [`with_provider_request_options`](Self::with_provider_request_options), and
/// only there: the complete options carry `reasoning` beside every other
/// provider field, so a profile that states them has stated it. That decision
/// outranks legacy [`RunSpec::with_effort`](super::RunSpec::with_effort)
/// regardless of which builder method was called later — the profile is the
/// complete contract, while `effort` is its compatibility fallback.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RunProfile {
    resolved_model: Option<ModelInfo>,
    tool_roster: Option<ToolRoster>,
    provider_request_options: Option<ProviderRequestOptions>,
    /// Outer `None` inherits; `Some(None)` explicitly clears.
    max_output_tokens: Option<Option<u32>>,
    compaction: Option<Compaction>,
    /// Outer `None` inherits; `Some(None)` explicitly clears.
    tool_result_paging: Option<Option<ToolResultPagingConfig>>,
    system_prompt: Option<SystemPrompt>,
}

/// Hand-written because provider request options can contain credentials in
/// `session.extra_headers`, while a system prompt can contain arbitrary host
/// data. Presence and set/clear posture are enough to diagnose composition;
/// neither payload belongs in logs.
impl std::fmt::Debug for RunProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunProfile")
            .field(
                "resolved_model",
                &self
                    .resolved_model
                    .as_ref()
                    .map(|model| (model.id.as_str(), model.provider.as_str())),
            )
            .field("tool_roster", &set_if(self.tool_roster.as_ref()))
            .field(
                "provider_request_options",
                &redacted_if(self.provider_request_options.as_ref()),
            )
            .field("max_output_tokens", &set_or_clear(&self.max_output_tokens))
            .field("compaction", &self.compaction)
            .field(
                "tool_result_paging",
                &set_or_clear(&self.tool_result_paging),
            )
            .field(
                "system_prompt",
                &self.system_prompt.as_ref().map(|prompt| match prompt {
                    SystemPrompt::Replace(_) => "<replace redacted>",
                    SystemPrompt::Append(_) => "<append redacted>",
                }),
            )
            .finish()
    }
}

impl RunProfile {
    /// An empty profile: every value comes from the workspace.
    pub fn new() -> Self {
        Self::default()
    }

    /// Uses complete, already-resolved model metadata for this run.
    ///
    /// No model listing is performed. The model must name the runtime's
    /// registered provider; [`Workspace::prepare`](super::Workspace::prepare)
    /// and [`Workspace::resume`](super::Workspace::resume) reject a mismatch
    /// before minting or resuming a session.
    #[must_use]
    pub fn with_resolved_model(self, model: ModelInfo) -> Self {
        Self {
            resolved_model: Some(model),
            ..self
        }
    }

    /// Offers this exact base roster for the run.
    ///
    /// Per-mint foreign-tool hiding still narrows it afterwards: an exact
    /// roster cannot grant a sibling workspace's declared or MCP tools.
    #[must_use]
    pub fn with_tool_roster(self, tool_roster: ToolRoster) -> Self {
        Self {
            tool_roster: Some(tool_roster),
            ..self
        }
    }

    /// Replaces the complete provider-specific request options for this run.
    ///
    /// This call is also the last word on reasoning for the run, including when
    /// `options.reasoning` is `None`: a stated contract is complete, so the
    /// legacy [`RunSpec::with_effort`](super::RunSpec::with_effort) fallback
    /// does not fill the gap back in.
    ///
    /// Nonempty `options.session.extra_headers` are accepted only when the
    /// runtime was explicitly built with
    /// [`RuntimeBuilder::with_ephemeral_history`](crate::RuntimeBuilder::with_ephemeral_history):
    /// Mentra persists the complete options inside its agent config, so a
    /// durable runtime is refused before mint rather than writing a possible
    /// credential to disk.
    #[must_use]
    pub fn with_provider_request_options(self, options: ProviderRequestOptions) -> Self {
        Self {
            provider_request_options: Some(options),
            ..self
        }
    }

    /// Sets the provider's maximum output tokens, or explicitly clears the
    /// workspace value with `None`.
    #[must_use]
    pub fn with_max_output_tokens(self, max_output_tokens: Option<u32>) -> Self {
        Self {
            max_output_tokens: Some(max_output_tokens),
            ..self
        }
    }

    /// Replaces the workspace's compaction posture for this run.
    #[must_use]
    pub fn with_compaction(self, compaction: Compaction) -> Self {
        Self {
            compaction: Some(compaction),
            ..self
        }
    }

    /// Enables tool-result paging with exact settings, or explicitly disables
    /// an inherited paging posture with `None`.
    #[must_use]
    pub fn with_tool_result_paging(
        self,
        tool_result_paging: Option<ToolResultPagingConfig>,
    ) -> Self {
        Self {
            tool_result_paging: Some(tool_result_paging),
            ..self
        }
    }

    /// Replaces or appends to the workspace's rendered system prompt for this
    /// run, according to [`SystemPrompt`].
    #[must_use]
    pub fn with_system_prompt(self, system_prompt: SystemPrompt) -> Self {
        Self {
            system_prompt: Some(system_prompt),
            ..self
        }
    }

    pub(crate) fn resolved_model(&self) -> Option<&ModelInfo> {
        self.resolved_model.as_ref()
    }

    /// Whether this profile, rather than `RunSpec::effort` or workspace
    /// config, has answered the reasoning question.
    pub(crate) fn decides_reasoning(&self) -> bool {
        self.provider_request_options.is_some()
    }

    /// Whether complete request options contain headers that would be
    /// persisted with a durable Mentra agent config.
    pub(crate) fn has_extra_headers(&self) -> bool {
        self.provider_request_options
            .as_ref()
            .is_some_and(|options| !options.session.extra_headers.is_empty())
    }

    /// The first field Mentra 0.23 cannot change on an already resumed agent.
    ///
    /// The model is deliberately absent: `Session` exposes an exact setter for
    /// it. Full provider options are present even if only their reasoning field
    /// differs, because accepting a partial projection would silently drop the
    /// rest of the caller's contract.
    pub(crate) fn unsupported_on_resume(&self) -> Option<&'static str> {
        if self.tool_roster.is_some() {
            Some("tool_roster")
        } else if self.provider_request_options.is_some() {
            Some("provider_request_options")
        } else if self.max_output_tokens.is_some() {
            Some("max_output_tokens")
        } else if self.compaction.is_some() {
            Some("compaction")
        } else if self.tool_result_paging.is_some() {
            Some("tool_result_paging")
        } else if self.system_prompt.is_some() {
            Some("system_prompt")
        } else {
            None
        }
    }

    /// Applies every stated field to a cloned workspace agent configuration.
    pub(crate) fn apply_to(
        &self,
        mut agent: mentra::agent::AgentConfig,
    ) -> mentra::agent::AgentConfig {
        if let Some(roster) = &self.tool_roster {
            agent.tool_profile = roster.clone().into_profile();
        }
        if let Some(options) = &self.provider_request_options {
            agent.provider_request_options = options.clone();
        }
        if let Some(max_output_tokens) = self.max_output_tokens {
            agent.max_output_tokens = max_output_tokens;
        }
        if let Some(compaction) = self.compaction {
            let transcript_dir = agent.compaction.transcript_dir.clone();
            agent.compaction = compaction.into_mentra(transcript_dir);
        }
        if let Some(tool_result_paging) = self.tool_result_paging {
            agent.tool_result_paging = tool_result_paging;
        }
        if let Some(system_prompt) = &self.system_prompt {
            agent.system = apply_system_prompt(agent.system, system_prompt);
        }

        agent
    }
}

fn set_if<T>(value: Option<&T>) -> Option<&'static str> {
    value.map(|_| "<set>")
}

fn redacted_if<T>(value: Option<&T>) -> Option<&'static str> {
    value.map(|_| "<redacted>")
}

fn set_or_clear<T>(value: &Option<Option<T>>) -> Option<&'static str> {
    value
        .as_ref()
        .map(|inner| if inner.is_some() { "<set>" } else { "<clear>" })
}

fn apply_system_prompt(current: Option<String>, profile: &SystemPrompt) -> Option<String> {
    match profile {
        SystemPrompt::Replace(text) => spoken(text),
        SystemPrompt::Append(text) => join(current.and_then(|text| spoken(&text)), spoken(text)),
    }
}

fn spoken(text: &str) -> Option<String> {
    (!text.trim().is_empty()).then(|| text.to_string())
}

fn join(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) => Some(format!("{first}\n\n{second}")),
        (Some(first), None) => Some(first),
        (None, Some(second)) => Some(second),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use mentra::ReasoningOptions;

    use super::*;
    use crate::run::Effort;

    fn reasoning(effort: Effort) -> ReasoningOptions {
        ReasoningOptions {
            effort: Some(effort.into()),
            summary: None,
        }
    }

    #[test]
    fn an_empty_profile_changes_nothing() {
        let mut agent = mentra::agent::AgentConfig {
            system: Some("workspace".to_string()),
            max_output_tokens: Some(777),
            tool_result_paging: Some(ToolResultPagingConfig {
                threshold_bytes: 10,
                page_bytes: 5,
            }),
            ..Default::default()
        };
        agent.provider_request_options.reasoning = Some(reasoning(Effort::Low));

        let applied = RunProfile::new().apply_to(agent.clone());
        assert_eq!(
            serde_json::to_value(applied).expect("applied config serializes"),
            serde_json::to_value(agent).expect("original config serializes")
        );
    }

    #[test]
    fn nested_options_distinguish_clear_from_inherit() {
        let mut agent = mentra::agent::AgentConfig {
            max_output_tokens: Some(777),
            tool_result_paging: Some(ToolResultPagingConfig {
                threshold_bytes: 10,
                page_bytes: 5,
            }),
            ..Default::default()
        };
        agent.provider_request_options.reasoning = Some(reasoning(Effort::Low));

        let cleared = RunProfile::new()
            .with_max_output_tokens(None)
            .with_tool_result_paging(None)
            .apply_to(agent);

        assert_eq!(cleared.max_output_tokens, None);
        assert_eq!(cleared.tool_result_paging, None);
    }

    #[test]
    fn compaction_and_paging_reach_the_final_agent_without_moving_transcripts() {
        let transcript_dir = PathBuf::from("/host/runtime/transcripts");
        let agent = mentra::agent::AgentConfig {
            compaction: mentra::agent::CompactionConfig {
                transcript_dir: transcript_dir.clone(),
                ..Default::default()
            },
            ..Default::default()
        };
        let paging = ToolResultPagingConfig {
            threshold_bytes: 128 * 1024,
            page_bytes: 16 * 1024,
        };

        let applied = RunProfile::new()
            .with_compaction(
                Compaction::default()
                    .with_keep_recent_tool_results(Some(2))
                    .with_auto_threshold_tokens(Some(12_345))
                    .with_auto_threshold_percent(Some(60))
                    .with_preserve_recent_user_tokens(777),
            )
            .with_tool_result_paging(Some(paging))
            .apply_to(agent);

        assert_eq!(applied.compaction.keep_recent_tool_results, 2);
        assert_eq!(
            applied.compaction.auto_compact_threshold_tokens,
            Some(12_345)
        );
        assert_eq!(applied.compaction.auto_compact_threshold_percent, Some(60));
        assert_eq!(applied.compaction.preserve_recent_user_tokens, 777);
        assert_eq!(applied.compaction.transcript_dir, transcript_dir);
        assert_eq!(applied.tool_result_paging, Some(paging));
    }

    #[test]
    fn system_append_keeps_the_workspace_prompt_and_lands_last() {
        let agent = mentra::agent::AgentConfig {
            system: Some("workspace".to_string()),
            ..Default::default()
        };

        let applied = RunProfile::new()
            .with_system_prompt(SystemPrompt::Append("host".to_string()))
            .apply_to(agent);

        assert_eq!(applied.system.as_deref(), Some("workspace\n\nhost"));
    }

    #[test]
    fn stated_request_options_replace_the_agent_reasoning_wholesale() {
        let mut agent = mentra::agent::AgentConfig::default();
        agent.provider_request_options.reasoning = Some(reasoning(Effort::High));
        let mut options = ProviderRequestOptions {
            reasoning: Some(reasoning(Effort::Low)),
            ..Default::default()
        };
        options.responses.service_tier = Some("priority".to_string());

        let applied = RunProfile::new()
            .with_provider_request_options(options)
            .apply_to(agent);

        assert_eq!(
            applied.provider_request_options.reasoning,
            Some(reasoning(Effort::Low))
        );
        assert_eq!(
            applied
                .provider_request_options
                .responses
                .service_tier
                .as_deref(),
            Some("priority")
        );
    }

    #[test]
    fn debug_redacts_headers_and_system_text() {
        let mut options = ProviderRequestOptions::default();
        options.session.extra_headers =
            BTreeMap::from([("authorization".to_string(), "Bearer secret".to_string())]);
        let profile = RunProfile::new()
            .with_provider_request_options(options)
            .with_system_prompt(SystemPrompt::Replace("private instructions".to_string()));

        let rendered = format!("{profile:?}");

        assert!(!rendered.contains("Bearer secret"));
        assert!(!rendered.contains("private instructions"));
        assert!(rendered.contains("<redacted>"));
    }
}
