//! One JSON-RPC message per WebSocket frame.
//!
//! ACP over stdio is newline-delimited JSON, and the SDK's [`Lines`] transport
//! is that framing made explicit: a `Sink<String>` going out, a
//! `Stream<String>` coming in. A WebSocket already delivers messages whole, so
//! the bridge is a mapping between two framings that agree — one frame is one
//! line — and nothing above it changes.
//!
//! That agreement is why the bridge calls [`serve`](lan_acp::serve)
//! directly instead of spawning a `lan` subprocess and copying bytes between
//! its pipes and a socket. A subprocess would buy nothing: the server is
//! already generic over its transport, so the tested server is the same server,
//! one layer lower. It would cost a process per connection, a second place for
//! configuration to be passed, and a class of failure — the child died — that
//! has no meaning to the client.

use std::io;

use agent_client_protocol::Lines;
use futures::{SinkExt, StreamExt, future, stream::SplitSink};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Error as WsError, Message},
};

/// Wraps an accepted WebSocket as an ACP transport.
///
/// Public because the handshake is not lan's to own: a host already running an
/// HTTP server has done the upgrade itself and holds the
/// [`WebSocketStream`] this takes. Handing it here is the whole integration —
/// pass the result to [`serve`](lan_acp::serve).
pub fn websocket_transport<S>(
    socket: WebSocketStream<S>,
) -> Lines<
    impl futures::Sink<String, Error = io::Error> + Send + 'static,
    impl futures::Stream<Item = io::Result<String>> + Send + 'static,
>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (sink, stream) = socket.split();

    // `send` rather than `feed`: each message is flushed before the next is
    // written, which is what keeps an answer arriving as the model produces it
    // instead of in one batch when the turn ends. Stdio framing makes the same
    // promise, so a client cannot tell the two transports apart by timing.
    let outgoing = futures::sink::unfold(
        sink,
        async move |mut sink: SplitSink<WebSocketStream<S>, Message>, line: String| {
            sink.send(Message::text(line)).await.map_err(as_io)?;
            Ok::<_, io::Error>(sink)
        },
    );

    let incoming = stream.filter_map(|frame| future::ready(line_from(frame)));

    Lines::new(outgoing, incoming)
}

/// One frame as the line the JSON-RPC layer reads — or nothing, when the frame
/// carries no message.
///
/// Text is the framing that matters: ACP messages are JSON and browsers send
/// text. A binary frame is decoded anyway, because a client that sends its
/// JSON as bytes is not wrong and refusing it would present as an unexplained
/// silence. Ping and pong never carry a message — tungstenite has already
/// answered the ping by the time this sees it — and a close frame only means
/// the stream is about to end on its own.
fn line_from(frame: Result<Message, WsError>) -> Option<io::Result<String>> {
    match frame {
        Ok(Message::Text(text)) => carries_a_message(text.as_str()),
        Ok(Message::Binary(bytes)) => match std::str::from_utf8(&bytes) {
            Ok(text) => carries_a_message(text),
            Err(_) => Some(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a binary frame that is not UTF-8 cannot be a JSON-RPC message",
            ))),
        },
        Ok(Message::Ping(_) | Message::Pong(_) | Message::Close(_) | Message::Frame(_)) => None,
        Err(error) => Some(Err(as_io(error))),
    }
}

/// Drops a blank frame instead of parsing it. A client using an empty frame as
/// a keepalive would otherwise be answered with a JSON-RPC parse error for it,
/// which is a confusing reply to a message it never meant to send.
fn carries_a_message(line: &str) -> Option<io::Result<String>> {
    (!line.trim().is_empty()).then(|| Ok(line.to_string()))
}

/// A websocket failure ends the connection either way, so what the layer above
/// needs is an `io::Error` it can report — not the protocol detail of how.
fn as_io(error: WsError) -> io::Error {
    match error {
        WsError::Io(error) => error,
        other => io::Error::other(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(frame: Message) -> Option<String> {
        line_from(Ok(frame)).map(|result| result.expect("a well-formed frame is not an error"))
    }

    #[test]
    fn a_text_frame_is_one_line() {
        assert_eq!(
            line(Message::text(r#"{"jsonrpc":"2.0","id":1}"#)),
            Some(r#"{"jsonrpc":"2.0","id":1}"#.to_string())
        );
    }

    #[test]
    fn a_binary_frame_of_utf8_is_read_rather_than_refused() {
        assert_eq!(
            line(Message::binary(br#"{"jsonrpc":"2.0"}"#.to_vec())),
            Some(r#"{"jsonrpc":"2.0"}"#.to_string()),
            "a client that sends its JSON as bytes is still speaking ACP"
        );
    }

    #[test]
    fn a_binary_frame_that_is_not_text_is_an_error() {
        let refused = line_from(Ok(Message::binary(vec![0xff, 0xfe])))
            .expect("the frame is reported, not dropped")
            .expect_err("bytes that are not UTF-8 cannot be a message");

        assert_eq!(refused.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn control_frames_carry_no_message() {
        for frame in [
            Message::Ping(Vec::new().into()),
            Message::Pong(Vec::new().into()),
            Message::Close(None),
        ] {
            assert!(
                line_from(Ok(frame.clone())).is_none(),
                "{frame:?} is protocol traffic, not a JSON-RPC message"
            );
        }
    }

    #[test]
    fn a_blank_frame_is_dropped_rather_than_answered_with_a_parse_error() {
        assert!(line(Message::text("")).is_none());
        assert!(line(Message::text("  \n\t ")).is_none());
    }

    #[test]
    fn a_transport_failure_reaches_the_reader() {
        let failed = line_from(Err(WsError::Io(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "gone",
        ))))
        .expect("a failure is reported")
        .expect_err("a failure is an error");

        assert_eq!(
            failed.kind(),
            io::ErrorKind::ConnectionReset,
            "an io failure keeps its kind rather than being flattened"
        );
    }
}
