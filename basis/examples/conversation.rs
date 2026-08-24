//! Live two-turn check against a real model.
//!
//! `cargo run -p basis --example conversation` — needs BASIS_API_KEY and usually
//! BASIS_BASE_URL/BASIS_MODEL. Proves against a real provider what
//! `tests/conversation.rs` proves against a mock: the second turn sees the
//! first, because the session survives.

use basis::{AllowAll, CollectingSink, NullSink, Runtime, Workspace};
use mentra::ModelSelector;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut runtime = Runtime::builder();
    if let Ok(base_url) = std::env::var("BASIS_BASE_URL") {
        runtime = runtime.with_base_url(base_url);
    }
    let mut builder = Workspace::builder(std::env::current_dir()?).with_runtime_builder(runtime);
    if let Ok(model) = std::env::var("BASIS_MODEL") {
        builder = builder.with_model(ModelSelector::Id(model));
    }
    let workspace = builder.open().await?;

    let mut run = workspace.prepare("Remember the number 41. Just acknowledge.")?;
    println!("session: {}", run.session_id());
    println!("agent:   {}", run.agent_id());

    let first = run.execute(NullSink).await?;
    println!(
        "\nturn 1: {}",
        first.final_message.unwrap_or_default().trim()
    );

    let second = run
        .send(
            "What number did I ask you to remember? Reply with digits only.",
            CollectingSink::new(),
            AllowAll,
        )
        .await?;
    let answer = second.final_message.unwrap_or_default();
    println!("turn 2: {}", answer.trim());

    // The point of the whole refactor: turn two could only answer this by
    // having seen turn one.
    if answer.contains("41") {
        println!("\nthe session carried the conversation");
        Ok(())
    } else {
        Err(format!("turn 2 did not recall turn 1: {answer:?}").into())
    }
}
