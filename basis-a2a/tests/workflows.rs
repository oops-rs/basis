//! A served agent held to workflows: its card lists them, a request may use
//! only the ones its authenticator verified, a run is built with those
//! workflows' tools and skills and no others — on a conversation's first
//! message and on every later one — and a task is stopped once a workflow
//! it was built with is withdrawn from its caller.
//!
//! The gateway is stood in for by an authenticator that reads the allowed
//! set off the bearer token, so each request can carry a different one, as
//! each request through a real gateway carries a freshly signed set.

#[path = "support/fake_host.rs"]
mod fake_host;
#[path = "support/harness.rs"]
mod harness;

use std::{collections::BTreeSet, sync::Arc};

use basis_a2a::{
    A2aService, AgentIdentity, Authenticator, ExtendedCard, InboundApproval, OpenError, Principal,
    Refusal, RequestHead, Security, ServeConfig, Workflow, Workflows,
    a2a::{AgentSkill, HttpAuthSecurityScheme, SecurityScheme, error_code},
};
use fake_host::FakeHost;
use futures::future::BoxFuture;
use harness::{Reply, ScriptedEndpoint, Served, base_config, isolate_environment, user_message};
use serde_json::{Value, json};

const EXTENSION: &str = "urn:example:a2a:workflows:v1";
const OWNER: &str = "the platform team";

/// `Bearer <name>|<id>,<id>` is the principal `<name>`, allowed those
/// workflows for this request. `Bearer <name>` alone is a caller whose
/// token says nothing about workflows: one that did not come through the
/// gateway that decides them.
struct Gateway;

impl Authenticator for Gateway {
    fn authenticate<'a>(
        &'a self,
        head: &'a RequestHead,
    ) -> BoxFuture<'a, Result<Principal, Refusal>> {
        Box::pin(async move {
            let token = head
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .ok_or(Refusal::Unauthenticated)?;
            let (name, workflows) = match token.split_once('|') {
                Some((name, workflows)) => (name, Some(workflows)),
                None => (token, None),
            };
            let principal = Principal::new(name).ok_or(Refusal::Unauthenticated)?;
            Ok(match workflows {
                Some(workflows) => {
                    principal.with_workflows(workflows.split(',').filter(|id| !id.is_empty()))
                }
                None => principal,
            })
        })
    }

    fn security(&self) -> Option<Security> {
        Some(Security::required(
            "gateway",
            SecurityScheme::HttpAuth(HttpAuthSecurityScheme {
                scheme: "Bearer".to_string(),
                description: None,
                bearer_format: Some("JWT".to_string()),
            }),
        ))
    }
}

/// `build` writes files and has a skill; `ask` reads. Nothing is shared, so
/// what a run is offered says exactly which workflows it was built for.
fn workflows() -> Workflows {
    Workflows::declaring(EXTENSION)
        .with_workflow(
            Workflow::new("build", "Build", "Runs builds.")
                .with_tag("ci")
                .with_tools(["write"])
                .with_skills(["release"]),
        )
        .with_workflow(Workflow::new("ask", "Ask", "Answers questions.").with_tools(["read"]))
        .with_contact(OWNER)
}

async fn serve(model: ScriptedEndpoint) -> Served {
    let served = Served::start_anonymous(model, |config| {
        config
            .with_authenticator(Arc::new(Gateway))
            .with_workflows(workflows())
            // Every consequential call is allowed, so a tool that does not
            // run was stopped by the roster and by nothing else.
            .with_approval(InboundApproval::Always)
    })
    .await;
    let skill = served.workspace.path().join(".basis/skills/release");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: release\ndescription: Cuts a release\n---\nSteps.\n",
    )
    .unwrap();
    served
}

fn send(text: &str, context_id: Option<&str>, immediately: bool) -> Value {
    json!({
        "message": user_message(text, context_id),
        "configuration": {"returnImmediately": immediately},
    })
}

/// A model turn that writes `built.txt`: the `build` workflow's work.
fn write_built() -> Reply {
    Reply::ToolCall {
        name: "write".to_string(),
        arguments: json!({"path": "built.txt", "content": "built"}),
    }
}

