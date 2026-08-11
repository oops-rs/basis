//! Embedding lan in-process — the primary case (PROPOSAL.md Bet 1).
//!
//! The binary is a thin shell over exactly this. A Rust host gets the same
//! run, the same events, and the same context discovery without a subprocess
//! or a protocol in between.
//!
//! ```sh
//! export LAN_API_KEY=…                       # or ANTHROPIC_API_KEY, etc.
//! export LAN_BASE_URL=http://127.0.0.1:3455/v1   # optional
//! cargo run -p lan-core --example embed -- "what does this repo do?"
//! ```

use std::sync::{Arc, Mutex};

use lan_core::{Event, FnSink, RunConfig, RunOutcome};
use mentra::ModelSelector;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Name this project in one short sentence.".to_string());

    let workspace = std::env::current_dir()?;

    let mut config = RunConfig::new(workspace, prompt);
    if let Ok(model) = std::env::var("LAN_MODEL") {
        config = config.with_model(ModelSelector::Id(model));
    }

    // A host usually wants to react to events, not just collect them. Anything
    // that is `FnMut(Event) -> io::Result<()>` is a sink; here we count tool
    // calls and print the assistant's text as it streams.
    let tool_calls = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = Arc::clone(&tool_calls);

    let report = lan_core::run(
        config,
        FnSink::new(move |event| {
            match event {
                Event::RunStarted {
                    model,
                    context_files,
                    skills,
                    ..
                } => {
                    println!("model: {model}");
                    for file in &context_files {
                        println!("context: {} ({})", file.path.display(), file.scope);
                    }
                    for skill in &skills {
                        println!("skill: {} — {}", skill.name, skill.description);
                    }
                    println!("---");
                }
                Event::AssistantDelta { text } => print!("{text}"),
                Event::ToolQueued { tool_name, .. } => {
                    seen.lock().expect("not poisoned").push(tool_name);
                }
                Event::RunFinished { .. } => println!(),
                _ => {}
            }
            Ok(())
        }),
    )
    .await?;

    println!("---");
    println!("session: {}", report.session_id);
    println!(
        "tools used: {}",
        tool_calls.lock().expect("not poisoned").join(", ")
    );

    match report.outcome {
        RunOutcome::Ok => Ok(()),
        RunOutcome::Error { message } => Err(message.into()),
    }
}
