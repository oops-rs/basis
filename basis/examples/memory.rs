//! Compaction facts become memories — a sink filing what a summarizing pass
//! replaced, where the next session's index will find it.
//!
//! Memory is files, not a subsystem (D2): there is no store to subscribe to
//! and no API to learn. A host that wants durable memory out of a run's own
//! lifecycle listens to the event stream it already has — here,
//! [`Event::CompactionCompleted`], the moment a conversation's past is about
//! to live only in a summary — and writes a markdown file. The next
//! `Workspace::open` over the same memory root indexes it into the system
//! prompt like any memory a person wrote.
//!
//! ```sh
//! export BASIS_API_KEY=…                    # or ANTHROPIC_API_KEY, etc.
//! export BASIS_MODEL=…                      # optional
//! cargo run -p basis --example memory -- /repo "read the tests, then refactor" ./memories
//! ```
//!
//! The third argument is the memory root, configured explicitly so the
//! example is self-contained; the CLI's own layout would derive it beside the
//! store instead ([`basis::memory`]).

use std::{env, path::PathBuf};

use basis::{
    AllowAll, Event, FnSink, MemoryConfig, ModelSelector, Workspace, WorkspaceMemoryRoot,
    memory::{MemoryKind, file_contents},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let path = args.next().unwrap_or_else(|| ".".to_string());
    let prompt = args
        .next()
        .unwrap_or_else(|| "Summarize this repository in detail.".to_string());
    let memory_root = PathBuf::from(args.next().unwrap_or_else(|| "./memories".to_string()));

    let mut builder = Workspace::builder(&path).with_memory(MemoryConfig {
        // The user's global memories are left out so the example touches
        // nothing but the directory it was pointed at.
        global_root: None,
        workspace_root: WorkspaceMemoryRoot::Dir(memory_root.clone()),
    });
    if let Ok(model) = env::var("BASIS_MODEL") {
        builder = builder.with_model(ModelSelector::Id(model));
    }
    let workspace = builder.open().await?;

    // What earlier runs left behind is already in the system prompt as an
    // index; this is the host's view of the same list.
    for memory in workspace.memories() {
        println!("remembered: {} — {}", memory.name, memory.description);
    }

    // The sink is the whole mechanism. `memory::file_contents` serializes the
    // frontmatter so the file parses back on the next open; everything else
    // is the filesystem the host already has.
    let root = memory_root.clone();
    let mut filed = 0_usize;
    let report = workspace
        .prepare(prompt)?
        .execute_with_approver(
            FnSink::new(move |event| {
                if let Event::CompactionCompleted {
                    replaced_items,
                    preserved_items,
                    extracted_facts,
                    summary_preview,
                    ..
                } = event
                {
                    filed += 1;
                    let body = format!(
                        "{replaced_items} earlier items were replaced by a summary \
                     ({preserved_items} kept, {extracted_facts} facts extracted).\n\n\
                     Summary preview:\n\n{summary_preview}",
                    );
                    std::fs::create_dir_all(&root)?;
                    std::fs::write(
                        root.join(format!("compaction-{filed}.md")),
                        file_contents(
                            &format!("compaction-{filed}"),
                            "what a summarizing pass replaced, kept for the next session",
                            MemoryKind::Reference,
                            &body,
                        ),
                    )?;
                    eprintln!("filed compaction-{filed}.md");
                }
                Ok(())
            }),
            AllowAll,
        )
        .await?;

    println!(
        "{}",
        report.final_message.as_deref().unwrap_or("(no message)")
    );
    Ok(())
}
