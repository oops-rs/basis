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
async fn compatible_provider_skips_automatic_previous_response_id_chaining() {
    let (base_url, handle) = spawn_two_response_server();
    let provider = compatible_provider(&base_url, "test-key");

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

/// An executor that reaches nothing, standing in for whatever a host actually
/// writes: what is under test is the routing table, not the transport.
struct Nowhere;

#[async_trait::async_trait]
impl mentra::runtime::RuntimeExecutor for Nowhere {
    async fn run(
        &self,
        _request: mentra::runtime::CommandRequest,
    ) -> Result<mentra::runtime::CommandOutput, String> {
        Err("this executor reaches nothing".to_string())
    }
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
