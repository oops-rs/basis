//! Asking the caller (ADR-0030 §7; proposal 0005 §1): a served task's
//! consequential call, put to whoever sent the task, as `INPUT_REQUIRED`.
//!
//! The mechanism is `basis-tasks`' own: a task recorded with
//! `Approve::Prompt` asks the [`PromptHost`] of whichever process drives it.
//! This server drives every task it accepts, and hands each driver a host
//! bound to that task's [`Questions`]. Its approver parks the turn on a
//! question with a fresh call id, which the task then projects as
//! `INPUT_REQUIRED`; a message on the task that answers that call id — in
//! words or as a data part, see `docs/a2a/approval-v1.md` — unparks it.
//!
//! The rules the proposal binds, and where each is kept:
//!
//! - **One question per task.** [`Questions::ask`] holds a lock for the life
//!   of a question, so parallel calls in one turn wait their turn to ask.
//! - **An answer answers exactly its call.** [`Questions::answer`] refuses a
//!   call id other than the open one, and an answer with no decision never
//!   reaches it; either way the question stays open.
//! - **The deadline answers `deny`.**
//! - **A cancel while parked cancels.** [`Questions::cancel`] denies the open
//!   question and every later one, so the turn ends and the cancel lands at
//!   its boundary. A question being opened as the cancel lands is refused
//!   too: both happen under one lock.
//! - **No answer outlives its question.** Call ids are minted per question
//!   and held only in memory: a restart re-drives the turn, which asks again
//!   under a new id, and an answer to the old one is refused. So is an answer
//!   that arrives as the deadline closes the question: the caller is told it
//!   came too late rather than that it was taken.
//! - **A shutdown records nothing.** [`Questions::interrupt`] resolves the
//!   open question and every later one as interrupted; the server's driver
//!   has already abandoned the turn, which the next start re-drives.

use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use a2a::{A2AError, Message, PartContent};
use basis::{ApprovalAnswer, ApprovalDecision, ApprovalRequest, Approver, async_trait};
use basis_tasks::PromptHost;
use serde_json::Value;
use tokio::sync::{oneshot, watch};

/// The approval extension's URI, as the card declares it and a question's
/// data part is keyed by.
pub(crate) const APPROVAL_EXTENSION: &str =
    "https://github.com/oops-rs/basis/blob/main/docs/a2a/approval-v1.md";

/// The longest preview of a call's input a question carries.
const PREVIEW_BYTES: usize = 2 * 1024;

/// A question a task has put to its caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Asked {
    pub(crate) call_id: String,
    pub(crate) tool: String,
    pub(crate) preview: String,
}

/// One task's question, if it has one open.
pub(crate) struct Questions {
    /// Held for the life of a question: the one-at-a-time rule.
    asking: tokio::sync::Mutex<()>,
    open: Mutex<Open>,
    asked: watch::Sender<Option<Asked>>,
}

/// The open question, and whether questions are still being taken — one
/// lock, so a question cannot open after a cancel has looked for it.
#[derive(Default)]
struct Open {
    question: Option<(String, oneshot::Sender<ApprovalAnswer>)>,
    /// Once set, every question is answered with this at once: the task is
    /// being canceled, or the server is going away.
    closed: Option<ApprovalAnswer>,
}

/// Why an answer was refused.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Unanswered {
    /// No question is open. It stays that way.
    NotAsking,
    /// Another question is open. It stays open.
    Mismatch { open: String },
    /// The question had closed — its deadline passed — as the answer
    /// arrived, so the answer was not taken.
    Closed,
}

impl Questions {
    pub(crate) fn new() -> Self {
        Self {
            asking: tokio::sync::Mutex::new(()),
            open: Mutex::new(Open::default()),
            asked: watch::channel(None).0,
        }
    }

    fn open(&self) -> MutexGuard<'_, Open> {
        self.open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The question open now, if any.
    pub(crate) fn asked(&self) -> Option<Asked> {
        self.asked.borrow().clone()
    }

    /// Follows the open question as it is asked and answered.
    pub(crate) fn watch(&self) -> watch::Receiver<Option<Asked>> {
        self.asked.subscribe()
    }

