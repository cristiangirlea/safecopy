//! QUIC helpers, behind the `quic` feature (off by default).
//!
//! A bundle needs nothing of these: quinn's streams are tokio's
//! [`AsyncRead`] and [`AsyncWrite`], so [`BundleWriter`] and
//! [`BundleReader`] take them as they are. These carry a program's answer
//! the QUIC way, by stopping the stream with a code: the receiver stops a
//! bundle it refuses ([`stop`]), and the sender reads the code back
//! ([`stopped`]). The codes are the program's.
//!
//! [`AsyncRead`]: tokio::io::AsyncRead
//! [`AsyncWrite`]: tokio::io::AsyncWrite
//! [`BundleWriter`]: crate::BundleWriter
//! [`BundleReader`]: crate::BundleReader

pub use quinn;

/// Asks the sender to stop sending on `stream` with `code`. `false` when
/// the stream had already ended or been stopped.
pub fn stop(stream: &mut quinn::RecvStream, code: u32) -> bool {
    stream.stop(quinn::VarInt::from_u32(code)).is_ok()
}

/// Waits until the receiver stops `stream` or has read all of it: the code
/// it stopped it with, or `None` when it read it all, the connection was
/// lost, or the code does not fit in 32 bits.
pub async fn stopped(stream: &quinn::SendStream) -> Option<u32> {
    match stream.stopped().await {
        Ok(Some(code)) => u32::try_from(code.into_inner()).ok(),
        _ => None,
    }
}
