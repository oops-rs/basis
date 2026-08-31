//! The child policy, driven — D4's claims that only a real turn can settle.
//!
//! The unit tests beside `tools::spawn::child` pin what a spec *describes*;
//! what they cannot show is that the overrides reach the spawned child and
//! nothing else moves. So these run scripted turns on a runtime carrying
//! **two** provider instances — the parent's and a cheap one — and check the
//! four things a reader would otherwise take on trust:
//!
//! - a triage child (prompt-prefix match) really runs on the cheap provider,
//!   with the narrowed roster and the replaced system prompt;
//! - the approver is shown what the child will be, and a policy that answers
//!   inherit changes the preview by nothing at all;
//! - the bounds still bind on the template path: a child's spend lands on the
//!   parent's counter exactly as it does on the inherit path;
//! - a child of a child still sees one door, because the policy — like the
//!   tool that consults it — is runtime-scoped and applies at every depth.
//!
//! Nothing here reaches a network or a model.

use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use basis::{
    AllowAll, ApprovalAnswer, ApprovalRequest, Approver, Bound, ChildContext, ChildSpec,
    CollectingSink, SpawnTool, ToolRoster, TurnOptions, approval::ApprovalGate,
    run::prepare_with_session, tools::SPAWN,
};
use mentra::{
    BuiltinProvider, ContentBlock, ModelInfo, Role, Runtime, RuntimePolicy, Session, TokenUsage,
    agent::{AgentConfig, ToolProfile, WorkspaceConfig},
    provider::{
        Provider, ProviderDescriptor, ProviderError, ProviderEventStream, Request, Response,
        provider_event_stream_from_response,
    },
    runtime::VolatileRuntimeStore,
};
use serde_json::json;

/// Every run here must finish well inside this. Exceeding it means a
/// permission request went unanswered and the turn is stuck.
const NOT_STUCK: Duration = Duration::from_secs(20);

/// One scripted assistant round, with what it reports spending.
#[derive(Debug, Clone)]
struct Turn {
    content: Vec<ContentBlock>,
    tokens: u64,
}

impl Turn {
    fn calling(id: &str, input: &str) -> Self {
        Self {
            content: vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: SPAWN.to_string(),
                input: json!({ "input": input }),
            }],
            tokens: 0,
        }
    }

    fn saying(text: &str) -> Self {
        Self {
            content: vec![ContentBlock::text(text)],
            tokens: 0,
        }
    }

    fn costing(self, tokens: u64) -> Self {
        Self { tokens, ..self }
    }

    fn usage(&self) -> Option<TokenUsage> {
        (self.tokens > 0).then(|| TokenUsage {
            input_tokens: Some(self.tokens),
            output_tokens: Some(0),
            total_tokens: Some(self.tokens),
            ..TokenUsage::default()
        })
    }
}

/// What one provider call was asked to do — enough to tell whose roster,
/// whose model, and whose voice a request carried.
#[derive(Debug, Clone)]
struct Asked {
    model: String,
    tools: Vec<String>,
    system: String,
}

/// Replays a fixed script of assistant turns and remembers what it was sent.
/// One instance per provider identity, which is the whole point here: the
/// parent's requests and an overridden child's land on different instances.
struct ScriptedProvider {
    id: BuiltinProvider,
    models: Vec<ModelInfo>,
    turns: Mutex<VecDeque<Turn>>,
    asked: Arc<Mutex<Vec<Asked>>>,
}

impl ScriptedProvider {
    fn new(
        id: BuiltinProvider,
        models: Vec<ModelInfo>,
        turns: Vec<Turn>,
    ) -> (Self, Arc<Mutex<Vec<Asked>>>) {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let provider = Self {
            id,
            models,
            turns: Mutex::new(turns.into()),
            asked: Arc::clone(&asked),
        };

        (provider, asked)
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(self.id)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(self.models.clone())
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.asked.lock().expect("not poisoned").push(Asked {
            model: request.model.to_string(),
            tools: request.tools.iter().map(|tool| tool.name.clone()).collect(),
            system: request
                .system
                .as_deref()
                .map(str::to_string)
                .unwrap_or_default(),
        });

        let turn = self
            .turns
            .lock()
            .expect("not poisoned")
            .pop_front()
            .unwrap_or_else(|| Turn::saying("done"));

        Ok(provider_event_stream_from_response(Response {
            id: "scripted".to_string(),
            model: request.model.to_string(),
            role: Role::Assistant,
            usage: turn.usage(),
            content: turn.content,
            stop_reason: None,
        }))
    }
}

