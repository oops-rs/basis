//! The process/workspace options shared by durable and attended CLI runs.

use basis::{ModelSelector, RuntimeBuilder, ShellAccess, WorkspaceBuilder, provider};

/// Applies the CLI-owned process and workspace selections to basis's concrete
/// builders.
///
/// The durable path records these values before applying them; the attended
/// path applies them immediately. System prompt and turn bounds stay on their
/// respective run-spec paths because those two routes intentionally carry
/// them in different concrete types.
pub fn configure_builders(
    mut runtime: RuntimeBuilder,
    mut workspace: WorkspaceBuilder,
    provider_name: Option<&str>,
    base_url: Option<&str>,
    model: Option<&str>,
    shell: ShellAccess,
) -> Result<(RuntimeBuilder, WorkspaceBuilder), provider::ProviderError> {
    if let Some(name) = provider_name {
        runtime = runtime.with_provider(provider::parse(name)?);
    }
    if let Some(base_url) = base_url {
        runtime = runtime.with_base_url(base_url);
    }

    workspace = workspace.with_shell(shell);
    if let Some(model) = model {
        workspace = workspace.with_model(ModelSelector::Id(model.to_string()));
    }

    Ok((runtime, workspace))
}
