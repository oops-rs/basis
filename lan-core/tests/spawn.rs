//! `spawn`, driven — the half of ADR-0016 that only a real turn can settle.
//!
//! The unit tests beside the tool pin what it *decides*: the parse, the
//! preview, the depth floor. What they cannot show is the ordering those
//! decisions depend on, and ordering is the whole of the security claim here.
//! So these run turns against a scripted provider, on a runtime built the way
//! `WorkspaceBuilder::open` builds one, and check the four things a reader
//! would otherwise have to take on trust:
//!
//! - the model is offered one door, at every depth;
//! - a command reaches the approver *before* it runs, carrying the parsed call;
//! - a remembered rule answers ahead of the approver, so an allowlist is data;
//! - `--no-shell` still refuses, on the path `spawn` now uses.
//!
//! Nothing here reaches a network or a model. The one thing that does leave the
//! process is `echo`, which is how a command proves it ran.

use std::{
    collections::VecDeque,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use lan_core::{
    AllowAll, ApprovalAnswer, ApprovalDecision, ApprovalRequest, Approver, Bound, CollectingSink,
    Event, RunConfig, RunUsage, SpawnTool, TurnOptions, approval::ApprovalGate,
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
    session::{PermissionRuleScope, RememberedRule, RuleKey},
};
use serde_json::{Value, json};

/// Every run here must finish well inside this. Exceeding it means a
/// permission request went unanswered and the turn is stuck.
const NOT_STUCK: Duration = Duration::from_secs(20);

/// What a command prints, so that "did this run" needs no filesystem.
const RAN: &str = "the-command-ran";

/// One round of a model's turn, as scripted, and what it reports spending.
///
/// Cost is part of a round because the accounting claim of ADR-0016 is about
/// *whose* budget a delegated round lands on, which is only visible when a
/// parent's round and its child's round cost different amounts.
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

    /// A round reports nothing unless a test said what it cost, so every test
    /// that is not about budgets is unaffected by there being one.
    fn usage(&self) -> Option<TokenUsage> {
        (self.tokens > 0).then(|| TokenUsage {
            input_tokens: Some(self.tokens),
            output_tokens: Some(0),
            total_tokens: Some(self.tokens),
            ..TokenUsage::default()
        })
    }
}

/// What one provider call was asked to do, kept so a test can see a roster or
/// a tool result that never reaches the parent's event stream.
#[derive(Debug, Clone)]
struct Asked {
    tools: Vec<String>,
    transcript: String,
}

/// Replays a fixed script of assistant turns and remembers what it was sent.
///
/// One instance serves the parent and every subagent — they share a runtime,
/// and delegation is sequential — so the script is simply the rounds in the
/// order they happen.
struct ScriptedProvider {
    model: ModelInfo,
    turns: Mutex<VecDeque<Turn>>,
    asked: Arc<Mutex<Vec<Asked>>>,
}

impl ScriptedProvider {
    fn new(model: ModelInfo, turns: Vec<Turn>) -> (Self, Arc<Mutex<Vec<Asked>>>) {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let provider = Self {
            model,
            turns: Mutex::new(turns.into()),
            asked: Arc::clone(&asked),
        };

        (provider, asked)
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(self.model.provider.clone())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![self.model.clone()])
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        self.asked.lock().expect("not poisoned").push(Asked {
            tools: request.tools.iter().map(|tool| tool.name.clone()).collect(),
            transcript: format!("{:?}", request.messages),
        });

        let turn = self
            .turns
            .lock()
            .expect("not poisoned")
            .pop_front()
            .unwrap_or_else(|| Turn::saying("done"));

        Ok(provider_event_stream_from_response(Response {
            id: "scripted".to_string(),
            model: self.model.id.clone(),
            role: Role::Assistant,
            usage: turn.usage(),
            content: turn.content,
            stop_reason: None,
        }))
    }
}

/// What a test asks of one run: the rounds, whether commands are on, what an
/// operator had already remembered, and the turn's own bounds.
///
/// A struct rather than five positional arguments, and every method returns a
/// new value, so a test reads as the one thing it varies.
struct Script {
    turns: Vec<Turn>,
    commands: bool,
    rules: Vec<RememberedRule>,
    options: TurnOptions,
}

impl Script {
    fn new(turns: Vec<Turn>) -> Self {
        Self {
            turns,
            commands: true,
            rules: Vec::new(),
            options: TurnOptions::default(),
        }
    }

