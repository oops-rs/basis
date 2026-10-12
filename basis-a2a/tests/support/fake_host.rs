//! An in-process [`TaskHost`] a test drives by hand: it records every call
//! the server makes, and a task moves only when the test moves it — or, for
//! a host built [`answering`](FakeHost::answering), completes as soon as it
//! starts.
//!
//! Like the real thing it keeps one executor per conversation: a start that
//! continues a task still running is refused as busy. A task the test
//! [`ask`](FakeHost::ask)s on is `InputRequired` until a reply resumes it;
//! a reply containing [`REFUSED`] is refused and leaves it asking.
//! [`NoReplies`] wraps one with `TaskHost::reply` left at its default.
//!
//! Included by path from each test file, so not every helper is used by
//! every one of them.
#![allow(dead_code)]

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use basis_a2a::{
    HostError, HostedProgress, HostedReply, HostedStanding, HostedStart, TaskHost, a2a::Part,
};
use futures::{
    FutureExt, StreamExt,
    future::BoxFuture,
    stream::{self, BoxStream},
};
use tokio::sync::{broadcast, watch};

/// A prompt containing this is refused as the caller's fault.
pub const REFUSED: &str = "refuse me";

/// What the host has been asked to do, in order.
#[derive(Debug, Default, Clone)]
pub struct Calls {
    pub starts: Vec<HostedStart>,
    pub cancels: Vec<String>,
    pub settled: Vec<String>,
    pub progress: Vec<String>,
    /// Each reply handed to a task, with the task's host id.
    pub replies: Vec<(String, HostedReply)>,
}

struct Run {
    standing: watch::Sender<HostedStanding>,
    /// Dropped when the task settles, or when the test ends its notes.
    notes: Option<broadcast::Sender<HostedProgress>>,
}

#[derive(Default)]
struct State {
    runs: HashMap<String, Run>,
    calls: Calls,
}

pub struct FakeHost {
    /// When set, every task completes with these parts as it starts.
    answer: Option<Vec<Part>>,
    state: Mutex<State>,
}

impl FakeHost {
    /// A host whose tasks move only when the test moves them.
    pub fn held() -> Arc<Self> {
        Arc::new(Self {
            answer: None,
            state: Mutex::default(),
        })
    }

