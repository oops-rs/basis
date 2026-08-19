//! Live two-turn check against a real model.
//!
//! `cargo run -p basis-core --example conversation` — needs BASIS_API_KEY and usually
//! BASIS_BASE_URL/BASIS_MODEL. Proves against a real provider what
//! `tests/conversation.rs` proves against a mock: the second turn sees the
//! first, because the session survives.

use basis_core::{AllowAll, CollectingSink, NullSink, RunConfig};
use mentra::ModelSelector;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = std::env::current_dir()?;

    let mut config = RunConfig::new(workspace, "Remember the number 41. Just acknowledge.");
    if let Ok(model) = std::env::var("BASIS_MODEL") {
        config = config.with_model(ModelSelector::Id(model));
    }
    if let Ok(base_url) = std::env::var("BASIS_BASE_URL") {
        config = config.with_base_url(base_url);
    }

    let mut run = basis_core::run::prepare(config).await?;
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
