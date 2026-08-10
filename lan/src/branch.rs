//! Returning to an earlier point in a conversation.
//!
//! A conversation is a tree, not a list. mentra records every entry with the
//! entry it continues from, so going back moves the leaf pointer rather than
//! truncating: what came after stays in the transcript and stays addressable.
//! That is the difference between "undo that exchange and try something else"
//! and "throw the last exchange away", and it is why this is not the same as
//! starting a new session — everything before the branch point is still there,
//! already paid for.
//!
//! # lan owns the shape
//!
//! [`TranscriptEntry`] is lan's type rather than a re-export of mentra's
//! `TranscriptItem`, for the same reason [`Event`](crate::Event) and
//! [`TurnOptions`](crate::TurnOptions) are lan's: one normalized surface every
//! embedding sees, and mentra's internals free to move underneath. The kind
//! mapping is exhaustive with no wildcard, so a new mentra entry kind breaks
//! this build instead of quietly arriving as something else.
//!
//! Entry ids cross the boundary as strings, because a string is what a client
//! can carry in a JSON message and hand back. mentra's `EntryId` cannot be
//! built from one — it can only be read off an entry that exists — so an id
//! arriving from outside is matched against the tree rather than parsed. That
//! is the stricter check of the two: an id lan cannot find is an id lan will
//! not act on.
//!
//! # Why here and not in `run::prepared`
//!
//! Branching is its own concern with its own types, and
//! [`PreparedRun`](crate::PreparedRun) already exposes everything it needs
//! through [`session`](crate::PreparedRun::session). Keeping it out of the
//! module that drives turns leaves both smaller than one module holding both
//! would be.

use mentra::{AgentTranscript, EntryId, TranscriptItem, TranscriptKind};

use crate::PreparedRun;

/// One entry in the conversation tree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TranscriptEntry {
    /// This entry's id — what [`branch_from`](PreparedRun::branch_from) takes.
    pub id: String,
    /// The entry this one continues from. `None` marks the start of the
    /// conversation.
    pub parent_id: Option<String>,
    #[serde(flatten)]
    pub kind: EntryKind,
    /// The entry's text, so a client can show a person what they would be
    /// going back to without fetching anything else.
    pub text: String,
}

/// What an entry is. lan's own naming of mentra's transcript kinds.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryKind {
    /// Something the user said.
    UserTurn,
    /// Something the model said.
    AssistantTurn,
    /// A tool call and its result.
    ToolExchange { is_error: bool },
    /// Context the harness supplied rather than either party saying it.
    CanonicalContext,
    /// Memory the runtime recalled into the conversation.
    MemoryRecall,
    /// Work handed to a subagent or teammate.
    DelegationRequest,
    /// What came back from it.
    DelegationResult,
    /// Earlier turns replaced by a summary of them.
    CompactionSummary,
}

/// Why a conversation could not be branched.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BranchError {
    #[error("this conversation has no entry '{0}'")]
    UnknownEntry(String),

    /// Named an entry that a previous branch left behind. mentra can move the
    /// leaf back along the active path but not onto an abandoned one, so
    /// returning to a path already left is not something lan can offer.
    #[error(
        "entry '{0}' was left behind by an earlier branch; a conversation can only return to a \
         point still on its active path"
    )]
    NotOnActivePath(String),

    #[error("the conversation could not be branched: {0}")]
    Failed(String),
}

impl PreparedRun {
    /// The conversation as the model sees it: the active path, oldest first.
    ///
    /// This is the list to pick a branch point from. Entries left behind by an
    /// earlier branch are not in it — [`abandoned`](Self::abandoned) has those.
    pub fn transcript(&self) -> Vec<TranscriptEntry> {
        self.replay()
            .items()
            .iter()
            .map(TranscriptEntry::from_item)
            .collect()
    }

    /// Entries no longer on the active path, in the order they left it.
    ///
    /// Kept rather than deleted, so a client can show what was tried and
    /// discarded. mentra cannot return to one; they are history, not a
    /// destination.
    pub fn abandoned(&self) -> Vec<TranscriptEntry> {
        self.replay()
            .archived()
            .iter()
            .map(TranscriptEntry::from_item)
            .collect()
    }

    /// The entry the next turn will continue from. `None` before anything has
    /// been said.
    pub fn leaf(&self) -> Option<String> {
        self.replay().leaf().map(EntryId::to_string)
    }

    /// The entries recorded as continuing from `entry`, in creation order.
    ///
    /// More than one means the conversation branched there: each is the start
    /// of a path explored from the same point, and at most one of them is on
    /// the active path. Empty for an entry this conversation does not have,
    /// which is the same answer as for a leaf — asking about an id is not an
    /// operation that can fail.
    pub fn children(&self, entry: &str) -> Vec<TranscriptEntry> {
        let Some(id) = entry_id(self.replay(), entry) else {
            return Vec::new();
        };

        self.session()
            .children(&id)
            .into_iter()
            .map(TranscriptEntry::from_item)
            .collect()
    }

