use std::future::Future;

/// Waits for `connection` — in production, the server-side mux driver future (see `mux.rs`) — to
/// finish for any reason, then runs `on_disconnect`.
///
/// This deliberately does **not** read the process's stdin on its own to detect EOF: once the
/// mux driver owns stdin (wrapped for yamux via `tokio_util::compat`), it is the only thing
/// allowed to read from it — a second independent reader would race it and corrupt the yamux
/// frame stream. The mux driver already surfaces "the other side is gone" correctly (yamux's
/// `poll_next_inbound` returns `None` on clean EOF, `Some(Err(_))` on a read error), so watching
/// *that future's completion* is the correct and only signal this module needs.
pub async fn watch<Fut, Cleanup>(connection: Fut, on_disconnect: Cleanup) -> Fut::Output
where
    Fut: Future,
    Cleanup: FnOnce(),
{
    let result = connection.await;
    on_disconnect();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test]
    async fn on_disconnect_runs_only_after_the_connection_future_completes() {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let called = Arc::new(AtomicBool::new(false));

        let watcher = {
            let called = called.clone();
            tokio::spawn(async move {
                let _ = watch(rx, move || called.store(true, Ordering::SeqCst)).await;
            })
        };

        // Give the watcher a chance to run; it must still be waiting on `rx`.
        tokio::task::yield_now().await;
        assert!(
            !called.load(Ordering::SeqCst),
            "on_disconnect ran before the connection future resolved"
        );

        tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), watcher)
            .await
            .expect("watch() did not return after the connection future resolved")
            .unwrap();

        assert!(called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn on_disconnect_runs_even_if_the_connection_future_errors() {
        let called = Arc::new(AtomicBool::new(false));
        {
            let called = called.clone();
            let failing = async { Err::<(), &str>("connection died") };
            let _ = watch(failing, move || called.store(true, Ordering::SeqCst)).await;
        }
        assert!(called.load(Ordering::SeqCst));
    }
}
