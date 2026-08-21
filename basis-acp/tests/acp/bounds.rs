//! An allowance that ran out, as the client hears about it.
//!
//! Its own module because it is the one thing a client does that is neither
//! prompting nor deciding: it set a budget before the conversation started and
//! finds out, one turn later, that the agent reached it. The failure this pins
//! is a translation, not a hang — a tripped bound used to arrive as `-32603`,
//! and a Zed user who set a tool budget read "internal error" for the thing
//! basis's own CLI is proud of reporting as exit `3` (ADR-0014).

use std::sync::Arc;

use agent_client_protocol::schema::v1::StopReason;
use basis::TurnOptions;

use crate::client::drive;
use crate::source::{MockSource, workspace, writing_mock};

#[tokio::test]
async fn a_turn_that_ran_out_of_tool_calls_stops_rather_than_fails() {
    // Zero, so the bound is reached by the first call the script makes and
    // reached deterministically — mentra checks the budget before it runs a
    // batch, so nothing here depends on how long anything took.
    let workspace = workspace();
    let mock = Arc::new(writing_mock(&workspace));
    let source =
        MockSource::new(&mock, &workspace).with_bounds(TurnOptions::default().with_tool_budget(0));

    let (stop_reasons, observed) = drive(source, vec!["write a file"], None).await;

    assert_eq!(
        stop_reasons,
        vec![StopReason::MaxTurnRequests],
        "an allowance the operator set is a stop reason, not a broken agent"
    );
    assert!(
        observed
            .lock()
            .expect("not poisoned")
            .permission_requests
            .is_empty(),
        "the budget refused the call before anyone could be asked about it"
    );
}
