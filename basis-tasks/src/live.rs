//! A secondary sink for a task's events, alongside the durable journal.
//!
//! Every attach writes every event to `events.jsonl` regardless — that is the
//! durable record `watch` and a later `wait` read back. Showing a run *live*,
//! on top of that, is a property of who asked for it rather than of the task
//! itself: a shell blocked on `basis wait` wants to watch, a host polling
//! `--json` wants only the settled object, and a child driven incidentally by
//! its parent's settle pass was not asked for at all. So it is supplied per
//! call, through [`DriveContext`], rather than fixed on [`Tasks`](crate::Tasks).

use std::sync::Arc;

use serde_json::Value;

use crate::approve::PromptHost;

/// Shown a task's events as they are journaled. `basis-cli`'s own terminal
/// renderer is the first implementation, but the trait carries no opinion
/// about a terminal — a host could log, forward over a socket, or update a
/// UI just as well.
pub trait LiveSink: Send + Sync {
    fn on_event(&self, event: &Value);
}

/// What a process attaching to drive a task brings to it: how to show
/// progress live, and how to answer `Approve::Prompt`. The two are
/// independent — a child driven quietly by its parent's settle pass still
/// answers prompts through the same host the parent would, it simply is not
/// shown (see [`hidden`](Self::hidden)).
#[derive(Clone, Default)]
pub(crate) struct DriveContext {
    pub(crate) live: Option<Arc<dyn LiveSink>>,
    pub(crate) prompt_host: Option<Arc<dyn PromptHost>>,
}

impl DriveContext {
    pub(crate) fn new(
        live: Option<Arc<dyn LiveSink>>,
        prompt_host: Option<Arc<dyn PromptHost>>,
    ) -> Self {
        Self { live, prompt_host }
    }

    /// This context, with progress silenced but the same say over prompts —
    /// what a child driven by its parent's settle pass gets: nobody asked to
    /// watch it, but it is still this process answering for it.
    pub(crate) fn hidden(&self) -> Self {
        Self {
            live: None,
            prompt_host: self.prompt_host.clone(),
        }
    }

    pub(crate) fn show(&self, event: &Value) {
        if let Some(live) = &self.live {
            live.on_event(event);
        }
    }

    pub(crate) fn can_ask(&self) -> bool {
        self.prompt_host.as_deref().is_some_and(PromptHost::can_ask)
    }

    pub(crate) fn approver(&self) -> Option<Box<dyn basis::Approver>> {
        self.prompt_host.as_deref().map(PromptHost::approver)
    }
}
