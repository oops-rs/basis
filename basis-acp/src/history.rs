//! Replaying a resumed conversation to the client.
//!
//! ACP draws the line between two methods here: `session/load` loads a session
//! *and replays its message history*, while `session/resume` picks the same
//! conversation up without replaying — for agents that cannot. basis can do
//! both, because mentra keeps the transcript and
//! [`PreparedRun::history`](basis::PreparedRun::history) hands it back, so a
//! client that reconnects sees the conversation rather than an empty pane.
//!
//! Only the chat messages are replayed. mentra's transcript also holds tool
//! calls and their results, but rebuilding those as ACP tool calls would mean
//! inventing ids and statuses for work that finished in another process — and
//! a client redrawing a conversation needs the conversation.

use agent_client_protocol::schema::v1::{ContentBlock, ContentChunk, SessionUpdate, TextContent};
use basis::runtime::Role;

/// The updates that replay `history`, oldest first.
pub fn replay(history: impl IntoIterator<Item = (Role, String)>) -> Vec<SessionUpdate> {
    history.into_iter().filter_map(replayed).collect()
}

fn replayed((role, text): (Role, String)) -> Option<SessionUpdate> {
    // A turn that only called tools leaves an assistant message with no text.
    // Replaying it as an empty chunk would draw an empty bubble.
    if text.trim().is_empty() {
        return None;
    }

    let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));

    match role {
        Role::User => Some(SessionUpdate::UserMessageChunk(chunk)),
        Role::Assistant => Some(SessionUpdate::AgentMessageChunk(chunk)),
        // A role neither side named — a tool result, or something a provider
        // invented. It is not part of the conversation a person had.
        Role::Unknown(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(update: &SessionUpdate) -> String {
        let chunk = match update {
            SessionUpdate::UserMessageChunk(chunk) | SessionUpdate::AgentMessageChunk(chunk) => {
                chunk
            }
            other => panic!("expected a message chunk, got {other:?}"),
        };
        match &chunk.content {
            ContentBlock::Text(text) => text.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    #[test]
    fn a_conversation_replays_in_order_and_keeps_who_said_what() {
        let updates = replay([
            (Role::User, "remember 41".to_string()),
            (Role::Assistant, "noted".to_string()),
        ]);

        assert_eq!(updates.len(), 2);
        assert!(matches!(updates[0], SessionUpdate::UserMessageChunk(_)));
        assert_eq!(text_of(&updates[0]), "remember 41");
        assert!(matches!(updates[1], SessionUpdate::AgentMessageChunk(_)));
        assert_eq!(text_of(&updates[1]), "noted");
    }

    #[test]
    fn a_message_with_no_text_is_not_replayed() {
        // What a tool-calling turn leaves behind. An empty bubble is worse
        // than no bubble.
        let updates = replay([
            (Role::Assistant, "   ".to_string()),
            (Role::Unknown("tool".to_string()), "output".to_string()),
        ]);

        assert!(updates.is_empty());
    }

    #[test]
    fn an_empty_history_replays_nothing() {
        assert!(replay(Vec::<(Role, String)>::new()).is_empty());
    }
}
