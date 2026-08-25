//! What building a runtime settles, and what it must not.
//!
//! Split out of `builder.rs` for its size, the same ruling as
//! `workspace/builder/tests.rs` — where most of these lived before ADR-0018
//! moved the knobs they exercise.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use super::*;
// Spelled the way a downstream that depends only on `basis` has to spell it,
// which is what makes the stubs below a real check on the re-exports.
use crate::runtime::{
    CommandOutput, CommandRequest, CommandSpec, LocalRuntimeExecutor, RuntimeExecutor,
};
use crate::tools::SPAWN;

fn read_http_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut header_end = None;
    let mut content_length = 0_usize;

    loop {
        let read = stream.read(&mut buffer).expect("read request");
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if header_end.is_none()
            && let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let end = index + 4;
            header_end = Some(end);
            let headers = String::from_utf8_lossy(&bytes[..end]);
            content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .unwrap_or_default();
        }
        if header_end.is_some_and(|end| bytes.len() >= end + content_length) {
            break;
        }
    }

    String::from_utf8(bytes).expect("request should be utf8")
}

fn spawn_two_response_server() -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let address = listener.local_addr().expect("read server address");
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for index in 1..=2 {
            let (mut stream, _) = listener.accept().expect("accept request");
            requests.push(read_http_request(&mut stream));
            let response_id = format!("resp_{index}");
            let body = format!(
                concat!(
                    "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"{}\",\"model\":\"gpt-5\",\"status\":\"in_progress\"}}}}\n\n",
                    "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"{}\",\"model\":\"gpt-5\",\"status\":\"completed\"}}}}\n\n"
                ),
                response_id, response_id
            );
            let response = format!(
                concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "connection: close\r\n",
                    "content-type: text/event-stream\r\n",
                    "content-length: {}\r\n\r\n",
                    "{}"
                ),
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        }
        requests
    });

    (format!("http://{address}/"), handle)
}

#[tokio::test]
async fn a_responses_endpoint_skips_automatic_previous_response_id_chaining() {
    let (base_url, handle) = spawn_two_response_server();
    let provider = responses_provider(
        BuiltinProvider::OpenAI,
        &base_url,
        Credential::new(Some("test-key")),
    );

    for (index, message) in ["first", "second"].into_iter().enumerate() {
        let request = mentra::provider_core::Request {
            model: Cow::Borrowed("gpt-5"),
            system: None,
            messages: Cow::Owned(vec![mentra::Message::user(mentra::ContentBlock::text(
                message,
            ))]),
            tools: Cow::Owned(Vec::new()),
            tool_choice: None,
            temperature: None,
            max_output_tokens: None,
            metadata: Cow::Owned(BTreeMap::new()),
            provider_request_options: Default::default(),
        };
        let mut stream = provider
            .session()
            .stream_response(request)
            .await
            .expect("compatible provider should stream");
        while let Some(event) = stream.recv().await {
            event.expect("response event should decode");
        }
        if index == 0 {
            assert_eq!(
                provider.session().latest_response_id().as_deref(),
                Some("resp_1"),
                "the second request must have provider state available to suppress"
            );
        }
    }

    let requests = handle.join().expect("server should capture requests");
    for request in requests {
        let body = request.split("\r\n\r\n").nth(1).unwrap_or_default();
        let payload: serde_json::Value =
            serde_json::from_str(body).expect("request body should be json");
        assert!(payload.get("previous_response_id").is_none());
    }
}

#[test]
fn builders_return_new_values() {
    let base = RuntimeBuilder::default();
    let derived = base.with_provider(BuiltinProvider::Anthropic);

    assert_eq!(derived.provider, Some(BuiltinProvider::Anthropic));
    assert_eq!(
        RuntimeBuilder::default().provider,
        None,
        "a fresh builder detects the provider"
    );
}

#[test]
fn history_goes_where_mentra_puts_it_unless_the_caller_says_otherwise() {
    // The default must stay the default: a host with conversations already
    // in mentra's database would lose sight of them if building a runtime
    // started relocating the store on its own.
    assert_eq!(RuntimeBuilder::default().history, None);
    assert_eq!(
        RuntimeBuilder::default()
            .with_store_dir("/elsewhere")
            .history,
        Some(History::Directory(PathBuf::from("/elsewhere")))
    );
    assert_eq!(
        RuntimeBuilder::default().with_ephemeral_history().history,
        Some(History::Ephemeral)
    );
}

#[test]
fn the_last_word_about_history_is_the_one_that_counts() {
    // The two knobs answer one question, so they write one field and neither
    // can be left half in force. A helper that hands out ephemeral builders
    // has to be overridable by the caller that wants its history kept, and
    // the reverse has to work as plainly.
    assert_eq!(
        RuntimeBuilder::default()
            .with_store_dir("/elsewhere")
            .with_ephemeral_history()
            .history,
        Some(History::Ephemeral)
    );
    assert_eq!(
        RuntimeBuilder::default()
            .with_ephemeral_history()
            .with_store_dir("/elsewhere")
            .history,
        Some(History::Directory(PathBuf::from("/elsewhere")))
    );
}

