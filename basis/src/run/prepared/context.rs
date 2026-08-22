//! What a run's mint knew about its model's ceiling.
//!
//! Two facts, both established once — at `Workspace::minted` for a workspace
//! run, never for one built through
//! [`prepare_with_session`](super::super::prepare_with_session), which has no
//! workspace to ask — and read many times over the run's life. Their own type
//! rather than two more fields on [`PreparedRun`](super::PreparedRun) because
//! `prepared.rs` is already past this crate's file-size ceiling; nothing here
//! needed a second reason.

use mentra::Message;

/// The context window and system prompt a run's mint observed, if either was
/// known.
#[derive(Debug, Clone, Default)]
pub(crate) struct ContextSnapshot {
    context_window: Option<usize>,
    /// The exact string basis handed mentra as `AgentConfig.system`. Not the
    /// *effective* one mentra actually sends — see
    /// [`estimated_tokens`](Self::estimated_tokens) for what that costs.
    system_prompt: Option<String>,
}

impl ContextSnapshot {
    pub(crate) fn new(context_window: Option<usize>, system_prompt: Option<String>) -> Self {
        Self {
            context_window,
            system_prompt,
        }
    }

    pub(crate) const fn context_window(&self) -> Option<usize> {
        self.context_window
    }

    /// This snapshot with its window cleared, system prompt untouched.
    ///
    /// [`PreparedRun::set_model`](super::PreparedRun::set_model) takes this
    /// path: it hands mentra a model by id alone, so it cannot know what that
    /// model's window is, and carrying the old one forward would describe a
    /// model this run is no longer on. The system prompt is unaffected by a
    /// model change, so it survives.
    pub(crate) fn without_window(&self) -> Self {
        Self {
            context_window: None,
            system_prompt: self.system_prompt.clone(),
        }
    }

    /// Estimates the token cost of `history` plus the system prompt this
    /// snapshot knows. See
    /// [`PreparedRun::estimated_context_tokens`](super::PreparedRun::estimated_context_tokens)
    /// for what this covers and what it does not.
    pub(crate) fn estimated_tokens(&self, history: &[Message]) -> usize {
        mentra::memory::estimated_request_tokens(history, self.system_prompt.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_known_window_and_prompt_are_both_kept() {
        let snapshot = ContextSnapshot::new(Some(128_000), Some("be brief".to_string()));

        assert_eq!(snapshot.context_window(), Some(128_000));
    }

    #[test]
    fn clearing_the_window_leaves_the_prompt_alone() {
        let snapshot =
            ContextSnapshot::new(Some(128_000), Some("be brief".to_string())).without_window();

        assert_eq!(snapshot.context_window(), None);
        assert_eq!(snapshot.system_prompt.as_deref(), Some("be brief"));
    }

    #[test]
    fn the_estimate_covers_history_and_the_known_system_prompt() {
        let history = vec![mentra::Message::user(mentra::ContentBlock::text(
            "hello there",
        ))];

        let with_prompt =
            ContextSnapshot::new(None, Some("x".repeat(400))).estimated_tokens(&history);
        let without_prompt = ContextSnapshot::default().estimated_tokens(&history);

        assert!(
            with_prompt > without_prompt,
            "a system prompt basis knows about must widen the estimate"
        );
    }

    #[test]
    fn an_unknown_window_reports_none() {
        assert_eq!(ContextSnapshot::default().context_window(), None);
    }
}
