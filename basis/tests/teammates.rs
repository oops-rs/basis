//! ADR-0037 §5–§7, driven: a name no remote agent has makes a teammate the
//! first time and continues it after, the model is told the teammate's name
//! and id from the next request on, and a child's teammate is its own.
//!
//! One scripted provider answers parent and teammate alike, in the order
//! the turns happen, and records what each request carried: how many
//! messages, and what `spawn` was described as.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use basis::{
    AllowAll, CollectingSink, ContextConfig, Event, MemoryConfig, ModelInfo, ModelSelector,
    Runtime, TurnOptions, Workspace, WorkspaceBuilder, async_trait,
    runtime::{
        ContentBlock, ProviderCapabilities, ProviderDescriptor, ProviderError, ProviderEventStream,
        Request, Response, Role, provider_event_stream_from_response,
    },
    skills::SkillsConfig,
    templates::TemplatesConfig,
    tools::{SPAWN, spawn::Reach, spawn::ReachPolicy},
};
use serde_json::json;

const PROVIDER: &str = "teammates-provider";
const MODEL: &str = "teammates-model";

/// What one request carried.
#[derive(Debug, Clone)]
struct Asked {
    messages: usize,
    spawn: String,
}

/// One scripted assistant turn.
#[derive(Debug, Clone)]
struct Turn(Vec<ContentBlock>);

/// A scripted turn, or one written when its request arrives — for a call
/// that needs something only an earlier reply told the model, such as an id.
#[derive(Clone)]
enum Step {
    Fixed(Turn),
    Dynamic(Arc<dyn Fn(&Asked) -> Turn + Send + Sync>),
}

impl Turn {
    fn calling(id: &str, input: &str) -> Self {
        Self(vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: SPAWN.to_string(),
            input: json!({ "input": input }),
        }])
    }

    fn calling_reaching(id: &str, input: &str, reach: Reach) -> Self {
        Self(vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: SPAWN.to_string(),
            input: json!({ "input": input, "reach": reach.as_str() }),
        }])
    }

    fn saying(text: &str) -> Self {
        Self(vec![ContentBlock::text(text)])
    }
}

#[derive(Clone)]
struct Scripted {
    steps: Arc<Mutex<VecDeque<Step>>>,
    asked: Arc<Mutex<Vec<Asked>>>,
}

impl Scripted {
    fn new(turns: Vec<Turn>) -> Self {
        Self::stepping(turns.into_iter().map(Step::Fixed).collect())
    }

    fn stepping(steps: Vec<Step>) -> Self {
        Self {
            steps: Arc::new(Mutex::new(steps.into())),
            asked: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn asked(&self) -> Vec<Asked> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl basis::Provider for Scripted {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(PROVIDER)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_streaming: true,
            supports_tool_calls: true,
            ..Default::default()
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![model()])
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let asked = Asked {
            messages: request.messages.len(),
            spawn: request
                .tools
                .iter()
                .find(|tool| tool.name == SPAWN)
                .and_then(|tool| tool.description.clone())
                .unwrap_or_default(),
        };
        self.asked.lock().unwrap().push(asked.clone());
        let turn = match self.steps.lock().unwrap().pop_front() {
            Some(Step::Fixed(turn)) => turn,
            Some(Step::Dynamic(write)) => write(&asked),
            None => Turn::saying("done"),
        };
        Ok(provider_event_stream_from_response(Response {
            id: "scripted".to_string(),
            model: MODEL.to_string(),
            role: Role::Assistant,
            content: turn.0,
            stop_reason: None,
            usage: None,
        }))
    }
}

fn model() -> ModelInfo {
    ModelInfo::new(MODEL, PROVIDER).with_context_window(64_000)
}

fn runtime(scripted: &Scripted) -> Arc<Runtime> {
    Arc::new(
        Runtime::builder()
            .with_provider_instance(scripted.clone())
            .with_ephemeral_history()
            .build()
            .expect("builds"),
    )
}

fn runtime_reaching(scripted: &Scripted, policy: ReachPolicy) -> Arc<Runtime> {
    Arc::new(
        Runtime::builder()
            .with_provider_instance(scripted.clone())
            .with_ephemeral_history()
            .with_reach_policy(policy)
            .build()
            .expect("builds"),
    )
}

/// Discovery pinned, so the developer's own configuration never reaches a
/// test.
fn pinned(workspace: &Path, runtime: Arc<Runtime>) -> WorkspaceBuilder {
    let builder = Workspace::builder(workspace)
        .with_runtime(runtime)
        .with_model(ModelSelector::Id(MODEL.to_string()))
        .with_resolved_model(model())
        .with_context(ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: None,
            walk_parents: false,
        })
        .with_skills(SkillsConfig {
            workspace_subdir: None,
            shared_workspace_dir: false,
            global_dir: None,
            shared_home_dir: false,
        })
        .with_templates(TemplatesConfig {
            workspace_subdir: PathBuf::from(".basis/templates"),
            global_dir: None,
        })
        .with_memory(MemoryConfig::disabled());
    #[cfg(feature = "a2a")]
    let builder = builder.with_a2a(basis::A2aConfig {
        global_dir: None,
        ..basis::A2aConfig::default()
    });
    builder
}

/// Every `spawn` result the parent's stream carried, in order.
fn results(events: &[Event]) -> Vec<(bool, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::ToolCompleted {
                tool_name,
                is_error,
                summary,
                ..
            } if tool_name == SPAWN => Some((*is_error, summary.clone())),
            _ => None,
        })
        .collect()
}