#[test]
fn a_relocated_store_takes_the_compaction_snapshots_with_it() {
    // The hazard `with_store_dir` exists for, applied to the other file mentra
    // writes: a snapshot is a verbatim copy of a conversation, and left at
    // mentra's default it lands under a directory keyed by the *process's* cwd
    // — so `basis -C /other/repo` would file this workspace's transcript under
    // whichever one the process happened to start in.
    let store = tempfile::tempdir().expect("tempdir");
    let runtime = offline()
        .with_store_dir(store.path())
        .build()
        .expect("builds offline");

    assert_eq!(
        runtime.transcripts_dir(),
        store.path().join("transcripts"),
        "snapshots belong beside the database of the same conversations"
    );
}

#[test]
fn the_default_snapshot_directory_is_the_one_mentra_would_have_used() {
    // Relocation, not a second scheme: a runtime that said nothing must land
    // where it always did, or an upgrade would strand every transcript already
    // written.
    let runtime = RuntimeBuilder::default()
        .with_base_url("http://127.0.0.1:1/v1")
        .with_api_key("test-key")
        .build()
        .expect("builds offline");

    assert_eq!(
        runtime.transcripts_dir(),
        store::default_directory().join("transcripts")
    );
}

#[test]
fn an_ephemeral_runtime_keeps_its_snapshots_out_of_the_users_data() {
    // mentra writes the snapshot before it summarizes and does not ask the
    // store first — `allows_disk_artifacts` gates tool-output spill, not this —
    // so "nowhere" is not on offer. The temp directory is: never the user's
    // data directory, never the workspace, and never shared between two
    // runtimes that were each promised their own disposable history.
    let one = offline().build().expect("builds offline");
    let two = offline().build().expect("builds offline");

    assert!(
        one.transcripts_dir().starts_with(std::env::temp_dir()),
        "{}",
        one.transcripts_dir().display()
    );
    assert_ne!(one.transcripts_dir(), two.transcripts_dir());
}

#[test]
fn a_supplied_credential_is_not_printed() {
    let printed = format!(
        "{:?}",
        RuntimeBuilder::default().with_api_key("sk-secret-value")
    );

    assert!(!printed.contains("sk-secret-value"));
    assert!(printed.contains("redacted"));
}

#[test]
fn command_environment_is_scoped_and_redacted() {
    let builder = RuntimeBuilder::default()
        .with_command_environment("BASIS_TASK_ID", "parent")
        .with_command_environment("BASIS_TASK_ID", "child");

    assert_eq!(
        builder.command_environment.get("BASIS_TASK_ID"),
        Some(&"child".to_string()),
        "the last fixed value is the only value a command should receive"
    );
    let printed = format!("{builder:?}");
    assert!(printed.contains("BASIS_TASK_ID"), "{printed}");
    assert!(!printed.contains("child"), "{printed}");
}

