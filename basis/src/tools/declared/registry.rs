//! Putting a workspace's declared tools on the runtime it borrows, and taking
//! the claim back when the workspace goes.
//!
//! The shape is `mcp::connections`'s, because the problem is the same one: the
//! tool registry is the *runtime's* and single, while what is being registered
//! came out of one repository's file and belongs to that repository. So a name
//! is claimed before anything is registered, and the claim — with the tool
//! under it — is released when the last workspace holding it goes.
//!
//! Where it differs is the collision rule, and [`Runtime::claim_declared_tool`]
//! carries the argument: a bridged MCP tool's name is synthetic and can be
//! suffixed, a declared tool's name is the identity an operator writes rules
//! against and cannot.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use mentra::tool::ToolAudience;

use crate::runtime::{DeclaredToolOrigin, Runtime};

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
    /// The origin each claim must release beside its name.
    origins: Vec<DeclaredToolOrigin>,
}

impl DeclaredTools {
    #[cfg(test)]
    pub(crate) fn register(
        runtime: Arc<Runtime>,
        audience: &ToolAudience,
        root: &Path,
        sources: &[ToolsSource],
    ) -> Result<Self, DeclaredToolError> {
        Self::register_with_supplied(runtime, audience, root, sources, &[])
    }

    /// Claims every name, then registers the tools this open is the first
    /// holder of. Supplied declarations are layered before file sources.
    ///
    /// Two passes rather than one, and the order is the point: a manifest whose
    /// fourth tool collides must leave the first three unregistered, because on
    /// a shared runtime a half-registered manifest from a workspace that failed
    /// to open would still be in every other workspace's roster.
    ///
    /// The second pass registers with `try_register_tool_for_audience`, which
    /// is the difference between a check-then-act that is safe *because* of the
    /// claim map and one that is safe on its own. Nothing basis does can reach
    /// the gap between the two passes — the claim map serializes every
    /// workspace on this runtime — but a host holding the same `mentra::Runtime`
    /// can call `register_tool` on it directly, and a claim it walked past
    /// would otherwise be a repository's program silently answering to the
    /// host's name, or the reverse.
    ///
    /// `audience` is the workspace's own, so a tool a repository declares is
    /// offered to that repository's runs and to nothing else on the runtime —
    /// mentra's ladder resolves a foreign audience's name to `Hidden`, which is
    /// what the roster used to have to hide by hand at every mint.
    pub(crate) fn register_with_supplied(
        runtime: Arc<Runtime>,
        audience: &ToolAudience,
        root: &Path,
        sources: &[ToolsSource],
        supplied: &[DeclaredToolSpec],
    ) -> Result<Self, DeclaredToolError> {
        let declared = layer(supplied, sources);

        let mut claimed = Self {
            runtime,
            root: root.to_path_buf(),
            names: Vec::new(),
            origins: Vec::new(),
        };
        let mut permissions = Vec::new();

        for (path, spec) in &declared {
            // On failure `claimed` drops, releasing whatever it had taken, so a
            // refused open leaves the runtime as it found it.
            let origin = match path {
                Some(_) => DeclaredToolOrigin::File,
                None => DeclaredToolOrigin::Supplied,
            };
            let claim = claimed
                .runtime
                .claim_declared_tool(root, spec, origin)
                .map_err(|reason| name_taken(path.as_deref(), &spec.name, reason))?;
            claimed.names.push(spec.name.clone());
            claimed.origins.push(origin);
            permissions.push(claim);
        }

        for ((path, spec), claim) in declared.into_iter().zip(permissions) {
            // `None` is a sibling open of this same root that already
            // registered it, and one name is one program: joining that
            // registration is what keeps the sibling's running agents on the
            // program they started with.
            let Some(claim) = claim else {
                continue;
            };

            let name = spec.name.clone();
            let tool = wrapped(&claimed.runtime, spec, root);
            claimed
                .runtime
                .install_declared_tool(audience, claim, tool)
                .map_err(|reason| name_taken(path.as_deref(), &name, reason))?;
        }

        Ok(claimed)
    }