/// Everything one provider was sent, in order.
struct Requests(Arc<Mutex<Vec<Asked>>>);

impl Requests {
    fn all(&self) -> Vec<Asked> {
        self.0.lock().expect("not poisoned").clone()
    }

    fn nth(&self, index: usize) -> Asked {
        self.all()
            .get(index)
            .unwrap_or_else(|| panic!("no request at index {index}"))
            .clone()
    }
}

/// Records what it was asked, then allows.
struct Recording {
    seen: Arc<Mutex<Vec<ApprovalRequest>>>,
}

#[async_trait]
impl Approver for Recording {
    async fn approve(&mut self, request: &ApprovalRequest) -> ApprovalAnswer {
        self.seen
            .lock()
            .expect("not poisoned")
            .push(request.clone());
        AllowAll.approve(request).await
    }
}

fn parent_model() -> ModelInfo {
    ModelInfo::new("parent-model", BuiltinProvider::OpenAI)
}

fn cheap_model() -> ModelInfo {
    ModelInfo::new("cheap-model", BuiltinProvider::Anthropic)
}

/// The policy under test: a triage child — named by a prompt-prefix
/// convention — gets the cheap provider's model, a narrowed roster with the
/// one door still on it, and a voice of its own; everything else inherits.
fn triage_policy(child: &ChildContext<'_>) -> ChildSpec {
    if child.prompt().starts_with("triage:") {
        ChildSpec::inherit()
            .with_roster(ToolRoster::only(["read", SPAWN]))
            .with_model(cheap_model())
            .with_system("You are a triage gate. Answer yes or no.")
    } else {
        ChildSpec::inherit()
    }
}

