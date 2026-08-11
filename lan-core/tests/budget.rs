//! One allowance, several runs: what a [`BudgetPool`] does against a real turn.
//!
//! The unit tests in `src/budget.rs` check the arithmetic. These check the two
//! claims that only a driven turn can settle:
//!
//! 1. **Spending reaches the pool as it happens.** The pool is not a tally lan
//!    reconciles afterwards — it is the counter mentra adds each round's usage
//!    to, so `pool.spent()` moves because a run ran, without anyone settling
//!    anything.
//! 2. **A spent pool refuses rather than sending.** The turn ends before the
//!    prompt goes out, the endpoint is never contacted, and the conversation is
//!    left as it was.
//!
//! The second exists because of the fourth test here, which pins what mentra
//! does with `token_budget: Some(0)` — the alternative lan rejected. That
//! behavior is upstream and could change; if it does, this test says so, and the
//! reasoning written on `BudgetPool` needs revisiting rather than quietly
//! becoming wrong.
//!
//! The endpoint is the one `tests/workspace.rs` uses, with usage added to the
//! completed response — loopback, no name resolved, no packet off the machine.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use lan_core::{
    BudgetPool, CollectingSink, ContextConfig, RunError, RunOutcome, RunSpec, TurnOptions,
    Workspace, WorkspaceBuilder, hooks::HooksConfig, skills::SkillsConfig,
    templates::TemplatesConfig,
};
use mentra::ModelSelector;

/// What every scripted response reports spending. Input plus output is what the
/// budget counts, so one round of this costs a pool 120 tokens.
const INPUT_TOKENS: u64 = 100;
const OUTPUT_TOKENS: u64 = 20;
const ROUND_COST: u64 = INPUT_TOKENS + OUTPUT_TOKENS;

/// A builder that looks nowhere except where the test put something, and that
/// contacts nothing while opening. `tests/workspace.rs` explains the choices.
fn offline(workspace: &Path) -> WorkspaceBuilder {
    Workspace::builder(workspace)
        .with_api_key("test-key")
        .with_model(ModelSelector::Id("test-model".to_string()))
        .with_ephemeral_history()
        .with_context(ContextConfig {
            file_name: "AGENTS.md".to_string(),
            global_dir: None,
            walk_parents: false,
        })
        .with_skills(SkillsConfig {
            workspace_subdir: PathBuf::from(".lan/skills"),
            global_dir: None,
        })
        .with_templates(TemplatesConfig {
            workspace_subdir: PathBuf::from(".lan/templates"),
            global_dir: None,
        })
        .with_hooks(HooksConfig {
            workspace_file: PathBuf::from(".lan/hooks.json"),
            global_dir: None,
        })
}

async fn workspace_on(endpoint: &ScriptedEndpoint) -> Workspace {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("AGENTS.md"), "house rules").expect("write");

    offline(dir.path())
        .with_base_url(&endpoint.base_url)
        .open()
        .await
        .expect("opens")
}

#[tokio::test]
async fn a_fan_out_draws_on_one_figure_rather_than_one_each() {
    let endpoint = ScriptedEndpoint::start();
    let workspace = workspace_on(&endpoint).await;
    let pool = BudgetPool::new(500_000);

    let mut first = workspace
        .prepare(pool.spec("review the tests"))
        .expect("mints");
    let mut second = workspace
        .prepare(pool.spec("review the docs"))
        .expect("mints");

    let (left, right) = tokio::join!(
        first.execute(CollectingSink::default()),
        second.execute(CollectingSink::default()),
    );
    let left = left.expect("the first run completes");
    let right = right.expect("the second run completes");

    assert!(matches!(left.outcome, RunOutcome::Ok));
    assert!(matches!(right.outcome, RunOutcome::Ok));

    // The claim: both runs reported into the same figure, without either
    // knowing about the other and without anyone settling a report afterwards.
    assert_eq!(
        pool.spent(),
        left.usage.total_tokens() + right.usage.total_tokens(),
        "one allowance, spent by two runs"
    );
    assert_eq!(pool.spent(), 2 * ROUND_COST);
    assert_eq!(pool.remaining(), 500_000 - 2 * ROUND_COST);
}

