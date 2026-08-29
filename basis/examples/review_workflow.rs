//! A review as a fan-out: many typed runs, one workspace, one allowance, one
//! stream — and a verdict folded out of what they said.
//!
//! This is ADR-0010's claim made concrete. Claude Code ships a workflow feature
//! as a JavaScript DSL because the product is closed and orchestration has to
//! travel *to* the harness as a script. basis's bet is the inverse: the harness
//! travels to the host as a crate, so `parallel` is a `JoinSet`, `pipeline` is
//! the `.await` on the next line, and a judge panel is a `Vec` of typed values
//! with a real debugger behind it. Nothing below is interpreted by basis.
//!
//! Four primitives carry it, and each is one line:
//!
//! - one `Workspace`, so `AGENTS.md` is read once rather than once per reviewer;
//! - one `BudgetPool`, so "this whole review costs ≤ N tokens" is a figure and
//!   not N figures that have to be rebalanced;
//! - `output::<T>()`, so each reviewer answers in a shape the next step can use
//!   instead of prose the next step would have to parse;
//! - `EventFanIn`, so N concurrent runs narrate into one pane without losing
//!   which of them said what.
//!
//! The one thing to copy carefully is on `review` below: each reviewer takes
//! two turns, one to read and one to answer. A typed turn can do both at once
//! (`OutputSpec::with_tools`); two turns are the choice this workflow makes,
//! for the reason written out there.
//!
//! ```sh
//! export BASIS_API_KEY=…                    # or ANTHROPIC_API_KEY, etc.
//! export BASIS_BASE_URL=http://…/v1         # optional
//! export BASIS_MODEL=…                      # optional
//! cargo run -p basis --example review_workflow -- /repo "the parser in src/lex.rs" 200000
//! ```

use std::{env, error::Error, sync::Arc, time::Duration};

