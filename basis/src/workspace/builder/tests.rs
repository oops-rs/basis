//! What opening a workspace settles, and what it must not.
//!
//! Split out of `builder.rs` only for its size. The tests for the knobs
//! ADR-0018 moved to the runtime — history, credential redaction, interceptor
//! ordering, the compatible provider — moved with them, to
//! `runtime/builder/tests.rs`.

use mentra::{
    ContentBlock,
    test::{MockRuntime, MockToolCall},
};

use crate::context::{ContextDocument, ContextScope};
use crate::memory::{Memory, MemoryKind};

use super::*;

/// The agent config a workspace that said nothing produces, with the snapshot
/// directory pinned so no test can name a real one.
fn defaults(workspace: &Path, context: &WorkspaceContext) -> mentra::agent::AgentConfig {
    configured(workspace, context, None)
}

/// `defaults`, with the host's say over the prompt.
fn configured(
    workspace: &Path,
    context: &WorkspaceContext,
    system_prompt: Option<&SystemPrompt>,
) -> mentra::agent::AgentConfig {
    remembering(workspace, context, system_prompt, &[])
}

/// `configured`, with the index block `memories` render.
fn remembering(
    workspace: &Path,
    context: &WorkspaceContext,
    system_prompt: Option<&SystemPrompt>,
    memories: &[Memory],
) -> mentra::agent::AgentConfig {
    with_roster(
        workspace,
        context,
        system_prompt,
        memories,
        ToolRoster::default(),
    )
}

/// `remembering`, with the roster stated explicitly instead of defaulted —
/// what item (d) of decision D3 needs to pin: the rendered prompt does not
/// consult the roster at all.
fn with_roster(
    workspace: &Path,
    context: &WorkspaceContext,
    system_prompt: Option<&SystemPrompt>,
    memories: &[Memory],
    roster: ToolRoster,
) -> mentra::agent::AgentConfig {
    agent_config(
        workspace,
        context,
        system_prompt,
        crate::memory::index_block(memories).as_deref(),
        roster,
        Compaction::default(),
        PathBuf::from("/transcripts"),
    )
}

/// One memory, for the prompt tests below.
fn deploy_memories() -> Vec<Memory> {
    vec![Memory {
        name: "deploy-notes".to_string(),
        description: "how deploys go out".to_string(),
        kind: MemoryKind::Project,
        path: PathBuf::from("/mem/deploy-notes.md"),
        scope: ContextScope::Global,
    }]
}

/// One workspace document, for the prompt tests below.
fn house_rules() -> WorkspaceContext {
    WorkspaceContext::from_documents(vec![ContextDocument {
        path: PathBuf::from("/repo/AGENTS.md"),
        scope: ContextScope::Workspace,
        content: "house rules".to_string(),
    }])
}

#[test]
fn context_becomes_the_system_prompt_and_the_workspace_is_scoped() {
    let agent = defaults(Path::new("/repo"), &house_rules());

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
    let agent = defaults(Path::new("/repo"), &WorkspaceContext::default());

    assert_eq!(agent.system, None);
}

#[test]
fn a_fresh_builder_says_nothing_about_the_system_prompt() {
    // The seam is not a default: basis ships no system prompt of its own, and
    // an unset knob has to leave the discovery-only prompt byte-identical.
    assert_eq!(WorkspaceBuilder::new("/repo").system_prompt, None);
    assert_eq!(
        configured(Path::new("/repo"), &house_rules(), None).system,
        house_rules().render()
    );
}

#[test]
fn an_appended_prompt_comes_after_the_workspace_because_it_is_more_specific() {
    // The rendered block tells the model later blocks take precedence, and the
    // host's text is the statement of the program running this agent — which no
    // repository can know about, and none should be able to overrule.
    let agent = configured(
        Path::new("/repo"),
        &house_rules(),
        Some(&SystemPrompt::Append("answer in Chinese".to_string())),
    );

    let system = agent.system.expect("a system prompt");
    let context = system.find("house rules").expect("the workspace's say");
    let host = system.find("answer in Chinese").expect("the host's say");
    assert!(context < host, "{system}");
}