    /// What `lan --no-shell` leaves a workspace with.
    fn without_commands(self) -> Self {
        Self {
            commands: false,
            ..self
        }
    }

    fn remembering(self, rule: RememberedRule) -> Self {
        let rules = self.rules.into_iter().chain([rule]).collect();
        Self { rules, ..self }
    }

    fn with_token_budget(self, budget: u64) -> Self {
        Self {
            options: self.options.with_token_budget(budget),
            ..self
        }
    }
}

/// A runtime built the way [`lan_core::WorkspaceBuilder::open`] builds one:
/// `spawn` registered, the approval gate installed, and commands allowed or
/// not exactly as [`lan_core::ShellAccess`] would have set them.
fn runtime(workspace: &Path, turns: Vec<Turn>, commands: bool) -> (Runtime, ModelInfo, Requests) {
    let model = ModelInfo::new("scripted-model", BuiltinProvider::OpenAI);
    let (provider, asked) = ScriptedProvider::new(model.clone(), turns);

    let runtime = Runtime::builder()
        .with_provider_instance(provider)
        .with_store(VolatileRuntimeStore::new())
        .with_policy(
            RuntimePolicy::workspace_bounded(workspace)
                .allow_shell_commands(commands)
                .allow_background_commands(commands),
        )
        .with_tool_authorizer(ApprovalGate::new())
        .with_tool(SpawnTool::new())
        .build()
        .expect("runtime builds");

    (runtime, model, Requests(asked))
}

