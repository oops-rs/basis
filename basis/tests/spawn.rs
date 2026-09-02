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
//! - `--no-shell` still refuses, on the path `spawn` now uses;
//! - a command that named a target arrives at the executor still naming it,
//!   and one that named none arrives naming none (ADR-0021).
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
use basis::{
    AllowAll, ApprovalAnswer, ApprovalDecision, ApprovalRequest, Approver, Bound, CollectingSink,
    Event, RunUsage, SpawnTool, TurnOptions, approval::ApprovalGate, run::prepare_with_session,
    tools::SPAWN,
};
use mentra::{
    BuiltinProvider, ContentBlock, ModelInfo, Role, Runtime, RuntimePolicy, Session, TokenUsage,
    agent::{AgentConfig, ToolProfile, WorkspaceConfig},
    provider::{
        Provider, ProviderDescriptor, ProviderError, ProviderEventStream, Request, Response,
        provider_event_stream_from_response,
    },
    runtime::{CommandOutput, CommandRequest, RuntimeExecutor, VolatileRuntimeStore},
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

/// Remembers every command request the runtime routed to it, and runs none of
/// them.
///
/// Installed only by the tests that are about routing: with one in place no
/// command actually executes, and the rest of this file needs `echo` to really
/// run.
#[derive(Clone, Default)]
struct RecordingExecutor(Arc<Mutex<Vec<CommandRequest>>>);

impl RecordingExecutor {
    /// The target each routed command named, in order.
    fn targets(&self) -> Vec<Option<String>> {
        self.0
            .lock()
            .expect("not poisoned")
            .iter()
            .map(|request| request.target.clone())
            .collect()
    }
}

#[async_trait]
impl RuntimeExecutor for RecordingExecutor {
    async fn run(&self, request: CommandRequest) -> Result<CommandOutput, String> {
        let where_it_went = request
            .target
            .clone()
            .unwrap_or_else(|| "local".to_string());
        self.0.lock().expect("not poisoned").push(request);

        Ok(CommandOutput {
            stdout: format!("ran on {where_it_went}"),
            stderr: String::new(),
            success: true,
            status_code: Some(0),
            timed_out: false,
            stdout_truncated: false,
            stderr_truncated: false,
        })
    }
}

/// What a test asks of one run: the rounds, whether commands are on, what an
/// operator had already remembered, the turn's own bounds, and — for the tests
/// about routing — which targets exist and who serves them.
///
/// A struct rather than six positional arguments, and every method returns a
/// new value, so a test reads as the one thing it varies.
struct Script {
    turns: Vec<Turn>,
    commands: bool,
    rules: Vec<RememberedRule>,
    options: TurnOptions,
    targets: Vec<String>,
    executor: Option<RecordingExecutor>,
}

impl Script {
    fn new(turns: Vec<Turn>) -> Self {
        Self {
            turns,
            commands: true,
            rules: Vec::new(),
            options: TurnOptions::default(),
            targets: Vec::new(),
            executor: None,
        }
    }

    /// Registers `names` as this runtime's command targets and routes every
    /// command through `executor`, which runs nothing and remembers
    /// everything.
    fn routing(self, names: &[&str], executor: &RecordingExecutor) -> Self {
        Self {
            targets: names.iter().map(|name| (*name).to_string()).collect(),
            executor: Some(executor.clone()),
            ..self
        }
    }

    /// What `basis --no-shell` leaves a workspace with.
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

/// A runtime built the way [`basis::WorkspaceBuilder::open`] builds one:
/// `spawn` registered, the approval gate installed, and commands allowed or
/// not exactly as [`basis::ShellAccess`] would have set them.
fn runtime(
    workspace: &Path,
    turns: Vec<Turn>,
    commands: bool,
    targets: Vec<String>,
    executor: Option<RecordingExecutor>,
) -> (Runtime, ModelInfo, Requests) {
    let model = ModelInfo::new("scripted-model", BuiltinProvider::OpenAI);
    let (provider, asked) = ScriptedProvider::new(model.clone(), turns);

    let builder = Runtime::builder()
        .with_provider_instance(provider)
        .with_store(VolatileRuntimeStore::new())
        .with_policy(
            RuntimePolicy::workspace_bounded(workspace)
                .allow_shell_commands(commands)
                .allow_background_commands(commands),
        )
        .with_tool_authorizer(ApprovalGate::new())
        // The names basis's own `RuntimeBuilder` would have collected; this
        // file drives mentra directly, because a scripted provider is not
        // something basis's builder can be handed.
        .with_tool(SpawnTool::with_targets(targets));

    let builder = match executor {
        Some(executor) => builder.with_executor(executor),
        None => builder,
    };

    (
        builder.build().expect("runtime builds"),
        model,
        Requests(asked),
    )
}

/// The roster `agent_config` produces. That basis's own builder produces exactly
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

fn context() -> basis::ContextConfig {
    basis::ContextConfig {
        file_name: "AGENTS.md".to_string(),
        global_dir: None,
        walk_parents: false,
    }
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
    let (runtime, model, requests) = runtime(
        workspace,
        script.turns,
        script.commands,
        script.targets,
        script.executor,
    );
    let session = session(&runtime, workspace, model);
    for rule in script.rules {
        session.rule_store().add_rule(rule);
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut prepared = prepare_with_session(
        session,
        workspace,
        "do the thing",
        &context(),
        "openai",
        "scripted-model",
    )
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
        "and basis reads its output back: {output}"
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

    // The bound and the tally are one figure, which is what `RunUsage`'s own
    // doc promises: what a run reports spending counts the rounds of the work
    // it delegated as well. A subagent has its own event bus, so that only
    // holds because `spawn` relays the child's `UsageReport`s onto the
    // parent's — 10 for the parent's first round, 200 for the child's.
    assert_eq!(
        run.usage.total_tokens(),
        210,
        "a run that stopped on 210 tokens must not report having spent 10"
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
    // and does nothing for a tool basis registered, so this is spawn's own guard.
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

#[tokio::test]
async fn a_targeted_command_arrives_at_the_executor_still_naming_its_target() {
    // ADR-0021 end to end: the `!@mac` prefix is read once at the boundary and
    // the name it leaves behind survives the preview, the approver, and
    // mentra's policy, arriving on the `CommandRequest` the installed executor
    // reads. Nothing between those two points re-reads the model's string.
    let workspace = tempfile::tempdir().expect("tempdir");
    let executor = RecordingExecutor::default();

    let run = drive(
        workspace.path(),
        Script::new(vec![
            Turn::calling("call-0", "!@mac xcodebuild -list"),
            Turn::calling("call-1", "!cargo test -q"),
        ])
        .routing(&["mac"], &executor),
        AllowAll,
    )
    .await;

    assert_eq!(
        executor.targets(),
        vec![Some("mac".to_string()), None],
        "the targeted command names its target and the untargeted one names none"
    );

    let results = run.results();
    assert_eq!(
        results,
        vec![
            (false, "ran on mac".to_string()),
            (false, "ran on local".to_string()),
        ]
    );
}

#[tokio::test]
async fn the_approver_is_told_where_a_command_was_going() {
    // The routing decision is data, on the same wire contract the mode and the
    // body ride: an approver can answer differently per destination, and a
    // remembered rule can glob the same key.
    let workspace = tempfile::tempdir().expect("tempdir");
    let executor = RecordingExecutor::default();

    let run = drive(
        workspace.path(),
        Script::new(vec![
            Turn::calling("call-0", "!@mac xcodebuild -list"),
            Turn::calling("call-1", "!cargo test -q"),
        ])
        .routing(&["mac"], &executor),
        AllowAll,
    )
    .await;

    assert_eq!(run.asked[0].input["target"], "mac");
    assert_eq!(
        run.asked[1].input["target"], "local",
        "*here* has a spelling, so a rule can be written about it"
    );
}

#[tokio::test]
async fn a_target_nothing_registered_never_reaches_the_approver_or_the_executor() {
    // Refused in the preview, ahead of the approver and ahead of the rule
    // store, exactly as the delegation depth floor is: a destination that does
    // not exist is not a judgement call, so it is not a question for a person.
    let workspace = tempfile::tempdir().expect("tempdir");
    let executor = RecordingExecutor::default();

    let run = drive(
        workspace.path(),
        Script::new(vec![Turn::calling("call-0", "!@linux uname -a")]).routing(&["mac"], &executor),
        AllowAll,
    )
    .await;

    assert!(
        run.asked.is_empty(),
        "an unroutable name must never become a question: {:?}",
        run.asked
    );
    assert!(
        executor.targets().is_empty(),
        "and nothing may have run: {:?}",
        executor.targets()
    );

    let (failed, output) = run.first_result();
    assert!(failed, "{output}");
    assert!(output.contains("`linux`"), "the model reads why: {output}");
    assert!(output.contains("`mac`"), "and what does exist: {output}");
}

#[tokio::test]
async fn no_shell_refuses_a_targeted_command_too() {
    // A targeted command is still `Mode::Command`, so every shell-off guard
    // applies unchanged. Routing a command elsewhere is not a route around the
    // policy that guards running one at all (ADR-0021, ADR-0013).
    let workspace = tempfile::tempdir().expect("tempdir");
    let executor = RecordingExecutor::default();

    let run = drive(
        workspace.path(),
        Script::new(vec![Turn::calling("call-0", "!@mac xcodebuild -list")])
            .routing(&["mac"], &executor)
            .without_commands(),
        AllowAll,
    )
    .await;

    let (failed, output) = run.first_result();
    assert!(
        failed,
        "a targeted command must not succeed with commands off"
    );
    assert!(
        executor.targets().is_empty(),
        "and it must never have reached the executor"
    );
    assert!(
        output.contains("Shell command execution is disabled"),
        "the refusal has to say what refused it: {output}"
    );
}

#[tokio::test]
async fn a_pattern_rule_can_allow_a_command_on_one_target_and_not_another() {
    // The consequence ADR-0021 names: the target is in the same serialized
    // object every other key is, so an operator who wants the line drawn per
    // destination can draw it — deliberately, in the pattern, rather than for
    // free.
    //
    // Written `**` for continuity with rules stored under mentra's older
    // spelling; under 0.18.2 these patterns are matched as *data*, where `*`
    // and `**` mean the same thing and JSON's punctuation is literal. Before
    // 0.18.2 a path globber read them and none of this worked at all — see
    // ADR-0021's consequences.
    let workspace = tempfile::tempdir().expect("tempdir");
    let executor = RecordingExecutor::default();

    let run = drive(
        workspace.path(),
        Script::new(vec![
            Turn::calling("call-0", "!@mac xcodebuild -list"),
            Turn::calling("call-1", "!@builder xcodebuild -list"),
            Turn::calling("call-2", "!xcodebuild -list"),
        ])
        .routing(&["mac", "builder"], &executor)
        .remembering(RememberedRule {
            key: RuleKey {
                tool_name: SPAWN.to_string(),
                pattern: Some("**\"target\":\"mac\"**".to_string()),
            },
            allow: true,
            scope: PermissionRuleScope::Session,
            reason: None,
        }),
        // Anything the rule does not cover is refused, so this catches a
        // pattern that matched too much as well as one that matched nothing.
        RefusesForGood,
    )
    .await;

    // One question reached the reviewer: the `builder` call. The `mac` call was
    // answered by the pattern above it, and the untargeted one by the bare
    // `DenyForSession` the reviewer's own refusal left behind — so a pattern
    // naming `mac` covered neither another target nor here.
    assert_eq!(
        run.asked.len(),
        1,
        "{:?}",
        run.asked
            .iter()
            .map(|request| request.input.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(run.asked[0].input["target"], "builder");

    let results = run.results();
    assert_eq!(results[0], (false, "ran on mac".to_string()));
    for refused in &results[1..] {
        assert!(refused.0 && refused.1.contains(REFUSAL), "{refused:?}");
    }
    assert_eq!(
        executor.targets(),
        vec![Some("mac".to_string())],
        "only the allowlisted destination may have been reached"
    );
}

#[tokio::test]
async fn a_target_pattern_answers_before_the_approver_is_reached() {
    // The rung below the approver, on the new key: a rule that pins the
    // destination answers first, so an allowlist for one machine costs no
    // model round trip and the reviewer never sees the calls it covers. The
    // sibling above shows the same rule declining to cover a different
    // destination; this one shows it never reaching a person at all.
    //
    // Worth one line about why the short spelling is the spelling: on mentra
    // 0.18.1 and earlier this pattern matched *nothing*, because rules were
    // matched with a path globber and `cwd` — an absolute path — serializes
    // ahead of `target`. 0.18.2 matches them as data, which is why basis
    // requires it (ADR-0021).
    let workspace = tempfile::tempdir().expect("tempdir");
    let executor = RecordingExecutor::default();

    let run = drive(
        workspace.path(),
        Script::new(vec![Turn::calling("call-0", "!@mac xcodebuild -list")])
            .routing(&["mac"], &executor)
            .remembering(RememberedRule {
                key: RuleKey {
                    tool_name: SPAWN.to_string(),
                    pattern: Some("**\"target\":\"mac\"**".to_string()),
                },
                allow: true,
                scope: PermissionRuleScope::Session,
                reason: None,
            }),
        // The strictest approver there is, so this catches a pattern that
        // failed to match as surely as one that matched too much.
        RefusesForGood,
    )
    .await;

    assert!(
        run.asked.is_empty(),
        "a target the operator allowlisted must never reach the reviewer: {:?}",
        run.asked
    );
    assert_eq!(executor.targets(), vec![Some("mac".to_string())]);
}