#[test]
fn an_appended_prompt_stands_alone_when_the_workspace_says_nothing() {
    let agent = configured(
        Path::new("/repo"),
        &WorkspaceContext::default(),
        Some(&SystemPrompt::Append("answer in Chinese".to_string())),
    );

    assert_eq!(agent.system.as_deref(), Some("answer in Chinese"));
}

#[test]
fn a_replaced_prompt_drops_the_context_block_entirely() {
    let agent = configured(
        Path::new("/repo"),
        &house_rules(),
        Some(&SystemPrompt::Replace(
            "you are Acme's reviewer".to_string(),
        )),
    );

    assert_eq!(agent.system.as_deref(), Some("you are Acme's reviewer"));
}

#[test]
fn appending_nothing_leaves_the_workspaces_prompt_as_it_was() {
    // One obvious meaning, and it is the one the workspace already had.
    let agent = configured(
        Path::new("/repo"),
        &house_rules(),
        Some(&SystemPrompt::Append("   \n".to_string())),
    );

    assert_eq!(agent.system, house_rules().render());
}

#[test]
fn replacing_with_nothing_is_how_a_host_asks_for_no_system_prompt() {
    // Not an error: "the prompt is nothing" is a thing a host can mean, and it
    // renders as `None` for the same reason an empty workspace does.
    let agent = configured(
        Path::new("/repo"),
        &house_rules(),
        Some(&SystemPrompt::Replace(String::new())),
    );

    assert_eq!(agent.system, None);
}

#[test]
fn the_last_system_prompt_said_is_the_one_that_holds() {
    // One field, so replace-then-append is append rather than both — and the
    // enum is what makes "both" unspellable instead of undefined.
    let builder = WorkspaceBuilder::new("/repo")
        .with_system_prompt(SystemPrompt::Replace("first".to_string()))
        .with_system_prompt(SystemPrompt::Append("second".to_string()));

    assert_eq!(
        builder.system_prompt,
        Some(SystemPrompt::Append("second".to_string()))
    );
}

#[test]
fn the_memory_index_lands_after_the_context_documents() {
    // The index is workspace data, so it goes where the rendered block's
    // preamble puts the more specific statement — after the documents.
    let agent = remembering(Path::new("/repo"), &house_rules(), None, &deploy_memories());

    let system = agent.system.expect("a system prompt");
    let context = system.find("house rules").expect("the workspace's say");
    let index = system.find("<memories>").expect("the index");
    assert!(context < index, "{system}");
    assert!(system.contains("deploy-notes — how deploys go out"));
}

#[test]
fn a_replaced_prompt_drops_the_memory_index_too() {
    // `Replace` is the whole prompt; the index rides the same render path as
    // the documents, so it goes with them rather than surviving them.
    let agent = remembering(
        Path::new("/repo"),
        &house_rules(),
        Some(&SystemPrompt::Replace(
            "you are Acme's reviewer".to_string(),
        )),
        &deploy_memories(),
    );

    assert_eq!(agent.system.as_deref(), Some("you are Acme's reviewer"));
}

#[test]
fn a_hosts_append_still_lands_after_the_memory_index() {
    // The host's text stays the most specific statement — nothing on disk,
    // memories included, may outrank the program actually running this agent.
    let agent = remembering(
        Path::new("/repo"),
        &house_rules(),
        Some(&SystemPrompt::Append("answer in Chinese".to_string())),
        &deploy_memories(),
    );

    let system = agent.system.expect("a system prompt");
    let index = system.find("<memories>").expect("the index");
    let host = system.find("answer in Chinese").expect("the host's say");
    assert!(index < host, "{system}");
}

#[test]
fn no_memories_leave_the_prompt_byte_identical() {
    // Zero memories, zero cost: a workspace that never heard of the
    // convention renders exactly the prompt it always did.
    let agent = remembering(Path::new("/repo"), &house_rules(), None, &[]);

    assert_eq!(agent.system, house_rules().render());
}

