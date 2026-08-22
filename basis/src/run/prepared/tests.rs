//! What `prepared.rs` settles without a provider.
//!
//! Split out for the parent's size, the same remedy `basis-acp/src/server.rs`
//! took: the file was past the 800-line ceiling with these inline. What is
//! here is the header a run opens with, which is the one thing a run can be
//! asked about before anything is sent. Why a turn *ended* is asked of
//! [`outcome`](super::outcome) and tested beside it; anything needing a live
//! session is driven end to end from `basis/tests/`.

use super::*;
use crate::context::{ContextDocument, ContextScope};

#[test]
fn the_header_lists_context_files_weakest_first() {
    let context = WorkspaceContext::from_documents(vec![
        ContextDocument {
            path: PathBuf::from("/AGENTS.md"),
            scope: ContextScope::Ancestor { depth: 2 },
            content: "outer".to_string(),
        },
        ContextDocument {
            path: PathBuf::from("/repo/AGENTS.md"),
            scope: ContextScope::Workspace,
            content: "inner".to_string(),
        },
    ]);

    let files: Vec<ContextFile> = context
        .documents()
        .iter()
        .map(|document| ContextFile {
            path: document.path.clone(),
            scope: document.scope.label(),
        })
        .collect();

    assert_eq!(files[0].scope, "ancestor:2");
    assert_eq!(files[1].scope, "workspace");
}
