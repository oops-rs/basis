//! ADR-0042, driven: a delegation started in the background returns its
//! handle at once, its answer reaches the model on its own or through a
//! wait, the turn does not end while one is still running, and the width
//! holds.
//!
//! One provider answers parent and children alike. Children run at the same
//! time, so the order requests arrive in is not scripted: each request is
//! answered by what it carries — the task a child was handed, or the tool
//! result the parent just read — and the provider keeps a log of what it saw.

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use basis::{
    AllowAll, CollectingSink, ContextConfig, Event, MemoryConfig, ModelInfo, ModelSelector,
    Runtime, TurnOptions, Workspace, WorkspaceBuilder, async_trait,
    event::{TaskKind, TaskStatus},
    provider_core::ToolResultContent,
    runtime::{
        ContentBlock, ProviderCapabilities, ProviderDescriptor, ProviderError, ProviderEventStream,
        Request, Response, Role, provider_event_stream_from_response,
    },
    skills::SkillsConfig,
    templates::TemplatesConfig,
    tools::SPAWN,
};
use serde_json::{Value, json};

const PROVIDER: &str = "background-provider";
const MODEL: &str = "background-model";

/// What one request carried, as the router reads it.
#[derive(Debug, Clone)]
struct Seen {
    /// The text of the last user message, for a child's first request and for
    /// the parent's prompt.
    last_text: String,
    /// The ids of the tool results in the last message, for the parent's
    /// round after a `spawn` call.
    tool_results: Vec<(String, String)>,
    /// Every user-side text in the request, for the parent's round after
    /// results arrived on their own.
    all_text: String,
}

impl Seen {
    fn result_of(&self, call: &str) -> Option<&str> {
        self.tool_results
            .iter()
            .find(|(id, _)| id == call)
            .map(|(_, text)| text.as_str())
    }
}

/// One scripted assistant turn.
#[derive(Debug, Clone)]
struct Turn(Vec<ContentBlock>);

impl Turn {
    fn calling(id: &str, input: Value) -> Self {
        Self(vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: SPAWN.to_string(),
            input,
        }])
    }

    fn saying(text: &str) -> Self {
        Self(vec![ContentBlock::text(text)])
    }
}

type Route = Arc<dyn Fn(&Seen) -> (Turn, Duration) + Send + Sync>;