struct Named(&'static str);

#[async_trait::async_trait]
impl Interceptor for Named {
    fn name(&self) -> &str {
        self.0
    }

    async fn intercept(
        &self,
        _call: &crate::HookRequest,
    ) -> Result<crate::HookOutcome, crate::InterceptorError> {
        Ok(crate::HookOutcome::Allow)
    }
}

#[test]
fn interceptors_append_in_the_order_they_were_registered() {
    // Ordering is the whole of what registration decides — the first
    // refusal short-circuits — so it has to be the caller's, not a set's.
    let builder = RuntimeBuilder::default()
        .with_interceptor(Named("first"))
        .with_interceptor(Named("second"));

    assert_eq!(
        builder
            .interceptors
            .iter()
            .map(|interceptor| interceptor.name())
            .collect::<Vec<_>>(),
        vec!["first", "second"]
    );
    assert!(
        RuntimeBuilder::default().interceptors.is_empty(),
        "a fresh builder intercepts nothing"
    );
}

#[test]
fn a_registered_interceptor_is_named_in_the_debug_view() {
    // The Debug impl is hand-written to keep a credential out of a log, so
    // each new field has to be added to it deliberately.
    let printed = format!(
        "{:?}",
        RuntimeBuilder::default().with_interceptor(Named("redact"))
    );

    assert!(printed.contains("redact"), "{printed}");
}

#[test]
fn the_shared_policy_grants_commands_with_workspace_bounded_patience() {
    // `RuntimePolicy` exposes no getters, so the pin reads its derived Debug
    // — brittle against a mentra rename, but a rename would fail loudly here
    // rather than silently changing what a shared runtime may do.
    let printed = format!("{:?}", shared_policy());

    assert!(printed.contains("allow_shell_commands: true"), "{printed}");
    assert!(
        printed.contains("allow_background_commands: true"),
        "{printed}"
    );
    assert!(
        printed.contains("default_command_timeout: 120s"),
        "{printed}"
    );
    assert!(printed.contains("max_command_timeout: 600s"), "{printed}");
    assert!(
        printed.contains("allowed_read_roots: []") && printed.contains("allowed_write_roots: []"),
        "no roots: each agent is confined to its own base_dir, and no \
         workspace's root may widen another's: {printed}"
    );
}

/// A minimal native tool, standing in for whatever a host actually needs:
/// close over an id, echo it back. Proves `with_tool` carries a concrete
/// type through to mentra's own registry without basis touching its shape.
struct Echo(&'static str);

impl mentra::tool::ToolDefinition for Echo {
    fn descriptor(&self) -> mentra::tool::RuntimeToolDescriptor {
        mentra::tool::RuntimeToolDescriptor::builder(self.0)
            .description("echoes its own name")
            .input_schema(serde_json::json!({"type": "object", "properties": {}}))
            .build()
    }
}

#[async_trait::async_trait]
impl mentra::tool::ToolExecutor for Echo {
    async fn execute(
        &self,
        _ctx: mentra::tool::ParallelToolContext,
        _input: serde_json::Value,
    ) -> mentra::tool::ToolResult {
        Ok(self.0.to_string())
    }
}

#[test]
fn a_host_tool_reaches_mentras_own_registry() {
    // The whole point of `with_tool`: a type basis never defined, carried
    // through build() by value, ends up exactly where `spawn` does.
    let runtime = RuntimeBuilder::default()
        .with_base_url("http://127.0.0.1:1/v1")
        .with_api_key("test-key")
        .with_ephemeral_history()
        .with_tool(Echo("host_echo"))
        .build()
        .expect("builds offline");

    let tools = runtime.mentra_runtime().tools();
    assert!(
        tools.iter().any(|tool| tool.provider.name == "host_echo"),
        "a host tool registered via with_tool must reach mentra's own registry: {tools:?}"
    );
    assert!(
        tools.iter().any(|tool| tool.provider.name == SPAWN),
        "a host tool must not replace basis's own spawn: {tools:?}"
    );
}

#[test]
fn host_tools_register_in_the_order_they_were_added() {
    let runtime = RuntimeBuilder::default()
        .with_base_url("http://127.0.0.1:1/v1")
        .with_api_key("test-key")
        .with_ephemeral_history()
        .with_tool(Echo("first"))
        .with_tool(Echo("second"))
        .build()
        .expect("builds offline");

    let tools = runtime.mentra_runtime().tools();
    for name in ["first", "second"] {
        assert!(
            tools.iter().any(|tool| tool.provider.name == name),
            "missing {name}: {tools:?}"
        );
    }
}

#[test]
fn a_shared_runtime_resolves_its_provider_without_the_network() {
    // `build` is sync, so everything it does must be local: credential
    // lookup, assembly, nothing else. A closed port proves nothing is
    // contacted — reaching it would error, not hang.
    let runtime = RuntimeBuilder::default()
        .with_base_url("http://127.0.0.1:1/v1")
        .with_api_key("test-key")
        .with_ephemeral_history()
        .build()
        .expect("builds offline");

    assert_eq!(runtime.provider(), "openai");
}

/// A base URL is filed under the id the model will be looked up by, on either
/// wire.
///
/// [`Runtime::resolve_model`](crate::Runtime::resolve_model) asks mentra for a
/// model *under a provider id*, and the id it asks with is the one resolution
/// settled on. A definition registered under any other name is a provider
/// mentra cannot find, and the failure arrives at the first turn rather than at
/// `build` — so it is asserted where it is decided. A *named* provider beside
/// a base URL is what distinguishes it: `openai` is the id an unnamed one
/// resolves to anyway, so it would pass whatever the code did.
#[test]
fn a_custom_endpoint_is_filed_under_the_provider_the_choice_resolved() {
    for wire in [Wire::ChatCompletions, Wire::Responses] {
        let runtime = RuntimeBuilder::default()
            .with_provider(BuiltinProvider::OpenRouter)
            .with_base_url("http://127.0.0.1:1/v1")
            .with_api_key("test-key")
            .with_wire(wire)
            .with_ephemeral_history()
            .build()
            .expect("builds offline");

        assert_eq!(runtime.provider(), "openrouter");
        let descriptors = runtime.mentra_runtime().providers();
        let registered: Vec<&str> = descriptors
            .iter()
            .map(|descriptor| descriptor.id.as_str())
            .collect();
        assert!(
            registered.contains(&"openrouter"),
            "{wire:?} filed the endpoint where the model is not looked up: {registered:?}"
        );
    }
}

/// Resolution can answer "no key" — a local preset, or a base URL with none
/// exported — and the Responses provider basis builds from that answer must
/// ask the server for nothing rather than present an empty bearer it would
/// refuse. (The chat/completions branch is mentra's `with_openai_compatible`,
/// which takes the same `Option` and is pinned end to end in
/// `basis/tests/workspace.rs`.)
#[test]
fn a_responses_endpoint_without_a_key_is_not_asked_to_bear_one() {
    let responses = responses_provider(
        BuiltinProvider::OpenAI,
        "http://127.0.0.1:1/",
        Credential::new(None),
    );
    assert!(matches!(
        responses.definition().auth_scheme,
        AuthScheme::None
    ));
}

#[test]
fn a_responses_endpoint_with_a_key_bears_it() {
    let responses = responses_provider(
        BuiltinProvider::OpenAI,
        "http://127.0.0.1:1/",
        Credential::new(Some("k")),
    );
    assert!(matches!(
        responses.definition().auth_scheme,
        AuthScheme::BearerToken
    ));
}

/// A local preset resolves with no key at all, and the runtime builds on it
/// offline exactly as a keyed preset does.
#[test]
fn a_local_preset_builds_without_a_key() {
    let runtime = RuntimeBuilder::default()
        .with_provider(BuiltinProvider::Ollama)
        .with_ephemeral_history()
        .build()
        .expect("builds offline");

    assert_eq!(runtime.provider(), "ollama");
}

/// The smallest provider a host could hand to
/// [`RuntimeBuilder::with_provider_instance`]. Its stream is never driven
/// here — what these tests pin is selection, refusal and identity; the
/// end-to-end streaming case is `tests/provider_instance.rs`, written against
/// `basis` alone so the authoring re-exports are enforced by compilation.
struct StubProvider;

#[crate::async_trait]
impl Provider for StubProvider {
    fn descriptor(&self) -> mentra::ProviderDescriptor {
        mentra::ProviderDescriptor::new("stub")
    }

    async fn list_models(&self) -> Result<Vec<mentra::ModelInfo>, mentra::ProviderError> {
        Ok(vec![mentra::ModelInfo::new("stub-model", "stub")])
    }

    async fn stream(
        &self,
        _request: mentra::Request<'_>,
    ) -> Result<mentra::ProviderEventStream, mentra::ProviderError> {
        Err(mentra::ProviderError::UnsupportedCapability(
            "this stub is never streamed".to_string(),
        ))
    }
}

/// An instance is an answer: nothing is resolved, nothing is read from the
/// environment, and the runtime is registered — and reported — under the id
/// the instance's own descriptor chose.
#[test]
fn a_supplied_instance_builds_offline_and_answers_to_its_own_id() {
    let runtime = RuntimeBuilder::default()
        .with_provider_instance(StubProvider)
        .with_ephemeral_history()
        .build()
        .expect("no credential, no environment, no network");

    assert_eq!(runtime.provider(), "stub");
    let registered: Vec<String> = runtime
        .mentra_runtime()
        .providers()
        .iter()
        .map(|descriptor| descriptor.id.to_string())
        .collect();
    assert!(
        registered.contains(&"stub".to_string()),
        "the instance must be filed where models are looked up: {registered:?}"
    );
}

/// The refusal, once per knob resolution reads, and not all in one call
/// order: which was said first must not matter, because both are still in
/// force when `build` runs.
#[test]
fn an_instance_beside_a_resolution_knob_is_refused_by_name() {
    let cases: Vec<(RuntimeBuilder, &str)> = vec![
        (
            RuntimeBuilder::default()
                .with_provider_instance(StubProvider)
                .with_provider(BuiltinProvider::OpenAI),
            "with_provider",
        ),
        (
            RuntimeBuilder::default()
                .with_base_url("http://127.0.0.1:1/v1")
                .with_provider_instance(StubProvider),
            "with_base_url",
        ),
        (
            RuntimeBuilder::default()
                .with_provider_instance(StubProvider)
                .with_api_key("sk-unattributable"),
            "with_api_key",
        ),
    ];

    for (told_twice, knob) in cases {
        let error = told_twice
            .with_ephemeral_history()
            .build()
            .expect_err("two answers to one question must not rank silently");
        match error {
            RunError::Provider(provider::ProviderError::AmbiguousProviderSource {
                knob: named,
            }) => assert_eq!(named, knob),
            other => panic!("the refusal must name the knob, got: {other:?}"),
        }
    }
}

/// A file yields where an explicit call is refused: `with_config` fills
/// emptiness, and an instance means the provider question is not empty. The
/// model policy still arrives — which model is asked for is orthogonal to
/// who answers.
#[test]
fn a_config_files_provider_yields_to_a_supplied_instance() {
    let (_dir, config) = config_saying(EVERY_KEY);

    let filled = RuntimeBuilder::default()
        .with_provider_instance(StubProvider)
        .with_config(&config);

    assert_eq!(filled.provider, None, "the file's provider goes unread");
    assert_eq!(filled.base_url, None, "and so does its base URL");
    assert_eq!(
        filled.model,
        Some(ModelSelector::Id("from-the-file".to_string()))
    );

    let runtime = filled
        .with_ephemeral_history()
        .build()
        .expect("a yielded file is not a conflict");
    assert_eq!(runtime.provider(), "stub");
}

#[test]
fn a_supplied_instance_is_named_in_the_debug_view() {
    let printed = format!(
        "{:?}",
        RuntimeBuilder::default().with_provider_instance(StubProvider)
    );

    assert!(printed.contains("stub"), "{printed}");
}

/// An executor that reaches nothing, standing in for whatever a host actually
/// writes: what is under test is the routing table, not the transport.
///
/// Written against `crate::…` paths and nothing else, deliberately. That is
/// exactly what a downstream depending only on `basis` can write, so if a
/// mentra type ever stops being re-exported this stops compiling — which is
/// the whole point of the rule on [`crate::CancellationToken`], enforced here
/// rather than promised in a doc comment.
struct Nowhere;

#[crate::async_trait]
impl RuntimeExecutor for Nowhere {
    async fn run(&self, _request: CommandRequest) -> Result<CommandOutput, String> {
        Err("this executor reaches nothing".to_string())
    }
}

/// The same, using every re-exported type an executor's body actually touches:
/// the request, the spec inside it, the output it answers with, and the local
/// executor a wrapper delegates the ordinary case to.
struct Echoes;

#[crate::async_trait]
impl RuntimeExecutor for Echoes {
    async fn run(&self, request: CommandRequest) -> Result<CommandOutput, String> {
        if request.target.is_none() {
            return LocalRuntimeExecutor.run(request).await;
        }
        let CommandSpec::Shell { command } = &request.spec;

        Ok(CommandOutput {
            stdout: command.clone(),
            stderr: String::new(),
            success: true,
            status_code: Some(0),
            timed_out: false,
            stdout_truncated: false,
            stderr_truncated: false,
        })
    }
}

#[test]
fn an_executor_can_be_written_against_basis_alone() {
    // A host that depends only on `basis` must be able to satisfy the bound
    // `with_command_target` asks for. This compiles and registers, which is
    // the assertion; `Echoes` above is where the type coverage lives.
    let runtime = offline()
        .with_command_target("echo", Echoes)
        .build()
        .expect("builds offline");

    assert_eq!(runtime.provider(), "openai");
}

fn offline() -> RuntimeBuilder {
    RuntimeBuilder::default()
        .with_base_url("http://127.0.0.1:1/v1")
        .with_api_key("test-key")
        .with_ephemeral_history()
}

#[test]
fn command_targets_are_scoped_and_the_last_registration_wins() {
    let builder = RuntimeBuilder::default()
        .with_command_target("mac", Nowhere)
        .with_command_target("mac", Nowhere)
        .with_command_target("builder", Nowhere);

    assert_eq!(
        builder.command_targets.keys().collect::<Vec<_>>(),
        vec!["builder", "mac"],
        "one name is one destination, and the last word about it counts"
    );
}

#[test]
fn a_registered_target_is_named_in_the_debug_view_and_its_executor_is_not() {
    // The Debug impl is hand-written to keep a credential out of a log, so
    // each new field has to be added to it deliberately — and an executor
    // closes over whatever reaches its machine.
    let printed = format!(
        "{:?}",
        RuntimeBuilder::default().with_command_target("mac", Nowhere)
    );

    assert!(printed.contains("command_targets"), "{printed}");
    assert!(printed.contains("mac"), "{printed}");
    assert!(!printed.contains("Nowhere"), "{printed}");
}

#[test]
fn a_name_that_cannot_be_routed_on_is_refused_by_build_rather_than_by_a_panic() {
    // Every other piece of bad input on this builder is answered here — an
    // unattributed credential is refused by `provider::resolve_with` at the
    // same moment — so a host reading its targets out of its own configuration
    // can report a bad one instead of losing the process to it.
    for (name, expected) in [
        ("my target", "letters, digits"),
        ("mac/os", "letters, digits"),
        ("", "letters, digits"),
        (LOCAL_TARGET, "names no target"),
    ] {
        let error = offline()
            .with_command_target(name, Nowhere)
            .build()
            .expect_err("this name cannot be routed on");

        assert!(
            matches!(&error, RunError::CommandTarget { name: refused, .. } if refused == name),
            "{name}: {error}"
        );
        assert!(error.to_string().contains(expected), "{name}: {error}");
    }
}

#[test]
fn a_usable_name_builds_and_the_model_is_told_the_prefix() {
    // The end of the wiring: the names the builder collected reach the one
    // tool that has to know them, and the description a model reads gains the
    // `!@` prefix exactly when there is somewhere to route to (ADR-0021).
    let runtime = offline()
        .with_command_target("mac", Nowhere)
        .build()
        .expect("builds offline");

    let described = runtime
        .mentra_runtime()
        .tools()
        .into_iter()
        .find(|tool| tool.provider.name == SPAWN)
        .and_then(|tool| tool.provider.description)
        .expect("spawn is registered and described");

    assert!(described.contains("!@<target> <command>"), "{described}");
    assert!(described.contains("`mac`"), "{described}");
}

#[test]
fn a_runtime_with_no_targets_never_mentions_the_prefix() {
    let runtime = offline().build().expect("builds offline");

    let described = runtime
        .mentra_runtime()
        .tools()
        .into_iter()
        .find(|tool| tool.provider.name == SPAWN)
        .and_then(|tool| tool.provider.description)
        .expect("spawn is registered and described");

    assert!(
        !described.contains("!@"),
        "a door that is not there must not be advertised: {described}"
    );
}

/// A schedule visibly unlike mentra's default in every field, so a test that
/// finds it somewhere knows it travelled rather than coincided.
fn patient() -> ProviderRetry {
    ProviderRetry {
        base_delay: std::time::Duration::from_secs(2),
        max_delay: std::time::Duration::from_secs(30),
        retry_after_cap: std::time::Duration::from_secs(120),
    }
}

#[test]
fn a_retry_schedule_and_its_budget_reach_the_runtime_together() {
    // They are one statement about one provider connection, set by two knobs
    // because mentra keeps the count and the waits apart. A runtime that
    // carried one and dropped the other would be half of what the host said.
    let runtime = offline()
        .with_provider_retry(patient())
        .with_provider_retry_budget(9)
        .build()
        .expect("builds offline");

    assert_eq!(runtime.provider_retry(), (patient(), 9));
}

#[test]
fn an_untouched_builder_retries_exactly_as_mentra_would() {
    // The load-bearing default. basis applies the schedule unconditionally, so
    // if this ever drifted from mentra's own, every run basis mints would
    // silently retry on a schedule nobody chose.
    let runtime = offline().build().expect("builds offline");
    let mentra_default = mentra::runtime::RunOptions::default();

    assert_eq!(
        runtime.provider_retry(),
        (mentra_default.provider_retry, mentra_default.retry_budget),
        "an unset builder must reproduce mentra's own schedule and count"
    );
}

#[test]
fn the_last_word_about_retrying_is_the_one_that_counts() {
    // The rule every single-valued knob on this builder follows, restated for
    // these two because a helper handing out patient builders has to be
    // overridable by a caller that wants to fail fast.
    let builder = RuntimeBuilder::default()
        .with_provider_retry(patient())
        .with_provider_retry_budget(9)
        .with_provider_retry(ProviderRetry::default())
        .with_provider_retry_budget(2);

    assert_eq!(builder.provider_retry, ProviderRetry::default());
    assert_eq!(builder.provider_retry_budget, 2);
}

#[test]
fn the_retry_schedule_is_named_in_the_debug_view() {
    // The Debug impl is hand-written to keep a credential out of a log, so
    // each new field has to be added to it deliberately.
    let printed = format!(
        "{:?}",
        RuntimeBuilder::default().with_provider_retry(patient())
    );

    assert!(printed.contains("provider_retry"), "{printed}");
    assert!(printed.contains("provider_retry_budget"), "{printed}");
    assert!(
        printed.contains("30s"),
        "the chosen ceiling should print: {printed}"
    );
}

/// A workspace on `runtime` over an empty directory, looking nowhere except
/// where this test put something — the same pinning `tests/workspace.rs`
/// explains, so no developer's real global config can reach these assertions.
async fn workspace_on(runtime: std::sync::Arc<Runtime>, root: &Path) -> crate::Workspace {
    crate::Workspace::builder(root)
        .with_runtime(runtime)
        // An id rather than `NewestAvailable`, which would ask the provider
        // for a model list and so reach the closed port `offline()` names.
        .with_model(ModelSelector::Id("test-model".to_string()))
        .with_context(crate::ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: None,
            walk_parents: false,
        })
        .with_skills(crate::SkillsConfig {
            workspace_subdir: Some(PathBuf::from(".basis/skills")),
            shared_workspace_dir: true,
            global_dir: None,
            shared_home_dir: false,
        })
        .with_templates(crate::TemplatesConfig {
            workspace_subdir: PathBuf::from(".basis/templates"),
            global_dir: None,
        })
        .open()
        .await
        .expect("a pinned workspace opens offline")
}

#[tokio::test]
async fn a_prepared_runs_options_carry_the_runtimes_retry_schedule() {
    // The end of the wiring, and the only assertion that proves the knob does
    // anything: a runtime-scoped value has to survive the mint and land on the
    // `RunOptions` a turn is actually driven on.
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = std::sync::Arc::new(
        offline()
            .with_provider_retry(patient())
            .with_provider_retry_budget(9)
            .build()
            .expect("builds offline"),
    );

    let workspace = workspace_on(runtime, dir.path()).await;
    let run = workspace.prepare("go").expect("mints");
    let options = run.run_options(crate::TurnOptions::default());

    assert_eq!(options.provider_retry, patient());
    assert_eq!(options.retry_budget, 9);
}
#[tokio::test]
async fn a_delegated_runs_options_carry_it_too() {
    // `spawn`'s delegation drives its subagent on `ToolContext::child_run_options`
    // — mentra's `RunOptions::child` — so a delegated run is exactly as patient
    // as the run that delegated it. That is not basis's code to get right, it
    // is basis's dependency: a `child` that reset these to the default would
    // make a subagent give up after twelve and a half seconds against the same
    // rate limit its parent was told to wait a minute for, and nothing in basis
    // would say so. Pinned here rather than assumed.
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = std::sync::Arc::new(
        offline()
            .with_provider_retry(patient())
            .with_provider_retry_budget(9)
            .build()
            .expect("builds offline"),
    );

    let workspace = workspace_on(runtime, dir.path()).await;
    let run = workspace.prepare("go").expect("mints");
    let child = run.run_options(crate::TurnOptions::default()).child();

    assert_eq!(child.provider_retry, patient());
    assert_eq!(child.retry_budget, 9);
}

#[tokio::test]
async fn a_run_on_an_untouched_runtime_is_left_exactly_as_it_was() {
    // The other half of the default claim, asserted where it matters: not just
    // that the runtime holds mentra's numbers, but that a run minted from one
    // is indistinguishable from a run whose options basis never touched.
    let dir = tempfile::tempdir().expect("tempdir");
    let runtime = std::sync::Arc::new(offline().build().expect("builds offline"));

    let workspace = workspace_on(runtime, dir.path()).await;
    let run = workspace.prepare("go").expect("mints");
    let options = run.run_options(crate::TurnOptions::default());
    let untouched = mentra::runtime::RunOptions::default();

    assert_eq!(options.provider_retry, untouched.provider_retry);
    assert_eq!(options.retry_budget, untouched.retry_budget);
}

#[test]
fn a_chosen_transport_reaches_mentras_own_runtime() {
    // The far side of the seam, which is the half that matters: not that basis
    // held the choice, but that mentra's runtime came out of `build` holding
    // it. mentra reads it back through `Runtime::responses_transport`, the
    // same kind of window `tools()` is for a host tool.
    let chosen = offline()
        .with_responses_transport(ResponsesTransport::WebSocket)
        .build()
        .expect("builds offline");

    assert_eq!(
        chosen.mentra_runtime().responses_transport(),
        Some(ResponsesTransport::WebSocket)
    );
    assert_eq!(
        offline()
            .build()
            .expect("builds offline")
            .mentra_runtime()
            .responses_transport(),
        None,
        "unset must leave the choice to mentra rather than restating its default"
    );
}

#[test]
fn the_last_word_about_the_transport_is_the_one_that_counts() {
    // The rule every single-valued knob here follows: a helper that hands out
    // websocket builders has to be overridable by a caller that wants HTTP.
    let builder = RuntimeBuilder::default()
        .with_responses_transport(ResponsesTransport::WebSocket)
        .with_responses_transport(ResponsesTransport::HttpSse);

    assert_eq!(
        builder.responses_transport,
        Some(ResponsesTransport::HttpSse)
    );
    assert_eq!(
        RuntimeBuilder::default().responses_transport,
        None,
        "a fresh builder states no transport at all"
    );
}

#[test]
fn a_chosen_transport_is_named_in_the_debug_view() {
    // The Debug impl is hand-written to keep a credential out of a log, so
    // each new field has to be added to it deliberately.
    let printed = format!(
        "{:?}",
        RuntimeBuilder::default().with_responses_transport(ResponsesTransport::WebSocket)
    );

    assert!(printed.contains("responses_transport"), "{printed}");
    assert!(printed.contains("WebSocket"), "{printed}");
}

/// The names of every tool registered on a runtime, sorted by mentra.
fn registered(runtime: &Runtime) -> Vec<String> {
    runtime
        .mentra_runtime()
        .tools()
        .into_iter()
        .map(|tool| tool.provider.name)
        .collect()
}

#[test]
fn the_model_is_offered_the_split_file_tools_by_default() {
    // basis's opinion, not mentra's default. The six names are what models in
    // this class are trained on; the `files` they replace was one tool with a
    // nine-variant `oneOf` for its `operations` array, and no `glob` at all.
    let names = registered(&offline().build().expect("builds offline"));

    for split in ["read", "ls", "grep", "glob", "write", "edit"] {
        assert!(
            names.iter().any(|name| name == split),
            "{split} must be registered: {names:?}"
        );
    }
    assert!(
        !names.iter().any(|name| name == "files"),
        "the batched tool must be gone rather than sitting beside its own replacement: {names:?}"
    );
}

#[test]
fn a_host_whose_rules_name_files_can_keep_the_batched_tool() {
    // The migration path the default costs: `hooks.json` matchers and
    // remembered rules key on the exact tool name, so a host that has written
    // some against `files` needs the old roster until it has rewritten them.
    let names = registered(
        &offline()
            .with_file_tools(FileToolProfile::Batched)
            .build()
            .expect("builds offline"),
    );

    assert!(names.iter().any(|name| name == "files"), "{names:?}");
    for split in ["read", "ls", "grep", "glob", "write", "edit"] {
        assert!(
            !names.iter().any(|name| name == split),
            "{split} must not survive a host's choice of Batched: {names:?}"
        );
    }
}

#[test]
fn the_last_word_about_the_file_tools_is_the_one_that_counts() {
    // The rule every single-valued knob here follows, and the reason this
    // field is a plain value: there is no *unsaid*, because basis's default
    // is not mentra's, so the field always states an answer.
    let builder = RuntimeBuilder::default()
        .with_file_tools(FileToolProfile::Batched)
        .with_file_tools(FileToolProfile::Both);

    assert_eq!(builder.file_tools, FileToolProfile::Both);
    assert_eq!(
        RuntimeBuilder::default().file_tools,
        FileToolProfile::Split,
        "a fresh builder offers the split tools"
    );
}

#[test]
fn a_chosen_file_tool_profile_is_named_in_the_debug_view() {
    let printed = format!(
        "{:?}",
        RuntimeBuilder::default().with_file_tools(FileToolProfile::Batched)
    );

    assert!(printed.contains("file_tools"), "{printed}");
    assert!(printed.contains("Batched"), "{printed}");
}

/// A config file discovered in a fresh directory, for the layering assertions
/// below. The temp directory comes back because it must outlive the read.
fn config_saying(body: &str) -> (tempfile::TempDir, crate::Config) {
    let dir = tempfile::tempdir().expect("tempdir");
    let global = dir.path().join("global");
    std::fs::create_dir_all(&global).expect("create global");
    std::fs::write(global.join("config.json"), body).expect("write global config");

    let config = crate::Config::discover(dir.path(), Some(&global)).expect("a well-formed file");

    (dir, config)
}

/// A file with an answer to every key this builder can take from one.
const EVERY_KEY: &str = r#"{
    "schema": 1,
    "provider": "openai",
    "base_url": "http://from-the-file/v1",
    "model": "from-the-file"
}"#;