#[tokio::test]
async fn a_pooled_run_and_its_own_report_agree_on_what_it_spent() {
    // The pool counts rounds through mentra's handle; `RunUsage` counts them
    // through lan's event stream. Two paths, and they have to arrive at the
    // same number or one of them is lying about the bill.
    let endpoint = ScriptedEndpoint::start();
    let workspace = workspace_on(&endpoint).await;
    let pool = BudgetPool::new(10_000);

    let mut run = workspace.prepare(pool.spec("go")).expect("mints");
    let report = run
        .execute(CollectingSink::default())
        .await
        .expect("completes");

    assert_eq!(report.usage.total_tokens(), ROUND_COST);
    assert_eq!(pool.spent(), report.usage.total_tokens());
}

#[tokio::test]
async fn a_pool_that_runs_dry_ends_the_remaining_runs_minting() {
    // A limit one round cannot stay under: the first run spends it all, and
    // every run behind it in the fan-out is refused — the decision a caller
    // fanning out over a shared allowance has to be able to act on.
    let endpoint = ScriptedEndpoint::start();
    let workspace = workspace_on(&endpoint).await;
    let pool = BudgetPool::new(ROUND_COST);

    let mut first = workspace
        .prepare(pool.spec("the one that runs"))
        .expect("mints");
    first
        .execute(CollectingSink::default())
        .await
        .expect("completes");

    assert!(pool.is_exhausted());
    assert_eq!(pool.remaining(), 0);

    let mut second = workspace
        .prepare(pool.spec("the one that does not"))
        .expect("minting is still free — spending is what is refused");
    let refused = second
        .execute(CollectingSink::default())
        .await
        .expect_err("a spent pool refuses the turn");

    assert!(
        matches!(
            refused,
            RunError::BudgetExhausted { limit, spent }
                if limit == ROUND_COST && spent == ROUND_COST
        ),
        "the refusal names the figures rather than reading as a provider failure: {refused}"
    );
    assert_eq!(
        endpoint.served(),
        1,
        "the refused run never reaches the provider"
    );
    assert!(
        second.history().is_empty(),
        "and leaves nothing in the conversation"
    );
}

#[tokio::test]
async fn a_zero_token_budget_is_what_refusing_avoids() {
    // Pinning the upstream behavior `BudgetPool` chose against. mentra checks
    // `reported >= budget`, so a budget of zero is already crossed before the
    // first round: the run ends gracefully having done nothing, owes its caller
    // a final assistant message it never got, and surfaces as "run completed
    // without a final assistant message" — a provider-shaped failure for an
    // accounting decision, with the prompt left committed.
    //
    // One part of that has since improved: mentra records which bound ended a
    // run, so the report names the token budget even though the message still
    // does not. What refusing avoids is narrower for it and no less real — a
    // turn that spent a slot in the conversation on nothing, failing over a
    // limit it could have been told about before the prompt went out.
    //
    // If this ever stops being true, the argument on `BudgetPool` for refusing
    // instead needs rewriting rather than silently going stale.
    let endpoint = ScriptedEndpoint::start();
    let workspace = workspace_on(&endpoint).await;

    let mut run = workspace.prepare("go").expect("mints");
    let report = run
        .execute_with_options(
            CollectingSink::default(),
            TurnOptions::default().with_token_budget(0),
        )
        .await
        .expect("the turn is taken rather than refused");

    let RunOutcome::Error { message } = &report.outcome else {
        panic!("a zero-budget turn does not answer");
    };
    assert!(
        message.contains("without a final assistant message"),
        "the failure reads as a provider problem: {message}"
    );
    assert_eq!(
        report.stopped_by,
        Some(lan_core::Bound::TokenBudget),
        "though the bound names itself, which is what tells the two apart"
    );
    assert_eq!(endpoint.served(), 0, "no round ever ran");
    assert_eq!(
        run.history().len(),
        1,
        "yet the prompt stayed in the conversation, unanswered"
    );
}

#[tokio::test]
async fn a_second_prompt_on_one_conversation_draws_on_the_same_pool() {
    // A pool bounds the run, not its first turn: a conversation that keeps
    // going keeps spending, and stops when the job's allowance is gone.
    let endpoint = ScriptedEndpoint::start();
    let workspace = workspace_on(&endpoint).await;
    let pool = BudgetPool::new(ROUND_COST + 1);

    let mut run = workspace
        .prepare(RunSpec::new("first").with_budget(pool.clone()))
        .expect("mints");
    run.execute(CollectingSink::default())
        .await
        .expect("the first turn completes");

    assert_eq!(pool.remaining(), 1, "one token short of another round");

    let taken = run
        .send("second", CollectingSink::default(), lan_core::AllowAll)
        .await;
    assert!(
        taken.is_ok(),
        "a pool with something left still takes the turn"
    );
    assert!(pool.is_exhausted());

    // The documented overshoot, in the smallest case there is. One token of
    // headroom bought a whole round, because usage is only known once that
    // round has streamed — so the pool lands at nearly twice its limit with a
    // single run in flight, and a fan-out can do this once per concurrent run.
    // `limit` is what a job may start spending at, never a ceiling on the bill.
    assert_eq!(pool.spent(), 2 * ROUND_COST);
    assert_eq!(pool.limit(), ROUND_COST + 1);

    let third = run
        .send("third", CollectingSink::default(), lan_core::AllowAll)
        .await
        .expect_err("and refuses once it has nothing");
    assert!(matches!(third, RunError::BudgetExhausted { .. }));
}

