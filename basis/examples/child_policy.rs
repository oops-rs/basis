//! Cheap triage beside a full fixer — the child policy in ~30 lines of host code.
//!
//! One agent, two kinds of delegation. The parent works on the task with its
//! full roster and full-strength model. Whenever it delegates a task whose
//! prompt starts with `triage:` — a convention this host chose, taught in the
//! prompt below — the policy hands that child a read-only roster, a cheaper
//! model, and a two-sentence system prompt of its own. Every other delegation
//! inherits everything, exactly as it always has: the policy makes the clone a
//! *default* rather than the only shape (D4).
//!
//! What to notice while it runs:
//!
//! - the triage child cannot write, run a command, or delegate — its roster
//!   simply does not offer the tools — while the fixer child, spawned by the
//!   same `spawn` tool one line later, inherits the parent's full set;
//! - both children draw on the same run: one deadline, one token counter, the
//!   parent's cancellation — bounds never travel through the policy;
//! - the approver (auto-allow here) is shown a `child` key describing each
//!   triage child, so a real deployment can write remembered rules against
//!   what the child will be.
//!
//! ```sh
//! export BASIS_API_KEY=…                    # or ANTHROPIC_API_KEY, etc.
//! cargo run -p basis --example child_policy -- /repo \
//!     "the flaky retry test in tests/net.rs" gpt-5-mini openai
//! ```
//!
//! The last two arguments are the triage child's model and the provider id it
//! resolves against — the same id the runtime itself runs on, unless you have
//! registered another.

use std::{env, error::Error, time::Duration};

use basis::{ChildSpec, Event, FnSink, ModelInfo, RunSpec, Runtime, ToolRoster, Workspace};

/// The convention the policy matches on. Taught to the parent in its prompt,
/// matched by the policy — one string, two readers, so it is named once.
const TRIAGE: &str = "triage:";

/// What a triage child is, instead of a clone of its parent: read-only eyes
/// and a yes-or-no mouth. `ToolRoster::only` genuinely stops offering
/// everything unnamed — including `spawn`, so a triage child is a leaf.
const TRIAGE_TOOLS: [&str; 3] = ["read", "grep", "glob"];
const TRIAGE_VOICE: &str = "You are a triage gate. Read only what you need, then answer in one \
     short paragraph starting with YES (worth fixing now) or NO, and say why.";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut args = env::args().skip(1);
    let (Some(workspace), Some(task), Some(triage_model), Some(triage_provider)) =
        (args.next(), args.next(), args.next(), args.next())
    else {
        return Err("usage: child_policy <workspace> <task> <triage-model> <provider-id>".into());
    };

    // The policy is the whole feature: one function, runtime-scoped, consulted
    // for every delegation at every depth. Prompts carrying the convention get
    // the cheap shape; everything else is the inherit-everything default.
    let triage = ModelInfo::new(triage_model, triage_provider.as_str());
    let runtime = Runtime::builder().with_child_policy(move |child| {
        if child.prompt().trim_start().starts_with(TRIAGE) {
            ChildSpec::inherit()
                .with_roster(ToolRoster::only(TRIAGE_TOOLS))
                .with_model(triage.clone())
                .with_system(TRIAGE_VOICE)
        } else {
            ChildSpec::inherit()
        }
    });

    let workspace = Workspace::builder(workspace)
        .with_runtime_builder(runtime)
        .open()
        .await?;

    let spec = RunSpec::new(format!(
        "Investigate this report: {task}\n\n\
         First delegate one task whose prompt starts with `{TRIAGE}` asking whether the \
         problem is real and worth fixing now — that child is a cheap, read-only gate. \
         If it answers YES, delegate the actual fix as an ordinary task (no prefix) and \
         then summarise what was done; if NO, explain why and stop."
    ))
    .with_deadline(Duration::from_secs(600));

    // One pane for the whole tree: the parent's stream carries each child's
    // lifecycle (spawned, finished) and the parent's own words.
    let report = workspace
        .prepare(spec)?
        .execute_with_approver(
            FnSink::new(|event| {
                match event {
                    Event::TaskUpdated { title, status, .. } => {
                        println!("[child] {title}: {status:?}");
                    }
                    Event::AssistantMessage { text } => println!("{text}"),
                    _ => {}
                }
                Ok(())
            }),
            basis::AllowAll,
        )
        .await?;

    println!(
        "\n{} tokens, outcome {:?}",
        report.usage.total_tokens(),
        report.outcome
    );
    Ok(())
}
