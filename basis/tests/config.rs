//! What `.basis/config.json` decides once something applies it.
//!
//! `basis/src/config/tests.rs` pins what the files *say*. This pins what
//! happens next: which model a workspace resolves, which effort a turn asks
//! the provider for, and — the point of the whole precedence chain — that a
//! caller who named either still gets what it named.
//!
//! Every builder here looks nowhere except where the test put something, the
//! pinning `tests/workspace.rs` explains: a developer's own
//! `~/.config/basis/config.json` must not be able to move an assertion.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

use basis::{
    CollectingSink, Config, ContextConfig, Effort, RunConfig, Runtime, Workspace, WorkspaceBuilder,
    hooks::HooksConfig, skills::SkillsConfig, templates::TemplatesConfig,
    tools::declared::ToolsConfig,
};
use mentra::ModelSelector;

/// A builder that discovers nothing it was not shown — except the config file,
/// which is what these tests are about.
fn pinned(workspace: &Path) -> WorkspaceBuilder {
    Workspace::builder(workspace)
        .with_context(ContextConfig {
            file_name: "AGENTS.md".to_string(),
            // Also the directory config discovery reads its global file from,
            // so this one line keeps a real one out of every assertion below.
            global_dir: None,
            walk_parents: false,
        })
        .with_skills(SkillsConfig {
            workspace_subdir: PathBuf::from(".basis/skills"),
            global_dir: None,
        })
        .with_templates(TemplatesConfig {
            workspace_subdir: PathBuf::from(".basis/templates"),
            global_dir: None,
        })
        .with_hooks(HooksConfig {
            workspace_file: PathBuf::from(".basis/hooks.json"),
            global_dir: None,
        })
        .with_tools(ToolsConfig {
            workspace_file: PathBuf::from(".basis/tools.json"),
            global_dir: None,
        })
}

/// A runtime that resolves its provider locally and reaches a closed port if
/// anything tries to use it. Every model below is named by id, which resolves
/// without asking the provider for a list.
fn offline() -> Arc<Runtime> {
    Arc::new(
        Runtime::builder()
            .with_base_url("http://127.0.0.1:1/v1")
            .with_api_key("test-key")
            .with_ephemeral_history()
            .build()
            .expect("builds offline"),
    )
}

fn write_config(workspace: &Path, body: &str) {
    let path = workspace.join(".basis").join("config.json");
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("create .basis");
    std::fs::write(path, body).expect("write config");
}