#[tokio::test]
async fn spending_recorded_by_hand_draws_the_same_pool_down() {
    // The escape hatch: work that spent against the same allowance without
    // drawing on the pool — including a subagent's, if a host managed to
    // observe it — is charged here, and bounds the runs that follow.
    let endpoint = ScriptedEndpoint::start();
    let workspace = workspace_on(&endpoint).await;
    let pool = BudgetPool::new(1_000);

    pool.record(lan_core::RunUsage {
        input_tokens: 1_000,
        ..lan_core::RunUsage::default()
    });

    let mut run = workspace.prepare(pool.spec("go")).expect("mints");
    let refused = run
        .execute(CollectingSink::default())
        .await
        .expect_err("the pool was spent before any run touched it");

    assert!(matches!(refused, RunError::BudgetExhausted { .. }));
    assert_eq!(endpoint.served(), 0);
}

/// An OpenAI-compatible endpoint on loopback that completes any turn, reporting
/// usage as a real provider does.
///
/// The usage is the point: without it no round reports anything, and a pool
/// bounded against silence would never fill.
struct ScriptedEndpoint {
    base_url: String,
    served: Arc<AtomicUsize>,
}

impl ScriptedEndpoint {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test endpoint");
        let address = listener.local_addr().expect("read endpoint address");
        let served = Arc::new(AtomicUsize::new(0));

        let counted = Arc::clone(&served);
        thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                // One thread per connection, so two runs in flight are answered
                // in parallel rather than in turn.
                let index = counted.fetch_add(1, Ordering::SeqCst) + 1;
                thread::spawn(move || answer(stream, index));
            }
        });

        Self {
            base_url: format!("http://{address}/"),
            served,
        }
    }

    /// How many requests reached the provider. Zero is the assertion a refused
    /// turn is checked with.
    fn served(&self) -> usize {
        self.served.load(Ordering::SeqCst)
    }
}

/// Reads one request and writes one completed response.
fn answer(mut stream: TcpStream, index: usize) {
    read_http_request(&mut stream);

    let body = sse_body(index);
    let response = format!(
        "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

/// The smallest stream that is a finished assistant turn, with usage on the
/// completed response — which is where the Responses wire format puts it, and
/// where mentra reads it from.
fn sse_body(index: usize) -> String {
    [
        format!(
            r#"{{"type":"response.created","response":{{"id":"resp_{index}","model":"test-model","status":"in_progress"}}}}"#
        ),
        r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","content":[]}}"#.to_string(),
        format!(
            r#"{{"type":"response.output_item.done","output_index":0,"item":{{"type":"message","content":[{{"type":"output_text","text":"reply-{index}"}}]}}}}"#
        ),
        format!(
            r#"{{"type":"response.completed","response":{{"id":"resp_{index}","model":"test-model","status":"completed","usage":{{"input_tokens":{INPUT_TOKENS},"output_tokens":{OUTPUT_TOKENS},"total_tokens":{ROUND_COST}}}}}}}"#
        ),
    ]
    .iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect()
}

/// Reads a request up to the end of its declared body.
///
/// Reading to end-of-stream would deadlock: the client keeps the connection
/// open waiting for the response it has not been sent yet.
fn read_http_request(stream: &mut TcpStream) {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut header_end = None;
    let mut content_length = 0_usize;

    loop {
        let Ok(read) = stream.read(&mut buffer) else {
            return;
        };
        if read == 0 {
            return;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if header_end.is_none()
            && let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let end = index + 4;
            header_end = Some(end);
            content_length = String::from_utf8_lossy(&bytes[..end])
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap_or_default())
                })
                .unwrap_or_default();
        }
        if header_end.is_some_and(|end| bytes.len() >= end + content_length) {
            return;
        }
    }
}
