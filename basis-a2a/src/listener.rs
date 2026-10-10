//! The listener [`A2aServer`](crate::A2aServer) serves on: a TCP listener
//! that bounds what a connection can hold before it has asked for anything.
//!
//! The request [`gate`](crate::gate) runs once a request's head has arrived.
//! Before that, a connection costs a socket and a task, and a client that
//! opens many and sends nothing — or sends a head one byte a minute — would
//! hold them for good. So:
//!
//! - at most [`MAX_CONNECTIONS`] are open at once; past that the listener
//!   stops accepting, and new ones wait in the kernel's backlog;
//! - a connection must deliver its first request head within
//!   [`FIRST_REQUEST_TIMEOUT`] of being accepted, or it is closed. The clock
//!   stops at the server's first write — a response begun — so a long
//!   blocking send or an open stream is never cut.

use std::{
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore},
    time::Sleep,
};

/// The most connections open at once.
pub(crate) const MAX_CONNECTIONS: usize = 512;

/// How long a new connection has to deliver its first request.
pub(crate) const FIRST_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct BoundedListener {
    inner: TcpListener,
    connections: Arc<Semaphore>,
}

impl BoundedListener {
    pub(crate) fn new(inner: TcpListener) -> Self {
        Self {
            inner,
            connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        }
    }
}

impl axum::serve::Listener for BoundedListener {
    type Io = Bounded;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let place = Arc::clone(&self.connections)
            .acquire_owned()
            .await
            .expect("the semaphore is never closed");
        loop {
            match self.inner.accept().await {
                Ok((stream, address)) => {
                    return (
                        Bounded {
                            stream,
                            _place: place,
                            first_request: Some(Box::pin(tokio::time::sleep(
                                FIRST_REQUEST_TIMEOUT,
                            ))),
                        },
                        address,
                    );
                }
                // A failed accept is the one connection's problem (it reset,
                // or the process is out of descriptors for a moment); pause
                // briefly rather than spin, as axum's own listener does.
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// One accepted connection, holding its place under the cap.
pub(crate) struct Bounded {
    stream: TcpStream,
    _place: OwnedSemaphorePermit,
    /// Armed until the server first writes.
    first_request: Option<Pin<Box<Sleep>>>,
}

impl AsyncRead for Bounded {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        if let Some(deadline) = this.first_request.as_mut()
            && deadline.as_mut().poll(cx).is_ready()
        {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no request arrived in time",
            )));
        }
        Pin::new(&mut this.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for Bounded {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.first_request = None;
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.first_request = None;
        Pin::new(&mut self.stream).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use axum::serve::Listener;

    #[tokio::test(start_paused = true)]
    async fn a_connection_that_never_asks_is_closed() {
        let inner = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = inner.local_addr().unwrap();
        let mut listener = BoundedListener::new(inner);
        let _client = TcpStream::connect(address).await.unwrap();
        let (mut accepted, _) = listener.accept().await;
        let mut buffer = [0_u8; 8];
        let read = tokio::time::timeout(
            FIRST_REQUEST_TIMEOUT + Duration::from_secs(1),
            accepted.read(&mut buffer),
        )
        .await
        .expect("the read ends");
        assert_eq!(read.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test(start_paused = true)]
    async fn the_clock_stops_once_the_server_answers() {
        let inner = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = inner.local_addr().unwrap();
        let mut listener = BoundedListener::new(inner);
        let mut client = TcpStream::connect(address).await.unwrap();
        let (mut accepted, _) = listener.accept().await;
        accepted.write_all(b"HTTP/1.1 200 OK\r\n").await.unwrap();
        tokio::time::advance(FIRST_REQUEST_TIMEOUT * 2).await;
        client.write_all(b"x").await.unwrap();
        let mut buffer = [0_u8; 1];
        accepted.read_exact(&mut buffer).await.unwrap();
        assert_eq!(&buffer, b"x");
    }
}