    /// Puts `request` to the caller and waits, at most `deadline`, for the
    /// answer.
    pub(crate) async fn ask(
        &self,
        request: &ApprovalRequest,
        deadline: Duration,
    ) -> ApprovalAnswer {
        if let Some(answer) = self.open().closed.clone() {
            return answer;
        }
        let _turn = self.asking.lock().await;
        let call_id = uuid::Uuid::new_v4().simple().to_string();
        let (sender, mut receiver) = oneshot::channel();
        {
            // Checked under the lock `cancel` and `interrupt` take, so one
            // landing now either finds this question open or keeps it from
            // opening — never misses it.
            let mut open = self.open();
            if let Some(answer) = open.closed.clone() {
                return answer;
            }
            open.question = Some((call_id.clone(), sender));
        }
        self.asked.send_replace(Some(Asked {
            call_id: call_id.clone(),
            tool: request.tool_name.clone(),
            preview: preview(request),
        }));
        let answer = match tokio::time::timeout(deadline, &mut receiver).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => ApprovalAnswer::new(ApprovalDecision::Deny)
                .because("the question was withdrawn before the caller answered it"),
            // Closing the receiver settles the race with an answer arriving
            // right now: one sent before this is taken, and one after is
            // refused to whoever sent it, who is told it came too late.
            Err(_) => {
                receiver.close();
                receiver.try_recv().unwrap_or_else(|_| {
                    ApprovalAnswer::new(ApprovalDecision::Deny).because(format!(
                        "the caller did not answer within {}s, so the call was not made",
                        deadline.as_secs()
                    ))
                })
            }
        };
        self.close(&call_id);
        answer
    }

    /// Answers the open question, when `call_id` is the one it was asked
    /// under.
    ///
    /// [`Unanswered::Closed`] when the question's deadline closed it as this
    /// arrived: the answer was not taken, and the caller must not be told it
    /// was.
    pub(crate) fn answer(&self, call_id: &str, approve: bool) -> Result<(), Unanswered> {
        let mut open = self.open();
        let Some((open_id, _)) = open.question.as_ref() else {
            return Err(Unanswered::NotAsking);
        };
        if !open_id.eq_ignore_ascii_case(call_id.trim()) {
            return Err(Unanswered::Mismatch {
                open: open_id.clone(),
            });
        }
        let (open_id, sender) = open.question.take().expect("checked above");
        drop(open);
        self.clear(&open_id);
        let answer = if approve {
            ApprovalAnswer::new(ApprovalDecision::Allow)
        } else {
            ApprovalAnswer::new(ApprovalDecision::Deny).because("the caller denied this call")
        };
        sender.send(answer).map_err(|_| Unanswered::Closed)
    }

    /// Denies the open question and every later one: the task is being
    /// canceled, and its turn must end for the cancel to land.
    pub(crate) fn cancel(&self) {
        self.close_all(canceled());
    }

    /// Resolves the open question and every later one as interrupted: the
    /// server is going away, and its driver has abandoned the turn. Nothing
    /// is made of the answer — the turn it would have gone to is gone — so
    /// all it does is let whatever was waiting on the question return.
    pub(crate) fn interrupt(&self) {
        self.close_all(ApprovalAnswer::new(ApprovalDecision::Deny).because(
            "the server stopped while this question was open; the call was not made, and it \
             will be asked again when the task resumes",
        ));
    }

    /// Answers the open question, and every later one, with `answer` — or
    /// with whatever an earlier call here already settled on.
    fn close_all(&self, answer: ApprovalAnswer) {
        let mut open = self.open();
        let answer = open.closed.get_or_insert(answer).clone();
        let question = open.question.take();
        drop(open);
        if let Some((call_id, sender)) = question {
            self.clear(&call_id);
            let _ = sender.send(answer);
        }
    }

    /// Closes `call_id`'s question if it is still the open one.
    fn close(&self, call_id: &str) {
        let mut open = self.open();
        if open
            .question
            .as_ref()
            .is_some_and(|(open_id, _)| open_id == call_id)
        {
            open.question = None;
        }
        drop(open);
        self.clear(call_id);
    }

    fn clear(&self, call_id: &str) {
        self.asked.send_if_modified(|asked| {
            let current = asked.as_ref().is_some_and(|asked| asked.call_id == call_id);
            if current {
                *asked = None;
            }
            current
        });
    }
}

