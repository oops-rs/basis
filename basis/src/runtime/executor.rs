//! Where one runtime's commands run, and what environment they carry.
//!
//! Two rules live here, in this order, and the order is the point.
//!
//! **The environment first.** Mentra deliberately clears the ambient process
//! environment before running a model command. A host can still need to attach
//! non-secret execution context — a task identity, a registry directory —
//! without exporting it process-wide, so this executor adds exactly the pairs
//! the runtime builder was given. Merged before routing, because what a
//! command carries is a fact about *this runtime* rather than about where the
//! command lands: a target that received a different environment from the
//! local executor would be a second thing to keep in step, and nobody would
//! notice the day they drifted.
//!
//! **Then the destination.** ADR-0021 made *where a command runs* a dimension
//! of a `spawn` call rather than a second tool, and this is the one place that
//! dimension is acted on: `None` is mentra's local executor, and a name is the
//! executor registered under it — though nothing currently registers one; see
//! `docs/targets.md`'s dateline note. basis ships no executors and writes
//! none — what a target can reach is whatever the host's own code can reach,
//! and basis never describes it as anything else (ADR-0013, and
//! `docs/targets.md`).
//!
//! A request whose target nothing serves is an error and never a local run.
//! That direction is not a preference: a command a host addressed to a build
//! machine, silently executing here instead, is the one failure mode a target
//! exists to prevent. mentra's own [`LocalRuntimeExecutor`] takes the same
//! ruling for the same reason.
//!
//! Runtime-scoped since ADR-0018 moved the executor with the rest of the
//! process knobs: on a shared runtime every workspace's commands see the same
//! pairs and reach the same targets, and a host that wants two workspaces to
//! differ gives each its own runtime via
//! [`WorkspaceBuilder::with_runtime_builder`](crate::WorkspaceBuilder::with_runtime_builder).

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use mentra::runtime::{CommandOutput, CommandRequest, LocalRuntimeExecutor, RuntimeExecutor};

/// The names a runtime routes on, and what each one resolves to.
///
/// A `BTreeMap` so the set a model is told about, and the set a refusal lists,
/// are in one order on every build rather than in whatever order a hash seed
/// produced.
pub(super) type CommandTargets = BTreeMap<String, Arc<dyn RuntimeExecutor>>;

#[derive(Clone)]
pub(super) struct TargetedExecutor {
    environment: Arc<BTreeMap<String, String>>,
    targets: Arc<CommandTargets>,
}

impl TargetedExecutor {
    /// Takes the environment behind an `Arc` because the runtime keeps the same
    /// one: a declared tool's subprocess receives these pairs too, and two
    /// copies of the host's statement would be two things to keep in step.
    pub(super) fn new(environment: Arc<BTreeMap<String, String>>, targets: CommandTargets) -> Self {
        Self {
            environment,
            targets: Arc::new(targets),
        }
    }
}

#[async_trait]
impl RuntimeExecutor for TargetedExecutor {
    async fn run(&self, mut request: CommandRequest) -> Result<CommandOutput, String> {
        merge(&mut request.env, &self.environment);

        let Some(target) = request.target.clone() else {
            // Everything mentra's local executor ever did, unchanged: the
            // timeout, the output cap, the process group, the cleanup.
            return LocalRuntimeExecutor.run(request).await;
        };

        match self.targets.get(&target) {
            // The name is left on the request rather than stripped, so an
            // executor a host registered under two names can still tell which
            // one it was called as — and so that a host executor which serves
            // only some of them can refuse the rest, as mentra asks.
            Some(executor) => executor.run(request).await,
            None => Err(unregistered(&target, &self.targets)),
        }
    }
}

/// What a request naming an unregistered target is answered with.
///
/// `spawn` refuses such a call twice before it can reach here — in the
/// authorization preview and again in `execute_mut` — so this is the floor
/// under a host driving mentra's runtime directly, or under a `SpawnTool`
/// built with names its runtime does not serve. It names the set for the same
/// reason those refusals do: a reader who cannot see the registered names
/// cannot tell a typo from a missing registration.
fn unregistered(target: &str, targets: &CommandTargets) -> String {
    if targets.is_empty() {
        return format!(
            "no executor is registered for command target `{target}`; this runtime registers no \
             command targets"
        );
    }

    format!(
        "no executor is registered for command target `{target}`; this runtime registers {}",
        targets
            .keys()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn merge(current: &mut Vec<(String, String)>, fixed: &BTreeMap<String, String>) {
    current.retain(|(name, _)| !fixed.contains_key(name));
    current.extend(
        fixed
            .iter()
            .map(|(name, value)| (name.clone(), value.clone())),
    );
}

#[cfg(test)]
mod tests;
