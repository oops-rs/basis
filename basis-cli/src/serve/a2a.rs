//! `basis serve --a2a`: this workspace's agent, served to other agents.
//!
//! Everything a caller cannot say is said here, by the operator, once: the
//! workspace, the card, the bounds, the token. What a caller *can* say is a
//! message, and `basis-a2a` turns each into one durable task (ADR-0028).

use std::{net::SocketAddr, path::Path, process::ExitCode};

use basis_a2a::{
    A2aServer, A2aService, AgentIdentity, BearerToken, InboundApproval, RunDefaults, ServeConfig,
};
use basis_host::ApprovalPolicy;

use crate::{
    cli::{self, A2aArgs, AcpArgs},
    exit::{EXIT_FAILED, EXIT_OK},
};

const A2A_RETRY: &str =
    "next: retry with `basis serve --a2a <ADDR>` after addressing the reported failure";

pub(crate) async fn serve_a2a(acp: AcpArgs, args: A2aArgs, bind: SocketAddr) -> ExitCode {
    let config = match a2a_config(acp, args) {
        Ok(config) => config,
        Err(message) => return failed(&message),
    };
    let service = match A2aService::open(config) {
        Ok(service) => service,
        Err(error) => return failed(&format!("a2a: {error}")),
    };
    let server = match A2aServer::bind(bind, service).await {
        Ok(server) => server,
        Err(error) => return failed(&format!("a2a: {error}")),
    };
    eprintln!("basis: a2a listening on {}", server.endpoint());
    eprintln!(
        "basis: a2a card at http://{}{}",
        server.local_addr(),
        basis_a2a::AGENT_CARD_PATH
    );
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    match server.serve_until(shutdown).await {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(error) => failed(&format!("a2a: {error}")),
    }
}

fn failed(message: &str) -> ExitCode {
    eprintln!("basis: {message}");
    eprintln!("{A2A_RETRY}");
    ExitCode::from(EXIT_FAILED)
}

/// The served agent, from the flags. `--approve prompt` is refused here,
/// before anything listens: an A2A caller has nobody behind it to ask.
fn a2a_config(acp: AcpArgs, args: A2aArgs) -> Result<ServeConfig, String> {
    let approval = match acp.approve {
        None | Some(ApprovalPolicy::Never) => InboundApproval::Never,
        Some(ApprovalPolicy::Always) => InboundApproval::Always,
        Some(ApprovalPolicy::Prompt) => {
            return Err(
                "--approve prompt cannot serve A2A: the caller is another agent, with nobody \
                 behind it to answer; use never or always"
                    .to_string(),
            );
        }
    };
    let workspace = match args.workspace {
        Some(workspace) => workspace,
        None => std::env::current_dir()
            .map_err(|error| format!("read the current directory: {error}"))?,
    };
    let name = args.name.unwrap_or_else(|| directory_name(&workspace));
    let description = args
        .description
        .unwrap_or_else(|| format!("A basis agent working in {name}."));

    let mut run = RunDefaults::default();
    if let Some(provider) = acp.provider {
        run = run.with_provider(provider);
    }
    if let Some(base_url) = acp.base_url {
        run = run.with_base_url(base_url);
    }
    if let Some(model) = acp.model {
        run = run.with_model(model);
    }
    if let Some(effort) = acp.effort {
        run = run.with_effort(effort.into());
    }
    if let Some(system_prompt) = cli::system_prompt(acp.system_prompt, acp.append_system_prompt) {
        run = run.with_system_prompt(system_prompt);
    }
    if let Some(deadline) = args.deadline {
        run = run.with_deadline(deadline.duration());
    }
    if let Some(tool_budget) = args.tool_budget {
        run = run.with_tool_budget(tool_budget);
    }
    if let Some(token_budget) = args.token_budget {
        run = run.with_token_budget(token_budget);
    }

    let mut config = ServeConfig::new(workspace, AgentIdentity::new(name, description))
        .with_approval(approval)
        .with_run_defaults(run);
    if args.shell {
        config = config.with_shell();
    }
    if let Some(variable) = args.token_env {
        config = config.with_token(token_from(&variable)?);
    }
    if let Some(public_url) = args.public_url {
        config = config.with_public_url(public_url);
    }
    Ok(config)
}

/// The token in `variable`. Named by variable rather than passed as a flag
/// so it never sits in a process listing or a shell history.
fn token_from(variable: &str) -> Result<BearerToken, String> {
    let value = std::env::var(variable)
        .map_err(|_| format!("--token-env names {variable}, which is not set"))?;
    BearerToken::new(value).ok_or_else(|| format!("{variable} is empty; a token must be set"))
}

fn directory_name(workspace: &Path) -> String {
    workspace
        .canonicalize()
        .ok()
        .as_deref()
        .unwrap_or(workspace)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "basis".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_is_refused_before_anything_listens() {
        let acp = AcpArgs {
            approve: Some(ApprovalPolicy::Prompt),
            ..AcpArgs::default()
        };
        let error = a2a_config(acp, A2aArgs::default()).unwrap_err();
        assert!(error.contains("nobody"), "{error}");
    }

    #[test]
    fn an_unset_token_variable_is_named_and_its_value_never_is() {
        let error = token_from("BASIS_A2A_TEST_TOKEN_THAT_IS_NOT_SET").unwrap_err();
        assert!(
            error.contains("BASIS_A2A_TEST_TOKEN_THAT_IS_NOT_SET"),
            "{error}"
        );
    }

    #[test]
    fn the_default_name_is_the_workspace_directorys() {
        let dir = tempfile::tempdir().unwrap();
        let named = dir.path().join("my-repo");
        std::fs::create_dir(&named).unwrap();
        assert_eq!(directory_name(&named), "my-repo");
    }
}
