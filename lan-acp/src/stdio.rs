//! Serving ACP on stdio, with a signpost where the trap used to be.
//!
//! Bare `lan` speaks JSON-RPC on stdin, and from this process's seat an editor
//! connecting looks exactly like a shell pipe: no arguments, stdin not a
//! terminal. Nothing distinguishes them, so lan does not guess (ADR-0015) —
//! prompt-from-stdin stays explicit, at `lan spawn -`.
//!
//! What is left is the one case that is certainly a mistake: a first line that
//! is not a message at all. Answering it costs a peek and turns an unexplained
//! silence into a sentence naming the fix.
//!
//! The peek is why this builds its own transport instead of using the SDK's
//! [`Stdio`](agent_client_protocol::Stdio): the line has to be read to be
//! judged, and then handed back so the server sees the stream whole.

use std::io;

use agent_client_protocol::{Lines, schema::v1::Error};
use futures::{AsyncBufReadExt, AsyncWriteExt, Sink, Stream, StreamExt};
use serde_json::Value;
use thiserror::Error as ThisError;

use crate::server::{ServeConfig, serve};

/// Why serving stdio ended.
#[derive(Debug, ThisError)]
pub enum StdioError {
    /// The first line was not a JSON-RPC message, so nothing was served.
    ///
    /// Its own variant rather than a message, because the caller is the one
    /// who knows what to suggest instead — the binary names `lan spawn -`, an
    /// embedder would name itself.
    #[error("the first line on stdin was not a JSON-RPC message")]
    NotAClient,

