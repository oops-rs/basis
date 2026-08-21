//! Many runs, one stream.
//!
//! ADR-0010 makes orchestration the host's own Rust: a fan-out is
//! [`tokio::join!`] over runs minted from one [`Workspace`](crate::Workspace),
//! not a DSL basis interprets. What that leaves open is the other direction. Each
//! run wants an [`EventSink`] of its own, and the host wants *one* view of all
//! of them — a single progress pane, a single log — without losing which run
//! said what.
//!
//! [`EventFanIn`] mints one [`TaggedSink`] per run and hands back the single
//! [`MergedEvents`] they all feed:
//!
//! ```no_run
//! # async fn example() -> Result<(), basis::RunError> {
//! use basis::{EventFanIn, Workspace};
//!
//! let workspace = Workspace::open("/repo").await?;
//! let mut survey = workspace.prepare("what does this repo do?")?;
//! let mut coverage = workspace.prepare("what is not tested?")?;
//!
//! let fan = EventFanIn::new();
//! let (first, second) = (fan.sink("survey"), fan.sink("coverage"));
//! let mut merged = fan.into_events();
//!
//! let runs = async move {
//!     let (survey, coverage) = tokio::join!(survey.execute(first), coverage.execute(second));
//!     // Taking the answers out of the reports drops the sinks with them,
//!     // which is what tells `merged` the stream is over.
//!     Ok::<_, basis::RunError>((survey?.final_message, coverage?.final_message))
//! };
//! let watch = async {
//!     while let Some(tagged) = merged.recv().await {
//!         println!("[{}] {:?}", tagged.tag, tagged.event);
//!     }
//! };
//!
//! let (answers, ()) = tokio::join!(runs, watch);
//! # let _ = answers?;
//! # Ok(())
//! # }
//! ```
//!
//! The tag is whatever the host finds useful — a name, an index, an enum. It is
//! chosen at the call that mints the sink, because only the host knows what
//! distinguishes its runs.
//!
//! # The tag never reaches the wire
//!
//! [`Event`] is a versioned contract ([`EVENT_SCHEMA_VERSION`]), and a consumer
//! that reads the schema number is entitled to the shape that number promises.
//! So the tag rides *outside* the event, in a [`TaggedEvent`] that exists only
//! in this process. A host writing merged runs to one file is inventing a line
//! format of its own and should version it as its own, rather than putting an
//! extra key on basis's.
//!
//! # Ordering
//!
//! Within one run, order is exact: a run's events arrive in the order its
//! forwarding task emitted them, because they travel down one queue in one
//! sequence.
//!
//! Across runs, order is *arrival* order and nothing more — which of two
//! concurrent agents got its delta into the queue first. It is a record of what
//! this process observed, not a claim about which thing happened first, and
//! nothing correlates two runs' clocks. Read a merged stream as N interleaved
//! transcripts, and group by tag before drawing conclusions from adjacency.
//!
//! # Why a channel, and why unbounded
//!
//! The consumer is a receiver rather than a callback or a shared
//! [`EventSink`] behind a lock, because those two would run the host's code
//! *on the run's forwarding task* — the task that also answers permission
//! requests while mentra blocks the turn on them (see `forward_events`). A
//! consumer that paused to repaint would pause the agent. A queue is what
//! decouples the two, and it is also what lets one consumer serve runs on
//! other threads.
//!
//! The queue is unbounded because [`EventSink::emit`] is synchronous and runs
//! on that same task, so a bounded queue could push back in exactly two ways:
//! block the task — turning a slow progress bar into a hung agent — or fail
//! the send, which the forwarder reads as "this sink is gone" and answers by
//! muting the run's narration for the rest of the turn. Neither is a price a
//! consumer's slowness should be able to charge the run.
//!
//! What that costs is memory: a consumer that never reads holds a run's whole
//! transcript in the queue. That is the same ceiling [`CollectingSink`] has by
//! design, on a stream whose upstream — mentra's session broadcast — drops
//! rather than grows, and says so with an [`Event::Notice`] when it does.
//!
//! [`EVENT_SCHEMA_VERSION`]: crate::event::EVENT_SCHEMA_VERSION
//! [`CollectingSink`]: super::CollectingSink

