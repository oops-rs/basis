//! The WebSocket bridge: ACP for clients that cannot spawn a process.
//!
//! ACP is stdio, and stdio assumes the client can start the agent. A browser
//! cannot. [acp-ui](https://github.com/formulahendry/acp-ui) is a real ACP
//! client that runs in a page; what it lacks is a pipe. This module is that
//! pipe and nothing more — a socket, a handshake, and the transport swap. lan
//! ships no web UI and never will: the client is adopted, not built
//! (ADR-0002, PROPOSAL.md Bet 2).
//!
//! # Shape
//!
//! One connection is one ACP conversation, exactly as one stdio process is.
//! [`transport`](self::transport) maps frames to lines, and
//! [`serve`](crate::acp::serve) takes it from there, so the server a browser
//! reaches is the same server an editor reaches — same handlers, same tests,
//! no second implementation to keep honest.
//!
//! Each connection gets its own session registry, again as a stdio process
//! would. Two browser tabs are two clients, not one; a tab that reconnects
//! reaches its conversation the way any client does, through `session/load`,
//! because mentra persists the agent behind it.
//!
//! # What this socket is worth to an attacker
//!
//! Everything the agent can do. It writes to the workspace, and where shell is
//! granted it runs commands as whoever started `lan`. There is no
//! authentication in ACP, so reachability *is* authorization here, and the two
//! ways to reach a socket are answered separately:
//!
//! - **Another machine** — refused by binding to loopback, which is the
//!   default and takes an explicit opt-in to leave.
//! - **A page in the user's browser** — refused by the `Origin` allowlist,
//!   which is empty by default. See [`origin`](self::origin) for why loopback
//!   alone does not cover this.
//!
//! Nothing here rate-limits or caps connections. A process that can open a
//! loopback socket is already running as this user and could simply run `lan`,
//! so a bound would cost a real client an unexplained refusal to deny an
//! attacker nothing.

mod origin;
mod transport;

use std::{
    io,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
};

use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::accept_hdr_async;

use crate::acp::{self, ServeConfig};

pub use transport::websocket_transport;

/// The port the bridge listens on unless told otherwise. Arbitrary, in the
/// registered range, and stable — a client is configured with a URL once.
pub const DEFAULT_PORT: u16 = 5260;

/// Where the bridge listens and who it will talk to.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// The address to listen on. Loopback unless `allow_non_loopback` says
    /// otherwise.
    pub bind: SocketAddr,
    /// Web origins allowed to connect, matched exactly. Empty means no page is
    /// served; native clients, which send no `Origin`, are unaffected.
    pub allowed_origins: Vec<String>,
    /// Says out loud that a bind address reachable beyond this machine is
    /// intended. The check exists because the mistake is silent otherwise.
    pub allow_non_loopback: bool,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            bind: SocketAddr::from((Ipv4Addr::LOCALHOST, DEFAULT_PORT)),
            allowed_origins: Vec::new(),
            allow_non_loopback: false,
        }
    }
}

impl BridgeConfig {
    pub fn new(bind: SocketAddr) -> Self {
        Self {
            bind,
            ..Self::default()
        }
    }

    /// Admits one web origin, e.g. `http://localhost:5173`.
    pub fn with_origin(mut self, origin: impl Into<String>) -> Self {
        self.allowed_origins.push(origin.into());
        self
    }

    pub fn with_origins(mut self, origins: impl IntoIterator<Item = String>) -> Self {
        self.allowed_origins.extend(origins);
        self
    }

    /// Accepts a bind address other than loopback. Read
    /// [the module docs](self) before reaching for this.
    pub fn allowing_non_loopback(mut self) -> Self {
        self.allow_non_loopback = true;
        self
    }
}

