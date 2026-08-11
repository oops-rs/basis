//! What opening a workspace settles, and what it must not.
//!
//! Split out of `builder.rs` only for its size — the file was past the
//! 800-line ceiling with these inline.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use crate::context::{ContextDocument, ContextScope};

use super::*;

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
fn context_becomes_the_system_prompt_and_the_workspace_is_scoped() {
    let context = WorkspaceContext::from_documents(vec![ContextDocument {
        path: PathBuf::from("/repo/AGENTS.md"),
        scope: ContextScope::Workspace,
        content: "house rules".to_string(),
    }]);

    let agent = agent_config(Path::new("/repo"), &context);

    assert!(
        agent
            .system
            .expect("a system prompt")
            .contains("house rules")
    );
    assert_eq!(agent.workspace.base_dir, PathBuf::from("/repo"));
}

#[test]
fn an_empty_workspace_context_leaves_the_system_prompt_unset() {
    let agent = agent_config(Path::new("/repo"), &WorkspaceContext::default());

    assert_eq!(agent.system, None);
}

#[test]
fn the_two_doors_spawn_replaces_leave_the_roster() {
    // ADR-0016. Left alongside `spawn` they would restore what it removed:
    // three names arriving at one approval gate, and three rule namespaces,
    // for a question an operator asks once.
    let agent = agent_config(Path::new("/repo"), &WorkspaceContext::default());

    for replaced in ["shell", "background_run", "task"] {
        assert!(
            !agent.tool_profile.allows(replaced),
            "{replaced} is still offered to the model"
        );
    }
    assert!(
        agent.tool_profile.allows(crate::tools::SPAWN),
        "the door that replaces them has to be open"
    );
}

#[test]
fn hiding_is_a_roster_fact_and_not_a_capability_one() {
    // What lets `spawn` still reach the command executor underneath: nothing
    // here is an allow-list, so the tools stay registered on the runtime and
    // only stop being *offered*. A profile that named an allowed set instead
    // would take the capability away with the listing.
    let agent = agent_config(Path::new("/repo"), &WorkspaceContext::default());

    assert_eq!(
        agent.tool_profile.allowed_tools, None,
        "an allow-list here would silently drop every tool nobody thought to name"
    );
    assert!(agent.tool_profile.allows("files"));
}

#[test]
fn commands_are_available_unless_the_caller_says_otherwise() {
    // ADR-0013: the first `lan "run the tests"` has to work.
    assert!(WorkspaceBuilder::new("/repo").shell.is_granted());
}

#[test]
fn builders_return_new_values() {
    let base = WorkspaceBuilder::new("/repo");
    let derived = base.with_provider(BuiltinProvider::Anthropic);

    assert_eq!(derived.provider, Some(BuiltinProvider::Anthropic));
    assert_eq!(
        WorkspaceBuilder::new("/repo").provider,
        None,
        "a fresh builder detects the provider"
    );
}

#[test]
fn history_goes_where_mentra_puts_it_unless_the_caller_says_otherwise() {
    // The default must stay the default: a host with conversations already
    // in mentra's database would lose sight of them if opening a workspace
    // started relocating the store on its own.
    assert_eq!(WorkspaceBuilder::new("/repo").history, None);
    assert_eq!(
        WorkspaceBuilder::new("/repo")
            .with_store_dir("/elsewhere")
            .history,
        Some(History::Directory(PathBuf::from("/elsewhere")))
    );
    assert_eq!(
        WorkspaceBuilder::new("/repo")
            .with_ephemeral_history()
            .history,
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
        WorkspaceBuilder::new("/repo")
            .with_store_dir("/elsewhere")
            .with_ephemeral_history()
            .history,
        Some(History::Ephemeral)
    );
    assert_eq!(
        WorkspaceBuilder::new("/repo")
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
        WorkspaceBuilder::new("/repo").with_api_key("sk-secret-value")
    );

    assert!(!printed.contains("sk-secret-value"));
    assert!(printed.contains("redacted"));
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
    let builder = WorkspaceBuilder::new("/repo")
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
        WorkspaceBuilder::new("/repo").interceptors.is_empty(),
        "a fresh builder intercepts nothing"
    );
}

#[test]
fn a_registered_interceptor_is_named_in_the_debug_view() {
    // The Debug impl is hand-written to keep a credential out of a log, so
    // each new field has to be added to it deliberately.
    let printed = format!(
        "{:?}",
        WorkspaceBuilder::new("/repo").with_interceptor(Named("redact"))
    );

    assert!(printed.contains("redact"), "{printed}");
}