use std::io;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use super::EventSink;
use crate::event::Event;

/// An event and the run it came from.
///
/// The tag is the host's own label, cloned onto each event as it passes. It
/// lives here rather than on [`Event`] so the wire schema stays exactly what
/// its version says it is.
#[derive(Debug, Clone, PartialEq)]
pub struct TaggedEvent<T> {
    pub tag: T,
    pub event: Event,
}

/// Mints the tagged sinks that feed one merged stream.
///
/// The order of operations is the API: mint every sink, then call
/// [`into_events`](Self::into_events) to start consuming. That is not
/// ceremony — consuming takes the fan-in by value, so the sender it holds goes
/// with it, and the merged stream can end when the last *sink* is dropped
/// without depending on the host remembering to drop the factory too.
pub struct EventFanIn<T> {
    sender: UnboundedSender<TaggedEvent<T>>,
    receiver: UnboundedReceiver<TaggedEvent<T>>,
}

/// Hand-written so the type prints as what it is rather than requiring a
/// `T: Debug` a tag has no reason to carry.
impl<T> std::fmt::Debug for EventFanIn<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventFanIn").finish_non_exhaustive()
    }
}

impl<T> Default for EventFanIn<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> EventFanIn<T> {
    pub fn new() -> Self {
        let (sender, receiver) = unbounded_channel();

        Self { sender, receiver }
    }

    /// Mints a sink that labels everything it carries with `tag`.
    ///
    /// Sinks are `Send`, so this is the handle that goes to the run —
    /// [`PreparedRun::execute`](crate::PreparedRun::execute) and its
    /// neighbours take one by value.
    pub fn sink(&self, tag: T) -> TaggedSink<T> {
        TaggedSink {
            tag,
            sender: self.sender.clone(),
        }
    }

    /// Starts consuming, closing minting.
    ///
    /// Taking `self` is the point: the fan-in's own sender is dropped here, so
    /// from this call on the only senders alive are the sinks that were minted,
    /// and "every sink is gone" is a state the consumer can actually reach.
    pub fn into_events(self) -> MergedEvents<T> {
        MergedEvents {
            receiver: self.receiver,
        }
    }
}

/// One run's end of a fan-in: an [`EventSink`] that labels each event with its
/// tag and hands it to the shared consumer.
///
/// Failure is the ordinary sink failure — an [`io::ErrorKind::BrokenPipe`] once
/// the consumer is gone — which the run's forwarder already knows what to do
/// with: it stops narrating and keeps answering approvals, so a host that
/// dropped its progress UI mid-run loses the commentary and not the run.
pub struct TaggedSink<T> {
    tag: T,
    sender: UnboundedSender<TaggedEvent<T>>,
}

/// Hand-written for the same reason [`EventFanIn`]'s is: a tag need not be
/// `Debug` for a sink to be printable.
impl<T> std::fmt::Debug for TaggedSink<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaggedSink").finish_non_exhaustive()
    }
}

impl<T> TaggedSink<T> {
    /// The label this sink stamps on every event.
    ///
    /// A run hands its sink back in [`RunReport`](crate::RunReport), and this
    /// is what makes that report say which run it belongs to.
    pub fn tag(&self) -> &T {
        &self.tag
    }
}

impl<T: Clone + Send + 'static> EventSink for TaggedSink<T> {
    fn emit(&mut self, event: Event) -> io::Result<()> {
        self.sender
            .send(TaggedEvent {
                tag: self.tag.clone(),
                event,
            })
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "the merged event stream was dropped",
                )
            })
    }
}