/// Why a bridge could not be opened or kept open.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error(
        "refusing to listen on {0}: a bridge reachable beyond this machine gives anyone who can \
         route to it an agent that writes to the workspace, and runs commands where shell is \
         granted. Bind to loopback, or say explicitly that this is what you want."
    )]
    NonLoopbackBind(SocketAddr),

    #[error("cannot listen on {address}: {source}")]
    Listen {
        address: SocketAddr,
        #[source]
        source: io::Error,
    },

    #[error("the bridge stopped accepting connections: {0}")]
    Accept(#[source] io::Error),

    #[error("the bridge has no address: {0}")]
    Unbound(#[source] io::Error),
}

/// A bound listener, before it starts serving.
///
/// Binding and serving are separate so a caller can learn the address first —
/// which matters when the port was left to the OS, as a test does, and when a
/// human needs the URL to paste into a client.
#[derive(Debug)]
pub struct Bridge {
    listener: TcpListener,
    allowed_origins: Arc<Vec<String>>,
}

impl Bridge {
    /// Takes the address, refusing a non-loopback bind that was not asked for.
    pub async fn bind(config: BridgeConfig) -> Result<Self, BridgeError> {
        if !config.allow_non_loopback && !config.bind.ip().is_loopback() {
            return Err(BridgeError::NonLoopbackBind(config.bind));
        }

        let listener =
            TcpListener::bind(config.bind)
                .await
                .map_err(|source| BridgeError::Listen {
                    address: config.bind,
                    source,
                })?;

        Ok(Self {
            listener,
            allowed_origins: Arc::new(config.allowed_origins),
        })
    }

    /// The address actually bound, which is what a caller that asked for port
    /// 0 needs and what a human needs to point a client at.
    pub fn local_addr(&self) -> Result<SocketAddr, BridgeError> {
        self.listener.local_addr().map_err(BridgeError::Unbound)
    }

    /// Serves every connection until the listener itself fails.
    ///
    /// Only a broken listener ends this. One client cannot: its connection is
    /// handled on its own task and its failures stay there — see
    /// [`serve_connection`].
    pub async fn serve(self, config: ServeConfig) -> Result<(), BridgeError> {
        loop {
            let (stream, _peer) = self.listener.accept().await.map_err(BridgeError::Accept)?;

            tokio::spawn(serve_connection(
                stream,
                Arc::clone(&self.allowed_origins),
                config.clone(),
            ));
        }
    }
}

/// Binds and serves in one call, for a caller that fixed the port itself.
pub async fn serve_websocket(bridge: BridgeConfig, config: ServeConfig) -> Result<(), BridgeError> {
    Bridge::bind(bridge).await?.serve(config).await
}

/// Serves one connection, start to finish.
///
/// Nothing leaves here. A listening socket meets clients that fail their
/// handshake, are refused outright, or vanish mid-message, and none of that is
/// the server's fault or the server's news: the party that needs to know is the
/// client, and it has already been told — a refused origin gets a 403 carrying
/// the reason, a broken connection gets closed. Letting any of it reach the
/// accept loop would let one bad client end every other conversation.
async fn serve_connection(
    stream: TcpStream,
    allowed_origins: Arc<Vec<String>>,
    config: ServeConfig,
) {
    let Ok(socket) = accept_hdr_async(stream, origin::origin_guard(allowed_origins)).await else {
        return;
    };

    // `serve` returns when the client disconnects, which is the normal end of
    // a connection; an error here would only describe how it ended.
    let _ = acp::serve(config, websocket_transport(socket)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    #[tokio::test]
    async fn a_non_loopback_bind_is_refused_unless_it_was_asked_for() {
        let address = SocketAddr::from(([0, 0, 0, 0], 0));

        let error = Bridge::bind(BridgeConfig::new(address))
            .await
            .expect_err("0.0.0.0 is every interface, which is not a default anyone chose");

        assert!(matches!(error, BridgeError::NonLoopbackBind(refused) if refused == address));
    }

    #[tokio::test]
    async fn loopback_binds_without_ceremony() {
        let bridge = Bridge::bind(BridgeConfig::new(SocketAddr::from((
            Ipv4Addr::LOCALHOST,
            0,
        ))))
        .await
        .expect("loopback needs no opt-in");

        assert!(bridge.local_addr().expect("bound").ip().is_loopback());
    }

    #[tokio::test]
    async fn ipv6_loopback_counts_as_loopback() {
        let bridge = Bridge::bind(BridgeConfig::new(SocketAddr::from((
            Ipv6Addr::LOCALHOST,
            0,
        ))))
        .await
        .expect("::1 is loopback too");

        assert!(bridge.local_addr().expect("bound").ip().is_loopback());
    }

    #[test]
    fn the_default_is_loopback_and_serves_no_page() {
        let config = BridgeConfig::default();

        assert!(config.bind.ip().is_loopback());
        assert_eq!(config.bind.port(), DEFAULT_PORT);
        assert!(
            config.allowed_origins.is_empty(),
            "a page reaches a loopback socket unasked; the allowlist is what stops it"
        );
        assert!(!config.allow_non_loopback);
    }

    #[test]
    fn origins_accumulate() {
        let config = BridgeConfig::default()
            .with_origin("http://localhost:5173")
            .with_origins(["https://acp.example".to_string()]);

        assert_eq!(
            config.allowed_origins,
            vec![
                "http://localhost:5173".to_string(),
                "https://acp.example".to_string()
            ]
        );
    }
}
