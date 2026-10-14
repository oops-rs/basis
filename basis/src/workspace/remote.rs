//! Where a workspace's conversations with remote agents stand, from the
//! outside (ADR-0035) — whatever answered the names (ADR-0038).

use super::Workspace;
use crate::agents::remote::Waiting;

#[cfg(doc)]
use crate::PreparedRun;

impl Workspace {
    /// Where the conversation with the remote agent `agent` stands for runs
    /// bound to the host's `key`
    /// ([`PreparedRun::with_remote_conversations`]), or `None` when no run
    /// bound to it has handed `agent` anything — or when what it handed has
    /// been forgotten. `agent` is the agent's id, which is its name unless
    /// its resolver said otherwise (ADR-0037 §6).
    ///
    /// What a host reads to tell its model, before a run, that `@<agent>` is
    /// waiting for an answer
    /// ([`RemoteConversationState::InputRequired`]). Read from the record
    /// each time, so it is what the last run on `key` left, in this open or
    /// another on the same store.
    pub fn remote_conversation(&self, key: &str, agent: &str) -> Option<RemoteConversationState> {
        let conversation = self.remote.keyed_conversation(key, agent)?;
        Some(match conversation.outstanding {
            None => RemoteConversationState::Open,
            Some(outstanding) => match outstanding.waiting_for {
                Waiting::Agent => RemoteConversationState::Working,
                Waiting::Input => RemoteConversationState::InputRequired,
            },
        })
    }
}

/// Where a remote agent's conversation stands for runs bound to one host key
/// — what [`Workspace::remote_conversation`] reports (ADR-0035).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RemoteConversationState {
    /// A conversation is open and nothing is outstanding: the next
    /// `@<agent> <task>` continues its context.
    Open,
    /// The agent is still working on the last task it was handed: `@<agent>`
    /// alone waits for it again.
    Working,
    /// The agent asked a question: `@<agent> <answer>` answers it, on the
    /// same remote task.
    InputRequired,
}
