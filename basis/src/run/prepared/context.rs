//! What a run's mint knew about the prompt it opened with.
//!
//! One fact, established once — at `Workspace::minted` for a workspace run,
//! never for one built through
//! [`prepare_with_session`](super::super::prepare_with_session), which has no
//! workspace to ask — and read many times over the run's life. Its own type
//! rather than one more field on [`PreparedRun`](super::PreparedRun) because
//! `prepared.rs` is already past this crate's file-size ceiling; nothing here
//! needed a second reason. The model's context window used to live beside it,
//! mirrored from what basis handed mentra; mentra's `Session` now answers that
//! itself, so the mirror and the desync it invited are gone.

use mentra::Message;

/// The system prompt a run's mint observed, if one was configured.
#[derive(Debug, Clone, Default)]
pub(crate) struct ContextSnapshot {
    /// The exact string basis handed mentra as `AgentConfig.system`. Not the
    /// *effective* one mentra actually sends — see
    /// [`estimated_tokens`](Self::estimated_tokens) for what that costs.
    system_prompt: Option<String>,
}

impl ContextSnapshot {
    pub(crate) const fn new(system_prompt: Option<String>) -> Self {
        Self { system_prompt }
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
    fn the_estimate_covers_history_and_the_known_system_prompt() {
        let history = vec![mentra::Message::user(mentra::ContentBlock::text(
            "hello there",
        ))];

        let with_prompt = ContextSnapshot::new(Some("x".repeat(400))).estimated_tokens(&history);
        let without_prompt = ContextSnapshot::default().estimated_tokens(&history);

        assert!(
            with_prompt > without_prompt,
            "a system prompt basis knows about must widen the estimate"
        );
    }
}
