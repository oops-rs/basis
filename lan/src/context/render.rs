//! Rendering discovered context into a single system-prompt block.

use super::{ContextDocument, ContextScope};

/// Joins documents in the order given — weakest precedence first — labelling
/// each with the path it came from so the model can attribute a rule, and so a
/// transcript shows which files were in effect.
///
/// Returns `None` when there is nothing to say, which keeps the caller from
/// injecting an empty section into the system prompt.
pub(super) fn render(documents: &[ContextDocument]) -> Option<String> {
    if documents.is_empty() {
        return None;
    }

    let sections = documents
        .iter()
        .map(|document| {
            format!(
                "<context path=\"{}\" scope=\"{}\">\n{}\n</context>",
                document.path.display(),
                scope_label(&document.scope),
                document.content.trim_end(),
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    Some(format!(
        "The following instructions come from the workspace. Later blocks are \
         more specific and take precedence over earlier ones.\n\n{sections}"
    ))
}

fn scope_label(scope: &ContextScope) -> String {
    match scope {
        ContextScope::Global => "global".to_string(),
        ContextScope::Ancestor { depth } => format!("ancestor:{depth}"),
        ContextScope::Workspace => "workspace".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn document(path: &str, scope: ContextScope, content: &str) -> ContextDocument {
        ContextDocument {
            path: PathBuf::from(path),
            scope,
            content: content.to_string(),
        }
    }

    #[test]
    fn nothing_renders_to_none() {
        assert_eq!(render(&[]), None);
    }

    #[test]
    fn each_document_carries_its_path_and_scope() {
        let rendered = render(&[document(
            "/repo/AGENTS.md",
            ContextScope::Workspace,
            "rule one",
        )])
        .expect("renders");

        assert!(rendered.contains("path=\"/repo/AGENTS.md\""));
        assert!(rendered.contains("scope=\"workspace\""));
        assert!(rendered.contains("rule one"));
    }

    #[test]
    fn ancestor_scope_records_its_distance() {
        let rendered = render(&[document(
            "/a/AGENTS.md",
            ContextScope::Ancestor { depth: 3 },
            "far",
        )])
        .expect("renders");

        assert!(rendered.contains("scope=\"ancestor:3\""));
    }

    #[test]
    fn documents_render_in_the_order_given() {
        let rendered = render(&[
            document("/a/AGENTS.md", ContextScope::Global, "FIRST"),
            document("/b/AGENTS.md", ContextScope::Workspace, "SECOND"),
        ])
        .expect("renders");

        assert!(rendered.find("FIRST") < rendered.find("SECOND"));
    }

    #[test]
    fn trailing_whitespace_does_not_reach_the_prompt() {
        let rendered = render(&[document(
            "/repo/AGENTS.md",
            ContextScope::Workspace,
            "body\n\n\n",
        )])
        .expect("renders");

        assert!(rendered.contains("body\n</context>"));
    }
}
