//! The private, local control protocol used by lifecycle commands.
//!
//! This is deliberately a binary-side adapter. `lan-core` remains transport
//! free; a host embedding the SDK can choose a different control plane. The
//! protocol is a length-delimited JSON envelope over a loopback TCP stream.
//! The bounded envelope is decoded first; the bearer token is checked before
//! an operation is dispatched or can mutate state.

use std::io;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(crate) const VERSION: u8 = 1;
pub(crate) const MAX_FRAME: usize = 8 * 1024 * 1024;
pub(crate) const MAX_PROMPT: usize = 256 * 1024;
pub(crate) const MAX_MESSAGE: usize = 256 * 1024;

/// One request per connection is enough to keep the server simple: handlers
/// are spawned independently, so a long `wait --await` cannot prevent a
/// separate connection from cancelling the task.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Request {
    pub version: u8,
    pub id: u64,
    pub token: String,
    #[serde(flatten)]
    pub operation: Operation,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Operation {
    Spawn {
        workspace: String,
        prompt: String,
        parent: Option<String>,
        detached: bool,
        await_result: bool,
        timeout_ms: Option<u64>,
        options: RunOptions,
    },
    Send {
        task: String,
        message: String,
        caller: Option<String>,
        await_result: bool,
        timeout_ms: Option<u64>,
    },
    Wait {
        task: String,
        caller: Option<String>,
        timeout_ms: u64,
    },
    Cancel {
        task: String,
    },
    Watch {
        task: String,
        since: u64,
        timeout_ms: u64,
    },
    Inbox {
        task: String,
    },
}

/// Per-run values that are safe to serialize into a local request. Credentials
/// are intentionally absent: the daemon owns its startup environment, just as
/// another long-lived host does, and the descriptor never contains a key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct RunOptions {
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub no_shell: bool,
    pub effort: Option<String>,
    pub approve: String,
    pub deadline_ms: Option<u64>,
    pub tool_budget: Option<usize>,
    pub token_budget: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Response {
    pub version: u8,
    pub id: u64,
    pub kind: ResponseKind,
    pub payload: Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResponseKind {
    Ok,
    Error,
}

pub(crate) fn ok(id: u64, payload: Value) -> Response {
    Response {
        version: VERSION,
        id,
        kind: ResponseKind::Ok,
        payload,
    }
}

pub(crate) fn error(id: u64, message: impl Into<String>) -> Response {
    Response {
        version: VERSION,
        id,
        kind: ResponseKind::Error,
        payload: serde_json::json!({"error": message.into()}),
    }
}

/// Reads one bounded JSON frame. Checking the length before allocating is the
/// first and most important resource boundary of the local service.
pub(crate) async fn read_frame<R, T>(reader: &mut R) -> io::Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let length = reader.read_u32().await? as usize;
    if length == 0 || length > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {length} is outside 1..={MAX_FRAME}"),
        ));
    }

    let mut bytes = vec![0_u8; length];
    reader.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid JSON frame: {error}"),
        )
    })
}

/// Writes one bounded JSON frame and flushes it before returning. A response
/// is never acknowledged before the complete frame is on the socket.
pub(crate) async fn write_frame<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(value).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("could not encode JSON: {error}"),
        )
    })?;
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "encoded frame length {} is outside 1..={MAX_FRAME}",
                bytes.len()
            ),
        ));
    }

    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn frames_survive_fragmentation_and_coalescing() {
        let (mut left, mut right) = duplex(1024);
        let first = serde_json::json!({"n": 1});
        let second = serde_json::json!({"n": 2});
        let expected_first = first.clone();
        let expected_second = second.clone();

        let writer = tokio::spawn(async move {
            write_frame(&mut left, &first).await.expect("first frame");
            write_frame(&mut left, &second).await.expect("second frame");
        });

        let a: Value = read_frame(&mut right).await.expect("first read");
        let b: Value = read_frame(&mut right).await.expect("second read");
        writer.await.expect("writer joins");

        assert_eq!(a, expected_first);
        assert_eq!(b, expected_second);
    }

    #[tokio::test]
    async fn zero_and_oversized_frames_are_rejected_before_allocation() {
        for length in [0_u32, (MAX_FRAME as u32).saturating_add(1)] {
            let (mut left, mut right) = duplex(64);
            let writer = tokio::spawn(async move {
                left.write_u32(length).await.expect("length");
            });
            let result: io::Result<Value> = read_frame(&mut right).await;
            writer.await.expect("writer joins");
            assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::InvalidData));
        }
    }

    #[tokio::test]
    async fn truncated_frames_are_errors() {
        let (mut left, mut right) = duplex(64);
        let writer = tokio::spawn(async move {
            left.write_u32(5).await.expect("length");
            left.write_all(b"{}").await.expect("partial body");
        });
        let result: io::Result<Value> = read_frame(&mut right).await;
        writer.await.expect("writer joins");
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::UnexpectedEof));
    }
}