#[test]
fn memories_alone_still_make_a_prompt() {
    // An empty workspace with memories is not an empty prompt: the index is
    // workspace data even when no document is.
    let agent = remembering(
        Path::new("/repo"),
        &WorkspaceContext::default(),
        None,
        &deploy_memories(),
    );

    let system = agent.system.expect("a system prompt");
    assert!(system.starts_with("<memories>"), "{system}");
}

#[test]
fn the_memory_index_renders_whatever_the_roster_says() {
    // Item (d) of decision D3: the roster decides which tools the model may
    // call, not what the system prompt says. A roster narrowed to `spawn`
    // alone still ships the same memory index a default roster would — the
    // prompt is `WorkspaceContext` and `memory::index_block`, assembled with
    // no view of `ToolRoster` at all.
    let agent = with_roster(
        Path::new("/repo"),
        &house_rules(),
        None,
        &deploy_memories(),
        ToolRoster::only([crate::tools::SPAWN]),
    );

    let system = agent.system.expect("a system prompt");
    assert!(system.contains("<memories>"), "{system}");
    assert!(system.contains("deploy-notes — how deploys go out"));
    assert!(
        !agent.tool_profile.allows("read"),
        "the roster still narrows what the model may actually call"
    );
}

#[tokio::test]
async fn the_memory_index_reaches_the_provider_request() {
    // The block is prompt, not schema: nothing on the event stream names it,
    // so the place to assert it is the request the provider is actually sent.
    let mock = MockRuntime::builder()
        .model("mock-model", "openai")
        .text("noted")
        .build()
        .expect("the mock runtime builds");
    let config = remembering(Path::new("/repo"), &house_rules(), None, &deploy_memories());
    let mut session = mock
        .runtime()
        .create_session_with_config("memories", mock.model(), config)
        .expect("session");

    session
        .append_turn(vec![ContentBlock::Text {
            text: "hello".to_string(),
        }])
        .await
        .expect("the scripted turn runs");

    let requests = mock.recorded_requests().await;
    let system = requests
        .last()
        .and_then(|request| request.system.as_deref())
        .expect("a system prompt reached the provider");
    assert!(system.contains("<memories>"), "{system}");
    assert!(system.contains("deploy-notes — how deploys go out"));
    assert!(
        system.find("house rules") < system.find("<memories>"),
        "the index comes after the documents: {system}"
    );
}

#[test]
fn the_two_doors_spawn_replaces_leave_the_roster() {
    // ADR-0016. Left alongside `spawn` they would restore what it removed:
    // three names arriving at one approval gate, and three rule namespaces,
    // for a question an operator asks once.
    let agent = defaults(Path::new("/repo"), &WorkspaceContext::default());

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
    let agent = defaults(Path::new("/repo"), &WorkspaceContext::default());

    assert_eq!(
        agent.tool_profile.allowed_tools, None,
        "an allow-list here would silently drop every tool nobody thought to name"
    );
    assert!(agent.tool_profile.allows("read"));
}

#[tokio::test]
async fn the_default_roster_is_exactly_this() {
    // The whole visible set, from the runtime's own registry rather than from
    // a list written here, so a tool mentra adds upstream arrives as a failing
    // test instead of as a silent new door. Adding a name to this list is a
    // decision to offer it; removing one is a decision to stop.
    //
    // Sorted, because `mentra::Runtime::tools` sorts by name.
    let runtime = crate::runtime::Runtime::builder()
        .with_base_url("http://127.0.0.1:1/v1")
        .with_api_key("test-key")
        .with_ephemeral_history()
        .build()
        .expect("builds offline");
    let agent = defaults(Path::new("/repo"), &WorkspaceContext::default());

    let offered = runtime
        .mentra_runtime()
        .tools()
        .into_iter()
        .map(|tool| tool.provider.name)
        .filter(|name| agent.tool_profile.allows(name))
        .collect::<Vec<_>>();

    assert_eq!(
        offered,
        [
            // mentra's compaction intrinsic.
            "compact",
            // mentra's split file tools (`RuntimeBuilder::with_file_tools`).
            "edit",
            "glob",
            "grep",
            "ls",
            "read",
            // basis's own, and ADR-0016's one door.
            crate::tools::SPAWN,
            "write",
        ],
        "the model's whole API, in one place"
    );

    // Registered later rather than at build — mentra registers `load_skill`
    // when a skill loader is installed — so it cannot appear above, and its
    // visibility is asserted here instead. Skills are basis's own convention;
    // this tool is how one is loaded.
    assert!(agent.tool_profile.allows("load_skill"));
}

