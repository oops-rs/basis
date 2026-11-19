//! One fresh ECMAScript context. The only application functions it receives
//! are named calls supplied by its owner. A call yields back to that owner so
//! it can borrow the live turn across an await. No tool executor lives here.

use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use mentra::tool::ToolResultContent;
use rquickjs::{
    AsyncContext, AsyncRuntime, CatchResultExt, Ctx, Exception, Function, IntoJs, Object, Promise,
    Value as JsValue,
};
use serde_json::Value;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};

type Result<T> = std::result::Result<T, String>;

mod binding;
pub(crate) use binding::CodeTool;

const HEAP_BYTES: usize = 32 * 1024 * 1024;
const SOURCE_BYTES: usize = 256 * 1024;
const JSON_BYTES: usize = 1024 * 1024;
// Pending promises hold argument copies in Rust, outside the JS heap limit.
const MAX_PENDING_CALLS: usize = 16;
// The script's own time: running JavaScript, or idle with no tool call
// outstanding. A tool call is bounded by what bounds it when called directly.
const SCRIPT_TIME: Duration = Duration::from_secs(120);

/// Counts the time a script spends with no tool call outstanding. Time inside
/// a call belongs to that call and to the run's limits, so a slow tool cannot
/// expire the script and expiry never drops a call in flight.
struct Clock {
    budget: Duration,
    state: Mutex<ClockState>,
    changed: Notify,
}

struct ClockState {
    left: Duration,
    outstanding: usize,
    resumed: Instant,
}

impl Clock {
    fn new(budget: Duration) -> Arc<Self> {
        Arc::new(Self {
            budget,
            state: Mutex::new(ClockState {
                left: budget,
                outstanding: 0,
                resumed: Instant::now(),
            }),
            changed: Notify::new(),
        })
    }

    fn pause(&self) {
        let mut state = self.state.lock().expect("script clock poisoned");
        if state.outstanding == 0 {
            state.left = state.left.saturating_sub(state.resumed.elapsed());
        }
        state.outstanding += 1;
        drop(state);
        self.changed.notify_one();
    }

    fn resume(&self) {
        let mut state = self.state.lock().expect("script clock poisoned");
        state.outstanding -= 1;
        if state.outstanding == 0 {
            state.resumed = Instant::now();
        }
        drop(state);
        self.changed.notify_one();
    }

    /// When the running script expires, or `None` while a call is outstanding.
    fn expiry(&self) -> Option<Instant> {
        let state = self.state.lock().expect("script clock poisoned");
        (state.outstanding == 0).then(|| state.resumed + state.left)
    }

    fn expired(&self) -> bool {
        self.expiry().is_some_and(|at| Instant::now() >= at)
    }

    async fn wait_expired(&self) {
        loop {
            match self.expiry() {
                Some(at) => tokio::select! {
                    () = tokio::time::sleep_until(at.into()) => {}
                    () = self.changed.notified() => {}
                },
                None => self.changed.notified().await,
            }
            if self.expired() {
                return;
            }
        }
    }

    fn timed_out(&self) -> String {
        format!(
            "JavaScript evaluation timed out after {:?} outside tool calls",
            self.budget
        )
    }
}

/// One outstanding tool call: a pending slot, and the script clock held still.
struct Outstanding {
    clock: Arc<Clock>,
    _slot: OwnedSemaphorePermit,
}

impl Outstanding {
    fn new(clock: Arc<Clock>, slot: OwnedSemaphorePermit) -> Self {
        clock.pause();
        Self { clock, _slot: slot }
    }
}

impl Drop for Outstanding {
    fn drop(&mut self) {
        self.clock.resume();
    }
}

struct Call {
    name: String,
    arguments: Value,
    reply: oneshot::Sender<Result<ToolResultContent>>,
}

/// Runs on a blocking worker because pure JS can spin without yielding.
/// Only the owning async task drives tool calls. Dropping that task drops its
/// dispatch future and closes the stop channel for both busy and idle JS.
async fn evaluate<F: Future<Output = ()>>(
    source: String,
    names: Vec<String>,
    drive: impl FnOnce(mpsc::Receiver<Call>) -> F,
) -> Result<Value> {
    evaluate_within(source, names, drive, SCRIPT_TIME).await
}

async fn evaluate_within<F: Future<Output = ()>>(
    source: String,
    names: Vec<String>,
    drive: impl FnOnce(mpsc::Receiver<Call>) -> F,
    script_time: Duration,
) -> Result<Value> {
    if source.len() > SOURCE_BYTES {
        return Err(format!("code source exceeds {SOURCE_BYTES} bytes"));
    }

    // Dropped with this future, which is how a stopped turn stops the worker.
    let (_keep_running, stop) = watch::channel(());
    let (sender, calls) = mpsc::channel::<Call>(1);
    let handle = tokio::runtime::Handle::current();
    let mut worker = tokio::task::spawn_blocking(move || {
        handle.block_on(run(source, names, sender, stop, Clock::new(script_time)))
    });
    tokio::select! {
        result = &mut worker => result,
        () = drive(calls) => (&mut worker).await,
    }
    .map_err(|error| format!("JavaScript worker: {error}"))?
}