async fn drive(workspace: &Workspace) -> Vec<Event> {
    let mut run = workspace.prepare("go").expect("mints");
    let report = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        run.execute_with_approver_and_options(
            CollectingSink::new(),
            AllowAll,
            TurnOptions::default(),
        ),
    )
    .await
    .expect("the run must not hang")
    .expect("the run completes");
    report.sink.into_events()
}

#[tokio::test]
async fn a_teammate_is_made_once_and_continued_by_name() {
    let scripted = Scripted::new(vec![
        // 1: the parent hands `reviewer` a task; nobody remote has the name.
        Turn::calling("call-1", "@reviewer look at the diff"),
        // 2: the teammate's first turn.
        Turn::saying("I see the diff. Which file matters most?"),
        // 3: the parent answers the question by name.
        Turn::calling("call-2", "@reviewer main.rs"),
        // 4: the teammate's second turn, on the same agent.
        Turn::saying("main.rs is fine."),
        // 5: the parent finishes.
        Turn::saying("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = pinned(dir.path(), runtime(&scripted))
        .open()
        .await
        .expect("opens");

    let events = drive(&workspace).await;

    let results = results(&events);
    assert_eq!(results.len(), 2, "{results:?}");
    let (failed, first) = &results[0];
    assert!(!failed, "{first}");
    assert!(
        first.starts_with("Made teammate `reviewer` (id `"),
        "{first}"
    );
    assert!(first.ends_with("Which file matters most?"), "{first}");
    let (failed, second) = &results[1];
    assert!(!failed, "{second}");
    assert!(second.starts_with("Teammate `reviewer` (id `"), "{second}");
    assert!(second.ends_with("main.rs is fine."), "{second}");
    let id_of = |reply: &str| {
        reply
            .split("(id `")
            .nth(1)
            .and_then(|rest| rest.split('`').next())
            .map(str::to_string)
            .expect("an id")
    };
    assert_eq!(id_of(first), id_of(second), "the same agent both times");

    let asked = scripted.asked();
    assert_eq!(asked.len(), 5, "{asked:?}");
    assert_eq!(asked[1].messages, 1, "a fresh teammate reads one message");
    assert_eq!(
        asked[3].messages, 3,
        "its second turn carries the first exchange: {:?}",
        asked[3]
    );
    assert!(
        !asked[0].spawn.contains("Your teammates:"),
        "none made yet: {}",
        asked[0].spawn
    );
    assert!(
        asked[2]
            .spawn
            .contains(&format!("- `reviewer` (id `{}`)", id_of(first))),
        "told from the next request on: {}",
        asked[2].spawn
    );
    assert!(
        !asked[1].spawn.contains("Your teammates:"),
        "the teammate's own list is its own, and empty: {}",
        asked[1].spawn
    );
}

#[tokio::test]
async fn a_teammate_is_reached_by_its_id_too() {
    let scripted = Scripted::stepping(vec![
        Step::Fixed(Turn::calling("call-1", "@scout find the entry point")),
        Step::Fixed(Turn::saying("main.rs")),
        // The parent reads the id off what `spawn` now teaches it, and calls
        // by that rather than by name.
        Step::Dynamic(Arc::new(|asked: &Asked| {
            let id = asked
                .spawn
                .split("- `scout` (id `")
                .nth(1)
                .and_then(|rest| rest.split('`').next())
                .expect("the teammate is listed with its id");
            Turn::calling("call-2", &format!("@{id} and the tests?"))
        })),
        Step::Fixed(Turn::saying("tests/")),
        Step::Fixed(Turn::saying("done")),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = pinned(dir.path(), runtime(&scripted))
        .open()
        .await
        .expect("opens");

    let results = results(&drive(&workspace).await);

    assert_eq!(results.len(), 2, "{results:?}");
    assert!(
        results[1].1.starts_with("Teammate `scout` (id `"),
        "reached by id, still called by name: {}",
        results[1].1
    );
    assert!(results[1].1.ends_with("tests/"), "{}", results[1].1);
    assert_eq!(scripted.asked()[3].messages, 3, "the same agent, continued");
}

#[tokio::test]
async fn reach_remote_refuses_a_teammate_and_a_fixed_policy_refuses_the_field() {
    let scripted = Scripted::new(vec![
        Turn::calling_reaching("call-1", "@reviewer look", Reach::Remote),
        Turn::saying("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = pinned(dir.path(), runtime(&scripted))
        .open()
        .await
        .expect("opens");

    let refused = results(&drive(&workspace).await);
    let (failed, text) = &refused[0];
    assert!(failed, "{text}");
    assert!(text.contains("no remote agents"), "{text}");

    let scripted = Scripted::new(vec![
        Turn::calling_reaching("call-1", "@reviewer look", Reach::Local),
        Turn::saying("done"),
    ]);
    let dir = tempfile::tempdir().unwrap();
    let workspace = pinned(
        dir.path(),
        runtime_reaching(&scripted, ReachPolicy::fixed(Reach::Remote)),
    )
    .open()
    .await
    .expect("opens");

    let refused = results(&drive(&workspace).await);
    let (failed, text) = &refused[0];
    assert!(failed, "{text}");
    assert!(text.contains("`reach: local` is not allowed"), "{text}");
    assert!(
        !scripted.asked()[0].spawn.contains("`reach`"),
        "no choice, nothing taught: {}",
        scripted.asked()[0].spawn
    );
}