#[test]
fn the_doors_basis_does_not_surface_stay_shut() {
    // Named individually rather than read off `UNSURFACED_TOOLS`, because a
    // test that reads the constant it is checking asserts nothing. Each of
    // these fails a different way and none of the failures is visible to the
    // person running the agent — see the constant for which is which.
    let agent = defaults(Path::new("/repo"), &WorkspaceContext::default());

    for shut in [
        // A second delegation door beside `spawn`, which is what ADR-0016
        // removed `task` for; plus the yield back to a teammate loop basis
        // never starts, which on a basis run just ends the turn.
        "team_spawn",
        "team_send",
        "team_read_inbox",
        "team_broadcast",
        "team_request",
        "team_respond",
        "team_list_requests",
        "idle",
        // A board nothing in basis reads: every call succeeds and nothing
        // observable happens.
        "task_create",
        "task_claim",
        "task_update",
        "task_list",
        "task_get",
        // Reports on `background_run`, which ADR-0016 hid.
        "check_background",
        // A store basis decided against (D2): recall is off, and a tool
        // writing where nothing reads would report success into a void.
        "memory_pin",
        "memory_forget",
        "memory_search",
    ] {
        assert!(
            !agent.tool_profile.allows(shut),
            "{shut} is still offered to the model"
        );
    }
}

#[test]
fn commands_are_available_unless_the_caller_says_otherwise() {
    // ADR-0013: the first `basis "run the tests"` has to work.
    assert!(WorkspaceBuilder::new("/repo").shell.is_granted());
}

#[test]
fn discovery_is_enabled_until_the_caller_turns_it_off() {
    assert!(WorkspaceBuilder::new("/repo").discovery_enabled);
    assert!(
        !WorkspaceBuilder::new("/repo")
            .without_discovery()
            .discovery_enabled
    );
}

#[test]
fn discovery_stays_off_after_later_discovery_setters() {
    let builder = WorkspaceBuilder::new("/repo")
        .without_discovery()
        .with_context(ContextConfig::default())
        .with_config(Config::default())
        .with_skills(SkillsConfig::default())
        .with_memory(MemoryConfig::default())
        .with_templates(TemplatesConfig::default())
        .with_hooks(HooksConfig::default())
        .with_tools(ToolsConfig::default());
    #[cfg(feature = "mcp")]
    let builder = builder.with_mcp(McpConfig::default());

    assert!(
        !builder.discovery_enabled,
        "a later source-specific setter must not reactivate discovery"
    );
}

#[test]
fn a_fresh_builder_carries_a_private_default_runtime_and_an_inherited_model() {
    // The sugar's shape: `Workspace::open(path)` must behave as it always
    // has, so the default source is a default private recipe — and the model
    // is *unsaid*, deferring to that runtime's policy.
    let builder = WorkspaceBuilder::new("/repo");

    assert!(matches!(builder.runtime, RuntimeSource::Private(_)));
    assert!(matches!(builder.model, WorkspaceModel::Inherited));
}

#[test]
fn builders_return_new_values() {
    let base = WorkspaceBuilder::new("/repo");
    let derived = base.with_model(ModelSelector::Id("pinned".to_string()));

    assert!(matches!(
        derived.model,
        WorkspaceModel::Selector(ModelSelector::Id(ref id)) if id == "pinned"
    ));
    assert!(matches!(
        WorkspaceBuilder::new("/repo").model,
        WorkspaceModel::Inherited
    ));
}

