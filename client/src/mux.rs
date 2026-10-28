use std::future::{Future, poll_fn};
use std::task::{Context, Poll};

use tokio_util::compat::TokioAsyncReadCompatExt;

/// Byte-for-byte identical to `server::mux::MuxError` — this used to live in a shared
/// `sockrelay-common` crate, but that crate's only other content (a relay helper) turned out to
/// be single-consumer once `server/src/pipe.rs` grew its own hand-rolled relay, so a whole extra
/// crate for one 9-line enum stopped being worth it; duplicating it here is cheaper to maintain
/// than the crate was.
#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    #[error("yamux connection error: {0}")]
    Connection(#[from] yamux::ConnectionError),
    #[error("mux driver has shut down")]
    Closed,
}

/// A request to open a new outbound yamux stream, answered by the driver loop.
struct OpenRequest {
    reply: tokio::sync::oneshot::Sender<Result<yamux::Stream, MuxError>>,
}

/// A cloneable handle used by any number of tasks to open outbound streams over the single
/// shared mux connection. The actual `yamux::Connection` is owned and driven exclusively by the
/// future returned alongside this handle from [`client`] — this handle only ever talks to it
/// over a channel.
#[derive(Clone)]
pub struct MuxHandle {
    open_tx: tokio::sync::mpsc::Sender<OpenRequest>,
}

impl MuxHandle {
    /// Opens a new outbound stream multiplexed over the shared connection. Resolves once the
    /// driver loop has actually created the stream (which may briefly block on yamux's own
    /// outbound-stream ACK backlog).
    pub async fn open_stream(&self) -> Result<yamux::Stream, MuxError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.open_tx
            .send(OpenRequest { reply: reply_tx })
            .await
            .map_err(|_| MuxError::Closed)?;
        reply_rx.await.map_err(|_| MuxError::Closed)?
    }
}

/// Builds a client-mode mux over `io` (the joined stdio of the spawned server process, or any
/// other duplex transport — see the tests below, which use `tokio::io::duplex`). Returns a
/// handle for opening streams plus the driver future, which the caller must poll to completion
/// (e.g. via `tokio::spawn`) for the connection to make any progress at all — this isn't
/// optional background bookkeeping, it's what actually reads/writes the underlying transport.
pub fn client<T>(io: T) -> (MuxHandle, impl Future<Output = Result<(), MuxError>>)
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let connection =
        yamux::Connection::new(io.compat(), yamux::Config::default(), yamux::Mode::Client);
    let (open_tx, open_rx) = tokio::sync::mpsc::channel(32);
    let mut driver = Driver {
        connection,
        open_rx,
        pending_reply: None,
    };
    (
        MuxHandle { open_tx },
        poll_fn(move |cx| driver.poll_drive(cx)),
    )
}

struct Driver<T> {
    connection: yamux::Connection<T>,
    open_rx: tokio::sync::mpsc::Receiver<OpenRequest>,
    pending_reply: Option<tokio::sync::oneshot::Sender<Result<yamux::Stream, MuxError>>>,
}

