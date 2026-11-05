//! The built server against a real named pipe, driven over its stdio the way the client drives it.
#![cfg(windows)]

use std::future::poll_fn;
use std::process::{ExitStatus, Stdio};
use std::task::Poll;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::ServerOptions;
use tokio::process::{Child, Command};
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

fn unique_pipe_path(name: &str) -> String {
    format!(
        r"\\.\pipe\sockrelay-server-test-{name}-{}",
        std::process::id()
    )
}

fn spawn_server(pipe_path: &str) -> Child {
    Command::new(env!("CARGO_BIN_EXE_sockrelay-server"))
        .args(["--pipe-path", pipe_path])
        .args(["--connect-timeout-secs", "5", "--jitter-max-ms", "10"])
        .args([librelay::LOG_FILTER_FLAG, "warn"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

async fn exit_status(server: &mut Child) -> ExitStatus {
    tokio::time::timeout(Duration::from_secs(10), server.wait())
        .await
        .expect("the server didn't exit")
        .unwrap()
}

/// An agent on `path` that echoes what it reads, for one connection.
fn spawn_echo_agent(path: &str) {
    let mut agent = ServerOptions::new().create(path).unwrap();
    tokio::spawn(async move {
        agent.connect().await.unwrap();
        let mut request = [0; 4];
        agent.read_exact(&mut request).await.unwrap();
        agent.write_all(&request).await.unwrap();
        std::future::pending::<()>().await;
    });
}

/// Opens one stream over the server's stdio and keeps the connection moving until `close` is
/// sent, then drops it, as a client that is gone does. The client in librelay only builds on unix.
fn spawn_client(
    server: &mut Child,
) -> (
    tokio::sync::oneshot::Receiver<yamux::Stream>,
    tokio::sync::oneshot::Sender<()>,
) {
    let io = tokio::io::join(server.stdout.take().unwrap(), server.stdin.take().unwrap());
    let mut connection =
        yamux::Connection::new(io.compat(), yamux::Config::default(), yamux::Mode::Client);
    let (stream_tx, stream_rx) = tokio::sync::oneshot::channel();
    let (close_tx, close_rx) = tokio::sync::oneshot::channel::<()>();
    let mut stream_tx = Some(stream_tx);
    tokio::spawn(async move {
        let driven = poll_fn(|cx| {
            if let Some(reply) = stream_tx.take() {
                match connection.poll_new_outbound(cx) {
                    Poll::Ready(stream) => {
                        let _ = reply.send(stream.unwrap());
                    }
                    Poll::Pending => stream_tx = Some(reply),
                }
            }
            while let Poll::Ready(Some(Ok(_))) = connection.poll_next_inbound(cx) {}
            Poll::<()>::Pending
        });
        tokio::select! {
            () = driven => {}
            _ = close_rx => {}
        }
    });
    (stream_rx, close_tx)
}

#[tokio::test]
async fn a_missing_pipe_fails_the_start() {
    let mut server = spawn_server(&unique_pipe_path("missing"));
    assert_eq!(exit_status(&mut server).await.code(), Some(1));
}

#[tokio::test]
async fn a_stream_reaches_the_agent_and_the_server_exits_once_the_client_is_gone() {
    let path = unique_pipe_path("relay");
    spawn_echo_agent(&path);
    let mut server = spawn_server(&path);
    let (stream, close) = spawn_client(&mut server);

    let mut stream = stream.await.unwrap().compat();
    stream.write_all(b"ping").await.unwrap();
    let mut answer = [0; 4];
    tokio::time::timeout(Duration::from_secs(10), stream.read_exact(&mut answer))
        .await
        .expect("the agent's answer never came back")
        .unwrap();
    assert_eq!(&answer, b"ping");

    drop(close);
    assert_eq!(exit_status(&mut server).await.code(), Some(0));
}

#[tokio::test]
async fn a_client_speaking_something_other_than_yamux_fails_the_server() {
    let path = unique_pipe_path("garbage");
    let _agent = ServerOptions::new().create(&path).unwrap();
    let mut server = spawn_server(&path);
    let mut stdin = server.stdin.take().unwrap();
    stdin.write_all(b"not a yamux frame").await.unwrap();
    drop(stdin);
    assert_eq!(exit_status(&mut server).await.code(), Some(1));
}