/// The roster `agent_config` produces. That lan's own builder produces exactly
/// this is pinned next to it, in `workspace::builder::tests`; what is under
/// test here is what mentra then does with it.
fn agent(workspace: &Path) -> AgentConfig {
    AgentConfig {
        tool_profile: ToolProfile::hide(["shell", "background_run", "task"]),
        workspace: WorkspaceConfig {
            base_dir: workspace.to_path_buf(),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn session(runtime: &Runtime, workspace: &Path, model: ModelInfo) -> Session {
    runtime
        .create_session_with_config("test", model, agent(workspace))
        .expect("session")
}

fn config(workspace: &Path) -> RunConfig {
    RunConfig::new(workspace, "do the thing").with_context(lan_core::ContextConfig {
        file_name: "AGENTS.md".to_string(),
        global_dir: None,
        walk_parents: false,
    })
}

/// Everything the scripted provider was sent, in order.
struct Requests(Arc<Mutex<Vec<Asked>>>);

impl Requests {
    fn all(&self) -> Vec<Asked> {
        self.0.lock().expect("not poisoned").clone()
    }

    /// The tool names offered on the nth provider call — the model's roster at
    /// that point, which for calls after the first is a subagent's.
    fn roster(&self, index: usize) -> Vec<String> {
        self.all()
            .get(index)
            .map(|asked| asked.tools.clone())
            .unwrap_or_default()
    }

    /// Whether any agent, at any depth, was ever shown `needle` in its
    /// transcript. The only way to see a subagent's tool results: a subagent
    /// has its own event bus, so none of them reach the parent's stream.
    fn any_transcript_contains(&self, needle: &str) -> bool {
        self.all()
            .iter()
            .any(|asked| asked.transcript.contains(needle))
    }
}

/// Records what it was asked, then lets the approver under test answer.
struct Recording<A> {
    inner: A,
    seen: Arc<Mutex<Vec<ApprovalRequest>>>,
}

#[async_trait]
impl<A: Approver> Approver for Recording<A> {
    async fn approve(&mut self, request: &ApprovalRequest) -> ApprovalAnswer {
        self.seen
            .lock()
            .expect("not poisoned")
            .push(request.clone());
        self.inner.approve(request).await
    }
}

/// Refuses once, for the rest of the session, in words of its own.
struct RefusesForGood;

const REFUSAL: &str = "this run does not run commands";

#[async_trait]
impl Approver for RefusesForGood {
    async fn approve(&mut self, _request: &ApprovalRequest) -> ApprovalAnswer {
        ApprovalAnswer::new(ApprovalDecision::DenyForSession).because(REFUSAL)
    }
}

struct Run {
    events: Vec<Event>,
    asked: Vec<ApprovalRequest>,
    requests: Requests,
    stopped_by: Option<Bound>,
    usage: RunUsage,
}

impl Run {
    /// Every `spawn` result the parent's stream carried, in order.
    fn results(&self) -> Vec<(bool, String)> {
        self.events
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

    fn first_result(&self) -> (bool, String) {
        self.results()
            .into_iter()
            .next()
            .expect("spawn must have completed at least once")
    }
}

/// Drives a script under `approver`, seeding the session's rule store first —
/// which is how a test stands in for an operator who has already answered this
/// question once.
async fn drive<A: Approver>(workspace: &Path, script: Script, approver: A) -> Run {
    let (runtime, model, requests) = runtime(workspace, script.turns, script.commands);
    let session = session(&runtime, workspace, model);
    for rule in script.rules {
        session.rule_store().add_rule(rule);
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut prepared =
        prepare_with_session(session, &config(workspace), "openai", "scripted-model")
            .expect("prepared");

    let report = tokio::time::timeout(
        NOT_STUCK,
        prepared.execute_with_approver_and_options(
            CollectingSink::new(),
            Recording {
                inner: approver,
                seen: Arc::clone(&seen),
            },
            script.options,
        ),
    )
    .await
    .expect("the run must not hang waiting on an unanswered approval")
    .expect("the run completes");

    let asked = seen.lock().expect("not poisoned").clone();
    Run {
        events: report.sink.into_events(),
        asked,
        requests,
        stopped_by: report.stopped_by,
        usage: report.usage,
    }
}

/// The common case: one scripted `spawn` call, commands allowed, nothing
/// remembered.
async fn one_call<A: Approver>(workspace: &Path, input: &str, approver: A) -> Run {
    drive(
        workspace,
        Script::new(vec![Turn::calling("call-0", input)]),
        approver,
    )
    .await
}

#[tokio::test]
async fn the_model_is_offered_one_door() {
    let workspace = tempfile::tempdir().expect("tempdir");

    let run = one_call(workspace.path(), &format!("!echo {RAN}"), AllowAll).await;

    let roster = run.requests.roster(0);
    assert!(
        roster.contains(&SPAWN.to_string()),
        "the one door has to be on the roster: {roster:?}"
    );
    for replaced in ["shell", "background_run", "task"] {
        assert!(
            !roster.contains(&replaced.to_string()),
            "{replaced} is still offered alongside spawn: {roster:?}"
        );
    }
}

#[tokio::test]
async fn a_command_is_answered_before_it_runs_and_then_runs() {
    let workspace = tempfile::tempdir().expect("tempdir");

    let run = one_call(workspace.path(), &format!("!echo {RAN}"), AllowAll).await;

    assert_eq!(
        run.asked.len(),
        1,
        "a command is never waved through: {:?}",
        run.asked
    );
    assert_eq!(run.asked[0].tool_name, SPAWN);

    let (failed, output) = run.first_result();
    assert!(!failed, "an approved command runs: {output}");
    assert!(
        output.contains(RAN),
        "and lan reads its output back: {output}"
    );
}

#[tokio::test]
async fn the_approver_is_shown_the_parsed_call_and_not_the_string() {
    // The claim the whole design rests on: `!` is read once, at the boundary,
    // and every consumer downstream sees the typed pair. An approver that had
    // to re-read the string could disagree with the tool about what it was.
    let workspace = tempfile::tempdir().expect("tempdir");

    let run = one_call(workspace.path(), &format!("!echo {RAN}"), AllowAll).await;

    let input = &run.asked[0].input;
    assert_eq!(input["mode"], "command");
    assert_eq!(input["body"], format!("echo {RAN}"));
    assert_eq!(
        input["cwd"],
        Value::String(workspace.path().to_string_lossy().into_owned()),
        "an approver cannot judge a command without knowing where it runs"
    );
}

#[tokio::test]
async fn a_delegation_reaches_the_approver_as_a_delegation() {
    let workspace = tempfile::tempdir().expect("tempdir");

    let run = one_call(workspace.path(), "summarise the README", AllowAll).await;

    assert_eq!(run.asked.len(), 1, "delegation is consequential too");
    assert_eq!(run.asked[0].input["mode"], "agent");
    assert_eq!(run.asked[0].input["body"], "summarise the README");
}

#[tokio::test]
async fn a_refused_command_does_not_run() {
    let workspace = tempfile::tempdir().expect("tempdir");

    let run = one_call(workspace.path(), &format!("!echo {RAN}"), RefusesForGood).await;

    let (failed, output) = run.first_result();
    assert!(failed, "a refused command fails visibly: {output}");
    assert!(
        !output.contains(RAN),
        "and its output cannot exist, because it never ran: {output}"
    );
    assert!(output.contains(REFUSAL), "the model reads why: {output}");
}

#[tokio::test]
async fn a_remembered_refusal_repeats_its_reason_with_nobody_asked() {
    // The rung below the approver, from mentra `b895ea0`: a rule answers
    // first, and a rule that dropped its reason would let the host explain
    // itself exactly once while the model kept trying.
    let workspace = tempfile::tempdir().expect("tempdir");

    let run = drive(
        workspace.path(),
        Script::new(vec![
            Turn::calling("call-0", &format!("!echo {RAN}")),
            Turn::calling("call-1", &format!("!echo {RAN}")),
        ]),
        RefusesForGood,
    )
    .await;

    assert_eq!(
        run.asked.len(),
        1,
        "the second call must be answered by the rule, not by the approver"
    );

    let results = run.results();
    assert_eq!(results.len(), 2, "both calls completed");
    for (failed, output) in &results {
        assert!(failed, "{output}");
        assert!(output.contains(REFUSAL), "{output}");
    }
    assert!(
        results[1].1.contains("remembered"),
        "the repeat says it is a repeat, or the model reads it as a fresh no: {}",
        results[1].1
    );
}

#[tokio::test]
async fn a_remembered_answer_on_the_name_covers_both_modes() {
    // ADR-0016's named trade: two names collapsed into one, so a bare rule an
    // operator set while refusing a command also answers a delegation. Telling
    // them apart is possible — it means writing a pattern on the parsed mode —
    // but it is no longer free.
    let workspace = tempfile::tempdir().expect("tempdir");

    let run = drive(
        workspace.path(),
        Script::new(vec![
            Turn::calling("call-0", &format!("!echo {RAN}")),
            Turn::calling("call-1", "summarise the README"),
        ]),
        RefusesForGood,
    )
    .await;

    assert_eq!(run.asked.len(), 1);
    let results = run.results();
    assert!(
        results[1].0 && results[1].1.contains(REFUSAL),
        "the delegation was answered by the command's rule: {:?}",
        results[1]
    );
}

#[tokio::test]
async fn a_pattern_rule_is_a_command_allowlist_expressible_as_data() {
    // Because the command rides inside spawn's input, mentra's existing glob
    // over the serialized structured input *is* an allowlist — no new
    // mechanism, and the approver never sees the calls it covers.
    let workspace = tempfile::tempdir().expect("tempdir");

    let run = drive(
        workspace.path(),
        Script::new(vec![Turn::calling("call-0", &format!("!echo {RAN}"))]).remembering(
            RememberedRule {
                key: RuleKey {
                    tool_name: SPAWN.to_string(),
                    // `**` rather than `*`, and this is a trap worth knowing
                    // about: mentra globs with `glob-match`, where a single
                    // `*` does not cross `/`. The serialized input carries
                    // `cwd`, so a rule written with one star silently matches
                    // nothing and the operator sees a reviewer they thought
                    // they had bypassed.
                    pattern: Some(format!("**\"body\":\"echo {RAN}\"**")),
                },
                allow: true,
                scope: PermissionRuleScope::Session,
                reason: None,
            },
        ),
        // The strictest approver there is: anything reaching it is refused, so
        // this catches an allowlist that failed to match as well as one that
        // matched too much.
        RefusesForGood,
    )
    .await;

    assert!(
        run.asked.is_empty(),
        "an allowlisted command must never reach the reviewer: {:?}",
        run.asked
    );
    let (failed, output) = run.first_result();
    assert!(!failed, "{output}");
    assert!(output.contains(RAN), "{output}");
}

#[tokio::test]
async fn no_shell_still_refuses_command_mode() {
    // ADR-0013's posture, unchanged by the change of route: `ShellAccess::Denied`
    // sets `allow_shell_commands(false)` and mentra's policy refuses on the
    // same path `spawn` calls — after this tool was authorized, before anything
    // executed. The approver saying yes is not what decides this.
    let workspace = tempfile::tempdir().expect("tempdir");

    let run = drive(
        workspace.path(),
        Script::new(vec![Turn::calling("call-0", &format!("!echo {RAN}"))]).without_commands(),
        AllowAll,
    )
    .await;

    let (failed, output) = run.first_result();
    assert!(failed, "a command must not succeed with commands off");
    assert!(!output.contains(RAN), "and nothing may have run: {output}");
    assert!(
        output.contains("Shell command execution is disabled"),
        "the refusal has to say what refused it: {output}"
    );
}

#[tokio::test]
async fn delegation_hands_work_over_and_reads_the_answer_back() {
    let workspace = tempfile::tempdir().expect("tempdir");

    let run = drive(
        workspace.path(),
        Script::new(vec![
            Turn::calling("call-0", "summarise the README"),
            Turn::saying("the README describes a harness"),
            Turn::saying("parent done"),
        ]),
        AllowAll,
    )
    .await;

    let (failed, answer) = run.first_result();
    assert!(!failed, "{answer}");
    assert_eq!(
        answer, "the README describes a harness",
        "the subagent's final answer is the tool's result"
    );
}

#[tokio::test]
async fn delegated_spend_lands_on_the_budget_that_delegated_it() {
    // ADR-0016's third point, and the reason agent mode runs on
    // `ToolContext::child_run_options` rather than `RunOptions::default()`: a
    // child on its own counter would give delegated work a fresh, unbounded
    // allowance, which is the difference between a bound and a suggestion.
    //
    // The script makes the *child* the spender. On a shared counter the
    // parent's next round boundary is past the budget and the run stops there,
    // two provider calls in. On separate counters the parent would be at 10 of
    // 100, would take its second round, and would answer normally.
    let workspace = tempfile::tempdir().expect("tempdir");

    let run = drive(
        workspace.path(),
        Script::new(vec![
            Turn::calling("call-0", "summarise the README").costing(10),
            Turn::saying("the README describes a harness").costing(200),
            Turn::saying("parent done").costing(10),
        ])
        .with_token_budget(100),
        AllowAll,
    )
    .await;

    assert_eq!(
        run.stopped_by,
        Some(Bound::TokenBudget),
        "what the child spent has to be what stops the parent"
    );
    assert_eq!(
        run.requests.all().len(),
        2,
        "the parent's second round must never have been started"
    );

    // The gap this pins deliberately, so nothing downstream assumes otherwise:
    // the *bound* is shared, the *tally* is not. A subagent has its own event
    // bus and mentra's relay of a delegated `UsageReport` onto the parent's bus
    // is `pub(crate)` — mentra does it for its own `task` intrinsic and a
    // host-registered tool cannot. So lan reports what the parent's own rounds
    // cost, and the run stopped on a total more than ten times that.
    assert_eq!(
        run.usage.total_tokens(),
        10,
        "lan tallies the rounds its own stream carried"
    );
}

#[tokio::test]
async fn a_subagent_gets_the_same_one_door() {
    // Uniformity is recursive by construction: mentra's subagent template
    // clones the parent's `AgentConfig`, hidden set included. Checked here
    // rather than assumed, because the whole point of hiding `shell` is lost
    // if the second level gets it back.
    let workspace = tempfile::tempdir().expect("tempdir");

    let run = drive(
        workspace.path(),
        Script::new(vec![
            Turn::calling("call-0", "summarise the README"),
            Turn::saying("child done"),
            Turn::saying("parent done"),
        ]),
        AllowAll,
    )
    .await;

    let child = run.requests.roster(1);
    assert!(
        child.contains(&SPAWN.to_string()),
        "a subagent still needs the door: {child:?}"
    );
    for replaced in ["shell", "background_run", "task"] {
        assert!(
            !child.contains(&replaced.to_string()),
            "{replaced} came back at depth one: {child:?}"
        );
    }
}

#[tokio::test]
async fn delegation_stops_at_the_floor() {
    // mentra's own floor is name-specific — it hides `task` from a subagent —
    // and does nothing for a tool lan registered, so this is spawn's own guard.
    // The refusal is only visible in the deepest agent's transcript: its events
    // never reach the parent's stream.
    let workspace = tempfile::tempdir().expect("tempdir");

    let run = drive(
        workspace.path(),
        Script::new(vec![
            Turn::calling("call-0", "level one"),
            Turn::calling("call-1", "level two"),
            Turn::calling("call-2", "level three"),
            Turn::saying("deepest done"),
            Turn::saying("middle done"),
            Turn::saying("parent done"),
        ]),
        AllowAll,
    )
    .await;

    assert!(
        run.requests
            .any_transcript_contains("goes no deeper than 2"),
        "the third level had to be refused, and told why"
    );
    assert_eq!(
        run.asked.len(),
        2,
        "a call refused by the floor never becomes a question for a person: {:?}",
        run.asked
            .iter()
            .map(|request| request.input.clone())
            .collect::<Vec<_>>()
    );
}