/// The body of the model's `n`th request.
fn asked(model: &ScriptedEndpoint, n: usize) -> Value {
    let request = &model.requests()[n];
    let body = request.split_once("\r\n\r\n").map_or("", |(_, body)| body);
    serde_json::from_str(body).expect("a JSON request body")
}

fn tools(request: &Value) -> BTreeSet<String> {
    request["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
        .collect()
}

fn names(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

/// Every message of `role` in `request`, joined.
fn said(request: &Value, role: &str) -> String {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == role)
        .map(|message| message["content"].to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_card_lists_each_workflow_as_a_skill_and_declares_the_extension() {
    let served = serve(ScriptedEndpoint::start(false)).await;
    let card = served.card().await;

    let skills = card["skills"].as_array().unwrap();
    assert_eq!(skills.len(), 2, "{card}");
    assert_eq!(skills[0]["id"], "build");
    assert_eq!(skills[0]["name"], "Build");
    assert_eq!(skills[0]["tags"], json!(["ci"]));
    assert_eq!(skills[1]["id"], "ask");
    let extension = card["capabilities"]["extensions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|extension| extension["uri"] == EXTENSION)
        .unwrap_or_else(|| panic!("the extension is declared: {card}"));
    assert_eq!(extension["required"], false);
}

#[tokio::test]
async fn a_request_allowed_no_workflow_starts_nothing() {
    let served = serve(ScriptedEndpoint::start(false)).await;

    // A token that says nothing about workflows did not come through the
    // gateway that decides them.
    let unsigned = served
        .as_token("alice")
        .call("SendMessage", send("hello", None, false))
        .await;
    assert_eq!(unsigned["error"]["code"], error_code::UNSUPPORTED_OPERATION);
    assert!(
        unsigned["error"]["message"]
            .as_str()
            .unwrap()
            .contains("without any"),
        "{unsigned}"
    );
    assert_eq!(
        served
            .as_token("alice")
            .error_code("ListTasks", json!({}))
            .await,
        error_code::UNSUPPORTED_OPERATION
    );

    // One that names only workflows this agent does not offer allows none.
    let none = served
        .as_token("alice|retired")
        .call("SendMessage", send("hello", None, false))
        .await;
    assert_eq!(none["error"]["code"], error_code::UNSUPPORTED_OPERATION);
    let message = none["error"]["message"].as_str().unwrap();
    assert!(message.contains("The platform team can open one"), "{none}");

    assert!(served.model.requests().is_empty(), "no run was started");
}

#[tokio::test]
async fn a_model_cannot_use_a_workflow_its_caller_is_closed_out_of() {
    // The model does what the caller asks whatever it was offered: it calls
    // the build workflow's tool by name.
    let served = serve(ScriptedEndpoint::scripted(false, vec![write_built()])).await;
    let alice = served.as_token("alice|ask");

    let task = alice
        .result("SendMessage", send("build it", None, false))
        .await["task"]
        .clone();
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED", "{task}");

    assert!(
        !served.workspace.path().join("built.txt").exists(),
        "the build workflow's tool ran for a caller closed out of it"
    );
    let first = asked(&served.model, 0);
    assert_eq!(
        tools(&first),
        names(&["read"]),
        "only `ask`'s tool exists for this run: no `write`, and no `load_skill` with no skill"
    );
    assert!(
        !said(&first, "system").contains("release"),
        "the build workflow's skill is not listed either"
    );
    // The run is told what is closed and whom to ask, so it can say so.
    let prompt = said(&first, "user");
    assert!(
        prompt.contains("closed to this caller: `build` (Build)"),
        "{prompt}"
    );
    assert!(prompt.contains(OWNER), "{prompt}");
    assert!(prompt.contains("build it"), "{prompt}");
    // And the call it made anyway came back as an error, not a write.
    let second = asked(&served.model, 1);
    let result = said(&second, "tool");
    assert!(!result.is_empty(), "the model is answered: {second}");
    assert!(!result.contains("built.txt"), "{result}");
    // The note is the host's: the history a caller reads has their own words.
    let history = task["history"].to_string();
    assert!(!history.contains("closed to this caller"), "{history}");
}

#[tokio::test]
async fn a_caller_allowed_the_workflow_gets_its_tools_and_skills() {
    let served = serve(ScriptedEndpoint::scripted(false, vec![write_built()])).await;
    let bob = served.as_token("bob|build,ask");

    let task = bob
        .result("SendMessage", send("build it", None, false))
        .await["task"]
        .clone();
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED", "{task}");

    assert_eq!(
        std::fs::read_to_string(served.workspace.path().join("built.txt")).unwrap(),
        "built"
    );
    let first = asked(&served.model, 0);
    assert_eq!(tools(&first), names(&["load_skill", "read", "write"]));
    assert!(said(&first, "system").contains("- release: Cuts a release"));
    assert!(
        !said(&first, "user").contains("closed to this caller"),
        "nothing is closed, so nothing is said about it"
    );
}

#[tokio::test]
async fn a_resumed_conversation_is_rebuilt_for_what_its_caller_is_allowed_now() {
    // Turn one answers in words; turn two, in the same conversation, reaches
    // for the tool the conversation's first run was built with.
    let served = serve(ScriptedEndpoint::scripted(
        false,
        vec![Reply::Numbered, write_built()],
    ))
    .await;

    let first = served
        .as_token("alice|build,ask")
        .result("SendMessage", send("hello", None, false))
        .await["task"]
        .clone();
    let context = first["contextId"].as_str().unwrap();
    assert_eq!(
        tools(&asked(&served.model, 0)),
        names(&["load_skill", "read", "write"])
    );

    // `build` has been withdrawn from alice by her next message.
    let second = served
        .as_token("alice|ask")
        .result("SendMessage", send("now build it", Some(context), false))
        .await["task"]
        .clone();
    assert_eq!(
        second["status"]["state"], "TASK_STATE_COMPLETED",
        "{second}"
    );
    assert_eq!(second["contextId"], context, "the same conversation");

    let resumed = asked(&served.model, 1);
    assert_eq!(
        tools(&resumed),
        names(&["read"]),
        "the roster the conversation's first run froze is not the one this run gets"
    );
    assert!(!said(&resumed, "system").contains("release"));
    assert!(
        !served.workspace.path().join("built.txt").exists(),
        "a resumed conversation ran a workflow its caller had lost"
    );
}

#[tokio::test]
async fn a_task_is_stopped_and_fails_once_its_workflow_is_withdrawn() {
    let served = serve(ScriptedEndpoint::start(true)).await;
    let id = served
        .as_token("alice|build")
        .result("SendMessage", send("build it", None, true))
        .await["task"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    served.model.reached(1).await;

    // While her set still holds `build`, the task is hers to watch.
    let working = served
        .as_token("alice|build,ask")
        .result("GetTask", json!({"id": id}))
        .await;
    assert_ne!(working["status"]["state"], "TASK_STATE_FAILED", "{working}");

    // The next request about it arrives without `build`.
    let stopped = served
        .as_token("alice|ask")
        .result("GetTask", json!({"id": id}))
        .await;
    assert_eq!(stopped["status"]["state"], "TASK_STATE_FAILED", "{stopped}");
    let reason = stopped["status"]["message"]["parts"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("a failed task says why: {stopped}"));
    assert!(reason.contains("`build`"), "{reason}");
    assert!(reason.contains("The platform team can open it"), "{reason}");

    // It stays failed, whoever asks next and whatever the run then does.
    served.model.release();
    for token in ["alice|build,ask", "alice|ask"] {
        let later = served
            .as_token(token)
            .wait_for(&id, "TASK_STATE_FAILED")
            .await;
        assert!(
            later["artifacts"].as_array().is_none_or(Vec::is_empty),
            "{later}"
        );
    }
    let canceled = served
        .as_token("alice|ask")
        .result("CancelTask", json!({"id": id}))
        .await;
    assert_eq!(
        canceled["status"]["state"], "TASK_STATE_FAILED",
        "{canceled}"
    );
    assert_eq!(
        served
            .as_token("alice|ask")
            .error_code("SubscribeToTask", json!({"id": id}))
            .await,
        error_code::UNSUPPORTED_OPERATION,
        "a stopped task has nothing left to stream"
    );
    let listed = served
        .as_token("alice|ask")
        .result("ListTasks", json!({}))
        .await;
    assert_eq!(
        listed["tasks"][0]["status"]["state"], "TASK_STATE_FAILED",
        "{listed}"
    );
}

#[tokio::test]
async fn each_operation_that_names_a_task_stops_one_whose_workflow_is_withdrawn() {
    for operation in ["CancelTask", "SubscribeToTask", "ListTasks", "SendMessage"] {
        let served = serve(ScriptedEndpoint::start(true)).await;
        let task = served
            .as_token("alice|build")
            .result("SendMessage", send("build it", None, true))
            .await["task"]
            .clone();
        let id = task["id"].as_str().unwrap();
        served.model.reached(1).await;

        let alice = served.as_token("alice|ask");
        match operation {
            "CancelTask" => {
                let stopped = alice.result("CancelTask", json!({"id": id})).await;
                assert_eq!(stopped["status"]["state"], "TASK_STATE_FAILED", "{stopped}");
            }
            "SubscribeToTask" => {
                alice.call("SubscribeToTask", json!({"id": id})).await;
            }
            "ListTasks" => {
                alice.result("ListTasks", json!({})).await;
            }
            // A follow-up into the conversation whose last task is running
            // on the withdrawn workflow.
            _ => {
                let context = task["contextId"].as_str().unwrap();
                alice
                    .call("SendMessage", send("and then?", Some(context), true))
                    .await;
            }
        }

        let stopped = served
            .as_token("alice|build,ask")
            .result("GetTask", json!({"id": id}))
            .await;
        assert_eq!(
            stopped["status"]["state"], "TASK_STATE_FAILED",
            "{operation} left the task running: {stopped}"
        );
        served.model.release();
    }
}

#[tokio::test]
async fn a_settled_task_is_read_as_it_settled_whatever_was_withdrawn_since() {
    let served = serve(ScriptedEndpoint::start(false)).await;
    let task = served
        .as_token("alice|build")
        .result("SendMessage", send("build it", None, false))
        .await["task"]
        .clone();
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED", "{task}");

    let later = served
        .as_token("alice|ask")
        .result("GetTask", json!({"id": task["id"]}))
        .await;
    assert_eq!(later["status"]["state"], "TASK_STATE_COMPLETED", "{later}");
}

#[tokio::test]
async fn another_callers_withdrawal_touches_nobody_elses_task() {
    let served = serve(ScriptedEndpoint::start(true)).await;
    let id = served
        .as_token("alice|build")
        .result("SendMessage", send("build it", None, true))
        .await["task"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    served.model.reached(1).await;

    // Bob is allowed less, and alice's task is not his to name.
    assert_eq!(
        served
            .as_token("bob|ask")
            .error_code("GetTask", json!({"id": id}))
            .await,
        error_code::TASK_NOT_FOUND
    );
    let still = served
        .as_token("alice|build")
        .result("GetTask", json!({"id": id}))
        .await;
    assert_ne!(still["status"]["state"], "TASK_STATE_FAILED", "{still}");
    served.model.release();
}

#[tokio::test]
async fn a_host_is_handed_the_allowed_set_and_its_task_is_bound_to_what_it_runs() {
    let host = FakeHost::held();
    // This host's dispatch is fixed: whatever it is allowed, it runs `ask`.
    host.runs_only(&["ask"]);
    let state = tempfile::tempdir().unwrap();
    let hosted = Workflows::declaring(EXTENSION)
        .with_workflow(Workflow::new("build", "Build", "Runs builds."))
        .with_workflow(Workflow::new("ask", "Ask", "Answers questions."))
        .with_contact(OWNER);
    let config = ServeConfig::hosted(
        AgentIdentity::new("host", "Runs its own tasks."),
        state.path(),
        Arc::clone(&host) as _,
    )
    .with_authenticator(Arc::new(Gateway))
    .with_workflows(hosted);
    let served = Served::start_config(config, state, None).await;

    // Granted out of card order, with one this agent does not offer.
    let id = served
        .as_token("alice|ask,retired,build")
        .result("SendMessage", send("what is it?", None, true))
        .await["task"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let run = host.started(0).await;
    assert_eq!(
        host.calls().starts[0].workflows.as_deref(),
        Some(&["build".to_string(), "ask".to_string()][..]),
        "card order, and only what the card lists"
    );

    // Losing `build` does not stop a task that runs `ask`.
    let running = served
        .as_token("alice|ask")
        .result("GetTask", json!({"id": id}))
        .await;
    assert_eq!(
        running["status"]["state"], "TASK_STATE_WORKING",
        "{running}"
    );
    assert!(host.calls().cancels.is_empty());

    // Losing `ask` does: the host is told to stop, and the task has failed.
    let stopped = served
        .as_token("alice|build")
        .result("GetTask", json!({"id": id}))
        .await;
    assert_eq!(stopped["status"]["state"], "TASK_STATE_FAILED", "{stopped}");
    assert!(
        stopped["status"]["message"]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .contains("`ask`"),
        "{stopped}"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while host.calls().cancels.is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the host was never told to stop"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(host.calls().cancels, [run]);
}

#[tokio::test]
async fn workflows_that_could_not_be_enforced_as_configured_are_refused() {
    isolate_environment();
    let model = ScriptedEndpoint::start(false);
    let open = |configure: &dyn Fn(ServeConfig) -> ServeConfig| {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let config = base_config(workspace.path(), data.path(), &model)
            .with_authenticator(Arc::new(Gateway));
        A2aService::open(configure(config)).err()
    };
    let skill = || AgentSkill {
        id: "extra".into(),
        name: "Extra".into(),
        description: "Not a workflow.".into(),
        tags: vec![],
        examples: None,
        input_modes: None,
        output_modes: None,
        security_requirements: None,
    };

    assert!(open(&|config| config.with_workflows(workflows())).is_none());
    assert!(matches!(
        open(&|config| config.with_workflows(Workflows::declaring(EXTENSION))),
        Some(OpenError::NoWorkflows)
    ));
    assert!(matches!(
        open(&|config| config.with_workflows(
            workflows().with_workflow(Workflow::new("ask", "Again", "Twice."))
        )),
        Some(OpenError::WorkflowId(id)) if id == "ask"
    ));
    assert!(matches!(
        open(&|config| config
            .with_workflows(workflows())
            .with_extended_card(ExtendedCard::new().with_skill(skill()))),
        Some(OpenError::SkillsBesideWorkflows("the extended card"))
    ));

    // A host runs its own tasks: tools and skills configure nothing there.
    let state = tempfile::tempdir().unwrap();
    let hosted = ServeConfig::hosted(
        AgentIdentity::new("host", "Runs its own tasks."),
        state.path(),
        FakeHost::held() as _,
    )
    .with_authenticator(Arc::new(Gateway))
    .with_workflows(workflows());
    assert!(matches!(
        A2aService::open(hosted).err(),
        Some(OpenError::NotForHostedTasks(_))
    ));
}

#[tokio::test]
async fn a_server_with_no_workflows_holds_nobody_to_any() {
    let served = Served::start_anonymous(ScriptedEndpoint::start(false), |config| {
        config.with_authenticator(Arc::new(Gateway))
    })
    .await;

    // Whatever a token says of workflows, or does not.
    for token in ["alice", "bob|build"] {
        let task = served
            .as_token(token)
            .result("SendMessage", send("hello", None, false))
            .await["task"]
            .clone();
        assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED", "{task}");
    }
    assert!(
        tools(&asked(&served.model, 0)).contains("grep"),
        "the workspace's own roster"
    );
}