/// The host's end of a fan-in: every tagged sink's events, in arrival order.
///
/// # When the stream ends
///
/// [`recv`](Self::recv) returns `None` once every sink minted from the fan-in
/// has been dropped, and not before.
///
/// The sharp edge is that a run *gives its sink back*: a finished run yields a
/// [`RunReport`](crate::RunReport) with the sink in it, so a report held is a
/// branch of the stream held open, even though that run has nothing left to
/// say. A host that awaits its runs and its consumer in one
/// [`tokio::join!`] must let the reports go inside the branch that produced
/// them — take the final message, the outcome, whatever it wanted — or the
/// join will wait on a stream that is waiting on the join.
///
/// The other shape has no edge at all: fan out, join the runs, and then
/// [`drain`](Self::drain) what arrived. Nothing was lost while nobody was
/// reading, because the queue is unbounded.
pub struct MergedEvents<T> {
    receiver: UnboundedReceiver<TaggedEvent<T>>,
}

/// Hand-written to match [`EventFanIn`]'s: printable without a `T: Debug`.
impl<T> std::fmt::Debug for MergedEvents<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MergedEvents").finish_non_exhaustive()
    }
}

impl<T> MergedEvents<T> {
    /// The next event from any run, or `None` once every sink is gone.
    ///
    /// Cancel-safe, so this composes with [`tokio::select!`] as well as with a
    /// plain `while let` loop.
    pub async fn recv(&mut self) -> Option<TaggedEvent<T>> {
        self.receiver.recv().await
    }

    /// [`recv`](Self::recv) for a consumer that is not on an async task — a
    /// dedicated thread driving a terminal UI, or a test.
    ///
    /// Panics if called from async code, which is tokio's rule for blocking on
    /// a channel and the reason [`recv`](Self::recv) exists.
    pub fn blocking_recv(&mut self) -> Option<TaggedEvent<T>> {
        self.receiver.blocking_recv()
    }