    #[error(transparent)]
    Protocol(#[from] Error),

    #[error("failed to read stdin: {0}")]
    Stdin(#[from] io::Error),
}

/// Serves ACP on stdin/stdout until the client disconnects.
///
/// This is what `lan` with no subcommand runs: the default mode, because
/// embedding is the primary case (ADR-0002, ADR-0003).
///
/// Nothing is written before the first line is read, and the first line is
/// handed to the server unchanged, so a client that speaks the protocol cannot
/// tell this from a transport that never looked.
pub async fn serve_stdio(config: ServeConfig) -> Result<(), StdioError> {
    let stdin = blocking::Unblock::new(io::stdin());
    let mut lines = Box::pin(futures::io::BufReader::new(stdin).lines());

    let (said, opening) = opening_lines(&mut lines).await?;
    if opening == Opening::NotAClient {
        return Err(StdioError::NotAClient);
    }

    let incoming = futures::stream::iter(said.into_iter().map(Ok)).chain(lines);
    serve(config, Lines::new(stdout_lines(), incoming)).await?;

    Ok(())
}

/// What the peer's opening proves about the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Opening {
    /// It sent something shaped like a message. Whether it is a *valid* one is
    /// the JSON-RPC layer's question, and it answers with a proper error.
    Client,
    /// It sent something that could not be a message under any reading.
    NotAClient,
    /// It sent nothing at all — an editor that spawned lan and closed the
    /// pipe, or a script piping an empty file. There is nothing to complain
    /// about, and serving an empty stream ends immediately anyway.
    Silence,
}

/// Everything the peer said up to and including its first real line, and what
/// that line proves.
///
/// The lines are returned rather than consumed: judging the opening must not
/// cost the server the message it was judging.
async fn opening_lines<S>(lines: &mut S) -> io::Result<(Vec<String>, Opening)>
where
    S: Stream<Item = io::Result<String>> + Unpin,
{
    let mut said = Vec::new();

    while let Some(line) = lines.next().await {
        let line = line?;
        // A blank line is neither a message nor prose, so it proves nothing.
        // It is passed through rather than dropped, so the JSON-RPC layer
        // keeps answering it the way it always has.
        let opening = (!line.trim().is_empty()).then(|| opening_for(&line));
        said.push(line);

        if let Some(opening) = opening {
            return Ok((said, opening));
        }
    }

    Ok((said, Opening::Silence))
}

/// Judges one line.
///
/// A JSON-RPC message is a JSON object, or an array of them in a batch — so
/// that, and nothing narrower, is the test. Requiring a `"jsonrpc"` member or
/// a known method would put lan in the business of policing a protocol the SDK
/// already validates, and the cost of being wrong here is refusing a real
/// client. Prose fails this on the first character.
fn opening_for(line: &str) -> Opening {
    match serde_json::from_str::<Value>(line) {
        Ok(Value::Object(_) | Value::Array(_)) => Opening::Client,
        _ => Opening::NotAClient,
    }
}

/// Stdout as a sink of lines, flushed one at a time.
///
/// Per line rather than per batch for the reason the bridge flushes per frame:
/// it is what makes an answer arrive as the model produces it.
fn stdout_lines() -> impl Sink<String, Error = io::Error> + Send + 'static {
    futures::sink::unfold(
        blocking::Unblock::new(io::stdout()),
        async move |mut out: blocking::Unblock<io::Stdout>, line: String| {
            let mut bytes = line.into_bytes();
            bytes.push(b'\n');
            out.write_all(&bytes).await?;
            out.flush().await?;
            Ok::<_, io::Error>(out)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn opening_of(lines: &[&str]) -> (Vec<String>, Opening) {
        let mut stream = futures::stream::iter(
            lines
                .iter()
                .map(|line| Ok((*line).to_string()))
                .collect::<Vec<_>>(),
        );

        opening_lines(&mut stream)
            .await
            .expect("an in-memory stream does not fail")
    }

    #[test]
    fn a_json_rpc_message_is_an_object_or_a_batch() {
        assert_eq!(
            opening_for(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#),
            Opening::Client
        );
        assert_eq!(
            opening_for(r#"[{"jsonrpc":"2.0","id":1}]"#),
            Opening::Client
        );
    }

    #[test]
    fn a_client_is_not_refused_over_a_field_lan_does_not_own() {
        // Version negotiation, method names, and the shape of `params` all
        // belong to the SDK, which answers a malformed one with a proper
        // JSON-RPC error. Judging them here would turn a protocol quibble into
        // a refusal to start.
        assert_eq!(opening_for(r#"{"method":"initialize"}"#), Opening::Client);
        assert_eq!(opening_for("{}"), Opening::Client);
    }

    #[test]
    fn prose_is_not_a_client() {
        for line in [
            "fix the failing test",
            "run the tests and summarize",
            "why is CI red?",
        ] {
            assert_eq!(opening_for(line), Opening::NotAClient, "{line}");
        }
    }

    #[tokio::test]
    async fn the_first_line_is_handed_to_the_server_unread() {
        // The point of the peek is that it costs the client nothing: the
        // message it opened with must still reach the dispatch loop.
        let message = r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;

        let (said, opening) = opening_of(&[message, r#"{"jsonrpc":"2.0","id":2}"#]).await;

        assert_eq!(opening, Opening::Client);
        assert_eq!(said, vec![message.to_string()]);
    }

    #[tokio::test]
    async fn blank_lines_before_a_message_are_kept_and_skipped_over() {
        let message = r#"{"jsonrpc":"2.0","id":1}"#;

        let (said, opening) = opening_of(&["", "   ", message]).await;

        assert_eq!(opening, Opening::Client);
        assert_eq!(
            said,
            vec!["".to_string(), "   ".to_string(), message.to_string()],
            "the layer that answered blank lines before must still see them"
        );
    }

    #[tokio::test]
    async fn a_peer_that_says_nothing_is_not_accused_of_anything() {
        let (said, opening) = opening_of(&[]).await;

        assert_eq!(opening, Opening::Silence);
        assert!(said.is_empty());
    }

    #[tokio::test]
    async fn prose_on_the_first_line_stops_before_the_server_starts() {
        let (said, opening) = opening_of(&["fix the failing test", "and push"]).await;

        assert_eq!(opening, Opening::NotAClient);
        assert_eq!(said, vec!["fix the failing test".to_string()]);
    }
}
