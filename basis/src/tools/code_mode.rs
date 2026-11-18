//! One fresh ECMAScript context. The only application functions it receives
//! are named calls supplied by its owner. A call yields back to that owner so
//! it can borrow the live turn across an await. No tool executor lives here.

use std::{
    ops::ControlFlow,
    sync::Arc,
    time::{Duration, Instant},
};

use mentra::tool::ToolResultContent;
use rquickjs::{
    AsyncContext, AsyncRuntime, CatchResultExt, Ctx, Exception, Function, IntoJs, Object, Promise,
    Value as JsValue,
};
use serde_json::Value;
use tokio::sync::{Semaphore, mpsc, oneshot, watch};

type Result<T> = std::result::Result<T, String>;

const HEAP_BYTES: usize = 32 * 1024 * 1024;
const SOURCE_BYTES: usize = 256 * 1024;
const JSON_BYTES: usize = 1024 * 1024;
// Pending promises hold argument copies in Rust, outside the JS heap limit.
const MAX_PENDING_CALLS: usize = 16;
const WALL_TIME: Duration = Duration::from_secs(120);

struct Call {
    name: String,
    arguments: Value,
    reply: oneshot::Sender<Result<ToolResultContent>>,
}

/// Runs on a blocking worker because pure JS can spin without yielding.
/// Only the owning async task drives tool calls. Dropping that task drops its
/// dispatch future and closes the stop channel for both busy and idle JS.
async fn evaluate<Stop>(
    source: String,
    names: Vec<String>,
    dispatch: impl AsyncFnMut(String, Value) -> ControlFlow<Stop, Result<ToolResultContent>>,
) -> ControlFlow<Stop, Result<Value>> {
    evaluate_with_timeout(source, names, dispatch, WALL_TIME).await
}

async fn evaluate_with_timeout<Stop>(
    source: String,
    names: Vec<String>,
    mut dispatch: impl AsyncFnMut(String, Value) -> ControlFlow<Stop, Result<ToolResultContent>>,
    timeout: Duration,
) -> ControlFlow<Stop, Result<Value>> {
    if source.len() > SOURCE_BYTES {
        return ControlFlow::Continue(Err(format!("code source exceeds {SOURCE_BYTES} bytes")));
    }

    let deadline = Instant::now() + timeout;
    let (keep_running, stop) = watch::channel(());
    let (sender, mut calls) = mpsc::channel::<Call>(1);
    let handle = tokio::runtime::Handle::current();
    let mut worker = tokio::task::spawn_blocking(move || {
        handle.block_on(run(source, names, sender, stop, deadline))
    });
    let execution = async {
        let drive = async {
            while let Some(call) = calls.recv().await {
                match dispatch(call.name, call.arguments).await {
                    ControlFlow::Continue(result) => {
                        let _ = call.reply.send(result);
                    }
                    ControlFlow::Break(stop) => return Some(stop),
                }
            }
            None
        };
        let outcome = tokio::select! {
            biased;
            // A terminal child outcome wins even if JS finished in the meantime.
            stop = drive => match stop {
                Some(stop) => return ControlFlow::Break(stop),
                None => (&mut worker).await,
            },
            result = &mut worker => result,
        };
        ControlFlow::Continue(
            outcome.unwrap_or_else(|error| Err(format!("JavaScript worker: {error}"))),
        )
    };
    let result = tokio::time::timeout_at(deadline.into(), execution).await;
    drop(keep_running);
    result.unwrap_or_else(|_| {
        ControlFlow::Continue(Err("JavaScript evaluation timed out".to_string()))
    })
}

async fn run(
    source: String,
    names: Vec<String>,
    sender: mpsc::Sender<Call>,
    mut stop: watch::Receiver<()>,
    deadline: Instant,
) -> Result<Value> {
    let runtime = AsyncRuntime::new().map_err(|error| error.to_string())?;
    runtime.set_memory_limit(HEAP_BYTES).await;
    let interrupt = stop.clone();
    runtime
        .set_interrupt_handler(Some(Box::new(move || {
            interrupt.has_changed().is_err() || Instant::now() >= deadline
        })))
        .await;
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
                        tool(&ctx, name.clone(), sender.clone(), slots.clone())?,
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
        result = evaluation => result,
        _ = stop.changed() => Err("JavaScript evaluation stopped".into()),
    }
}

fn tool<'js>(
    ctx: &Ctx<'js>,
    name: String,
    sender: mpsc::Sender<Call>,
    slots: Arc<Semaphore>,
) -> rquickjs::Result<Function<'js>> {
    Function::new(ctx.clone(), move |ctx: Ctx<'js>, args: JsValue<'js>| {
        let permit = slots.clone().try_acquire_owned().map_err(|_| {
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
        Promise::wrap_future(&ctx.clone(), async move {
            let _permit = permit;
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