    /// Returns to `entry`, so the next turn continues from there along a
    /// different path. Answers how many entries left the active path.
    ///
    /// The entries after `entry` are moved, not deleted: they stay in the
    /// transcript and stay reachable through [`children`](Self::children), so
    /// a client can still show what was abandoned.
    ///
    /// Branching emits `SessionEvent::Branched`, which lan maps to
    /// [`Event::Branched`](crate::Event::Branched). Nothing is streaming
    /// between turns, so that event reaches only a subscriber the host holds
    /// itself — the count returned here is what an ordinary caller reads.
    pub fn branch_from(&mut self, entry: &str) -> Result<usize, BranchError> {
        let transcript = self.replay();

        let target = match entry_on_path(transcript, entry) {
            Some(id) => id,
            None if entry_id(transcript, entry).is_some() => {
                return Err(BranchError::NotOnActivePath(entry.to_string()));
            }
            None => return Err(BranchError::UnknownEntry(entry.to_string())),
        };

        self.session_mut()
            .branch_from(&target)
            .map_err(|error| BranchError::Failed(error.to_string()))
    }

    /// The conversation's transcript tree, active path and all.
    fn replay(&self) -> &AgentTranscript {
        self.session().replay()
    }
}

/// mentra's id for `entry`, looked up anywhere in the tree.
fn entry_id(transcript: &AgentTranscript, entry: &str) -> Option<EntryId> {
    transcript
        .items()
        .iter()
        .chain(transcript.archived())
        .find(|item| item.id.as_str() == entry)
        .map(|item| item.id.clone())
}

/// The same, restricted to the active path — the only entries a branch can
/// return to.
fn entry_on_path(transcript: &AgentTranscript, entry: &str) -> Option<EntryId> {
    transcript
        .items()
        .iter()
        .find(|item| item.id.as_str() == entry)
        .map(|item| item.id.clone())
}

impl TranscriptEntry {
    fn from_item(item: &TranscriptItem) -> Self {
        Self {
            id: item.id.to_string(),
            parent_id: item.parent_id.as_ref().map(EntryId::to_string),
            kind: EntryKind::from_kind(&item.kind),
            text: item.text(),
        }
    }
}

impl EntryKind {
    /// Exhaustive with no wildcard, deliberately: a kind mentra adds must be
    /// named here rather than silently becoming whatever the catch-all was.
    fn from_kind(kind: &TranscriptKind) -> Self {
        match kind {
            TranscriptKind::UserTurn => Self::UserTurn,
            TranscriptKind::AssistantTurn => Self::AssistantTurn,
            TranscriptKind::ToolExchange { is_error, .. } => Self::ToolExchange {
                is_error: *is_error,
            },
            TranscriptKind::CanonicalContext => Self::CanonicalContext,
            TranscriptKind::MemoryRecall => Self::MemoryRecall,
            TranscriptKind::DelegationRequest { .. } => Self::DelegationRequest,
            TranscriptKind::DelegationResult { .. } => Self::DelegationResult,
            TranscriptKind::CompactionSummary { .. } => Self::CompactionSummary,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mentra::ContentBlock;

    fn transcript() -> AgentTranscript {
        let mut transcript = AgentTranscript::default();
        transcript.push(TranscriptItem::user_turn(mentra::Message::user(
            ContentBlock::text("hello"),
        )));
        transcript.push(TranscriptItem::assistant_turn(mentra::Message::assistant(
            ContentBlock::text("hi"),
        )));
        transcript
    }

    #[test]
    fn an_entry_carries_its_text_and_its_parent() {
        let transcript = transcript();
        let entries: Vec<TranscriptEntry> = transcript
            .items()
            .iter()
            .map(TranscriptEntry::from_item)
            .collect();

        assert_eq!(entries[0].kind, EntryKind::UserTurn);
        assert_eq!(entries[0].text, "hello");
        assert_eq!(
            entries[0].parent_id, None,
            "the first entry starts the conversation"
        );
        assert_eq!(entries[1].kind, EntryKind::AssistantTurn);
        assert_eq!(
            entries[1].parent_id.as_deref(),
            Some(entries[0].id.as_str()),
            "an entry names what it continues from"
        );
    }

    #[test]
    fn a_failed_tool_exchange_says_so() {
        let item = TranscriptItem::tool_exchange(
            mentra::Message::user(ContentBlock::text("result")),
            Some("call-1".to_string()),
            true,
        );

        assert_eq!(
            TranscriptEntry::from_item(&item).kind,
            EntryKind::ToolExchange { is_error: true }
        );
    }

    #[test]
    fn an_id_is_only_recognized_when_the_tree_has_it() {
        let transcript = transcript();
        let known = transcript.items()[0].id.to_string();

        assert_eq!(
            entry_id(&transcript, &known).map(|id| id.to_string()),
            Some(known)
        );
        assert!(
            entry_id(&transcript, "entry-made-up").is_none(),
            "an id lan cannot find is an id lan will not act on"
        );
    }

    #[test]
    fn an_archived_entry_is_found_but_not_on_the_path() {
        let mut transcript = transcript();
        let first = transcript.items()[0].id.clone();
        transcript.branch_from(&first).expect("the first entry");

        let abandoned = transcript.archived()[0].id.to_string();

        assert!(
            entry_id(&transcript, &abandoned).is_some(),
            "an abandoned entry stays addressable"
        );
        assert!(
            entry_on_path(&transcript, &abandoned).is_none(),
            "but it is not somewhere the conversation can return to"
        );
    }

    #[test]
    fn an_entry_serializes_with_its_kind_inline() {
        let entry = TranscriptEntry {
            id: "entry-1".to_string(),
            parent_id: None,
            kind: EntryKind::ToolExchange { is_error: false },
            text: "listed the files".to_string(),
        };

        let json = serde_json::to_value(&entry).expect("an entry serializes");

        assert_eq!(json["kind"], "tool_exchange");
        assert_eq!(json["is_error"], false);
        assert_eq!(json["id"], "entry-1");
    }
}