#[derive(Clone)]
struct Router {
    route: Route,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Router {
    fn new(route: impl Fn(&Seen) -> (Turn, Duration) + Send + Sync + 'static) -> Self {
        Self {
            route: Arc::new(route),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

fn now(turn: Turn) -> (Turn, Duration) {
    (turn, Duration::ZERO)
}

#[async_trait]
impl basis::Provider for Router {
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
        let last = request.messages.last();
        let seen = Seen {
            last_text: last.map(|message| message.text()).unwrap_or_default(),
            tool_results: last
                .map(|message| {
                    message
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::ToolResult {
                                tool_use_id,
                                content: ToolResultContent::Text(text),
                                ..
                            } => Some((tool_use_id.clone(), text.clone())),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default(),
            all_text: request
                .messages
                .iter()
                .filter(|message| message.role == Role::User)
                .map(|message| message.text())
                .collect::<Vec<_>>()
                .join("\n"),
        };
        self.seen.lock().unwrap().push(seen.clone());
        let (turn, delay) = (self.route)(&seen);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
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

fn runtime(router: &Router, width: Option<usize>) -> Arc<Runtime> {
    let builder = Runtime::builder()
        .with_provider_instance(router.clone())
        .with_ephemeral_history();
    let builder = match width {
        Some(width) => builder.with_delegation_width(width),
        None => builder,
    };
    Arc::new(builder.build().expect("builds"))
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

/// The subagent statuses the parent's stream announced, in order.
fn subagent_statuses(events: &[Event]) -> Vec<TaskStatus> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::TaskUpdated {
                kind: TaskKind::Subagent,
                status,
                ..
            } => Some(*status),
            _ => None,
        })
        .collect()
}

/// The handle a background start handed back, read off its result.
fn handle_in(result: &str) -> String {
    result
        .split('`')
        .nth(1)
        .map(str::to_string)
        .expect("the result opens with the handle in backticks")
}

async fn drive(workspace: &Workspace) -> (Vec<Event>, String) {
    let mut run = workspace.prepare("go").expect("mints");
    let report = tokio::time::timeout(
        Duration::from_secs(20),
        run.execute_with_approver_and_options(
            CollectingSink::new(),
            AllowAll,
            TurnOptions::default(),
        ),
    )
    .await
    .expect("the run must not hang")
    .expect("the run completes");
    let answer = run.last_assistant_text().unwrap_or_default();
    (report.sink.into_events(), answer)
}

#[tokio::test]
async fn answers_arrive_on_their_own_and_the_turn_waits_for_them() {
    let router = Router::new(|seen| {
        if seen.last_text == "go" {
            return now(Turn::calling(
                "call-1",
                json!({ "input": "find the TODOs", "background": true }),
            ));
        }
        if seen.result_of("call-1").is_some() {
            return now(Turn::calling(
                "call-2",
                json!({ "input": "count the tests", "background": true }),
            ));
        }
        if seen.result_of("call-2").is_some() {
            return now(Turn::saying("done, I think"));
        }
        if seen.last_text == "find the TODOs" {
            return (Turn::saying("no TODOs"), Duration::from_millis(50));
        }
        if seen.last_text == "count the tests" {
            return (Turn::saying("12 tests"), Duration::from_millis(100));
        }
        if seen.all_text.contains("<delegation-result") {
            return now(Turn::saying("final"));
        }
        panic!("an unexpected request: {seen:?}");
    });
    let dir = tempfile::tempdir().unwrap();
    let workspace = pinned(dir.path(), runtime(&router, None))
        .open()
        .await
        .expect("opens");

    let (events, answer) = drive(&workspace).await;

    let results = results(&events);
    assert_eq!(results.len(), 2, "{results:?}");
    for (failed, text) in &results {
        assert!(!failed, "{text}");
        assert!(
            text.starts_with("Subagent `") && text.contains("in the background"),
            "{text}"
        );
    }
    assert_eq!(
        answer, "final",
        "the model read the answers and spoke once more"
    );

    let seen = router.seen();
    let last = seen.last().expect("requests");
    assert!(
        last.all_text.contains("no TODOs") && last.all_text.contains("12 tests"),
        "both answers reached the model before its answer was final: {}",
        last.all_text
    );
    assert!(
        !seen[..seen.len() - 1]
            .iter()
            .any(|seen| seen.all_text.contains("no TODOs")),
        "and not before the model had ended its turn: the children were still running"
    );
    let statuses = subagent_statuses(&events);
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == TaskStatus::Finished)
            .count(),
        2,
        "both children were announced finished on the parent's stream: {statuses:?}"
    );
}

#[tokio::test]
async fn a_wait_collects_the_answer_and_nothing_is_delivered_twice() {
    let router = Router::new(|seen| {
        if seen.last_text == "go" {
            return now(Turn::calling(
                "call-1",
                json!({ "input": "find the TODOs", "background": true }),
            ));
        }
        if let Some(started) = seen.result_of("call-1") {
            let handle = handle_in(started);
            return now(Turn::calling("call-2", json!({ "wait": [handle] })));
        }
        if seen.result_of("call-2").is_some() {
            return now(Turn::saying("done"));
        }
        if seen.last_text == "find the TODOs" {
            return (Turn::saying("no TODOs"), Duration::from_millis(50));
        }
        panic!("an unexpected request: {seen:?}");
    });
    let dir = tempfile::tempdir().unwrap();
    let workspace = pinned(dir.path(), runtime(&router, None))
        .open()
        .await
        .expect("opens");

    let (events, answer) = drive(&workspace).await;

    let results = results(&events);
    assert_eq!(results.len(), 2, "{results:?}");
    let (failed, collected) = &results[1];
    assert!(!failed, "{collected}");
    assert!(
        collected.starts_with("subagent `") && collected.ends_with("finished:\n\nno TODOs"),
        "{collected}"
    );
    assert_eq!(answer, "done", "no extra round: the wait delivered it");
    let seen = router.seen();
    let last = seen.last().expect("requests");
    assert_eq!(
        last.all_text.matches("<delegation-result").count(),
        1,
        "the wait recorded the result once, and nothing delivered it again: {}",
        last.all_text
    );
    assert_eq!(
        seen.len(),
        4,
        "the parent's three rounds and the child's one, nothing more"
    );
}

#[tokio::test]
async fn at_alone_collects_a_teammate_working_in_the_background() {
    let router = Router::new(|seen| {
        if seen.last_text == "go" {
            return now(Turn::calling(
                "call-1",
                json!({ "input": "@reviewer look at the diff", "background": true }),
            ));
        }
        if seen.result_of("call-1").is_some() {
            return now(Turn::calling("call-2", json!({ "input": "@reviewer" })));
        }
        if seen.result_of("call-2").is_some() {
            return now(Turn::saying("done"));
        }
        if seen.last_text == "look at the diff" {
            return (Turn::saying("the diff is fine"), Duration::from_millis(50));
        }
        panic!("an unexpected request: {seen:?}");
    });
    let dir = tempfile::tempdir().unwrap();
    let workspace = pinned(dir.path(), runtime(&router, None))
        .open()
        .await
        .expect("opens");

    let (events, answer) = drive(&workspace).await;

    let results = results(&events);
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(
        results[0].1.starts_with("Made teammate `reviewer` (id `"),
        "{}",
        results[0].1
    );
    assert!(
        results[1].1.starts_with("teammate `reviewer` (id `")
            && results[1].1.ends_with("finished:\n\nthe diff is fine"),
        "{}",
        results[1].1
    );
    assert_eq!(answer, "done");
}

#[tokio::test]
async fn the_width_refuses_one_too_many_before_the_approver() {
    let router = Router::new(|seen| {
        if seen.last_text == "go" {
            return now(Turn::calling(
                "call-1",
                json!({ "input": "find the TODOs", "background": true }),
            ));
        }
        if seen.result_of("call-1").is_some() {
            return now(Turn::calling(
                "call-2",
                json!({ "input": "count the tests", "background": true }),
            ));
        }
        if seen.result_of("call-2").is_some() {
            return now(Turn::saying("done, I think"));
        }
        if seen.last_text == "find the TODOs" {
            return (Turn::saying("no TODOs"), Duration::from_millis(50));
        }
        if seen.all_text.contains("<delegation-result") {
            return now(Turn::saying("final"));
        }
        panic!("an unexpected request: {seen:?}");
    });
    let dir = tempfile::tempdir().unwrap();
    let workspace = pinned(dir.path(), runtime(&router, Some(1)))
        .open()
        .await
        .expect("opens");

    let (events, answer) = drive(&workspace).await;

    let results = results(&events);
    assert_eq!(results.len(), 2, "{results:?}");
    let (failed, refusal) = &results[1];
    assert!(failed, "{refusal}");
    assert!(
        refusal.contains("1 delegations are already running in the background")
            && refusal.contains("no more than 1 at once"),
        "{refusal}"
    );
    assert_eq!(answer, "final");
}

#[tokio::test]
async fn any_returns_the_first_to_finish_and_the_turn_still_waits_for_the_rest() {
    // The quick one's handle, kept from the round that read it: a tool
    // result is not user text, so it is not in a later request's `all_text`.
    let quick = Arc::new(Mutex::new(None::<String>));
    let remembered = Arc::clone(&quick);
    let router = Router::new(move |seen| {
        if seen.last_text == "go" {
            return now(Turn::calling(
                "call-1",
                json!({ "input": "the quick one", "background": true }),
            ));
        }
        if let Some(started) = seen.result_of("call-1") {
            *remembered.lock().unwrap() = Some(handle_in(started));
            return now(Turn::calling(
                "call-2",
                json!({ "input": "the slow one", "background": true }),
            ));
        }
        if let Some(started) = seen.result_of("call-2") {
            let slow = handle_in(started);
            let quick = remembered.lock().unwrap().clone().expect("read earlier");
            return now(Turn::calling(
                "call-3",
                json!({ "wait": [quick, slow], "any": true }),
            ));
        }
        if seen.result_of("call-3").is_some() {
            return now(Turn::saying("done, I think"));
        }
        if seen.last_text == "the quick one" {
            return (Turn::saying("quick done"), Duration::from_millis(20));
        }
        if seen.last_text == "the slow one" {
            return (Turn::saying("slow done"), Duration::from_millis(400));
        }
        if seen.all_text.contains("<delegation-result") {
            return now(Turn::saying("final"));
        }
        panic!("an unexpected request: {seen:?}");
    });
    let dir = tempfile::tempdir().unwrap();
    let workspace = pinned(dir.path(), runtime(&router, None))
        .open()
        .await
        .expect("opens");

    let (events, answer) = drive(&workspace).await;

    let results = results(&events);
    assert_eq!(results.len(), 3, "{results:?}");
    let (failed, collected) = &results[2];
    assert!(!failed, "{collected}");
    assert!(
        collected.contains("finished:\n\nquick done") && !collected.contains("slow done"),
        "{collected}"
    );
    assert!(collected.contains("Still running: "), "{collected}");
    assert_eq!(
        answer, "final",
        "the slow one was joined before the turn ended"
    );
    let last = router.seen().pop().expect("requests");
    assert!(
        last.all_text.contains("slow done"),
        "and its answer reached the model: {}",
        last.all_text
    );
}
