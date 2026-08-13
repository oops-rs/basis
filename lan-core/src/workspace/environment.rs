//! Fixed command environment for one opened workspace.
//!
//! Mentra deliberately clears the ambient process environment before running
//! a model command. A host can still need to attach non-secret execution
//! context that is different for two workspaces in the same process. This
//! executor adds exactly the pairs the workspace builder was given, then
//! delegates every timeout, output cap, process-group, and cleanup rule to
//! Mentra's local executor.

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use mentra::runtime::{CommandOutput, CommandRequest, LocalRuntimeExecutor, RuntimeExecutor};

#[derive(Clone)]
pub(super) struct EnvironmentExecutor {
    environment: Arc<BTreeMap<String, String>>,
}

impl EnvironmentExecutor {
    pub(super) fn new(environment: BTreeMap<String, String>) -> Self {
        Self {
            environment: Arc::new(environment),
        }
    }
}

#[async_trait]
impl RuntimeExecutor for EnvironmentExecutor {
    async fn run(&self, mut request: CommandRequest) -> Result<CommandOutput, String> {
        merge(&mut request.env, &self.environment);
        LocalRuntimeExecutor.run(request).await
    }
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
mod tests {
    use super::*;

    #[test]
    fn fixed_values_replace_ambient_values_without_duplicates() {
        let mut current = vec![
            ("PATH".to_string(), "/bin".to_string()),
            ("LAN_TASK_ID".to_string(), "wrong".to_string()),
        ];
        let fixed = BTreeMap::from([
            ("LAN_REGISTRY_DIR".to_string(), "/tmp/lan".to_string()),
            ("LAN_TASK_ID".to_string(), "task-1".to_string()),
        ]);

        merge(&mut current, &fixed);

        assert_eq!(
            current,
            vec![
                ("PATH".to_string(), "/bin".to_string()),
                ("LAN_REGISTRY_DIR".to_string(), "/tmp/lan".to_string()),
                ("LAN_TASK_ID".to_string(), "task-1".to_string()),
            ]
        );
    }
}
