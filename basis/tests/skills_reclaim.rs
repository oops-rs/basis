//! Taking a skills root back when the workspace that registered it goes.
//!
//! A shared `Runtime` (ADR-0018) outlives every `Workspace` opened on it, and
//! skills live on the runtime's single registry. Until mentra 0.24 there was
//! no way to withdraw a root, so a host that opened one repository after
//! another accumulated every repository's skills for the life of the process —
//! a name a run resolved could belong to a checkout closed an hour ago. These
//! tests pin the four claims the reclaim rests on:
//!
//! 1. **A dropped workspace takes its roots off.** A private runtime is
//!    dropped with the workspace and so has nothing left to observe; the
//!    guard is written once for both shapes, and `skills::registration`'s
//!    own unit test is where that uniformity is pinned.
//! 2. **A shared root survives its co-holder.** Two workspaces open on one
//!    runtime both register the user's global roots; the first to go must not
//!    free what the second is still serving.
//! 3. **A failed open leaves nothing registered.** mentra's
//!    `register_skills_dirs` is all-or-nothing, and basis's own guard drops
//!    before an open that fails later can return.
//! 4. **Sharing is real while it lasts.** A run in one workspace can
//!    `load_skill` a sibling's skill for as long as the sibling is open, and
//!    cannot once it is not — asserted through an actual tool call, because
//!    "unreachable" is a claim about `load_skill` and not about a listing.
//!
//! No network: the provider is a scripted in-process instance, so the only
//! thing under test is which skills the runtime answers to.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use basis::{
    AllowAll, CollectingSink, ContextConfig, MemoryConfig, ModelInfo, Provider, RunOutcome,
    Runtime, RuntimeBuilder, Workspace, WorkspaceBuilder, async_trait,
    hooks::HooksConfig,
    runtime::{
        ContentBlock, ProviderCapabilities, ProviderDescriptor, ProviderError, ProviderEventStream,
        Request, Response, Role, provider_event_stream_from_response,
    },
    skills::SkillsConfig,
    templates::TemplatesConfig,
    tools::declared::ToolsConfig,
};
use serde_json::json;

const PROVIDER: &str = "skills-provider";
const MODEL: &str = "skills-model";
const FINAL_MESSAGE: &str = "SKILL_RUN_COMPLETE";

/// A provider that asks for one skill by name and then stops.
///
/// The first turn is a `load_skill` call, the second is a final message. What
/// the tool answered is recorded from the *next* request's tool results, which
/// is the only place a host can read it — and the difference between a skill
/// that loaded and one that was refused is entirely in that string.
#[derive(Clone)]
struct SkillCaller {
    skill: String,
    results: Arc<Mutex<Vec<String>>>,
    streams: Arc<AtomicUsize>,
}

impl SkillCaller {
    fn new(skill: &str) -> Self {
        Self {
            skill: skill.to_string(),
            results: Arc::new(Mutex::new(Vec::new())),
            streams: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn results(&self) -> Vec<String> {
        self.results.lock().expect("tool results").clone()
    }
}

#[async_trait]
impl Provider for SkillCaller {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(PROVIDER)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_model_listing: true,
            supports_streaming: true,
            supports_tool_calls: true,
            ..Default::default()
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![resolved_model()])
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let turn = self.streams.fetch_add(1, Ordering::SeqCst);
        let mut answered = false;
        for block in request.messages.iter().flat_map(|message| &message.content) {
            if let ContentBlock::ToolResult { content, .. } = block {
                answered = true;
                self.results
                    .lock()
                    .expect("tool results")
                    .push(content.to_display_string());
            }
        }

        // Scripted off the conversation rather than off a counter, so a second
        // run on the same provider asks for the skill again instead of
        // resuming where the first left off.
        let (content, stop_reason) = if !answered {
            (
                vec![ContentBlock::ToolUse {
                    id: "skill-call-1".to_string(),
                    name: "load_skill".to_string(),
                    input: json!({ "name": self.skill }),
                }],
                Some("tool_use".to_string()),
            )
        } else {
            (vec![ContentBlock::text(FINAL_MESSAGE)], None)
        };

        Ok(provider_event_stream_from_response(Response {
            id: format!("skill-response-{turn}"),
            model: request.model.to_string(),
            role: Role::Assistant,
            content,
            stop_reason,
            usage: None,
        }))
    }
}