#[test]
fn model_inputs_are_mutually_exclusive_and_last_call_wins() {
    let mut resolved = ModelInfo::new("resolved", "openai").with_context_window(200_000);
    resolved.display_name = Some("Resolved model".to_string());

    let selector_last = WorkspaceBuilder::new("/repo")
        .with_resolved_model(resolved.clone())
        .with_model(ModelSelector::Id("selected".to_string()));
    assert!(matches!(
        selector_last.model,
        WorkspaceModel::Selector(ModelSelector::Id(ref id)) if id == "selected"
    ));

    let resolved_last = WorkspaceBuilder::new("/repo")
        .with_model(ModelSelector::Id("selected".to_string()))
        .with_resolved_model(resolved.clone());
    assert!(matches!(
        resolved_last.model,
        WorkspaceModel::Resolved(ref held) if held == &resolved
    ));
}

#[tokio::test]
async fn a_shared_runtime_is_the_one_the_workspace_borrows() {
    let runtime = Arc::new(
        crate::runtime::Runtime::builder()
            .with_base_url("http://127.0.0.1:1/v1")
            .with_api_key("test-key")
            .with_ephemeral_history()
            .build()
            .expect("builds offline"),
    );

    let builder = WorkspaceBuilder::new("/repo").with_runtime(Arc::clone(&runtime));

    match &builder.runtime {
        RuntimeSource::Shared(held) => assert!(
            Arc::ptr_eq(held, &runtime),
            "borrowing must not clone the substrate"
        ),
        RuntimeSource::Private(_) | RuntimeSource::Reusable(_) => {
            panic!("with_runtime must switch the source")
        }
    }
}

#[tokio::test]
async fn reusable_rebuild_refuses_an_unexpected_basis_runtime_owner() {
    let root = tempfile::tempdir().expect("workspace");
    let mut definition = mentra::provider_core::responses::openai_definition();
    definition.descriptor.id = mentra::ProviderId::new("reusable-owner-test");
    definition.base_url = Some("http://127.0.0.1:1/".to_string());
    let seed = mentra::provider_core::responses::ResponsesProvider::new(
        definition,
        mentra::provider_core::StaticCredentialSource::new("test-key"),
    );
    let recipe = RuntimeBuilder::default()
        .with_reusable_registered_provider(
            "reusable-owner-test",
            move || Ok::<_, std::io::Error>(seed.fresh_session_scope()),
            |_provider| async { Ok::<_, std::io::Error>(()) },
        )
        .with_ephemeral_history()
        .into_reusable_recipe()
        .expect("recipe");
    let workspace = Workspace::builder(root.path())
        .with_runtime_recipe(recipe)
        .without_discovery()
        .fresh_only()
        .with_resolved_model(ModelInfo::new("test-model", "reusable-owner-test"))
        .with_tool_roster(ToolRoster::only(std::iter::empty::<String>()))
        .open()
        .await
        .expect("workspace opens")
        .bind_host_tools(Vec::new())
        .expect("tool-free checkout binds");

    let unexpected_owner = Arc::clone(&workspace.runtime);
    let error = workspace
        .rebuild_for_reuse()
        .await
        .expect_err("the extra Basis runtime owner must consume the pool entry");
    assert!(matches!(error, RunError::ReusableRuntimeNotUnique));
    drop(unexpected_owner);
}

#[test]
fn a_builder_holding_a_credentialed_recipe_does_not_print_it() {
    // WorkspaceBuilder's Debug delegates to the recipe's, which redacts; this
    // pins that the delegation actually happens.
    let printed = format!(
        "{:?}",
        WorkspaceBuilder::new("/repo").with_runtime_builder(
            crate::runtime::RuntimeBuilder::default().with_api_key("sk-secret-value")
        )
    );

    assert!(!printed.contains("sk-secret-value"), "{printed}");
    assert!(printed.contains("redacted"), "{printed}");
}

#[test]
fn an_unconfigured_workspace_keeps_every_tool_result() {
    let agent = defaults(Path::new("/repo"), &WorkspaceContext::default());

    assert_eq!(
        agent.compaction.keep_recent_tool_results,
        usize::MAX,
        "mentra's off switch for micro-compaction is what basis defaults to"
    );
    assert_eq!(agent.compaction.auto_compact_threshold_tokens, Some(50_000));
    assert_eq!(agent.compaction.projected_tool_result_budget, None);
}

