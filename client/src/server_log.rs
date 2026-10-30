use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// Marks every line the spawned server writes to its stderr as the server's, so both processes'
/// diagnostics can share the journal and still be told apart.
pub const PREFIX: &str = "server: ";

/// Copies the spawned server's stderr into `out` line by line, each line prefixed with [`PREFIX`],
/// until the server closes it. The line itself is passed through as the server wrote it — its own
/// `tracing` setup already formatted it — apart from a trailing `\r`, which Windows processes may
/// end lines with.
pub async fn forward(server_stderr: impl AsyncRead + Unpin, mut out: impl AsyncWrite + Unpin) {
    let mut server_stderr = BufReader::new(server_stderr);
    let mut raw = Vec::new();
    loop {
        raw.clear();
        match server_stderr.read_until(b'\n', &mut raw).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let line = String::from_utf8_lossy(&raw);
                let line = line.strip_suffix('\n').unwrap_or(&line);
                let line = line.strip_suffix('\r').unwrap_or(line);
                // One write per line, flushed, so a line reaches the journal whole and can't
                // interleave with this process's own output.
                let _ = out.write_all(format!("{PREFIX}{line}\n").as_bytes()).await;
                let _ = out.flush().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prefixes_each_server_line() {
        let mut out = Vec::new();
        forward(&b"first\r\nsecond\n\xffthird"[..], &mut out).await;
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "server: first\nserver: second\nserver: \u{fffd}third\n"
        );
    }
}
