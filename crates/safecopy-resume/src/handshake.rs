//! The resume exchange over one bidirectional stream, for a program with no
//! control channel of its own.
//!
//! The receiver ([`answer`]) writes the framing version and what it holds
//! from this peer: a count and that many [`Held`]. The sender ([`offer`])
//! reads them, checks each against its own records ([`crate::sender_checks`])
//! and writes one reply per [`Held`], in order: `0` and the offset to
//! continue from, or `1` and a [`Verdict`]. Each side reads only what it
//! needs; a count past [`Limits::per_peer`], a reply that does not read, or
//! a continue from anywhere but the receiver's `have` is refused, and so
//! is a list naming one key twice.

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::wire::{read_array, read_varint, varint, write};
use crate::{Held, Limits, Next, ResumeError, Verdict};

/// The version of this exchange, its first byte.
pub const VERSION: u8 = 1;

const CONTINUE: u8 = 0;
const STOP: u8 = 1;

/// What the sender answered for one file the receiver holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// The sender continues it from `from`, the receiver's `have`.
    Continue {
        /// Where its bytes start.
        from: u64,
    },
    /// It does not continue: [`Verdict::Restart`] (it comes again from
    /// zero) or [`Verdict::Dropped`] (the receiver removes it).
    Stop(Verdict),
}

impl Reply {
    /// The reply for a sender's [`Next`].
    pub fn of(next: Next) -> Self {
        match next {
            Next::Continue { from } => Reply::Continue { from },
            Next::Restart => Reply::Stop(Verdict::Restart),
            Next::Dropped | Next::Changed => Reply::Stop(Verdict::Dropped),
        }
    }

    /// Its bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Reply::Continue { from } => {
                out.push(CONTINUE);
                varint(&mut out, *from);
            }
            Reply::Stop(verdict) => {
                out.push(STOP);
                out.push(verdict.code());
            }
        }
        out
    }
}

/// The receiver's side: states what it holds from this peer (as
/// [`crate::Store::held_from`] lists it), then reads the sender's reply
/// for each, in order.
pub async fn answer<S>(
    stream: &mut S,
    held: &[Held],
    limits: &Limits,
) -> Result<Vec<(Held, Reply)>, ResumeError>
where
    S: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    if held.len() > limits.per_peer {
        return Err(ResumeError::TooMany {
            most: limits.per_peer,
        });
    }
    let mut out = vec![VERSION];
    varint(&mut out, held.len() as u64);
    for one in held {
        out.extend_from_slice(&one.encode());
    }
    write(stream, &out).await?;
    stream.flush().await?;
    let mut replies = Vec::with_capacity(held.len());
    for one in held {
        let [tag] = read_array::<_, 1>(stream).await?;
        let reply = match tag {
            CONTINUE => {
                let from = read_varint(stream, u64::BITS).await?;
                // Only from what it holds: anything else would keep a wrong byte.
                if from != one.have {
                    return Err(ResumeError::Damaged);
                }
                Reply::Continue { from }
            }
            STOP => {
                let [code] = read_array::<_, 1>(stream).await?;
                match Verdict::from_code(code) {
                    Some(v @ (Verdict::Restart | Verdict::Dropped)) => Reply::Stop(v),
                    _ => return Err(ResumeError::Damaged),
                }
            }
            _ => return Err(ResumeError::Damaged),
        };
        replies.push((*one, reply));
    }
    Ok(replies)
}

/// The sender's side: reads what the receiver holds, checks each with
/// `check` (which runs [`crate::sender_checks`] against its records) and
/// writes the replies, in order. Returns each [`Held`] with its reply.
pub async fn offer<S, F>(
    stream: &mut S,
    limits: &Limits,
    mut check: F,
) -> Result<Vec<(Held, Reply)>, ResumeError>
where
    S: AsyncRead + AsyncWrite + Unpin + ?Sized,
    F: AsyncFnMut(&Held) -> Next,
{
    let [version] = read_array::<_, 1>(stream).await?;
    if version != VERSION {
        return Err(ResumeError::Damaged);
    }
    let count = read_varint(stream, u64::BITS).await?;
    if count > limits.per_peer as u64 {
        return Err(ResumeError::TooMany {
            most: limits.per_peer,
        });
    }
    let mut held: Vec<Held> = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let one = Held::read_from(stream).await?;
        // One file listed twice would be hashed twice for nothing.
        if held.iter().any(|h| h.key == one.key) {
            return Err(ResumeError::Damaged);
        }
        held.push(one);
    }
    let mut out = Vec::with_capacity(held.len());
    let mut bytes = Vec::new();
    for one in held {
        let mut reply = Reply::of(check(&one).await);
        // A continue is only ever from what the receiver holds.
        if matches!(reply, Reply::Continue { from } if from != one.have) {
            reply = Reply::Stop(Verdict::Restart);
        }
        bytes.extend_from_slice(&reply.encode());
        out.push((one, reply));
    }
    write(stream, &bytes).await?;
    stream.flush().await?;
    Ok(out)
}