fn resolved_model() -> ModelInfo {
    ModelInfo::new(MODEL, PROVIDER).with_context_window(262_144)
}

fn runtime_builder(provider: SkillCaller) -> RuntimeBuilder {
    Runtime::builder()
        .with_provider_instance(provider)
        .with_ephemeral_history()
}

/// A workspace that reads exactly two roots: its own `.basis/skills`, and the
/// `skills/` under whichever global directory the test names. Nothing else on
/// the discovery path is enabled, so a machine's own `$HOME` cannot change an
/// assertion here.
fn builder(workspace: &Path, global: Option<&Path>) -> WorkspaceBuilder {
    Workspace::builder(workspace)
        .with_resolved_model(resolved_model())
        .with_context(ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: None,
            walk_parents: false,
        })
        .with_skills(SkillsConfig {
            workspace_subdir: Some(PathBuf::from(".basis/skills")),
            shared_workspace_dir: true,
            global_dir: global.map(Path::to_path_buf),
            shared_home_dir: false,
        })
        .with_memory(MemoryConfig::disabled())
        .with_templates(TemplatesConfig {
            workspace_subdir: PathBuf::from(".basis/templates"),
            global_dir: None,
        })
        .with_hooks(HooksConfig {
            workspace_file: PathBuf::from(".basis/hooks.json"),
            global_dir: None,
            supplied: Vec::new(),
        })
        .with_tools(ToolsConfig {
            workspace_file: PathBuf::from(".basis/tools.json"),
            global_dir: None,
            supplied: Vec::new(),
        })
}

fn skill(path: &Path, name: &str, description: &str, body: &str) {
    write(
        &path.join(name).join("SKILL.md"),
        &format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
    );
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create dir");
    std::fs::write(path, body).expect("write file");
}