#[tokio::test]
async fn a_workspace_file_decides_the_model_when_nothing_else_did() {
    // The whole point of the file: without it, "no `--model`" means whatever
    // the provider lists newest today, which is not a thing a repository chose.
    let dir = tempfile::tempdir().expect("tempdir");
    write_config(dir.path(), r#"{"schema": 1, "model": "from-the-file"}"#);

    let workspace = pinned(dir.path())
        .with_runtime(offline())
        .open()
        .await
        .expect("opens offline");

    assert_eq!(workspace.model(), "from-the-file");
    assert_eq!(workspace.config_files().len(), 1);
    assert_eq!(workspace.config_files()[0].scope, "workspace");
    assert_eq!(
        workspace
            .config()
            .model
            .as_ref()
            .map(|model| model.value.as_str()),
        Some("from-the-file"),
        "the workspace keeps the answer and the file that gave it"
    );
}

#[tokio::test]
async fn an_explicit_model_outranks_the_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_config(dir.path(), r#"{"schema": 1, "model": "from-the-file"}"#);

    let workspace = pinned(dir.path())
        .with_runtime(offline())
        .with_model(ModelSelector::Id("from-the-caller".to_string()))
        .open()
        .await
        .expect("opens offline");

    assert_eq!(workspace.model(), "from-the-caller");
}

#[tokio::test]
async fn a_named_model_on_a_run_config_outranks_the_file() {
    // The CLI's path: `--model` lands on a `RunConfig`, and `split` is the one
    // mapping from that to a workspace builder.
    let dir = tempfile::tempdir().expect("tempdir");
    write_config(dir.path(), r#"{"schema": 1, "model": "from-the-file"}"#);

    let config =
        RunConfig::new(dir.path(), "hi").with_model(ModelSelector::Id("from-the-flag".to_string()));
    let (builder, _) = config.split();

    let workspace = pin(builder)
        .with_runtime(offline())
        .open()
        .await
        .expect("opens offline");

    assert_eq!(workspace.model(), "from-the-flag");
}

#[tokio::test]
async fn a_run_config_that_named_no_model_lets_the_file_decide() {
    // `RunConfig::new` seeds `NewestAvailable` because the field is not an
    // `Option` — so a caller that said nothing must not thereby outrank a file
    // that said something. This is the assertion that keeps that true.
    let dir = tempfile::tempdir().expect("tempdir");
    write_config(dir.path(), r#"{"schema": 1, "model": "from-the-file"}"#);

    let (builder, _) = RunConfig::new(dir.path(), "hi").split();

    let workspace = pin(builder)
        .with_runtime(offline())
        .open()
        .await
        .expect("opens offline");

    assert_eq!(workspace.model(), "from-the-file");
}

#[tokio::test]
async fn an_empty_config_is_the_off_switch() {
    // A host whose own configuration is the only configuration hands in a
    // `Config` that says nothing, and the file on disk stops being read.
    let dir = tempfile::tempdir().expect("tempdir");
    write_config(dir.path(), r#"{"schema": 1, "model": "from-the-file"}"#);

    let runtime = Arc::new(
        Runtime::builder()
            .with_base_url("http://127.0.0.1:1/v1")
            .with_api_key("test-key")
            .with_ephemeral_history()
            .with_model(ModelSelector::Id("the-runtime-policy".to_string()))
            .build()
            .expect("builds offline"),
    );

    let workspace = pinned(dir.path())
        .with_runtime(runtime)
        .with_config(Config::default())
        .open()
        .await
        .expect("opens offline");

    assert_eq!(workspace.model(), "the-runtime-policy");
    assert!(
        workspace.config_files().is_empty(),
        "nothing was read, so nothing may be reported"
    );
}

#[tokio::test]
async fn a_base_url_in_a_committed_file_fails_the_open_by_name() {
    // The refusal, at the surface that matters: not a warning, not an ignored
    // key — the workspace does not open, and the message names the file.
    let dir = tempfile::tempdir().expect("tempdir");
    write_config(
        dir.path(),
        r#"{"schema": 1, "base_url": "http://127.0.0.1:1/v1"}"#,
    );

    let error = pinned(dir.path())
        .with_runtime(offline())
        .open()
        .await
        .expect_err("refused");

    let rendered = error.to_string();
    assert!(rendered.contains("config.json"), "{rendered}");
    assert!(rendered.contains("base_url"), "{rendered}");
}

#[tokio::test]
async fn a_malformed_file_fails_the_open_rather_than_running_another_model() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_config(dir.path(), "{not json");

    let error = pinned(dir.path())
        .with_runtime(offline())
        .open()
        .await
        .expect_err("refused");

    assert!(error.to_string().contains("config.json"), "{error}");
}

#[tokio::test]
async fn the_workspace_files_effort_reaches_the_provider() {
    // The far side of the wiring: an `effort` in the file has to arrive as the
    // reasoning options on the request the model actually receives, because
    // nothing between here and there reports it.
    let endpoint = ScriptedEndpoint::start();
    let dir = tempfile::tempdir().expect("tempdir");
    write_config(
        dir.path(),
        r#"{"schema": 1, "model": "test-model", "effort": "high"}"#,
    );

    let workspace = pinned(dir.path())
        .with_runtime(endpoint.runtime())
        .open()
        .await
        .expect("opens against the scripted endpoint");

    let mut run = workspace.prepare("go").expect("mints");
    run.execute(CollectingSink::default())
        .await
        .expect("the scripted turn runs");

    let request = endpoint.first_request();
    assert!(
        request.contains(r#""effort":"high""#),
        "the file's effort never reached the request: {request}"
    );
}

#[tokio::test]
async fn a_run_that_asked_for_an_effort_keeps_its_own() {
    // A flag describes this invocation and the file describes the repository,
    // so the more specific one holds — the ordering every key in the file has.
    let endpoint = ScriptedEndpoint::start();
    let dir = tempfile::tempdir().expect("tempdir");
    write_config(
        dir.path(),
        r#"{"schema": 1, "model": "test-model", "effort": "high"}"#,
    );

    let workspace = pinned(dir.path())
        .with_runtime(endpoint.runtime())
        .open()
        .await
        .expect("opens against the scripted endpoint");

    let mut run = workspace
        .prepare(basis::workspace::RunSpec::new("go").with_effort(Effort::Low))
        .expect("mints");
    run.execute(CollectingSink::default())
        .await
        .expect("the scripted turn runs");

    let request = endpoint.first_request();
    assert!(
        request.contains(r#""effort":"low""#),
        "the run's own answer must win: {request}"
    );
}

#[tokio::test]
async fn a_run_reports_the_effort_it_was_opened_at() {
    // The reader a picker needs, and the case that makes it worth having: the
    // effort was applied at mint, from the repository's own file, and nothing
    // on this run ever asked for one. A `PreparedRun` that answered from what
    // it had been *told* would report "no effort requested" for a session that
    // is demonstrably at `high` — and an ACP client would draw its picker on
    // the wrong value.
    let dir = tempfile::tempdir().expect("tempdir");
    write_config(
        dir.path(),
        r#"{"schema": 1, "model": "test-model", "effort": "high"}"#,
    );

    let workspace = pinned(dir.path())
        .with_runtime(offline())
        .open()
        .await
        .expect("opens");

    let mut run = workspace.prepare("go").expect("mints");
    assert_eq!(run.effort(), Some(Effort::High));

    run.set_effort(Some(Effort::Low)).expect("sets");
    assert_eq!(run.effort(), Some(Effort::Low), "and it follows a change");

    run.set_effort(None).expect("clears");
    assert_eq!(
        run.effort(),
        None,
        "cleared means the provider's own default, which basis has no name for"
    );
}

#[tokio::test]
async fn a_run_nobody_asked_an_effort_of_reports_none() {
    // The other half: `None` has to mean "no level is being requested", not
    // "nobody has called `set_effort` yet".
    let dir = tempfile::tempdir().expect("tempdir");
    write_config(dir.path(), r#"{"schema": 1, "model": "test-model"}"#);

    let workspace = pinned(dir.path())
        .with_runtime(offline())
        .open()
        .await
        .expect("opens");

    assert_eq!(workspace.prepare("go").expect("mints").effort(), None);
}

/// The smallest endpoint that is a finished turn, with the request kept.
struct ScriptedEndpoint {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl ScriptedEndpoint {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
        let address = listener.local_addr().expect("read endpoint address");
        let requests = Arc::new(Mutex::new(Vec::new()));

        let recorded = Arc::clone(&requests);
        thread::spawn(move || {
            let mut index = 0_usize;
            while let Ok((stream, _)) = listener.accept() {
                index += 1;
                let recorded = Arc::clone(&recorded);
                thread::spawn(move || answer(stream, index, &recorded));
            }
        });

        Self {
            base_url: format!("http://{address}/"),
            requests,
        }
    }

    fn runtime(&self) -> Arc<Runtime> {
        Arc::new(
            Runtime::builder()
                .with_base_url(&self.base_url)
                .with_api_key("test-key")
                .with_ephemeral_history()
                .build()
                .expect("builds against the scripted endpoint"),
        )
    }

    fn first_request(&self) -> String {
        self.requests
            .lock()
            .expect("requests")
            .first()
            .cloned()
            .expect("the model was asked something")
    }
}

fn answer(mut stream: TcpStream, index: usize, recorded: &Mutex<Vec<String>>) {
    let request = read_http_request(&mut stream);
    recorded.lock().expect("requests").push(request);

    let body = format!(
        concat!(
            "data: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp_{0}\",\"model\":\"test-model\",\"status\":\"in_progress\"}}}}\n\n",
            "data: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"type\":\"message\",\"content\":[{{\"type\":\"output_text\",\"text\":\"done\"}}]}}}}\n\n",
            "data: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_{0}\",\"model\":\"test-model\",\"status\":\"completed\"}}}}\n\n"
        ),
        index
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

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

/// `pinned`'s discovery choices, applied to a builder that came from
/// [`RunConfig::split`] rather than being built here.
fn pin(builder: WorkspaceBuilder) -> WorkspaceBuilder {
    builder
        .with_context(ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: None,
            walk_parents: false,
        })
        .with_skills(SkillsConfig {
            workspace_subdir: PathBuf::from(".basis/skills"),
            global_dir: None,
        })
        .with_templates(TemplatesConfig {
            workspace_subdir: PathBuf::from(".basis/templates"),
            global_dir: None,
        })
        .with_hooks(HooksConfig {
            workspace_file: PathBuf::from(".basis/hooks.json"),
            global_dir: None,
        })
        .with_tools(ToolsConfig {
            workspace_file: PathBuf::from(".basis/tools.json"),
            global_dir: None,
        })
}
