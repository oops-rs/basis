//! Putting a workspace's declared tools on the runtime it borrows, and taking
//! the claim back when the workspace goes.
//!
//! The shape is `mcp::connections`'s, because the problem is the same
//! one: the tool registry is the *runtime's*, single, and has no unregister,
//! while what is being registered came out of one repository's file and belongs
//! to that repository. So a name is claimed before anything is registered, the
//! claim is released on drop, and what a dropped workspace left behind is kept
//! out of every other workspace's roster rather than removed.
//!
//! Where it differs is the collision rule, and [`Runtime::claim_declared_tool`]
//! carries the argument: a bridged MCP tool's name is synthetic and can be
//! suffixed, a declared tool's name is the identity an operator writes rules
//! against and cannot.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::runtime::Runtime;

use super::{
    manifest::{DeclaredToolError, DeclaredToolSpec, ToolsSource, layer},
    tool::DeclaredTool,
};

/// One workspace's declared tools, registered on a runtime it may share.
///
/// Names and a root only, so `Debug` carries nothing a manifest read: the
/// declarations themselves live on the registered tools.
#[derive(Debug)]
pub(crate) struct DeclaredTools {
    runtime: Arc<Runtime>,
    /// The claim owner; only this root can release its names.
    root: PathBuf,
    /// Claimed names, released on drop.
    names: Vec<String>,
}

impl DeclaredTools {
    /// Claims every name, then registers every tool.
    ///
    /// Two passes rather than one, and the order is the point: a manifest whose
    /// fourth tool collides must leave the first three unregistered, because on
    /// a shared runtime a half-registered manifest from a workspace that failed
    /// to open would still be in every other workspace's roster.
    pub(crate) fn register(
        runtime: Arc<Runtime>,
        root: &Path,
        sources: &[ToolsSource],
    ) -> Result<Self, DeclaredToolError> {
        let declared = layer(sources);

        let mut claimed = Self {
            runtime,
            root: root.to_path_buf(),
            names: Vec::new(),
        };

        for (path, spec) in &declared {
            // On failure `claimed` drops, releasing whatever it had taken, so a
            // refused open leaves the runtime as it found it.
            claimed
                .runtime
                .claim_declared_tool(&spec.name, root)
                .map_err(|reason| DeclaredToolError::NameTaken {
                    path: path.clone(),
                    name: spec.name.clone(),
                    reason,
                })?;
            claimed.names.push(spec.name.clone());
        }

        for (_, spec) in declared {
            claimed
                .runtime
                .mentra_runtime()
                .register_tool(wrapped(&claimed.runtime, spec, root));
        }

        Ok(claimed)
    }

    /// The names registered, in the order [`load`](super::load) layers them.
    pub(crate) fn names(&self) -> &[String] {
        &self.names
    }

    /// The root these names are claimed under, which is the one a mint has to
    /// ask [`Runtime::foreign_declared_tools`] with — claiming under one
    /// spelling of a directory and asking under another would have a workspace
    /// hiding its own tools.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
}

/// One declaration, wrapped with everything it needs from both scopes.
///
/// The seam this exists to name: a manifest is a *repository's* statement and a
/// command environment is the *runtime's* (ADR-0018), and a declared tool needs
/// both. The registry is where the two meet, because it is built per workspace
/// out of a runtime the workspace borrows — so this is the one place the
/// runtime's pairs are handed over, and it is a named function so a test can
/// ask what a tool was built with rather than infer it.
fn wrapped(runtime: &Runtime, spec: DeclaredToolSpec, root: &Path) -> DeclaredTool {
    DeclaredTool::new(spec, root).with_command_environment(runtime.command_environment())
}