/// A runtime built the way `basis::RuntimeBuilder` builds one — `spawn`
/// registered with the policy, the approval gate installed — but carrying two
/// provider instances, which is what lets a test prove the provider actually
/// switched rather than just the model id string.
fn runtime(
    workspace: &Path,
    parent_turns: Vec<Turn>,
    child_turns: Vec<Turn>,
    policy: impl Fn(&ChildContext<'_>) -> ChildSpec + Send + Sync + 'static,
) -> (Runtime, Requests, Requests) {
    let (parent, parent_asked) =
        ScriptedProvider::new(BuiltinProvider::OpenAI, vec![parent_model()], parent_turns);
    let (cheap, cheap_asked) =
        ScriptedProvider::new(BuiltinProvider::Anthropic, vec![cheap_model()], child_turns);

    let runtime = Runtime::builder()
        .with_provider_instance(parent)
        .with_provider_instance(cheap)
        .with_store(VolatileRuntimeStore::new())
        .with_policy(RuntimePolicy::workspace_bounded(workspace))
        // The file-tool roster basis's own builder states (`Split` — the six
        // names models are trained on), so `read` is a registered name the
        // triage roster can genuinely offer.
        .with_file_tools(mentra::FileToolProfile::Split)
        .with_tool_authorizer(ApprovalGate::new())
        .with_tool(SpawnTool::new().with_child_policy(policy))
        .build()
        .expect("runtime builds");

    (runtime, Requests(parent_asked), Requests(cheap_asked))
}

/// The roster `agent_config` produces — pinned as basis's own in
/// `workspace::builder::tests`; this file drives mentra directly.
fn session(runtime: &Runtime, workspace: &Path) -> Session {
    runtime
        .create_session_with_config(
            "test",
            parent_model(),
            AgentConfig {
                tool_profile: ToolProfile::hide(["shell", "background_run", "task"]),
                workspace: WorkspaceConfig {
                    base_dir: workspace.to_path_buf(),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("session")
}

fn context() -> basis::ContextConfig {
    basis::ContextConfig {
        file_name: "AGENTS.md".to_string(),
        global_dir: None,
        walk_parents: false,
    }
}

struct Run {
    asked: Vec<ApprovalRequest>,
    stopped_by: Option<Bound>,
    total_tokens: u64,
}

/// Drives one prepared run under a recording approver that allows everything.
async fn drive(workspace: &Path, runtime: &Runtime, options: TurnOptions) -> Run {
    let session = session(runtime, workspace);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut prepared = prepare_with_session(
        session,
        workspace,
        "do the thing",
        &context(),
        "openai",
        "parent-model",
    )
    .expect("prepared");

    let report = tokio::time::timeout(
        NOT_STUCK,
        prepared.execute_with_approver_and_options(
            CollectingSink::new(),
            Recording {
                seen: Arc::clone(&seen),
            },
            options,
        ),
    )
    .await
    .expect("the run must not hang waiting on an unanswered approval")
    .expect("the run completes");

    let asked = seen.lock().expect("not poisoned").clone();
    Run {
        asked,
        stopped_by: report.stopped_by,
        total_tokens: report.usage.total_tokens(),
    }
}

#[tokio::test]
async fn a_triage_child_runs_on_the_cheap_provider_with_the_narrow_roster() {
    let workspace = tempfile::tempdir().expect("tempdir");

    let (runtime, parent, cheap) = runtime(
        workspace.path(),
        vec![
            Turn::calling("call-0", "triage: is this bug report real?"),
            Turn::saying("parent done"),
        ],
        vec![Turn::saying("yes, real")],
        triage_policy,
    );
    drive(workspace.path(), &runtime, TurnOptions::default()).await;

    // The provider actually switched — not just the model id string: the
    // child's one request landed on the Anthropic-kind instance, and the
    // parent's two rounds never did.
    assert_eq!(cheap.all().len(), 1, "the triage child asks once");
    assert_eq!(parent.all().len(), 2, "the parent's rounds stay its own");
    let child = cheap.nth(0);
    assert_eq!(child.model, "cheap-model");

    // The narrowed roster reached the child: the allow-list, nothing else —
    // and the door stays a door because the policy named it.
    let mut tools = child.tools.clone();
    tools.sort();
    assert_eq!(tools, vec!["read".to_string(), SPAWN.to_string()]);

    // The replaced system prompt is the child's whole voice: the host's text,
    // mentra's standard subagent instructions after it — and none of the
    // parent's own system prompt travels along.
    assert!(
        child.system.contains("You are a triage gate."),
        "{}",
        child.system
    );
    assert!(
        child.system.contains("subagent"),
        "mentra's subagent instructions still apply to an overridden child: {}",
        child.system
    );
    let parent_request = parent.nth(0);
    assert_eq!(parent_request.model, "parent-model");
    assert!(
        !parent_request.system.contains("You are a triage gate."),
        "the child's voice must not leak into the parent: {}",
        parent_request.system
    );
}

#[tokio::test]
async fn the_approver_reads_what_the_child_will_be() {
    let workspace = tempfile::tempdir().expect("tempdir");

    let (runtime, _parent, _cheap) = runtime(
        workspace.path(),
        vec![
            Turn::calling("call-0", "triage: is this bug report real?"),
            Turn::saying("parent done"),
        ],
        vec![Turn::saying("yes, real")],
        triage_policy,
    );
    let run = drive(workspace.path(), &runtime, TurnOptions::default()).await;

    assert_eq!(run.asked.len(), 1, "one delegation, one question");
    let input = &run.asked[0].input;
    assert_eq!(input["mode"], "agent");
    assert_eq!(
        input["child"],
        json!({
            // The policy routes this child to a different vendor than the run
            // reported, and that is exactly the fact an operator would refuse
            // on — so the preview names the provider, not just the id.
            "model": { "id": "cheap-model", "provider": "anthropic" },
            "roster": { "offered": ["read", SPAWN] },
            "system": "replaced",
        }),
        "a remembered rule can match on what the child will be"
    );
    assert!(
        !input.to_string().contains("triage gate"),
        "the system prompt's text never travels in a preview: {input}"
    );
}

#[tokio::test]
async fn a_policy_that_answers_inherit_changes_nothing_observable() {
    // The prompt misses the triage prefix, so the policy answers inherit —
    // and everything must look exactly like a runtime with no policy at all:
    // the child on the parent's provider and model, the preview carrying the
    // four-key shape with no `child` in it.
    let workspace = tempfile::tempdir().expect("tempdir");

    let (runtime, parent, cheap) = runtime(
        workspace.path(),
        vec![
            Turn::calling("call-0", "summarise the README"),
            Turn::saying("child done"),
            Turn::saying("parent done"),
        ],
        Vec::new(),
        triage_policy,
    );
    let run = drive(workspace.path(), &runtime, TurnOptions::default()).await;

    assert_eq!(cheap.all().len(), 0, "no override, no cheap provider");
    assert_eq!(
        parent.all().len(),
        3,
        "parent round, inherited child round, parent round"
    );
    assert_eq!(parent.nth(1).model, "parent-model");

    let input = &run.asked[0].input;
    assert!(
        input.get("child").is_none(),
        "an inherited child leaves the preview byte-identical: {input}"
    );
}

#[tokio::test]
async fn the_bounds_still_bind_a_child_the_policy_reshaped() {
    // The accounting claim of ADR-0016, re-checked on the template path: the
    // overridden child runs on the same `child_run_options`, so its spend
    // lands on the parent's shared counter and stops the parent's next round.
    let workspace = tempfile::tempdir().expect("tempdir");

    let (runtime, parent, _cheap) = runtime(
        workspace.path(),
        vec![
            Turn::calling("call-0", "triage: is this bug report real?").costing(10),
            Turn::saying("parent done").costing(10),
        ],
        vec![Turn::saying("yes, real").costing(200)],
        triage_policy,
    );
    let run = drive(
        workspace.path(),
        &runtime,
        TurnOptions::default().with_token_budget(100),
    )
    .await;

    assert_eq!(
        run.stopped_by,
        Some(Bound::TokenBudget),
        "what the reshaped child spent has to be what stops the parent"
    );
    assert_eq!(
        parent.all().len(),
        1,
        "the parent's second round must never have been started"
    );
    assert_eq!(
        run.total_tokens, 210,
        "a run that stopped on 210 tokens must not report having spent 10"
    );
}

/// A roster override must not hand a child the sibling-workspace tools its
/// own parent is denied.
///
/// The hazard is specific to a shared runtime, so this one goes through
/// basis's real front door — two `Workspace`s on one `Runtime`, the child
/// policy on the builder — rather than the direct-to-mentra harness above:
/// what is under test is the wiring between `Workspace::minted_agent`'s
/// per-mint hiding and what `spawn` puts back after
/// `with_tool_profile` replaces the child's cloned profile.
///
/// A **`hide`** roster is the sharp case and the one `only` structurally
/// cannot show: an allow-list omits a sibling's tool by simply not naming it,
/// while a denylist built from basis's own set carries no sibling names at
/// all — so before the fix, a `hide` roster was the shortest path from
/// "narrow this child" to "offer it another repository's tools". Declared
/// tools stand in for the `mcp__*` half here because both land in one set by
/// the same two loops in `minted_agent`, and a declared tool needs no server
/// to exist.
#[tokio::test]
async fn a_narrowed_child_is_not_offered_a_siblings_tools() {
    let sibling = tempfile::tempdir().expect("tempdir");
    let mine = tempfile::tempdir().expect("tempdir");
    let program = sibling.path().join("jenkins");
    std::fs::write(&program, "#!/bin/sh\nprintf ok\n").expect("write program");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("make it executable");
    }
    std::fs::create_dir_all(sibling.path().join(".basis")).expect("dir");
    let manifest = json!({
        "schema": 1,
        "tools": {
            "jenkins_job": {
                "description": "Trigger a job.",
                "input_schema": {"type": "object", "properties": {}},
                "command": [program],
            },
        },
    });
    std::fs::write(
        sibling.path().join(".basis/tools.json"),
        serde_json::to_vec(&manifest).expect("serialize manifest"),
    )
    .expect("write manifest");

    let (provider, asked) = ScriptedProvider::new(
        BuiltinProvider::OpenAI,
        vec![parent_model()],
        vec![
            Turn::calling("call-0", "triage: is this real?"),
            Turn::saying("child done"),
            Turn::saying("parent done"),
        ],
    );
    let shared = Arc::new(
        basis::Runtime::builder()
            .with_provider_instance(provider)
            .with_ephemeral_history()
            // Narrows the child with a *denylist*, which keeps every name the
            // parent could use except the one this host does not want a
            // triage child running — and says nothing about a sibling's
            // tools, because a policy author has no way to know they exist.
            .with_child_policy(|child: &ChildContext<'_>| {
                if child.prompt().starts_with("triage:") {
                    ChildSpec::inherit().with_roster(ToolRoster::hide(["write"]))
                } else {
                    ChildSpec::inherit()
                }
            })
            .build()
            .expect("builds offline"),
    );

    let _declaring = offline_workspace(sibling.path(), Arc::clone(&shared))
        .open()
        .await
        .expect("the sibling opens and claims its tool");
    let workspace = offline_workspace(mine.path(), shared)
        .open()
        .await
        .expect("opens");

    let report = workspace
        .prepare(basis::RunSpec::new("do the thing"))
        .expect("mints")
        .execute_with_approver(CollectingSink::new(), AllowAll)
        .await
        .expect("the run completes");
    drop(report);

    let rosters: Vec<Vec<String>> = asked
        .lock()
        .expect("not poisoned")
        .iter()
        .map(|request| request.tools.clone())
        .collect();
    assert_eq!(rosters.len(), 3, "parent, child, parent");
    for (round, roster) in rosters.iter().enumerate() {
        assert!(
            !roster.contains(&"jenkins_job".to_string()),
            "round {round} was offered a sibling repository's tool: {roster:?}"
        );
    }
    assert!(
        rosters[1].contains(&SPAWN.to_string()),
        "the narrowed child keeps everything its parent had: {:?}",
        rosters[1]
    );
    assert!(
        !rosters[1].contains(&"write".to_string()),
        "and loses exactly what the policy hid: {:?}",
        rosters[1]
    );
}

/// A workspace that looks nowhere except where the test put something.
fn offline_workspace(path: &Path, runtime: Arc<basis::Runtime>) -> basis::WorkspaceBuilder {
    basis::Workspace::builder(path)
        .with_runtime(runtime)
        .with_model(basis::ModelSelector::Id("parent-model".to_string()))
        .with_context(basis::ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: None,
            walk_parents: false,
        })
        .with_skills(basis::skills::SkillsConfig {
            workspace_subdir: Some(std::path::PathBuf::from(".basis/skills")),
            shared_workspace_dir: true,
            global_dir: None,
            shared_home_dir: false,
        })
        .with_templates(basis::templates::TemplatesConfig {
            workspace_subdir: std::path::PathBuf::from(".basis/templates"),
            global_dir: None,
        })
        .with_hooks(basis::hooks::HooksConfig {
            workspace_file: std::path::PathBuf::from(".basis/hooks.json"),
            global_dir: None,
            supplied: Vec::new(),
        })
        .with_tools(basis::tools::declared::ToolsConfig {
            workspace_file: std::path::PathBuf::from(".basis/tools.json"),
            global_dir: None,
            supplied: Vec::new(),
        })
        .with_memory(basis::MemoryConfig::disabled())
}

#[tokio::test]
async fn a_child_of_a_child_still_sees_one_door() {
    // The policy is runtime-scoped like the tool that consults it, so it
    // applies at every depth: the child's own delegation is triaged too, and
    // the grandchild's roster still offers `spawn` and none of the replaced
    // doors — one door, recursively, with the policy in force.
    let workspace = tempfile::tempdir().expect("tempdir");

    let (runtime, _parent, cheap) = runtime(
        workspace.path(),
        vec![
            Turn::calling("call-0", "triage: level one"),
            Turn::saying("parent done"),
        ],
        vec![
            Turn::calling("call-1", "triage: level two"),
            Turn::saying("grandchild: yes"),
            Turn::saying("child: yes"),
        ],
        triage_policy,
    );
    drive(workspace.path(), &runtime, TurnOptions::default()).await;

    assert_eq!(
        cheap.all().len(),
        3,
        "child round, grandchild round, child round — all on the cheap model"
    );
    let grandchild = cheap.nth(1);
    assert_eq!(grandchild.model, "cheap-model");
    assert!(
        grandchild.tools.contains(&SPAWN.to_string()),
        "the one door is still on the grandchild's roster: {:?}",
        grandchild.tools
    );
    for replaced in ["shell", "background_run", "task"] {
        assert!(
            !grandchild.tools.contains(&replaced.to_string()),
            "{replaced} came back at depth two: {:?}",
            grandchild.tools
        );
    }
}
