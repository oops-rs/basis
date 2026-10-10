//! How many streams each principal holds open.
//!
//! Every `SendStreamingMessage` and `SubscribeToTask` keeps a poller running
//! for as long as its caller keeps the connection, so a caller that opens
//! streams and never reads them would have this server polling on its behalf
//! without bound. Each principal may hold at most its cap at once; a
//! [`StreamSlot`] is held by the stream itself and given back when the stream
//! ends or its caller goes away.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
};

use a2a::A2AError;

use crate::config::Principal;

/// How many streams one principal may hold open when the operator names no
/// other number.
pub const DEFAULT_STREAM_CAP: usize = 16;

pub(crate) struct Streams {
    cap: usize,
    open: Mutex<HashMap<String, usize>>,
}

/// One open stream's place under its principal's cap.
pub(crate) struct StreamSlot {
    streams: Arc<Streams>,
    principal: String,
}

impl Streams {
    pub(crate) fn new(cap: usize) -> Arc<Self> {
        Arc::new(Self {
            cap,
            open: Mutex::new(HashMap::new()),
        })
    }

    fn open(&self) -> MutexGuard<'_, HashMap<String, usize>> {
        self.open
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A place for one more of `caller`'s streams, or the refusal that says
    /// it has as many as it may.
    pub(crate) fn take(self: &Arc<Self>, caller: &Principal) -> Result<StreamSlot, A2AError> {
        let mut open = self.open();
        let count = open.entry(caller.name().to_string()).or_insert(0);
        if *count >= self.cap {
            return Err(A2AError::unsupported_operation(format!(
                "principal {} has {} streams open, the most this agent holds for one caller; \
                 close one, or poll GetTask",
                caller.name(),
                self.cap
            )));
        }
        *count += 1;
        Ok(StreamSlot {
            streams: Arc::clone(self),
            principal: caller.name().to_string(),
        })
    }
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        let mut open = self.streams.open();
        if let Some(count) = open.get_mut(&self.principal) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                open.remove(&self.principal);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_principal_holds_at_most_its_cap_and_a_closed_stream_gives_its_place_back() {
        let streams = Streams::new(2);
        let alice = Principal::new("alice").unwrap();
        let bob = Principal::new("bob").unwrap();
        let first = streams.take(&alice).unwrap();
        let _second = streams.take(&alice).unwrap();
        assert!(streams.take(&alice).is_err());
        assert!(streams.take(&bob).is_ok(), "the cap is per principal");
        drop(first);
        assert!(streams.take(&alice).is_ok());
    }
}