#[test]
fn what_a_host_says_about_compaction_is_what_the_agent_carries() {
    let compaction = Compaction::default()
        .with_keep_recent_tool_results(Some(2))
        .with_auto_threshold_tokens(Some(400_000))
        .with_preserve_recent_user_tokens(1_000);

    let agent = agent_config(
        Path::new("/repo"),
        &WorkspaceContext::default(),
        None,
        None,
        ToolRoster::default(),
        compaction,
        PathBuf::from("/transcripts"),
    );

    assert_eq!(agent.compaction.keep_recent_tool_results, 2);
    assert_eq!(agent.compaction.projected_tool_result_budget, None);
    assert_eq!(
        agent.compaction.auto_compact_threshold_tokens,
        Some(400_000)
    );
    assert_eq!(agent.compaction.preserve_recent_user_tokens, 1_000);
}

#[test]
fn with_compaction_returns_a_new_builder() {
    let base = WorkspaceBuilder::new("/repo");
    let derived =
        base.with_compaction(Compaction::default().with_keep_recent_tool_results(Some(1)));

    assert_eq!(
        derived.compaction,
        Compaction::default().with_keep_recent_tool_results(Some(1))
    );
    assert_eq!(
        WorkspaceBuilder::new("/repo").compaction,
        Compaction::default(),
        "a fresh builder keeps everything"
    );
}

/// A workspace holding five files a model would read before it edits anything,
/// each one long enough to be worth eliding — mentra blanks a tool result over
/// 100 bytes — and each carrying a marker no other file has.
fn workspace_of_five_files() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");

    for index in 1..=5 {
        std::fs::write(
            dir.path().join(format!("file{index}.txt")),
            format!("marker-{index} {}", "x".repeat(200)),
        )
        .expect("write file");
    }

    dir
}

/// One tool-calling turn per file, then the sentence the model ends on.
fn reading_all_five() -> MockRuntime {
    let mut builder = MockRuntime::builder().model("mock-model", "openai");

    for index in 1..=5 {
        builder = builder.tool_calls(vec![
            MockToolCall::new(
                "files",
                serde_json::json!({
                    "operations": [{
                        "op": "read",
                        "path": format!("file{index}.txt"),
                    }],
                }),
            )
            .with_id(format!("read-{index}")),
        ]);
    }

    builder
        .text("read them all")
        .build()
        .expect("the mock runtime builds")
}

#[tokio::test]
async fn every_tool_result_the_model_read_is_still_in_front_of_it() {
    // The invariant this pins. A finite `keep_recent_tool_results` replaces
    // older bodies with `[Previous: used files]` on the way to the provider,
    // at any context size and on any model. Mentra now reports that projection,
    // but the model still loses the body. Basis keeps every result unless a
    // host asks for elision by number.
    let workspace = workspace_of_five_files();
    let mock = reading_all_five();
    let mut session = mock
        .runtime()
        .create_session_with_config(
            "compaction",
            mock.model(),
            defaults(workspace.path(), &WorkspaceContext::default()),
        )
        .expect("session");

    session
        .append_turn(vec![ContentBlock::Text {
            text: "read every file".to_string(),
        }])
        .await
        .expect("the scripted turn runs");

    let requests = mock.recorded_requests().await;
    let last = requests.last().expect("the model was asked something");
    let results: Vec<String> = last
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.to_display_string()),
            _ => None,
        })
        .collect();

    assert_eq!(results.len(), 5, "five reads, five results");
    for (index, result) in results.iter().enumerate() {
        assert!(
            result.contains(&format!("marker-{}", index + 1)),
            "the model can no longer see what it read: {result}"
        );
    }
}

/// A workspace that opens without contacting anything: an explicit model id
/// short-circuits resolution, discovery is off so nothing on the machine
/// running this can move an assertion, and the base URL is a closed port.
fn offline(path: &Path) -> WorkspaceBuilder {
    Workspace::builder(path)
        .with_runtime_builder(
            RuntimeBuilder::default()
                .with_base_url("http://127.0.0.1:1/v1")
                .with_api_key("test-key")
                .with_ephemeral_history(),
        )
        .with_model(ModelSelector::Id("test-model".to_string()))
        .without_discovery()
}