    /// Everything that has already arrived, without waiting for more.
    ///
    /// What a host reads after joining its runs, and what a UI that repaints on
    /// its own schedule reads each frame. An empty result means nothing has
    /// arrived *yet* — it does not mean the stream is over, which only
    /// [`recv`](Self::recv) can say.
    pub fn drain(&mut self) -> Vec<TaggedEvent<T>> {
        let mut arrived = Vec::new();
        while let Ok(tagged) = self.receiver.try_recv() {
            arrived.push(tagged);
        }

        arrived
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::event::RunOutcome;

    fn delta(text: &str) -> Event {
        Event::AssistantDelta {
            text: text.to_string(),
        }
    }

    #[test]
    fn tags_say_which_run_an_event_came_from() {
        let fan = EventFanIn::new();
        let mut planner = fan.sink("planner");
        let mut reviewer = fan.sink("reviewer");
        let mut merged = fan.into_events();

        planner.emit(delta("plan")).expect("emits");
        reviewer.emit(delta("review")).expect("emits");
        planner.emit(delta("more plan")).expect("emits");

        let arrived: Vec<_> = merged
            .drain()
            .into_iter()
            .map(|tagged| (tagged.tag, tagged.event))
            .collect();

        assert_eq!(
            arrived,
            vec![
                ("planner", delta("plan")),
                ("reviewer", delta("review")),
                ("planner", delta("more plan")),
            ]
        );
        assert_eq!(planner.tag(), &"planner");
    }

    #[test]
    fn a_sink_travels_to_another_thread_and_the_consumer_needs_no_runtime() {
        let fan = EventFanIn::new();
        let mut sink = fan.sink("worker");
        let mut merged = fan.into_events();

        let run = std::thread::spawn(move || {
            sink.emit(delta("hello")).expect("emits");
            sink.emit(Event::RunFinished {
                outcome: RunOutcome::Ok,
                stopped_by: None,
                usage: None,
            })
            .expect("emits");
        });

        // Ends on its own: the thread drops the sink when it returns.
        let mut seen = Vec::new();
        while let Some(tagged) = merged.blocking_recv() {
            seen.push(tagged);
        }
        run.join().expect("the emitting thread finishes");

        assert_eq!(seen.len(), 2);
        assert!(seen.iter().all(|tagged| tagged.tag == "worker"));
        assert_eq!(seen[0].event, delta("hello"));
    }

    #[test]
    fn a_fan_in_nobody_minted_from_is_a_stream_that_is_already_over() {
        // `into_events` takes the fan-in by value and its sender goes too, so
        // the factory cannot be the thing holding a consumer open.
        let mut merged = EventFanIn::<&str>::new().into_events();

        assert!(merged.blocking_recv().is_none());
    }

    #[tokio::test]
    async fn the_stream_ends_when_the_last_sink_is_dropped() {
        let fan = EventFanIn::new();
        let first = fan.sink("first");
        let second = fan.sink("second");
        let mut merged = fan.into_events();

        drop(first);

        // One run finishing is not the fan-out finishing. Nothing can complete
        // this `recv` — no sender will send — so the timeout is a bound on the
        // test, not a race.
        assert!(
            tokio::time::timeout(Duration::from_millis(50), merged.recv())
                .await
                .is_err(),
            "a live sink keeps the stream open"
        );

        drop(second);

        assert!(
            merged.recv().await.is_none(),
            "the last sink going closes the stream"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn each_run_keeps_its_own_order_under_concurrent_emission() {
        const RUNS: usize = 4;
        const EVENTS: usize = 250;

        let fan = EventFanIn::new();
        let sinks: Vec<_> = (0..RUNS).map(|run| fan.sink(run)).collect();
        let mut merged = fan.into_events();

        let emitters: Vec<_> = sinks
            .into_iter()
            .map(|mut sink| {
                tokio::spawn(async move {
                    for step in 0..EVENTS {
                        sink.emit(delta(&step.to_string())).expect("emits");
                        // Hands the other runs a chance to interleave, which is
                        // the condition this test is about.
                        tokio::task::yield_now().await;
                    }
                })
            })
            .collect();

        let mut seen: Vec<Vec<String>> = vec![Vec::new(); RUNS];
        while let Some(tagged) = merged.recv().await {
            let Event::AssistantDelta { text } = tagged.event else {
                panic!("only deltas were emitted");
            };
            seen[tagged.tag].push(text);
        }
        for emitter in emitters {
            emitter.await.expect("emitter finishes");
        }

        let expected: Vec<String> = (0..EVENTS).map(|step| step.to_string()).collect();
        for (run, texts) in seen.iter().enumerate() {
            assert_eq!(texts, &expected, "run {run} arrived out of order");
        }
    }

    #[test]
    fn a_consumer_that_never_reads_does_not_stall_the_run_feeding_it() {
        const EVENTS: usize = 10_000;

        let fan = EventFanIn::new();
        let mut sink = fan.sink("chatty");
        let mut merged = fan.into_events();

        // Under a bounded queue this loop is where the run would stop and wait
        // on a consumer that has not read one event — and with the forwarding
        // task stopped, so would the approvals the turn is blocked on.
        for step in 0..EVENTS {
            sink.emit(delta(&step.to_string())).expect("emits");
        }
        drop(sink);

        assert_eq!(
            merged.drain().len(),
            EVENTS,
            "nothing is lost while nobody is reading"
        );
    }

    #[test]
    fn a_dropped_consumer_fails_the_sink_rather_than_blocking_it() {
        let fan = EventFanIn::new();
        let mut sink = fan.sink("orphan");
        drop(fan.into_events());

        let error = sink
            .emit(delta("nobody is listening"))
            .expect_err("the consumer is gone");

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        // Which is the failure the forwarder already handles: it mutes this
        // sink and carries on answering approvals. Emitting again must stay a
        // cheap error rather than becoming a panic or a wait.
        assert!(sink.emit(delta("still nobody")).is_err());
    }
}