fn canceled() -> ApprovalAnswer {
    ApprovalAnswer::new(ApprovalDecision::Deny).because("the task is being canceled")
}

/// What the caller is shown of a call: why it is asked, and its input,
/// bounded.
fn preview(request: &ApprovalRequest) -> String {
    let input = match &request.input {
        Value::Null => String::new(),
        input => serde_json::to_string_pretty(input).unwrap_or_default(),
    };
    let preview = [request.description.trim(), input.trim()]
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    if preview.len() <= PREVIEW_BYTES {
        return preview;
    }
    let mut end = PREVIEW_BYTES;
    while !preview.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &preview[..end])
}

/// The host a task's driver asks through: every question goes to that
/// task's [`Questions`].
pub(crate) struct CallerHost {
    pub(crate) questions: Arc<Questions>,
    pub(crate) deadline: Duration,
}

impl PromptHost for CallerHost {
    fn can_ask(&self) -> bool {
        true
    }

    fn approver(&self) -> Box<dyn Approver> {
        Box::new(CallerApprover {
            questions: Arc::clone(&self.questions),
            deadline: self.deadline,
        })
    }
}

struct CallerApprover {
    questions: Arc<Questions>,
    deadline: Duration,
}

#[async_trait]
impl Approver for CallerApprover {
    async fn approve(&mut self, request: &ApprovalRequest) -> ApprovalAnswer {
        self.questions.ask(request, self.deadline).await
    }
}

/// The host the service's own `Tasks` carries, so that a task recorded with
/// `Approve::Prompt` may be spawned at all. It never answers anything: each
/// task is driven through a [`CallerHost`] of its own, and a process that
/// reached this one would have nobody's question to ask.
pub(crate) struct NobodyHost;

impl PromptHost for NobodyHost {
    fn can_ask(&self) -> bool {
        true
    }

    fn approver(&self) -> Box<dyn Approver> {
        Box::new(basis::DenyAll)
    }
}

/// The answer a message gives: a call id and whether it is approved. Text
/// `approve <call id>` / `deny <call id>` (case-insensitive, surrounding
/// whitespace ignored), or a data part `{callId, decision}`, bare or under
/// the extension's URI.
pub(crate) fn answer_of(message: &Message) -> Result<(String, bool), A2AError> {
    for part in &message.parts {
        let found = match &part.content {
            PartContent::Text(text) => text_answer(text),
            PartContent::Data(value) => data_answer(value)?,
            _ => None,
        };
        if let Some(answer) = found {
            return Ok(answer);
        }
    }
    Err(A2AError::invalid_params(
        "this task is waiting for an answer: reply `approve <call id>` or `deny <call id>`, or \
         send a data part {\"callId\", \"decision\"}",
    ))
}

fn text_answer(text: &str) -> Option<(String, bool)> {
    let mut words = text.split_whitespace();
    let decision = words.next()?.to_ascii_lowercase();
    let call_id = words.next()?;
    if words.next().is_some() {
        return None;
    }
    decision_of(&decision).map(|approve| (call_id.to_string(), approve))
}

fn data_answer(value: &Value) -> Result<Option<(String, bool)>, A2AError> {
    let value = value.get(APPROVAL_EXTENSION).unwrap_or(value);
    let Some(call_id) = value.get("callId").and_then(Value::as_str) else {
        return Ok(None);
    };
    let decision = value
        .get("decision")
        .and_then(Value::as_str)
        .and_then(|decision| decision_of(&decision.trim().to_ascii_lowercase()))
        .ok_or_else(|| {
            A2AError::invalid_params("an answer needs a decision: \"approve\" or \"deny\"")
        })?;
    Ok(Some((call_id.to_string(), decision)))
}

fn decision_of(word: &str) -> Option<bool> {
    match word {
        "approve" => Some(true),
        "deny" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