impl<T> Driver<T>
where
    T: futures::AsyncRead + futures::AsyncWrite + Unpin,
{
    /// Drives the connection: services outbound-open requests and, whether or not there are any,
    /// keeps polling `poll_next_inbound` since that's the only thing that flushes queued frames
    /// and processes incoming ones (yamux 0.14 has no separate driving-loop primitive of its
    /// own — driving the connection *is* what makes progress).
    fn poll_drive(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), MuxError>> {
        loop {
            if let Some(reply) = self.pending_reply.take() {
                match self.connection.poll_new_outbound(cx) {
                    Poll::Ready(result) => {
                        let _ = reply.send(result.map_err(MuxError::Connection));
                        continue;
                    }
                    Poll::Pending => self.pending_reply = Some(reply),
                }
            } else {
                match self.open_rx.poll_recv(cx) {
                    Poll::Ready(Some(request)) => {
                        self.pending_reply = Some(request.reply);
                        continue;
                    }
                    Poll::Ready(None) => {
                        // Every `MuxHandle` was dropped: nothing can request new streams anymore,
                        // so close the connection gracefully instead of driving it forever.
                        return self.connection.poll_close(cx).map_err(MuxError::Connection);
                    }
                    Poll::Pending => {}
                }
            }

            return match self.connection.poll_next_inbound(cx) {
                Poll::Ready(Some(Ok(_stream))) => {
                    // The client side of this design never expects the server to open streams
                    // back to it; drop it and keep driving.
                    tracing::warn!("dropping unexpected inbound yamux stream on client mux");
                    continue;
                }
                Poll::Ready(Some(Err(e))) => Poll::Ready(Err(MuxError::Connection(e))),
                Poll::Ready(None) => Poll::Ready(Err(MuxError::Closed)),
                Poll::Pending => Poll::Pending,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::io::{AsyncReadExt, AsyncWriteExt};
    use std::future::Future as StdFuture;
    use std::pin::Pin;

    /// Drives a bare `Mode::Server` yamux connection in the background, echoing back whatever
    /// bytes it reads on each inbound stream. Stands in for the not-yet-written server-side
    /// `mux.rs` (task #5) so this test can exercise a real two-sided yamux exchange.
    fn spawn_echo_server<T>(io: T) -> tokio::task::JoinHandle<()>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        tokio::spawn(async move {
            let mut connection =
                yamux::Connection::new(io.compat(), yamux::Config::default(), yamux::Mode::Server);
            loop {
                let next = std::future::poll_fn(|cx| connection.poll_next_inbound(cx)).await;
                match next {
                    Some(Ok(mut stream)) => {
                        tokio::spawn(async move {
                            let mut buf = Vec::new();
                            if stream.read_to_end(&mut buf).await.is_ok() {
                                let _ = stream.write_all(&buf).await;
                                let _ = stream.close().await;
                            }
                        });
                    }
                    Some(Err(_)) | None => break,
                }
            }
        })
    }

    fn spawn_driver(
        driver: impl StdFuture<Output = Result<(), MuxError>> + Send + 'static,
    ) -> tokio::task::JoinHandle<Result<(), MuxError>> {
        tokio::spawn(Box::pin(driver) as Pin<Box<dyn StdFuture<Output = _> + Send>>)
    }

    #[tokio::test]
    async fn open_stream_relays_data_end_to_end() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (handle, driver) = client(client_io);
        spawn_driver(driver);
        spawn_echo_server(server_io);

        let mut stream = handle.open_stream().await.unwrap();
        stream.write_all(b"hello, agent").await.unwrap();
        stream.close().await.unwrap();

        let mut echoed = Vec::new();
        stream.read_to_end(&mut echoed).await.unwrap();
        assert_eq!(echoed, b"hello, agent");
    }

    #[tokio::test]
    async fn multiple_concurrent_streams_are_independent() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (handle, driver) = client(client_io);
        spawn_driver(driver);
        spawn_echo_server(server_io);

        let mut tasks = Vec::new();
        for i in 0..8u32 {
            let handle = handle.clone();
            tasks.push(tokio::spawn(async move {
                let payload = format!("stream-{i}").into_bytes();
                let mut stream = handle.open_stream().await.unwrap();
                stream.write_all(&payload).await.unwrap();
                stream.close().await.unwrap();

                let mut echoed = Vec::new();
                stream.read_to_end(&mut echoed).await.unwrap();
                assert_eq!(echoed, payload);
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn driver_shuts_down_once_all_handles_are_dropped() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (handle, driver) = client(client_io);
        spawn_echo_server(server_io);

        drop(handle);

        tokio::time::timeout(std::time::Duration::from_secs(5), driver)
            .await
            .expect("driver did not shut down after handles were dropped")
            .unwrap();
    }
}