/// Every skill name the runtime currently answers to.
fn registered(runtime: &Runtime) -> Vec<String> {
    let mut names: Vec<String> = runtime
        .mentra_runtime()
        .skills()
        .into_iter()
        .map(|skill| skill.name)
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn a_dropped_workspace_takes_its_roots_off_a_shared_runtime() {
    let dir = tempfile::tempdir().expect("tempdir");
    skill(
        &dir.path().join(".basis/skills"),
        "release",
        "cut one",
        "Steps.",
    );

    let runtime = Arc::new(
        runtime_builder(SkillCaller::new("release"))
            .build()
            .expect("the shared runtime builds"),
    );

    let workspace = builder(dir.path(), None)
        .with_runtime(Arc::clone(&runtime))
        .open()
        .await
        .expect("the workspace opens");
    assert_eq!(registered(&runtime), ["release"]);

    drop(workspace);

    assert!(
        registered(&runtime).is_empty(),
        "the root goes with the workspace that registered it"
    );
}

#[tokio::test]
async fn a_root_two_workspaces_share_survives_the_first_of_them() {
    // Both workspaces register the same global root, which is what every
    // workspace on a host with a `~/.agents/skills` does. The first to drop
    // must free only what it alone held.
    let global = tempfile::tempdir().expect("global dir");
    skill(
        &global.path().join("skills"),
        "personal",
        "user-wide",
        "Steps.",
    );
    let first = tempfile::tempdir().expect("first workspace");
    skill(
        &first.path().join(".basis/skills"),
        "first",
        "one repo",
        "Steps.",
    );
    let second = tempfile::tempdir().expect("second workspace");
    skill(
        &second.path().join(".basis/skills"),
        "second",
        "another",
        "Steps.",
    );

    let runtime = Arc::new(
        runtime_builder(SkillCaller::new("personal"))
            .build()
            .expect("the shared runtime builds"),
    );

    let one = builder(first.path(), Some(global.path()))
        .with_runtime(Arc::clone(&runtime))
        .open()
        .await
        .expect("the first workspace opens");
    let two = builder(second.path(), Some(global.path()))
        .with_runtime(Arc::clone(&runtime))
        .open()
        .await
        .expect("the second workspace opens");

    assert_eq!(registered(&runtime), ["first", "personal", "second"]);

    drop(one);

    assert_eq!(
        registered(&runtime),
        ["personal", "second"],
        "the shared global root is still held by the workspace that is open"
    );

    drop(two);

    assert!(registered(&runtime).is_empty());
}

#[tokio::test]
async fn an_open_that_fails_on_a_second_root_registers_neither() {
    // mentra 0.24 loads and validates every root before committing any, so a
    // batch with a bad root leaves the registry exactly as it was. The
    // sibling's skill is here to prove the assertion can tell "unchanged"
    // from "emptied".
    let sibling = tempfile::tempdir().expect("sibling workspace");
    skill(
        &sibling.path().join(".basis/skills"),
        "sibling",
        "already open",
        "Steps.",
    );
    let dir = tempfile::tempdir().expect("tempdir");
    skill(
        &dir.path().join(".basis/skills"),
        "valid",
        "loads fine",
        "Steps.",
    );
    // The second root basis discovers, with frontmatter mentra refuses.
    write(
        &dir.path().join(".agents/skills/broken/SKILL.md"),
        "---\nname: [not a string\n---\nbody",
    );

    let runtime = Arc::new(
        runtime_builder(SkillCaller::new("valid"))
            .build()
            .expect("the shared runtime builds"),
    );
    let held = builder(sibling.path(), None)
        .with_runtime(Arc::clone(&runtime))
        .open()
        .await
        .expect("the sibling workspace opens");

    let before = registered(&runtime);
    assert_eq!(before, ["sibling"]);

    let error = builder(dir.path(), None)
        .with_runtime(Arc::clone(&runtime))
        .open()
        .await
        .expect_err("a root mentra cannot load fails the open");
    assert!(error.to_string().contains("frontmatter"), "{error}");

    assert_eq!(
        registered(&runtime),
        before,
        "a failed open leaves the runtime exactly as it found it"
    );

    drop(held);
}

#[tokio::test]
async fn a_sibling_skill_is_loadable_while_its_workspace_is_open_and_not_after() {
    // The whole of what sharing a runtime shares, and the whole of what
    // reclaiming takes back — asserted through a real `load_skill` call,
    // because unreachable is a claim about the tool and not about a listing.
    let owner = tempfile::tempdir().expect("owning workspace");
    skill(
        &owner.path().join(".basis/skills"),
        "release",
        "cut a release",
        "OWNER_SKILL_BODY_MARKER",
    );
    let other = tempfile::tempdir().expect("other workspace");
    // Its own root, so `load_skill` is still registered after the sibling
    // goes: this test is about a skill being refused, not about the tool
    // being withdrawn with the last root.
    skill(
        &other.path().join(".basis/skills"),
        "own",
        "the neighbour's own",
        "Steps.",
    );

    let provider = SkillCaller::new("release");
    let runtime = Arc::new(
        runtime_builder(provider.clone())
            .build()
            .expect("the shared runtime builds"),
    );

    let owning = builder(owner.path(), None)
        .with_runtime(Arc::clone(&runtime))
        .open()
        .await
        .expect("the owning workspace opens");
    let neighbour = builder(other.path(), None)
        .with_runtime(Arc::clone(&runtime))
        .open()
        .await
        .expect("the neighbouring workspace opens");

    let report = neighbour
        .prepare("load the sibling's skill")
        .expect("mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the run completes");
    assert!(matches!(report.outcome, RunOutcome::Ok));

    let shared = provider.results();
    assert!(
        shared
            .iter()
            .any(|result| result.contains("OWNER_SKILL_BODY_MARKER")),
        "while both are open, sharing a runtime shares the skill: {shared:?}"
    );

    drop(owning);

    let report = neighbour
        .prepare("load the sibling's skill again")
        .expect("mints")
        .execute_with_approver(CollectingSink::default(), AllowAll)
        .await
        .expect("the run completes");
    assert!(matches!(report.outcome, RunOutcome::Ok));

    let after = provider.results();
    let refusals = &after[shared.len()..];
    assert!(
        !refusals.is_empty(),
        "the second run asked for the skill too"
    );
    assert!(
        refusals
            .iter()
            .all(|result| !result.contains("OWNER_SKILL_BODY_MARKER")),
        "the dropped workspace's skill is unreachable, not merely unlisted: {refusals:?}"
    );
}

#[tokio::test]
async fn a_loaded_skill_names_the_root_it_came_from() {
    // `SkillInfo::root` carried out to the host: with several roots
    // registered, attribution is the only way a host can say which of them a
    // name resolved to — and it is the same path it would hand back to close
    // that root.
    let global = tempfile::tempdir().expect("global dir");
    skill(
        &global.path().join("skills"),
        "personal",
        "user-wide",
        "Steps.",
    );
    let dir = tempfile::tempdir().expect("tempdir");
    skill(
        &dir.path().join(".basis/skills"),
        "release",
        "cut one",
        "Steps.",
    );

    let workspace = builder(dir.path(), Some(global.path()))
        .with_runtime_builder(runtime_builder(SkillCaller::new("release")))
        .open()
        .await
        .expect("the workspace opens");

    let attributed: Vec<(&str, &Path)> = workspace
        .skills()
        .iter()
        .map(|skill| (skill.name.as_str(), skill.root.as_path()))
        .collect();

    // The workspace root is canonical by the time discovery joins the subdir
    // onto it (`WorkspaceBuilder::open`'s one resolution), so the expectation
    // has to be too — on macOS a tempdir is reached through a symlink.
    let workspace_root = std::fs::canonicalize(dir.path()).expect("canonical workspace");
    assert_eq!(
        attributed,
        [
            ("personal", global.path().join("skills").as_path()),
            ("release", workspace_root.join(".basis/skills").as_path()),
        ]
    );
}

#[tokio::test]
async fn an_open_that_fails_after_the_skills_are_registered_still_takes_them_back() {
    // The atomicity above is mentra's. This is basis's half: the roots are
    // registered before the tool manifests are claimed, so an open refused any
    // time after that point has to hand them back — otherwise a shared runtime
    // keeps the skills of a workspace that never opened. The manifest here
    // declares `spawn`, a name the runtime already answers to, which
    // `DeclaredTools::register` refuses.
    let dir = tempfile::tempdir().expect("tempdir");
    skill(
        &dir.path().join(".basis/skills"),
        "orphan",
        "never opens",
        "Steps.",
    );
    write(
        &dir.path().join(".basis/tools.json"),
        &json!({
            "schema": 1,
            "tools": {
                "spawn": {
                    "description": "takes a name the runtime already answers to",
                    "input_schema": {"type": "object", "properties": {}},
                    "command": ["/bin/echo", "hi"]
                }
            }
        })
        .to_string(),
    );

    let runtime = Arc::new(
        runtime_builder(SkillCaller::new("orphan"))
            .build()
            .expect("the shared runtime builds"),
    );

    let error = builder(dir.path(), None)
        .with_runtime(Arc::clone(&runtime))
        .open()
        .await
        .expect_err("a declared tool cannot take the runtime's own name");
    assert!(error.to_string().contains("spawn"), "{error}");

    assert!(
        registered(&runtime).is_empty(),
        "a refused open leaves no skill behind on the runtime it borrowed"
    );
}