async fn run(
    source: String,
    names: Vec<String>,
    sender: mpsc::Sender<Call>,
    mut stop: watch::Receiver<()>,
    clock: Arc<Clock>,
) -> Result<Value> {
    let runtime = AsyncRuntime::new().map_err(|error| error.to_string())?;
    runtime.set_memory_limit(HEAP_BYTES).await;
    let interrupt = stop.clone();
    let running = clock.clone();
    runtime
        .set_interrupt_handler(Some(Box::new(move || {
            interrupt.has_changed().is_err() || running.expired()
        })))
        .await;
    let calls = clock.clone();
    let context = AsyncContext::full(&runtime)
        .await
        .map_err(|error| error.to_string())?;
    let evaluation = context.async_with(async move |ctx| {
        async {
            let tools = Object::new_proto(ctx.clone(), None)?;
            let slots = Arc::new(Semaphore::new(MAX_PENDING_CALLS));
            for name in names {
                if name != "code" {
                    tools.set(
                        name.as_str(),
                        tool(
                            &ctx,
                            name.clone(),
                            sender.clone(),
                            slots.clone(),
                            calls.clone(),
                        )?,
                    )?;
                }
            }

            // Passing the body as data to AsyncFunction lets the JS parser
            // enforce function-body syntax even when it contains braces.
            let start: Function = ctx.eval(
                r#"(body, tools) =>
                Object.getPrototypeOf(async () => {}).constructor(
                    "tools", '"use strict";\n' + body
                )(Object.freeze(tools))"#,
            )?;
            let promise: Promise = start.call((source, tools))?;
            let value: JsValue = promise.into_future().await?;
            if slots.available_permits() != MAX_PENDING_CALLS {
                return Err(Exception::throw_message(
                    &ctx,
                    "await pending tool calls before returning",
                ));
            }
            if value.is_undefined() {
                return Ok(Value::Null);
            }
            json_value(&ctx, value)
        }
        .await
        .catch(&ctx)
        .map_err(|error| {
            let mut message = error.to_string();
            if message.len() > JSON_BYTES {
                let boundary = message.floor_char_boundary(JSON_BYTES - 3);
                message.truncate(boundary);
                message.push_str("...");
            }
            message
        })
    });

    tokio::select! {
        // An interrupted busy script fails here with the engine's own error.
        result = evaluation => result.map_err(|error| {
            if clock.expired() { clock.timed_out() } else { error }
        }),
        () = clock.wait_expired() => Err(clock.timed_out()),
        _ = stop.changed() => Err("JavaScript evaluation stopped".into()),
    }
}

fn tool<'js>(
    ctx: &Ctx<'js>,
    name: String,
    sender: mpsc::Sender<Call>,
    slots: Arc<Semaphore>,
    clock: Arc<Clock>,
) -> rquickjs::Result<Function<'js>> {
    Function::new(ctx.clone(), move |ctx: Ctx<'js>, args: JsValue<'js>| {
        let slot = slots.clone().try_acquire_owned().map_err(|_| {
            Exception::throw_range(&ctx, "too many pending tool calls; await earlier calls")
        })?;
        // Snapshot at the point of invocation. JS can mutate the same object
        // immediately after receiving the promise.
        let arguments = json_value(&ctx, args)?;
        if !arguments.is_object() {
            return Err(Exception::throw_type(
                &ctx,
                "tool arguments must be a JSON object",
            ));
        }
        let name = name.clone();
        let sender = sender.clone();
        let outstanding = Outstanding::new(clock.clone(), slot);
        Promise::wrap_future(&ctx.clone(), async move {
            let _outstanding = outstanding;
            let (reply, result) = oneshot::channel();
            sender
                .send(Call {
                    name,
                    arguments,
                    reply,
                })
                .await
                .map_err(|_| Exception::throw_message(&ctx, "turn stopped"))?;
            let value = result
                .await
                .map_err(|_| Exception::throw_message(&ctx, "turn stopped"))?
                .map_err(|reason| Exception::throw_message(&ctx, &reason))?;
            if value.len() > HEAP_BYTES {
                return Err(Exception::throw_range(
                    &ctx,
                    "tool result exceeds JavaScript heap limit",
                ));
            }
            match value {
                ToolResultContent::Text(text) => text.into_js(&ctx),
                ToolResultContent::Structured(json) => ctx.json_parse(json.to_string()),
            }
        })
    })
}

fn json_value<'js>(ctx: &Ctx<'js>, value: JsValue<'js>) -> rquickjs::Result<Value> {
    let encoded = ctx
        .json_stringify(value)?
        .ok_or_else(|| Exception::throw_type(ctx, "expected a JSON value"))?
        .to_string()?;
    if encoded.len() > JSON_BYTES {
        return Err(Exception::throw_range(
            ctx,
            "JavaScript JSON value exceeds 1 MiB",
        ));
    }
    serde_json::from_str(&encoded).map_err(|error| Exception::throw_type(ctx, &error.to_string()))
}

#[cfg(test)]
mod tests;