impl Drop for DeclaredTools {
    fn drop(&mut self) {
        for name in self.names.drain(..) {
            self.runtime.release_declared_tool(&name, &self.root);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{
        context::ContextScope,
        tools::declared::manifest::{DeclaredToolSpec, SideEffect},
    };

    use super::*;

    fn runtime() -> Arc<Runtime> {
        Arc::new(
            Runtime::builder()
                .with_base_url("http://127.0.0.1:1/v1")
                .with_api_key("test-key")
                .with_ephemeral_history()
                .build()
                .expect("builds"),
        )
    }

    fn spec(name: &str) -> DeclaredToolSpec {
        DeclaredToolSpec {
            name: name.to_string(),
            description: "does the thing".to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
            command: vec!["./x".to_string()],
            cwd: None,
            env: Vec::new(),
            timeout_ms: None,
            side_effect: SideEffect::Process,
        }
    }

    fn source(path: &str, names: &[&str]) -> ToolsSource {
        ToolsSource {
            path: PathBuf::from(path),
            scope: ContextScope::Workspace,
            tools: names.iter().map(|name| spec(name)).collect(),
        }
    }

    #[test]
    fn a_declared_tool_reaches_the_model_under_the_name_the_file_gave_it() {
        let runtime = runtime();
        let sources = [source("/repo/.basis/tools.json", &["jenkins_job"])];

        let registered =
            DeclaredTools::register(Arc::clone(&runtime), Path::new("/repo"), &sources)
                .expect("registers");

        assert_eq!(registered.names(), ["jenkins_job"]);
        assert!(
            runtime
                .mentra_runtime()
                .tools()
                .iter()
                .any(|tool| tool.provider.name == "jenkins_job")
        );
    }

    #[test]
    fn a_declared_tool_is_built_with_the_runtimes_command_environment() {
        // The seam: a manifest is a repository's statement and the command
        // environment is the runtime's (ADR-0018), and this is the one place
        // the two meet. A host that told the runtime where its service lives
        // expects the program a declared tool runs to be told as well — that
        // it was not is the bug this closes.
        let runtime = Arc::new(
            Runtime::builder()
                .with_base_url("http://127.0.0.1:1/v1")
                .with_api_key("test-key")
                .with_ephemeral_history()
                .with_command_environment("NOUS_URL", "http://nous.internal")
                .build()
                .expect("builds"),
        );

        let tool = wrapped(&runtime, spec("ask_nous"), Path::new("/repo"));

        assert_eq!(
            tool.command_environment(),
            [("NOUS_URL".to_string(), "http://nous.internal".to_string())]
        );
        assert_eq!(
            tool.name(),
            "ask_nous",
            "and it is still the manifest's tool"
        );
    }

    #[test]
    fn a_runtime_that_was_told_nothing_hands_over_nothing() {
        let tool = wrapped(&runtime(), spec("ask_nous"), Path::new("/repo"));

        assert!(tool.command_environment().is_empty());
    }

    #[test]
    fn a_manifest_cannot_take_over_the_name_of_basiss_own_tool() {
        // Without the claim this would *replace* `spawn` in mentra's registry,
        // inheriting every remembered rule an operator ever wrote about it.
        let runtime = runtime();
        let sources = [source("/repo/.basis/tools.json", &[crate::tools::SPAWN])];

        let error =
            DeclaredTools::register(runtime, Path::new("/repo"), &sources).expect_err("refused");

        assert!(
            matches!(error, DeclaredToolError::NameTaken { .. }),
            "{error}"
        );
        assert!(error.to_string().contains(crate::tools::SPAWN), "{error}");
    }

    #[test]
    fn a_manifest_cannot_take_over_a_mentra_builtin_either() {
        let runtime = runtime();
        let sources = [source("/repo/.basis/tools.json", &["files"])];

        let error =
            DeclaredTools::register(runtime, Path::new("/repo"), &sources).expect_err("refused");

        assert!(
            matches!(error, DeclaredToolError::NameTaken { .. }),
            "{error}"
        );
    }

    #[test]
    fn one_name_is_one_program_across_the_workspaces_sharing_a_runtime() {
        let runtime = runtime();
        let first = DeclaredTools::register(
            Arc::clone(&runtime),
            Path::new("/repo/one"),
            &[source("/repo/one/.basis/tools.json", &["deploy"])],
        )
        .expect("the first claimant registers");

        let error = DeclaredTools::register(
            Arc::clone(&runtime),
            Path::new("/repo/two"),
            &[source("/repo/two/.basis/tools.json", &["deploy"])],
        )
        .expect_err("refused rather than silently renamed");

        assert!(
            matches!(error, DeclaredToolError::NameTaken { .. }),
            "{error}"
        );
        assert!(
            error.to_string().contains("/repo/two"),
            "the refusal names the file to fix: {error}"
        );

        drop(first);

        DeclaredTools::register(
            runtime,
            Path::new("/repo/two"),
            &[source("/repo/two/.basis/tools.json", &["deploy"])],
        )
        .expect("a released name is claimable again");
    }

    #[test]
    fn a_workspace_can_be_reopened_over_the_entry_its_last_open_left_behind() {
        // mentra has no unregister, so the registry still holds the tool after
        // the first workspace drops. Refusing on that would make a host that
        // opens a repository per request fail on the second one.
        let runtime = runtime();
        let sources = [source("/repo/.basis/tools.json", &["deploy"])];

        let first = DeclaredTools::register(Arc::clone(&runtime), Path::new("/repo"), &sources)
            .expect("registers");
        drop(first);

        DeclaredTools::register(runtime, Path::new("/repo"), &sources)
            .expect("the same workspace registers its own tool again");
    }

    #[test]
    fn two_opens_of_one_workspace_both_keep_their_tools() {
        let runtime = runtime();
        let sources = [source("/repo/.basis/tools.json", &["deploy"])];

        let first = DeclaredTools::register(Arc::clone(&runtime), Path::new("/repo"), &sources)
            .expect("registers");
        let second = DeclaredTools::register(Arc::clone(&runtime), Path::new("/repo"), &sources)
            .expect("registers");

        drop(first);

        // The second is still open, so the name is still spoken for.
        let error = DeclaredTools::register(
            Arc::clone(&runtime),
            Path::new("/elsewhere"),
            &[source("/elsewhere/.basis/tools.json", &["deploy"])],
        )
        .expect_err("still held");
        assert!(
            matches!(error, DeclaredToolError::NameTaken { .. }),
            "{error}"
        );

        drop(second);
        DeclaredTools::register(
            runtime,
            Path::new("/elsewhere"),
            &[source("/elsewhere/.basis/tools.json", &["deploy"])],
        )
        .expect("the last holder released it");
    }

    #[test]
    fn a_collision_partway_through_registers_nothing_at_all() {
        let runtime = runtime();
        let _held = DeclaredTools::register(
            Arc::clone(&runtime),
            Path::new("/repo/one"),
            &[source("/repo/one/.basis/tools.json", &["taken"])],
        )
        .expect("registers");

        let error = DeclaredTools::register(
            Arc::clone(&runtime),
            Path::new("/repo/two"),
            &[source("/repo/two/.basis/tools.json", &["fine", "taken"])],
        )
        .expect_err("refused");
        assert!(
            matches!(error, DeclaredToolError::NameTaken { .. }),
            "{error}"
        );

        assert!(
            !runtime
                .mentra_runtime()
                .tools()
                .iter()
                .any(|tool| tool.provider.name == "fine"),
            "a workspace that failed to open must leave nothing in a shared roster"
        );

        DeclaredTools::register(
            runtime,
            Path::new("/repo/three"),
            &[source("/repo/three/.basis/tools.json", &["fine"])],
        )
        .expect("and must hold no claim on the name it half-took either");
    }

    #[test]
    fn one_workspaces_tool_is_not_offered_to_another_on_the_same_runtime() {
        let runtime = runtime();
        let _held = DeclaredTools::register(
            Arc::clone(&runtime),
            Path::new("/repo/one"),
            &[source("/repo/one/.basis/tools.json", &["deploy"])],
        )
        .expect("registers");

        assert_eq!(
            runtime.foreign_declared_tools(Path::new("/repo/two")),
            vec!["deploy".to_string()],
            "a program one repository declared is not the other's to run"
        );
        assert!(
            runtime
                .foreign_declared_tools(Path::new("/repo/one"))
                .is_empty()
        );
    }
}
