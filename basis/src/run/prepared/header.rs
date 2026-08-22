//! What a run *is*, and the line it announces itself with.
//!
//! Split from [`prepared`](super) for the parent's size, along the seam the
//! two already had: everything here is the run's own description — where it
//! is, what it resolved, what discovery found — plus the one function that
//! turns that into [`Event::RunStarted`]. Nothing here drives anything.
//!
//! Distinct from [`context`](super::context), which is about the *model's*
//! context window and the system prompt: that answers how much room is left,
//! this answers what the run is.

use std::path::PathBuf;

use super::{
    ContextFile, EVENT_SCHEMA_VERSION, Event, SkillSummary, Template, TemplateSummary,
    WorkspaceContext,
};

/// What a run is about, once the runtime questions are settled.
#[derive(Debug, Clone)]
pub struct RunContext {
    pub workspace: PathBuf,
    pub prompt: String,
    pub provider: String,
    pub model: String,
    pub context: WorkspaceContext,
    /// Skills directories registered on the runtime, most specific first.
    pub skills_dirs: Vec<PathBuf>,
    /// The skills those directories actually produced, after layering.
    pub skills: Vec<LoadedSkill>,
    /// Template directories that exist, most specific first.
    pub templates_dirs: Vec<PathBuf>,
    /// The templates those directories produced, after layering, name-ordered.
    /// Over ACP these become the client's commands, mapped by `basis-acp`.
    pub templates: Vec<Template>,
    /// MCP configuration files in effect, weakest precedence first.
    pub mcp_files: Vec<ContextFile>,
    /// The servers those files produced, after layering. Names only: the
    /// header must not echo a command or a credential.
    pub mcp_servers: Vec<String>,
}

/// A skill available to the run, without its body.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LoadedSkill {
    pub name: String,
    pub description: String,
    /// Whether the model may reach this one.
    ///
    /// `false` when the `SKILL.md` frontmatter set `disable-model-invocation`
    /// (or `disable_model_invocation`): mentra keeps it out of the list the
    /// model is shown and `load_skill` refuses it, while leaving it in the set
    /// a host is shown — a skill a person invokes deliberately. Carried here
    /// because the distinction is only actionable by a host, and one that
    /// could not see it would have to re-read every `SKILL.md` to tell two
    /// entries apart that look alike and behave differently.
    pub model_invocable: bool,
    pub path: PathBuf,
}

/// Builds the opening line. Kept separate so [`PreparedRun::header`] and the
/// line actually emitted can never drift apart.
pub(super) fn header_for(session_id: &str, run: &RunContext) -> Event {
    Event::RunStarted {
        schema: EVENT_SCHEMA_VERSION,
        basis: env!("CARGO_PKG_VERSION").to_string(),
        session_id: session_id.to_string(),
        workspace: run.workspace.clone(),
        model: run.model.clone(),
        provider: run.provider.clone(),
        context_files: run
            .context
            .documents()
            .iter()
            .map(|document| ContextFile {
                path: document.path.clone(),
                scope: document.scope.label(),
            })
            .collect(),
        skills_dirs: run.skills_dirs.clone(),
        skills: run
            .skills
            .iter()
            .map(|skill| SkillSummary {
                name: skill.name.clone(),
                description: skill.description.clone(),
            })
            .collect(),
        templates_dirs: run.templates_dirs.clone(),
        templates: run
            .templates
            .iter()
            .map(|template| TemplateSummary {
                name: template.name.clone(),
                description: template.description.clone(),
                argument_hint: template.argument_hint.clone(),
            })
            .collect(),
        mcp_files: run.mcp_files.clone(),
        mcp_servers: run.mcp_servers.clone(),
    }
}

#[cfg(test)]
mod tests {
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
}