    /// A host whose tasks complete with `parts` as soon as they start.
    pub fn answering(parts: Vec<Part>) -> Arc<Self> {
        Arc::new(Self {
            answer: Some(parts),
            state: Mutex::default(),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    pub fn calls(&self) -> Calls {
        self.state().calls.clone()
    }

    /// The host id of the `n`th task started (from 0), once it has been.
    pub async fn started(&self, n: usize) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if self.calls().starts.len() > n {
                return format!("run-{n}");
            }
            assert!(Instant::now() < deadline, "task {n} was never started");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Waits until someone follows `id`'s progress `count` times over.
    pub async fn followed(&self, id: &str, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let following = self
                .calls()
                .progress
                .iter()
                .filter(|followed| *followed == id)
                .count();
            if following >= count {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{id}'s progress was never followed"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Waits until the server has awaited `id` settling `count` times over.
    pub async fn awaited(&self, id: &str, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let awaited = self
                .calls()
                .settled
                .iter()
                .filter(|awaited| *awaited == id)
                .count();
            if awaited >= count {
                return;
            }
            assert!(Instant::now() < deadline, "{id} was never awaited");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub fn set(&self, id: &str, standing: HostedStanding) {
        let mut state = self.state();
        let run = state.runs.get_mut(id).expect("a task this host started");
        if standing.is_terminal() {
            run.notes = None;
        }
        run.standing.send_replace(standing);
    }

    pub fn note(&self, id: &str, note: &str) {
        let state = self.state();
        let run = state.runs.get(id).expect("a task this host started");
        if let Some(notes) = &run.notes {
            let _ = notes.send(HostedProgress::new(note));
        }
    }

    /// Ends `id`'s progress notes while it is still running.
    pub fn end_notes(&self, id: &str) {
        let mut state = self.state();
        state
            .runs
            .get_mut(id)
            .expect("a task this host started")
            .notes = None;
    }

    /// Pauses `id` on a question to its caller.
    pub fn ask(&self, id: &str, parts: Vec<Part>) {
        self.set(id, HostedStanding::InputRequired { parts });
    }

    /// Waits until `id` has been replied to `count` times over.
    pub async fn replied(&self, id: &str, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let replies = self
                .calls()
                .replies
                .iter()
                .filter(|(replied, _)| replied == id)
                .count();
            if replies >= count {
                return;
            }
            assert!(Instant::now() < deadline, "{id} was never replied to");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    pub fn complete(&self, id: &str, parts: Vec<Part>) {
        self.set(id, HostedStanding::Completed { parts });
    }

    fn watch(&self, id: &str) -> Result<watch::Receiver<HostedStanding>, HostError> {
        self.state()
            .runs
            .get(id)
            .map(|run| run.standing.subscribe())
            .ok_or_else(|| HostError::Internal(format!("no task {id}")))
    }
}

impl TaskHost for FakeHost {
    fn start(&self, start: HostedStart) -> BoxFuture<'_, Result<String, HostError>> {
        async move {
            if start.prompt.contains(REFUSED) {
                return Err(HostError::Refused("this host will not do that".into()));
            }
            let mut state = self.state();
            if let Some(previous) = &start.continues {
                let running = state
                    .runs
                    .get(previous)
                    .is_some_and(|run| !run.standing.borrow().is_terminal());
                if running {
                    return Err(HostError::Busy(format!("{previous} is still working")));
                }
            }
            let id = format!("run-{}", state.calls.starts.len());
            state.calls.starts.push(start);
            let (standing, notes) = match &self.answer {
                Some(parts) => (
                    HostedStanding::Completed {
                        parts: parts.clone(),
                    },
                    None,
                ),
                None => (HostedStanding::Working, Some(broadcast::channel(64).0)),
            };
            state.runs.insert(
                id.clone(),
                Run {
                    standing: watch::channel(standing).0,
                    notes,
                },
            );
            Ok(id)
        }
        .boxed()
    }

    fn standing(&self, id: &str) -> BoxFuture<'_, Result<HostedStanding, HostError>> {
        let id = id.to_string();
        async move { Ok(self.watch(&id)?.borrow().clone()) }.boxed()
    }

    fn cancel(&self, id: &str) -> BoxFuture<'_, Result<(), HostError>> {
        let id = id.to_string();
        async move {
            self.state().calls.cancels.push(id.clone());
            self.watch(&id)?;
            self.set(&id, HostedStanding::Canceled);
            Ok(())
        }
        .boxed()
    }

    fn settled(&self, id: &str) -> BoxFuture<'_, Result<(), HostError>> {
        let id = id.to_string();
        async move {
            self.state().calls.settled.push(id.clone());
            let mut standing = self.watch(&id)?;
            standing
                .wait_for(HostedStanding::is_terminal)
                .await
                .map(|_| ())
                .map_err(|_| HostError::Internal("the host went away".into()))
        }
        .boxed()
    }

    fn reply(&self, id: &str, reply: HostedReply) -> BoxFuture<'_, Result<(), HostError>> {
        let id = id.to_string();
        async move {
            if reply.text.contains(REFUSED) {
                return Err(HostError::Refused("not an answer to that".into()));
            }
            let asking = matches!(
                *self.watch(&id)?.borrow(),
                HostedStanding::InputRequired { .. }
            );
            if !asking {
                return Err(HostError::Refused(format!("{id} asked nothing")));
            }
            self.state().calls.replies.push((id.clone(), reply));
            self.set(&id, HostedStanding::Working);
            Ok(())
        }
        .boxed()
    }

    fn progress(&self, id: &str) -> BoxStream<'static, HostedProgress> {
        let mut state = self.state();
        state.calls.progress.push(id.to_string());
        let Some(notes) = state.runs.get(id).and_then(|run| run.notes.as_ref()) else {
            return stream::empty().boxed();
        };
        stream::unfold(notes.subscribe(), |mut notes| async move {
            loop {
                match notes.recv().await {
                    Ok(note) => return Some((note, notes)),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        })
        .boxed()
    }
}

/// A [`FakeHost`] that leaves [`TaskHost::reply`] at its default.
pub struct NoReplies(pub Arc<FakeHost>);

impl TaskHost for NoReplies {
    fn start(&self, start: HostedStart) -> BoxFuture<'_, Result<String, HostError>> {
        self.0.start(start)
    }

    fn standing(&self, id: &str) -> BoxFuture<'_, Result<HostedStanding, HostError>> {
        self.0.standing(id)
    }

    fn cancel(&self, id: &str) -> BoxFuture<'_, Result<(), HostError>> {
        self.0.cancel(id)
    }

    fn settled(&self, id: &str) -> BoxFuture<'_, Result<(), HostError>> {
        self.0.settled(id)
    }

    fn progress(&self, id: &str) -> BoxStream<'static, HostedProgress> {
        self.0.progress(id)
    }
}