use basis::{
    AllowAll, BudgetPool, Event, EventFanIn, MergedEvents, ModelSelector, NullSink, OutputReport,
    OutputSpec, RunError, RunOutcome, RunSpec, TaggedEvent, TaggedSink, Workspace,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinSet;

/// What one reviewer may spend, beside the tokens it draws from the shared pool.
/// Per-run, so one reviewer that goes quiet or loops cannot hold the whole
/// fan-out open — and unset by default everywhere in basis, because no unattended
/// run should be relying on someone else to have guessed a bound (ADR-0014).
const DEADLINE: Duration = Duration::from_secs(300);
const TOOL_BUDGET: usize = 25;

/// The shaping turn's prompt. Short on purpose: everything it needs to know,
/// the conversation is already carrying.
const SUBMIT: &str = "Submit what you found, one entry per problem. \
     Report nothing you did not see for yourself in the files you read.";

/// A comfortable allowance for a handful of reviewers, overridable on the
/// command line. It is the *job's* figure: a reviewer that finishes cheaply
/// leaves its share to the others, with nobody rebalancing anything.
const DEFAULT_BUDGET: u64 = 200_000;

/// One reviewer, and the single thing it is asked to look for.
///
/// Splitting a review by dimension rather than by file is what makes the
/// fan-out worth doing: three narrow readers disagree usefully where one broad
/// reader averages everything into "looks fine".
struct Dimension {
    name: &'static str,
    brief: &'static str,
}

const DIMENSIONS: &[Dimension] = &[
    Dimension {
        name: "correctness",
        brief: "logic errors, unhandled failures, and inputs that would break it at runtime",
    },
    Dimension {
        name: "clarity",
        brief: "names, structure, and missing context that would slow down the next reader",
    },
    Dimension {
        name: "tests",
        brief: "behavior that nothing asserts, and assertions that would pass over broken code",
    },
];

/// What one reviewer answers with. The schema in [`findings_spec`] is what the
/// model is actually handed; this is what the host gets back.
#[derive(Debug, Deserialize, Serialize)]
struct Findings {
    findings: Vec<Finding>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Finding {
    file: String,
    note: String,
    /// The field the fold below turns on. A `bool` rather than a severity
    /// string because the host has to branch on it, and branching on prose is
    /// the thing structured output exists to stop.
    blocking: bool,
}

#[derive(Debug, Deserialize)]
struct Verdict {
    ship: bool,
    rationale: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut args = env::args().skip(1);
    let path = args.next().unwrap_or_else(|| ".".to_string());
    let subject = args
        .next()
        .unwrap_or_else(|| "the code in this workspace".to_string());
    let limit = args
        .next()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(DEFAULT_BUDGET);

    // Opened once and shared by every reviewer: one context discovery, one
    // resolved model, one set of MCP connections. An `Arc` because the runs are
    // spawned tasks, and `Workspace` is `Send + Sync` precisely so they can be.
    let workspace = Arc::new(
        Workspace::builder(&path)
            .with_model(selected_model())
            .open()
            .await?,
    );
    let pool = BudgetPool::new(limit);
    println!(
        "reviewing {subject} in {} with {} ({limit} tokens for the job)",
        workspace.root().display(),
        workspace.model()
    );

    // Fan out. Every sink is minted before `into_events`, which is the order the
    // API asks for: consuming takes the fan-in by value, so the merged stream
    // can end when the last *sink* is dropped.
    let fan = EventFanIn::new();
    let mut reviewers = JoinSet::new();
    for dimension in DIMENSIONS {
        let workspace = Arc::clone(&workspace);
        let sink = fan.sink(dimension.name);
        // `pool.spec` is the whole of "this run draws on the job's allowance".
        // Everything after it is this reviewer's own: what it is called, what it
        // is asked, and the bounds no unattended run should be without.
        let spec = pool
            .spec(brief(dimension, &subject))
            .with_session_name(dimension.name)
            .with_deadline(DEADLINE)
            .with_tool_budget(TOOL_BUDGET);

        reviewers.spawn(async move { (dimension.name, review(&workspace, spec, sink).await) });
    }
    let mut merged = fan.into_events();

    // The runs and the pane that watches them are two branches of one join. The
    // reviewers' reports are consumed inside `review`, never returned — a report
    // hands the sink back, so a held report is a branch of the merged stream
    // held open, and this join would wait on a stream waiting on the join.
    let (reviewed, ()) = tokio::join!(collect(&mut reviewers), narrate(&mut merged));

    println!("\n--- findings ---");
    for (dimension, found) in &reviewed {
        println!(
            "[{dimension}] {} findings, {} blocking",
            found.findings.len(),
            found.findings.iter().filter(|f| f.blocking).count()
        );
    }

    // Fan in, and the whole point of asking for values instead of prose: this
    // is a filter over typed data. It is also where the workflow branches on its
    // own results — with nothing blocking there is nothing to weigh, and the
    // cheapest run is the one that never happens.
    let blocking: Vec<&Finding> = reviewed
        .iter()
        .flat_map(|(_, found)| &found.findings)
        .filter(|finding| finding.blocking)
        .collect();

    if blocking.is_empty() {
        println!("\nverdict: ship — no reviewer raised anything blocking");
    } else {
        verify(&workspace, &pool, &subject, &reviewed).await?;
    }

    println!(
        "\nthe review cost {} of {} tokens ({} left)",
        pool.spent(),
        pool.limit(),
        pool.remaining()
    );

    Ok(())
}

/// One dimension's review, in two turns — a choice, not a constraint.
///
/// A typed turn is a *shaping* turn by default: it is handed exactly one tool,
/// the one that *is* the answer, and forced to call it on the first round, so
/// it cannot read a file or run a command. Asking for findings in a single
/// `output` call therefore returns an empty list from a model that never opened
/// anything, and returns it as a success. `OutputSpec::with_tools` lifts that —
/// one turn that reads and then answers — and a smaller workflow should reach
/// for it.
///
/// This one keeps the two turns on purpose. The reading turn is where a
/// reviewer's whole context goes, and here that context is deliberately narrow:
/// three reviewers read for three different things, and the shaping prompt
/// (`SUBMIT`) can then say "report what *you* found" without competing with a
/// tool roster for the model's attention. The session carries the reading
/// across, which is the entire reason a `PreparedRun` outlives a turn.
async fn review(
    workspace: &Workspace,
    spec: RunSpec,
    sink: TaggedSink<&'static str>,
) -> Result<Findings, Failure> {
    let mut run = workspace.prepare(spec)?;

    // Turn one: read the code. The prompt is the spec's, so what a reviewer is
    // and what it was asked stay one value.
    let report = run.execute(sink).await?;
    if let RunOutcome::Error { message } = report.outcome {
        // Nothing for the next turn to shape. A reviewer answering from an
        // empty conversation answers "no findings", which is the one wrong
        // answer that is indistinguishable from a right one.
        return Err(Failure::Reading(message));
    }

    // Turn two: shape it. The sink comes back inside the report and goes
    // straight into the next turn — and the report is not held past this line,
    // because a held report is a branch of the merged stream held open.
    // `map_err(RunError::from)` because this reviewer has nothing to do with
    // the rest of an `OutputFailure`: a shaping turn that produced no findings
    // is one reviewer's silence, and the fan-in below already prices the run
    // from the shared pool. A caller that wanted the usage, the bound, or the
    // sink back would keep the failure whole instead.
    let OutputReport { value, .. } = run
        .output::<Findings, _, _>(SUBMIT, findings_spec(), report.sink, AllowAll)
        .await
        .map_err(RunError::from)?;

    Ok(value)
}

/// Why a reviewer produced nothing.
///
/// A local enum rather than a `Box<dyn Error>` because the two cases are not
/// alike: one is basis telling the host something went wrong, the other is a turn
/// that completed and reported its own failure on the stream.
#[derive(Debug)]
enum Failure {
    Reading(String),
    Run(RunError),
}

impl From<RunError> for Failure {
    fn from(error: RunError) -> Self {
        Self::Run(error)
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reading(message) => write!(f, "the reading turn failed: {message}"),
            Self::Run(error) => write!(f, "{error}"),
        }
    }
}

/// Every reviewer's answer, gathered as they finish.
async fn collect(
    reviewers: &mut JoinSet<(&'static str, Result<Findings, Failure>)>,
) -> Vec<(&'static str, Findings)> {
    let mut reviewed = Vec::new();

    while let Some(joined) = reviewers.join_next().await {
        match joined {
            Ok((dimension, Ok(found))) => reviewed.push((dimension, found)),
            // One reviewer failing is not the review failing. The others have
            // already spent their share and still have something to say, and a
            // fan-out that threw all of it away on the first error would be a
            // worse harness than one that reports the gap.
            Ok((dimension, Err(error))) => eprintln!("[{dimension}] gave up: {error}"),
            Err(error) => eprintln!("a reviewer panicked: {error}"),
        }
    }

    reviewed
}

/// One pane for N runs, tagged so adjacency does not have to be guessed at.
///
/// Driven concurrently with the runs it watches: the queue only ends once every
/// sink is dropped, so reading it after the join would be reading a stream that
/// is already over — see `MergedEvents::drain` for that shape.
async fn narrate(merged: &mut MergedEvents<&'static str>) {
    while let Some(TaggedEvent { tag, event }) = merged.recv().await {
        match event {
            Event::ToolQueued { tool_name, .. } => println!("[{tag}] {tool_name}"),
            Event::Notice { severity, message } => println!("[{tag}] {severity:?}: {message}"),
            Event::RunFinished { outcome, .. } => println!("[{tag}] finished: {outcome:?}"),
            _ => {}
        }
    }
}

/// The fan-in step: one more bounded run, given the findings rather than the
/// code, drawing on what the reviewers left in the pool.
///
/// A single shaping turn is exactly right here, where in `review` it would have
/// been the wrong half of a trade: this judge has nothing to look up.
/// Everything it weighs is in the prompt, so being handed one tool and told to
/// answer costs it nothing — and the forcing that costs nothing is the forcing
/// worth keeping.
async fn verify(
    workspace: &Workspace,
    pool: &BudgetPool,
    subject: &str,
    reviewed: &[(&'static str, Findings)],
) -> Result<(), RunError> {
    let dossier: Value = reviewed
        .iter()
        .map(|(dimension, found)| ((*dimension).to_string(), json!(found.findings)))
        .collect();
    let prompt = format!(
        "Three reviewers looked at {subject} and reported the findings below. \
         Decide whether it ships. Judge only what is written here — do not open \
         the files.\n\n{dossier:#}"
    );

    let mut judge = workspace.prepare(
        RunSpec::default()
            .with_session_name("verdict")
            .with_budget(pool.clone())
            .with_deadline(DEADLINE),
    )?;

    match judge
        .output::<Verdict, _, _>(prompt, verdict_spec(), NullSink, AllowAll)
        .await
        .map_err(RunError::from)
    {
        Ok(OutputReport { value, .. }) => println!(
            "\nverdict: {} — {}",
            if value.ship { "ship" } else { "hold" },
            value.rationale
        ),
        // A decision, not a failure of the work: the reviewers spent the whole
        // allowance. This is the one error a fan-out answers by stopping rather
        // than by retrying, which is why it has a variant of its own.
        Err(RunError::BudgetExhausted { limit, spent }) => {
            println!("\nno verdict: the reviews spent {spent} of {limit} tokens");
        }
        Err(error) => return Err(error),
    }

    Ok(())
}

fn brief(dimension: &Dimension, subject: &str) -> String {
    format!(
        "Review {subject}. Look only for {}; another reviewer is covering \
         everything else, so report nothing outside your dimension. Read the \
         relevant files first, then submit what you found.",
        dimension.brief
    )
}

/// The shape a reviewer answers in.
///
/// Written by hand, because the descriptions are a *prompt*: they are what the
/// model reads to decide what belongs in each field, and nobody writes those by
/// deriving them from a struct.
fn findings_spec() -> OutputSpec {
    OutputSpec::new(
        "submit_findings",
        "Call this once you have read the relevant code and have nothing further \
         to check. An empty list is a valid answer and is better than a padded one.",
        json!({
            "type": "object",
            "properties": {
                "findings": {
                    "type": "array",
                    "description": "One entry per problem worth a reader's time.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "file": {
                                "type": "string",
                                "description": "Path relative to the workspace root."
                            },
                            "note": {
                                "type": "string",
                                "description": "What is wrong and why it matters, in one sentence."
                            },
                            "blocking": {
                                "type": "boolean",
                                "description": "True only when this must be fixed before the code ships. A style preference is never blocking."
                            }
                        },
                        "required": ["file", "note", "blocking"]
                    }
                }
            },
            "required": ["findings"]
        }),
    )
}

fn verdict_spec() -> OutputSpec {
    OutputSpec::new(
        "submit_verdict",
        "Call this once you have weighed every finding against every other.",
        json!({
            "type": "object",
            "properties": {
                "ship": {
                    "type": "boolean",
                    "description": "True when nothing reported should stop this from merging."
                },
                "rationale": {
                    "type": "string",
                    "description": "One or two sentences naming the findings that decided it."
                }
            },
            "required": ["ship", "rationale"]
        }),
    )
}

/// `BASIS_MODEL` when it is set, the provider's newest otherwise — the same
/// selection the other examples make.
fn selected_model() -> ModelSelector {
    match env::var("BASIS_MODEL") {
        Ok(id) => ModelSelector::Id(id),
        Err(_) => ModelSelector::NewestAvailable,
    }
}
