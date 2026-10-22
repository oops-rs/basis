//! `basis serve --a2a`: this workspace's agent, served to other agents.
//!
//! Everything a caller cannot say is said here, by the operator, once: the
//! workspace, the card, the bounds, who may call and what each is shown.
//! What a caller *can* say is a message, and `basis-a2a` turns each into one
//! durable task (ADR-0028, ADR-0030).

use std::{net::SocketAddr, path::Path, process::ExitCode, time::Duration};

use basis_a2a::{
    A2aServer, A2aService, AgentIdentity, BearerToken, ExtendedCard, InboundApproval, Principal,
    RunDefaults, ServeConfig, a2a::AgentSkill,
};
use basis_host::ApprovalPolicy;
use serde::Deserialize;

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
    eprintln!("basis: a2a HTTP+JSON at {}", server.http_json_url());
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

/// How long a question to the caller stays open when `--ask-deadline` does
/// not say: long enough for a caller to relay it to its own person, short
/// enough that a caller that went away does not hold a turn for good.
const DEFAULT_ASK_DEADLINE: Duration = Duration::from_secs(10 * 60);

/// The served agent, from the flags. `--approve prompt` asks the caller
/// (ADR-0030 §7): the one party behind an A2A request is whoever sent it.
fn a2a_config(acp: AcpArgs, args: A2aArgs) -> Result<ServeConfig, String> {
    let approval = match acp.approve {
        None | Some(ApprovalPolicy::Never) => InboundApproval::Never,
        Some(ApprovalPolicy::Always) => InboundApproval::Always,
        Some(ApprovalPolicy::Prompt) => InboundApproval::AskCaller {
            deadline: args
                .ask_deadline
                .map_or(DEFAULT_ASK_DEADLINE, |deadline| deadline.duration()),
        },
    };
    if args.ask_deadline.is_some() && !matches!(approval, InboundApproval::AskCaller { .. }) {
        return Err("--ask-deadline bounds questions to the caller; add --approve prompt".into());
    }
    let workspace = match args.workspace {
        Some(workspace) => workspace,
        None => std::env::current_dir()
            .map_err(|error| format!("read the current directory: {error}"))?,
    };
    let name = args.name.unwrap_or_else(|| directory_name(&workspace));
    let description = args
        .description
        .unwrap_or_else(|| format!("A basis agent working in {name}."));

    // Every task reads its workspace's and the user's `config.json`, as
    // `basis spawn` does; the SDK underneath reads none unless told to.
    let mut run = RunDefaults::default().with_config(basis::ConfigLookup::Discover);
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
    for principal in &args.principals {
        let (principal, token) = principal_from(principal)?;
        config = config.with_principal(principal, token);
    }
    if args.push {
        config = config.with_push_notifications();
    }
    if args.allow_private_webhooks {
        config = config.with_private_webhooks();
    }
    if let Some(path) = &args.extended_card {
        config = config.with_extended_card(extended_card_from(path)?);
    }
    if let Some(hop_cap) = args.hop_cap {
        config = config.with_hop_cap(hop_cap);
    }
    if args.trust_anonymous_callers {
        config = config.with_trusted_anonymous_callers();
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

/// A `--principal NAME=VAR`: the name, and the token read from `VAR`.
fn principal_from(argument: &str) -> Result<(Principal, BearerToken), String> {
    let (name, variable) = argument
        .split_once('=')
        .filter(|(name, variable)| !name.is_empty() && !variable.is_empty())
        .ok_or_else(|| format!("--principal takes NAME=VAR, not {argument:?}"))?;
    let principal = Principal::new(name).ok_or_else(|| {
        format!("principal name {name:?} must be 1-64 letters, digits, '-', '_' or '.'")
    })?;
    let value = std::env::var(variable)
        .map_err(|_| format!("--principal {name} names {variable}, which is not set"))?;
    let token = BearerToken::new(value)
        .ok_or_else(|| format!("{variable} is empty; principal {name} needs a token"))?;
    Ok((principal, token))
}

/// What `--extended-card FILE` holds.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtendedCardFile {
    description: Option<String>,
    #[serde(default)]
    skills: Vec<AgentSkill>,
}

fn extended_card_from(path: &Path) -> Result<ExtendedCard, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("read --extended-card {}: {error}", path.display()))?;
    let file: ExtendedCardFile = serde_json::from_slice(&bytes)
        .map_err(|error| format!("--extended-card {}: {error}", path.display()))?;
    let mut card = ExtendedCard::new();
    if let Some(description) = file.description {
        card = card.with_description(description);
    }
    Ok(file
        .skills
        .into_iter()
        .fold(card, |card, skill| card.with_skill(skill)))
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
    fn prompt_asks_the_caller_and_the_deadline_needs_it() {
        let acp = AcpArgs {
            approve: Some(ApprovalPolicy::Prompt),
            ..AcpArgs::default()
        };
        assert!(a2a_config(acp, A2aArgs::default()).is_ok());

        let args = A2aArgs {
            ask_deadline: Some("5m".parse().unwrap()),
            ..A2aArgs::default()
        };
        let error = a2a_config(AcpArgs::default(), args).unwrap_err();
        assert!(error.contains("--approve prompt"), "{error}");
    }

    #[test]
    fn a_principal_is_name_equals_variable_and_its_token_never_prints() {
        for bad in ["alice", "=VAR", "alice=", "a b=VAR"] {
            assert!(principal_from(bad).is_err(), "{bad}");
        }
        let error = principal_from("alice=BASIS_A2A_TEST_PRINCIPAL_THAT_IS_NOT_SET").unwrap_err();
        assert!(
            error.contains("BASIS_A2A_TEST_PRINCIPAL_THAT_IS_NOT_SET"),
            "{error}"
        );
    }

    #[test]
    fn an_extended_card_file_is_a_description_and_skills() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("card.json");
        std::fs::write(
            &path,
            r#"{"description": "More.", "skills": [{"id": "audit", "name": "Audit",
                "description": "Audits.", "tags": ["security"]}]}"#,
        )
        .unwrap();
        assert!(extended_card_from(&path).is_ok());
        std::fs::write(&path, r#"{"descriptoin": "typo"}"#).unwrap();
        assert!(
            extended_card_from(&path).is_err(),
            "unknown fields are refused"
        );
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
