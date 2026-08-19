//! Serving the protocol: on stdio, and on a websocket.
//!
//! One module for both, because they are one server with a different pipe
//! under it. They build their [`ServeConfig`](basis_acp::ServeConfig) from the
//! same [`AcpArgs`] by the same steps, and a shared builder is easier to keep
//! honest when the two callers are visible from it — the bridge must not
//! quietly configure a session differently from the editor path.
//!
//! What genuinely differs is only what a person had to be told: the bridge
//! prints the address it bound, because with `--bind 127.0.0.1:0` that is the
//! only way to learn the port, and warns when no origin is allowed. That
//! difference reads best next to the thing it differs from.
//!
//! Neither returns a `Result` to `main`. A server that could not start has
//! already said why in the vocabulary of the half that failed, and wrapping
//! that in one more "basis: " would only make it longer.

use std::process::ExitCode;

use basis::{RunConfig, ShellAccess, provider};
use basis_acp::StdioError;
use mentra::ModelSelector;

use crate::{
    bridge,
    cli::{AcpArgs, BridgeArgs},
    exit::{EXIT_FAILED, EXIT_OK, EXIT_USAGE},
};

/// What `basis serve --acp` says when the first thing on stdin is not a message.
///
/// The explicit server still guards the same transport boundary: an editor and
/// a shell pipe can both send non-TTY stdin, so `cat prompt.txt | basis serve
/// --acp` cannot be treated as a prompt. What can be done is answer, rather
/// than wait silently, once the input proves it was never a client (ADR-0017).
pub(crate) const NOT_A_CLIENT: &str = "expected an ACP client on stdio";
const NOT_A_CLIENT_NEXT: &str =
    "next: use `basis spawn -` for a prompt or `basis serve --acp` for ACP";
const ACP_RETRY: &str =
    "next: retry with `basis serve --acp` after addressing the reported failure";
const BRIDGE_RETRY: &str =
    "next: retry with `basis serve --bridge` after addressing the reported failure";

pub(crate) async fn serve_acp(args: AcpArgs) -> ExitCode {
    let config = match acp_config(args) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("basis: {message}");
            eprintln!("{ACP_RETRY}");
            return ExitCode::from(EXIT_FAILED);
        }
    };

    match basis_acp::serve_stdio(config).await {
        Ok(()) => ExitCode::from(EXIT_OK),
        // Not an error the server had — an invocation that was never going to
        // work. Said in the vocabulary of the command line, because that is
        // where the fix is.
        Err(StdioError::NotAClient) => {
            eprintln!("basis: {NOT_A_CLIENT}");
            eprintln!("{NOT_A_CLIENT_NEXT}");
            ExitCode::from(EXIT_USAGE)
        }
        Err(error) => {
            eprintln!("basis: acp: {error}");
            eprintln!("{ACP_RETRY}");
            ExitCode::from(EXIT_FAILED)
        }
    }
}

/// Serves the ACP server on a websocket instead of stdio.
///
/// The bound address is printed before serving: with `--bind 127.0.0.1:0` it
/// is the only way to learn the port, and with any bind it is the URL a client
/// is configured with.
pub(crate) async fn serve_bridge(acp: AcpArgs, args: BridgeArgs) -> ExitCode {
    let config = match acp_config(acp) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("basis: {message}");
            eprintln!("{BRIDGE_RETRY}");
            return ExitCode::from(EXIT_FAILED);
        }
    };

    let bind = args
        .bind
        .unwrap_or_else(|| bridge::BridgeConfig::default().bind);
    let mut bridge = bridge::BridgeConfig::new(bind).with_origins(args.allow_origin);
    if args.allow_non_loopback {
        bridge = bridge.allowing_non_loopback();
    }
    let serves_no_page = bridge.allowed_origins.is_empty();

    let bridge = match bridge::Bridge::bind(bridge).await {
        Ok(bridge) => bridge,
        Err(error) => {
            eprintln!("basis: bridge: {error}");
            eprintln!("{BRIDGE_RETRY}");
            return ExitCode::from(EXIT_FAILED);
        }
    };

    match bridge.local_addr() {
        Ok(address) => eprintln!("basis: bridge listening on ws://{address}"),
        Err(error) => {
            eprintln!("basis: bridge: {error}");
            eprintln!("{BRIDGE_RETRY}");
            return ExitCode::from(EXIT_FAILED);
        }
    }

    // Said after the address, not before: it explains why a browser client
    // that is about to be pointed here will be turned away, and it would be
    // noise on a bind that never happened.
    if serves_no_page {
        eprintln!(
            "basis: bridge: no web origin allowed, so no page is served. \
             Pass --allow-origin <ORIGIN> for a browser client."
        );
    }

    match bridge.serve(config).await {
        Ok(()) => ExitCode::from(EXIT_OK),
        Err(error) => {
            eprintln!("basis: bridge: {error}");
            eprintln!("{BRIDGE_RETRY}");
            ExitCode::from(EXIT_FAILED)
        }
    }
}

/// Builds the template each ACP session is configured from.
///
/// The workspace is a placeholder: every session replaces it with the `cwd`
/// the client sends. It has to be *something* because `RunConfig` requires
/// one, and the current directory is the least surprising stand-in.
fn acp_config(args: AcpArgs) -> Result<basis_acp::ServeConfig, String> {
    let workspace =
        std::env::current_dir().map_err(|error| format!("no working directory: {error}"))?;

    let mut config = RunConfig::new(workspace, "").with_session_name("basis acp");

    if let Some(name) = &args.provider {
        config = config.with_provider(provider::parse(name).map_err(|error| error.to_string())?);
    }
    if let Some(base_url) = args.base_url {
        config = config.with_base_url(base_url);
    }
    if let Some(model) = args.model {
        config = config.with_model(ModelSelector::Id(model));
    }

    config = config.with_shell(ShellAccess::from_flag(!args.no_shell));

    if let Some(effort) = args.effort {
        config = config.with_effort(effort.into());
    }

    Ok(basis_acp::ServeConfig::new(config).with_initial_mode(args.approve.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_signpost_names_the_invocation_that_would_have_worked() {
        // A silent wait was the old failure. The message replaces it, and the
        // only part that matters is that it says what to type instead.
        assert!(
            NOT_A_CLIENT_NEXT.contains("basis spawn -"),
            "the signpost must name the fix: {NOT_A_CLIENT_NEXT}"
        );
    }
}
