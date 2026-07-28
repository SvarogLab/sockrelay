use tokio::io::{copy_bidirectional, AsyncRead, AsyncWrite};

/// Relays bytes between `a` and `b` in both directions until either side closes — the byte-pump
/// for unix-socket↔yamux-stream relaying. A thin wrapper over `tokio::io::copy_bidirectional`,
/// which already handles half-close/shutdown propagation correctly for transports that support a
/// real half-close (unlike named pipes — see `server/src/pipe.rs`, which hand-rolls its own relay
/// for exactly that reason instead of using this one; that asymmetry is what left this function
/// with only one consumer, and is why it lives here rather than in a shared crate).
pub async fn relay<A, B>(a: &mut A, b: &mut B) -> std::io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    copy_bidirectional(a, b).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn relay_pumps_bytes_in_both_directions_and_reports_byte_counts() {
        // `left` stands in for e.g. a client's unix-socket connection, `right` for e.g. its
        // yamux stream — `relay` doesn't know or care which is which.
        let (mut left_outside, mut left_inside) = tokio::io::duplex(1024);
        let (mut right_inside, mut right_outside) = tokio::io::duplex(1024);

        let relay_task = tokio::spawn(async move { relay(&mut left_inside, &mut right_inside).await });

        left_outside.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        right_outside.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        right_outside.write_all(b"pong!").await.unwrap();
        let mut buf2 = [0u8; 5];
        left_outside.read_exact(&mut buf2).await.unwrap();
        assert_eq!(&buf2, b"pong!");

        // Closing one side should let `copy_bidirectional` finish and report how much it moved
        // in each direction.
        drop(left_outside);
        drop(right_outside);
        let (left_to_right, right_to_left) =
            tokio::time::timeout(Duration::from_secs(5), relay_task)
                .await
                .expect("relay() did not finish after both outside ends closed")
                .unwrap()
                .unwrap();
        assert_eq!(left_to_right, 4);
        assert_eq!(right_to_left, 5);
    }
}