#[test]
fn a_config_answers_only_what_the_builder_was_not_told() {
    let (_dir, config) = config_saying(EVERY_KEY);

    let filled = RuntimeBuilder::default().with_config(&config);

    assert_eq!(filled.provider, Some(BuiltinProvider::OpenAI));
    assert_eq!(filled.base_url.as_deref(), Some("http://from-the-file/"));
    assert_eq!(
        filled.model,
        Some(ModelSelector::Id("from-the-file".to_string()))
    );
}

#[test]
fn a_builder_that_was_told_keeps_what_it_was_told() {
    // The precedence rule this knob exists to obey: a file layers *under* the
    // host's own calls, never over them. Order must not matter either — what
    // `with_config` reads is emptiness, not who spoke last.
    let (_dir, config) = config_saying(EVERY_KEY);

    for told in [
        RuntimeBuilder::default()
            .with_provider(BuiltinProvider::Anthropic)
            .with_base_url("http://from-the-host/v1")
            .with_model(ModelSelector::Id("from-the-host".to_string()))
            .with_config(&config),
        RuntimeBuilder::default()
            .with_config(&config)
            .with_provider(BuiltinProvider::Anthropic)
            .with_base_url("http://from-the-host/v1")
            .with_model(ModelSelector::Id("from-the-host".to_string())),
    ] {
        assert_eq!(told.provider, Some(BuiltinProvider::Anthropic));
        assert_eq!(told.base_url.as_deref(), Some("http://from-the-host/v1"));
        assert_eq!(
            told.model,
            Some(ModelSelector::Id("from-the-host".to_string()))
        );
    }
}