/// What an open is expected to have made of a spelling: canonical, and in the
/// form every program on the platform accepts. `Path::canonicalize` alone is
/// the wrong expectation on Windows, where it yields the verbatim `\\?\C:\…`
/// that the open deliberately simplifies away — see `validate_workspace`.
fn resolved(path: &Path) -> PathBuf {
    let canonical = path.canonicalize().expect("canonical path");
    dunce::simplified(&canonical).to_path_buf()
}

/// Every name one opened workspace answers to for the directory it is scoped
/// to. They must be one path, or two of them disagree about where the run is.
///
/// Four of the five seams `open`'s doc comment promises. The fifth — the
/// private runtime's policy roots — cannot be read back: mentra's
/// `RuntimePolicy` keeps `allowed_working_roots`, `allowed_read_roots` and
/// `allowed_write_roots` private and offers no reader for them (an upstream
/// candidate), so nothing here can fail if a future edit hands
/// `workspace_bounded` a second spelling. The memory root and the run header
/// derive from `root()` rather than from `path` and are covered by it.
fn spellings(workspace: &Workspace) -> Vec<&Path> {
    vec![
        workspace.root(),
        workspace.path(),
        workspace.agent.workspace.base_dir.as_path(),
        workspace.declared_registration.root(),
        workspace.hook_registration.key(),
    ]
}

#[tokio::test]
async fn a_relative_path_is_made_absolute_at_open() {
    let workspace = offline(Path::new(".")).open().await.expect("opens");
    let here = resolved(&std::env::current_dir().expect("a working directory"));

    for spelling in spellings(&workspace) {
        assert_eq!(
            spelling, here,
            "a relative spelling must not survive the open"
        );
    }
}

// Unix only, like every other symlink test in this crate
// (`runtime::dispatch::tests`, `fingerprint`): `std::os::unix` does not exist
// on Windows, so an ungated call here is a build failure rather than a test
// failure — and CI compiles this crate's tests on all three platforms.
#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_spelling_opens_the_directory_it_names() {
    let base = tempfile::tempdir().expect("tempdir");
    let real = base.path().join("real");
    std::fs::create_dir(&real).expect("create the real directory");
    let link = base.path().join("link");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");

    let workspace = offline(&link).open().await.expect("opens");
    let canonical = resolved(&real);

    for spelling in spellings(&workspace) {
        assert_eq!(
            spelling, canonical,
            "a symlinked spelling must resolve once, at the open"
        );
    }
}

#[tokio::test]
async fn a_dotted_spelling_is_folded_at_open() {
    let base = tempfile::tempdir().expect("tempdir");
    let nested = base.path().join("nested");
    std::fs::create_dir(&nested).expect("create the nested directory");

    let workspace = offline(&nested.join("..").join("nested"))
        .open()
        .await
        .expect("opens");
    let canonical = resolved(&nested);

    for spelling in spellings(&workspace) {
        assert_eq!(spelling, canonical, "`..` must not survive the open");
    }
}

/// The resolved root is the spelling the rest of the world uses.
///
/// `std::fs::canonicalize` answers `\\?\C:\repo` on Windows, and this one
/// value becomes mentra's policy root and the agent's base directory. mentra
/// asks whether a path the model named is allowed with `starts_with` against
/// that root over components whose `Prefix` it copies through untouched, so a
/// verbatim root does not prefix the plain `C:\repo\file.txt` a model writes:
/// every absolute path it named would be refused. Nothing off Windows can
/// notice, which is why this is pinned here rather than left to the shared
/// assertions above.
#[cfg(windows)]
#[tokio::test]
async fn the_resolved_root_is_not_a_verbatim_windows_path() {
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = offline(base.path()).open().await.expect("opens");

    for spelling in spellings(&workspace) {
        assert!(
            !spelling.as_os_str().to_string_lossy().starts_with(r"\\?\"),
            "a verbatim root denies every absolute path the model names: {}",
            spelling.display()
        );
        assert!(
            spelling.is_absolute(),
            "simplifying must not cost the root its prefix: {}",
            spelling.display()
        );
    }
}