    /// The names registered, in the order [`load`](super::load) layers them.
    pub(crate) fn names(&self) -> &[String] {
        &self.names
    }

    /// The root these names are claimed under.
    ///
    /// Exposed so the open's own tests can assert it against
    /// [`Workspace::root`](crate::Workspace::root): the claim owner is one of
    /// the names an open promises to settle on one directory, and without a
    /// reader nothing failed if an edit reintroduced a second spelling of it.
    /// `#[cfg(test)]` because that assertion is the only caller — every
    /// claim and release in this module reads the field directly.
    #[cfg(test)]
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
}

fn name_taken(path: Option<&Path>, name: &str, reason: String) -> DeclaredToolError {
    match path {
        Some(path) => DeclaredToolError::NameTaken {
            path: path.to_path_buf(),
            name: name.to_string(),
            reason,
        },
        None => DeclaredToolError::SuppliedNameTaken {
            name: name.to_string(),
            reason,
        },
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
        for (name, origin) in self.names.drain(..).zip(self.origins.drain(..)) {
            self.runtime
                .release_declared_tool(&name, &self.root, origin);
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

    fn source_with(path: &str, spec: DeclaredToolSpec) -> ToolsSource {
        ToolsSource {
            path: PathBuf::from(path),
            scope: ContextScope::Workspace,
            tools: vec![spec],
        }
    }

    /// A workspace's audience, keyed on its root the way an open keys it.
    fn audience_for(root: &str) -> ToolAudience {
        crate::runtime::probe::audience_for(Path::new(root))
    }

    /// Whether mentra's registry answers to `name` for `root`'s audience — the
    /// only honest answer to "is this tool on the runtime", since basis's claim
    /// map is a separate ledger that could disagree. See
    /// [`crate::runtime::probe`] for why a read is written as a write.
    fn registers(runtime: &Runtime, root: &str, name: &str) -> bool {
        crate::runtime::probe::answers(runtime, Path::new(root), name)
    }

    #[test]
    fn a_declared_tool_reaches_the_model_under_the_name_the_file_gave_it() {
        let runtime = runtime();
        let sources = [source("/repo/.basis/tools.json", &["jenkins_job"])];

        let registered = DeclaredTools::register(
            Arc::clone(&runtime),
            &audience_for("/repo"),
            Path::new("/repo"),
            &sources,
        )
        .expect("registers");

        assert_eq!(registered.names(), ["jenkins_job"]);
        assert!(
            registers(&runtime, "/repo", "jenkins_job"),
            "the declaring workspace's own audience answers to it"
        );
        assert!(
            !registers(&runtime, "/elsewhere", "jenkins_job"),
            "and a sibling workspace's does not"
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

        let error = DeclaredTools::register(
            runtime,
            &audience_for("/repo"),
            Path::new("/repo"),
            &sources,
        )
        .expect_err("refused");

        assert!(
            matches!(error, DeclaredToolError::NameTaken { .. }),
            "{error}"
        );
        assert!(error.to_string().contains(crate::tools::SPAWN), "{error}");
    }

    #[test]
    fn a_manifest_cannot_take_over_a_mentra_builtin_either() {
        // Any registered name, not only basis's own. `edit` rather than the
        // `files` this used to name, because a basis runtime now offers
        // mentra's split file tools — the rule is unchanged, the roster it
        // reads is the thing that moved
        // (`RuntimeBuilder::with_file_tools`).
        let runtime = runtime();
        let sources = [source("/repo/.basis/tools.json", &["edit"])];

        let error = DeclaredTools::register(
            runtime,
            &audience_for("/repo"),
            Path::new("/repo"),
            &sources,
        )
        .expect_err("refused");

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
            &audience_for("/repo/one"),
            Path::new("/repo/one"),
            &[source("/repo/one/.basis/tools.json", &["deploy"])],
        )
        .expect("the first claimant registers");

        let error = DeclaredTools::register(
            Arc::clone(&runtime),
            &audience_for("/repo/two"),
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
        assert!(
            error.to_string().contains("/repo/one"),
            "and the claimant it collided with, which is the other half of the fix: {error}"
        );

        drop(first);

        DeclaredTools::register(
            runtime,
            &audience_for("/repo/two"),
            Path::new("/repo/two"),
            &[source("/repo/two/.basis/tools.json", &["deploy"])],
        )
        .expect("a released name is claimable again");
    }

    #[test]
    fn a_dropped_workspace_takes_its_tools_off_the_runtime() {
        // What a workspace registered on a runtime it borrows lives exactly as
        // long as the workspace does. Before mentra had an unregister these
        // entries stayed forever — hidden from every other workspace's roster,
        // but still on a registry a long-running host keeps for its whole
        // process.
        let runtime = runtime();
        let sources = [source("/repo/.basis/tools.json", &["deploy"])];

        let held = DeclaredTools::register(
            Arc::clone(&runtime),
            &audience_for("/repo"),
            Path::new("/repo"),
            &sources,
        )
        .expect("registers");
        assert!(registers(&runtime, "/repo", "deploy"));

        drop(held);

        assert!(
            !registers(&runtime, "/repo", "deploy"),
            "the workspace is gone and so is what it declared"
        );
    }

    #[test]
    fn a_workspace_can_be_reopened_after_its_last_open_released_the_name() {
        let runtime = runtime();
        let sources = [source("/repo/.basis/tools.json", &["deploy"])];

        let first = DeclaredTools::register(
            Arc::clone(&runtime),
            &audience_for("/repo"),
            Path::new("/repo"),
            &sources,
        )
        .expect("registers");
        drop(first);

        DeclaredTools::register(
            runtime,
            &audience_for("/repo"),
            Path::new("/repo"),
            &sources,
        )
        .expect("the same workspace registers its own tool again");
    }

    #[test]
    fn a_second_open_does_not_swap_the_program_the_first_one_is_serving() {
        // Both opens hold the same name, so there is one tool and a precedence
        // question — the module's own rule. The live registration answers it:
        // an agent running in the first workspace must not have the program
        // under its feet replaced because somebody opened the repository again.
        let runtime = runtime();
        let serving = |name: &str, description: &str| ToolsSource {
            path: PathBuf::from("/repo/.basis/tools.json"),
            scope: ContextScope::Workspace,
            tools: vec![DeclaredToolSpec {
                description: description.to_string(),
                ..spec(name)
            }],
        };

        let _first = DeclaredTools::register(
            Arc::clone(&runtime),
            &audience_for("/repo"),
            Path::new("/repo"),
            &[serving("deploy", "the first open's program")],
        )
        .expect("registers");
        let _second = DeclaredTools::register(
            Arc::clone(&runtime),
            &audience_for("/repo"),
            Path::new("/repo"),
            &[serving("deploy", "the second open's program")],
        )
        .expect("registers");

        assert_eq!(
            runtime
                .declared_tool_descriptor("deploy")
                .expect("registered")
                .provider
                .description
                .as_deref(),
            Some("the first open's program")
        );
    }

    #[test]
    fn two_opens_of_one_workspace_both_keep_their_tools() {
        let runtime = runtime();
        let sources = [source("/repo/.basis/tools.json", &["deploy"])];

        let first = DeclaredTools::register(
            Arc::clone(&runtime),
            &audience_for("/repo"),
            Path::new("/repo"),
            &sources,
        )
        .expect("registers");
        let second = DeclaredTools::register(
            Arc::clone(&runtime),
            &audience_for("/repo"),
            Path::new("/repo"),
            &sources,
        )
        .expect("registers");

        drop(first);
        assert!(
            registers(&runtime, "/repo", "deploy"),
            "one holder went, the other is still serving it"
        );

        // The second is still open, so the name is still spoken for.
        let error = DeclaredTools::register(
            Arc::clone(&runtime),
            &audience_for("/elsewhere"),
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
            &audience_for("/elsewhere"),
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
            &audience_for("/repo/one"),
            Path::new("/repo/one"),
            &[source("/repo/one/.basis/tools.json", &["taken"])],
        )
        .expect("registers");

        let error = DeclaredTools::register(
            Arc::clone(&runtime),
            &audience_for("/repo/two"),
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
            &audience_for("/repo/three"),
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
            &audience_for("/repo/one"),
            Path::new("/repo/one"),
            &[source("/repo/one/.basis/tools.json", &["deploy"])],
        )
        .expect("registers");

        assert!(
            registers(&runtime, "/repo/one", "deploy"),
            "the declaring repository's runs can reach the program it declared"
        );
        assert!(
            !registers(&runtime, "/repo/two", "deploy"),
            "a program one repository declared is not the other's to run"
        );
    }

    #[test]
    fn supplied_same_root_holders_must_match_command_and_environment() {
        let first = DeclaredToolSpec {
            command: vec!["./first".to_string()],
            env: vec![("TOKEN".to_string(), "first-secret".to_string())],
            ..spec("deploy")
        };
        for differing in [
            DeclaredToolSpec {
                command: vec!["./second".to_string()],
                ..first.clone()
            },
            DeclaredToolSpec {
                env: vec![("TOKEN".to_string(), "second-secret".to_string())],
                ..first.clone()
            },
        ] {
            let runtime = runtime();
            let _held = DeclaredTools::register_with_supplied(
                Arc::clone(&runtime),
                &audience_for("/repo"),
                Path::new("/repo"),
                &[],
                std::slice::from_ref(&first),
            )
            .expect("the first supplied declaration registers");

            let error = DeclaredTools::register_with_supplied(
                runtime,
                &audience_for("/repo"),
                Path::new("/repo"),
                &[],
                &[differing],
            )
            .expect_err("a different supplied implementation cannot join the live name");

            assert!(matches!(
                &error,
                DeclaredToolError::SuppliedNameTaken { .. }
            ));
            let message = error.to_string();
            assert!(message.contains("deploy"), "{message}");
            assert!(message.contains("different configuration"), "{message}");
            assert!(!message.contains("first-secret"), "{message}");
            assert!(!message.contains("second-secret"), "{message}");
            assert!(!message.contains(".basis/tools.json"), "{message}");
        }
    }

    #[test]
    fn mixed_same_root_origins_refuse_mismatches_in_both_orders() {
        let file = DeclaredToolSpec {
            command: vec!["./from-file".to_string()],
            env: vec![("TOKEN".to_string(), "file-secret".to_string())],
            ..spec("deploy")
        };
        let supplied = DeclaredToolSpec {
            command: vec!["./from-supplied".to_string()],
            env: vec![("TOKEN".to_string(), "supplied-secret".to_string())],
            ..spec("deploy")
        };
        let path = "/repo/.basis/tools.json";

        {
            let runtime = runtime();
            let _file = DeclaredTools::register(
                Arc::clone(&runtime),
                &audience_for("/repo"),
                Path::new("/repo"),
                &[source_with(path, file.clone())],
            )
            .expect("the file declaration registers first");
            let error = DeclaredTools::register_with_supplied(
                runtime,
                &audience_for("/repo"),
                Path::new("/repo"),
                &[],
                std::slice::from_ref(&supplied),
            )
            .expect_err("a differing supplied declaration cannot join a live file claim");

            assert!(matches!(error, DeclaredToolError::SuppliedNameTaken { .. }));
            let message = error.to_string();
            assert!(!message.contains(path), "{message}");
            assert!(!message.contains("file-secret"), "{message}");
            assert!(!message.contains("supplied-secret"), "{message}");
        }

        {
            let runtime = runtime();
            let _supplied = DeclaredTools::register_with_supplied(
                Arc::clone(&runtime),
                &audience_for("/repo"),
                Path::new("/repo"),
                &[],
                std::slice::from_ref(&supplied),
            )
            .expect("the supplied declaration registers first");
            let error = DeclaredTools::register(
                runtime,
                &audience_for("/repo"),
                Path::new("/repo"),
                &[source_with(path, file)],
            )
            .expect_err("a differing file declaration cannot join a live supplied claim");

            assert!(matches!(&error, DeclaredToolError::NameTaken { .. }));
            let message = error.to_string();
            assert!(message.contains(path), "{message}");
            assert!(!message.contains("file-secret"), "{message}");
            assert!(!message.contains("supplied-secret"), "{message}");
        }
    }
}
