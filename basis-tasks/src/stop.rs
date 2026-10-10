//! A host's word that it is going away.
//!
//! A process that drives tasks on behalf of others — the A2A server is the
//! first — outlives none of them on purpose: when it shuts down, every task
//! it was driving must be left exactly as resumable as a crash would leave
//! it, and nothing it was in the middle of may be recorded as an outcome. A
//! turn cut short by a shutdown did not finish, and a turn that finished
//! after the host stopped listening answered nobody.
//!
//! [`Stop`] is that word. Given to [`Tasks::with_stop`](crate::Tasks::with_stop),
//! it is honored at once rather than at the next turn boundary: the turn in
//! flight is abandoned where it stands (its conversation keeps what it
//! committed, which the next attach continues or re-drives), no terminal
//! record is written, and every wait driving under it returns.

use std::sync::Arc;

use tokio::sync::watch;

/// A one-way switch from running to stopping, shared by every clone.
#[derive(Clone, Debug)]
pub struct Stop {
    stopping: Arc<watch::Sender<bool>>,
}

impl Default for Stop {
    fn default() -> Self {
        Self::new()
    }
}

impl Stop {
    pub fn new() -> Self {
        Self {
            stopping: Arc::new(watch::channel(false).0),
        }
    }

    /// Flips the switch. Idempotent; there is no way back.
    pub fn stop(&self) {
        self.stopping.send_replace(true);
    }

    pub fn is_stopping(&self) -> bool {
        *self.stopping.borrow()
    }

    /// Resolves once [`stop`](Self::stop) has been called.
    pub async fn stopping(&self) {
        let mut receiver = self.stopping.subscribe();
        // The sender lives as long as `self`, so this cannot fail.
        let _ = receiver.wait_for(|stopping| *stopping).await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::Stop;

    #[tokio::test]
    async fn every_clone_sees_the_stop_and_waiting_on_it_returns() {
        let stop = Stop::new();
        let clone = stop.clone();
        assert!(!clone.is_stopping());
        let waiting = tokio::spawn(async move { clone.stopping().await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!waiting.is_finished());

        stop.stop();
        stop.stop();
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("the wait returns")
            .unwrap();
        assert!(stop.is_stopping());
        stop.stopping().await;
    }
}