#[test]
fn a_config_that_says_nothing_changes_nothing() {
    let untouched = RuntimeBuilder::default().with_config(&crate::Config::default());

    assert_eq!(untouched.provider, None);
    assert_eq!(untouched.base_url, None);
    assert_eq!(
        untouched.model, None,
        "unsaid, which `build` resolves to the newest available"
    );
}

#[test]
fn a_config_beats_the_environment_by_being_asked_for() {
    // The layer below a file is the environment, and this is the whole
    // mechanism: a provider a file named arrives at `provider::resolve_with`
    // as a *requested* one, and a requested provider reads its own variable
    // instead of auto-detecting whichever key happens to be exported — which
    // `provider`'s own tests pin, against an injected environment. Nothing
    // here reads the real one.
    let (_dir, config) = config_saying(r#"{"schema": 1, "provider": "gemini"}"#);

    let asked = RuntimeBuilder::default().with_config(&config);

    assert_eq!(
        asked.provider,
        Some(BuiltinProvider::Gemini),
        "auto-detection is skipped exactly when a provider is requested"
    );
}

/// The memory half of the private policy: the roots sit outside the
/// workspace, and the write is exercised through a real session rather than
/// asserted against the policy struct, because mentra's authorization is
/// `pub(crate)` and what basis promises is the tool call landing.
#[tokio::test]
async fn a_write_tool_reaches_the_memory_root_the_policy_names() {
    use mentra::{
        ContentBlock,
        agent::{AgentConfig, WorkspaceConfig},
        test::{MockRuntime, MockToolCall},
    };

    let workspace = tempfile::tempdir().expect("tempdir");
    let memory_root = tempfile::tempdir().expect("tempdir");
    let target = memory_root.path().join("deploy-notes.md");

    let mock = MockRuntime::builder()
        .model("mock-model", "openai")
        .with_policy(workspace_policy(
            workspace.path(),
            ShellAccess::Granted,
            &[memory_root.path().to_path_buf()],
        ))
        .tool_calls(vec![MockToolCall::new(
            "files",
            serde_json::json!({
                "operations": [{
                    "op": "create",
                    "path": target,
                    "content": "---\nname: deploy-notes\ndescription: d\ntype: project\n---\nbody\n",
                }],
            }),
        )])
        .text("wrote it")
        .build()
        .expect("the mock runtime builds");

    let mut session = mock
        .runtime()
        .create_session_with_config(
            "memory-write",
            mock.model(),
            AgentConfig {
                workspace: WorkspaceConfig {
                    base_dir: workspace.path().to_path_buf(),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("session");

    session
        .append_turn(vec![ContentBlock::Text {
            text: "keep a note".to_string(),
        }])
        .await
        .expect("the scripted turn runs");

    assert!(
        target.exists(),
        "a memory root the policy names must be writable through the file tools"
    );
}

/// The control: the same write with no memory roots stays refused, so the
/// grant above is the roots' doing and not a loosened workspace bound.
#[tokio::test]
async fn without_the_roots_the_same_write_is_refused() {
    use mentra::{
        ContentBlock,
        agent::{AgentConfig, WorkspaceConfig},
        test::{MockRuntime, MockToolCall},
    };

    let workspace = tempfile::tempdir().expect("tempdir");
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let target = elsewhere.path().join("deploy-notes.md");

    let mock = MockRuntime::builder()
        .model("mock-model", "openai")
        .with_policy(workspace_policy(
            workspace.path(),
            ShellAccess::Granted,
            &[],
        ))
        .tool_calls(vec![MockToolCall::new(
            "files",
            serde_json::json!({
                "operations": [{
                    "op": "create",
                    "path": target,
                    "content": "outside every root",
                }],
            }),
        )])
        .text("tried")
        .build()
        .expect("the mock runtime builds");

    let mut session = mock
        .runtime()
        .create_session_with_config(
            "memory-write-refused",
            mock.model(),
            AgentConfig {
                workspace: WorkspaceConfig {
                    base_dir: workspace.path().to_path_buf(),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("session");

    session
        .append_turn(vec![ContentBlock::Text {
            text: "keep a note".to_string(),
        }])
        .await
        .expect("the scripted turn still completes");

    assert!(
        !target.exists(),
        "a path outside the workspace and every root must stay refused"
    );
}
