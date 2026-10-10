//! Where a reattached task picks up: which of its turns finished, and which
//! one a crash cut off.
//!
//! Two facts decide it, both read off the resumed conversation. mentra
//! recovers a turn the crash cut off before the model committed anything by
//! rolling it back, so it simply is not there, and the work that started it
//! runs again. A turn cut off *after* the model acted — tool calls made, no
//! answer yet — is kept, its unfinished calls closed as having an unknown
//! effect (mentra#64), and is continued rather than asked again: asking
//! again would put the prompt into the conversation twice, and settling it
//! would report an answer nobody gave.

use basis::PreparedRun;

use crate::{inbox, state::TaskMeta};

use crate::data_dir::AgentPaths;

/// What one turn of the attach loop sends.
pub(super) enum Work {
    /// The task's recorded prompt.
    Prompt,
    /// A message from the inbox: its id, then its body.
    Message(String, String),
    /// The turn a crash interrupted, finished from where the conversation
    /// ends; the id is the message whose turn it was, `None` for the
    /// prompt's.
    Continue(Option<String>),
}

impl Work {
    /// The inbox message this turn's reply answers, if any.
    pub(super) fn message_id(&self) -> Option<&str> {
        match self {
            Self::Message(id, _) | Self::Continue(Some(id)) => Some(id),
            Self::Prompt | Self::Continue(None) => None,
        }
    }
}

/// Where a reattached task's turn loop starts.
#[derive(Default)]
pub(super) struct Recovered {
    /// The recorded prompt has been answered — by a turn that finished.
    pub(super) initial_done: bool,
    /// A turn to continue before anything else: `Some(None)` is the
    /// prompt's, `Some(Some(id))` message `id`'s.
    pub(super) interrupted: Option<Option<String>>,
    /// The newest answer the conversation holds, standing in for the crashed
    /// process's unrecorded result when nothing is left to run.
    pub(super) last_result: String,
}

/// Reads where `run`, this task's own conversation resumed, stands.
/// `reverted` are the inbox messages this attach found in flight and put
/// back to pending; a continued message's turn is claimed back from them.
pub(super) fn recover(
    paths: &AgentPaths,
    meta: &TaskMeta,
    run: &PreparedRun,
    reverted: &[String],
) -> Result<Recovered, String> {
    let past_inherited = run.answered_turns() > meta.answered_before;
    let last_result = run.last_assistant_text().unwrap_or_default();
    if !run.has_interrupted_turn() {
        // Every turn in the conversation finished. One past what this task
        // inherited means the recorded prompt was answered, and running it
        // again would ask it twice.
        return Ok(Recovered {
            initial_done: past_inherited,
            interrupted: None,
            last_result,
        });
    }
    // A message is in flight only between the turn that starts it and the
    // reply that ends it, and messages start only once the prompt finished:
    // an interrupted turn with one in flight is that message's.
    if let Some(id) = reverted.last()
        && inbox::claim(paths, id)?
    {
        return Ok(Recovered {
            initial_done: true,
            interrupted: Some(Some(id.clone())),
            last_result,
        });
    }
    // Otherwise it is the prompt's — provided the prompt's turn is there at
    // all. A conversation this task continues may arrive already
    // interrupted; if the prompt's own turn rolled back, nothing past what
    // was inherited was committed, and the prompt is still to be asked.
    Ok(Recovered {
        initial_done: false,
        interrupted: past_inherited.then_some(None),
        last_result,
    })
}
