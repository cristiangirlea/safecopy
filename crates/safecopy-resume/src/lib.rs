//! Resuming an interrupted transfer.
//!
//! A file cut off by a lost connection keeps what it received and
//! continues from its last whole chunk; on any doubt it starts again from
//! zero, never keeping a wrong byte.
//!
//! - **Resume records** ([`Store`]): the receiver keeps, for a file of at
//!   least one whole chunk ([`CHUNK`]), where its partial file is (found
//!   again by identity, as `safecopy-journal` finds it), `have` (whole
//!   chunks, flushed to the disk before they are recorded) and `prefix`
//!   (the SHA-256 of those bytes); the sender keeps which file it was
//!   sending. One file per transfer, two checksummed slots, so a torn
//!   write leaves the other slot whole. Records are bounded in count and
//!   bytes and expire, never while their session runs.
//! - **The checks** ([`receiver_takes`], [`sender_checks`], [`Resent`]):
//!   what the receiver states it holds, what the sender checks of its own
//!   file, and what a stream under a kept key may be.
//! - **The handshake** ([`handshake`]): the exchange that decides where
//!   each file continues, over one bidirectional stream, for a program
//!   with no control channel of its own.
//!
//! What both sides must agree on is fixed: the wire types' encoding
//! ([`TransferKey`], [`Resumable`], [`Held`], [`Verdict`]), held by
//! golden-bytes tests, and the chunk. Everything else is a setting
//! ([`Limits`]).
//!
//! Records hold no file contents and are not sealed: whoever can write the
//! records' folder runs as the person. A tampered receiving record can
//! only make a file start from zero, since every byte is checked against
//! the whole-file hash.

use std::fmt;
use std::io;
use std::time::Duration;

pub use safecopy_chunks::{CHUNK, Digest};

mod checks;
pub mod handshake;
mod record;
mod space;
mod store;
mod wire;

pub use checks::{Next, Resent, Source, Take, receiver_takes, sender_checks};
pub use record::{Receiving, Sending};
pub use store::{Expired, Store};

/// A transfer's key: 16 random bytes the sender chose for a file the first
/// time it goes. It names the file to the receiver's record and grants
/// nothing by itself: a record is only looked up with its [`PeerId`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TransferKey(pub [u8; 16]);

impl fmt::Debug for TransferKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TransferKey(")?;
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        f.write_str(")")
    }
}

impl TransferKey {
    /// A new key from the system's secure random source.
    pub fn random() -> Result<Self, ResumeError> {
        use ring::rand::SecureRandom;
        let mut key = [0; 16];
        ring::rand::SystemRandom::new()
            .fill(&mut key)
            .map_err(|_| ResumeError::Random)?;
        Ok(Self(key))
    }
}

/// The other side, as the program proved who it is: opaque here.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerId(pub [u8; 32]);

impl fmt::Debug for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PeerId(")?;
        for byte in &self.0[..4] {
            write!(f, "{byte:02x}")?;
        }
        f.write_str("..)")
    }
}

/// What a resumable stream says first, after the framing version: which
/// file, where its bytes start and how many there are in all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resumable {
    /// The file's transfer key.
    pub key: TransferKey,
    /// The first byte the stream carries: 0 the first time, a kept `have`
    /// on a resume.
    pub from: u64,
    /// The file's whole size.
    pub size: u64,
}

/// What the receiver keeps of one file from a peer: `have` bytes, a whole
/// number of chunks, whose SHA-256 is `prefix`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Held {
    /// The file's transfer key.
    pub key: TransferKey,
    /// The bytes kept.
    pub have: u64,
    /// The SHA-256 of those bytes.
    pub prefix: Digest,
}

/// What one side tells the other of a file, which each program maps to its
/// own codes and words.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verdict {
    /// Its hash was not the trailer's: sent once more from zero.
    Damaged,
    /// The receiver doubts what it holds: sent once more from zero.
    Restart,
    /// Not taken: a stream that does not match, or one too many.
    Refused,
    /// The sender will not continue it: its record is gone, its file
    /// changed, or the person stopped it. The receiver removes it.
    Dropped,
    /// An identical file is already there.
    AlreadyThere,
    /// Paused by a person: nothing removed.
    Paused,
}

/// How many resume records a store keeps and for how long. `Default`
/// gives the values below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most files kept from one peer: 64. Past it, that peer's oldest
    /// goes. Also the most [`Held`] a list may carry.
    pub per_peer: usize,
    /// The most files kept in all: 256. Past it, the oldest of the peer
    /// keeping the most goes.
    pub total: usize,
    /// The share of a disk kept files may hold, of its free space and
    /// what they hold, as a numerator and a denominator: a quarter.
    pub disk_share: (u64, u64),
    /// How long a file is kept after its last byte: 24 hours.
    pub keep_for: Duration,
    /// A receiving record is rewritten once at least this many more bytes
    /// are whole: 64 MiB.
    pub rewrite_bytes: u64,
    /// And at least this long after the last write: 1 second.
    pub rewrite_every: Duration,
    /// The most bytes of the caller's `same` a record keeps: 4 KiB.
    pub same_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            per_peer: 64,
            total: 256,
            disk_share: (1, 4),
            keep_for: Duration::from_secs(24 * 60 * 60),
            rewrite_bytes: 64 << 20,
            rewrite_every: Duration::from_secs(1),
            same_bytes: 4096,
        }
    }
}

/// Why a resume step failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResumeError {
    /// `have` or `from` past the file's size.
    PastSize,
    /// `have` or `from` not a whole number of chunks, or less than one
    /// chunk kept.
    OffChunk,
    /// No record of this key for this peer.
    NotHeld,
    /// The partial file is shorter than its `have`.
    WrongSize,
    /// The partial file or its folder is not there any more, or another
    /// file has its name.
    Gone,
    /// The caller's `same` is longer than [`Limits::same_bytes`], or a
    /// record would not fit its slot.
    TooLarge {
        /// The limit it passed.
        most: usize,
    },
    /// Bytes from the other side, or a record, that do not read.
    Damaged,
    /// A list longer than [`Limits::per_peer`].
    TooMany {
        /// The limit it passed.
        most: usize,
    },
    /// The system's random source failed.
    Random,
    /// The journal crate refused.
    Journal(safecopy_journal::JournalError),
    /// The system's error.
    Io(io::ErrorKind),
}

impl fmt::Display for ResumeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The caller words it; this is the kind for logs and tests.
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for ResumeError {}

impl From<io::Error> for ResumeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.kind())
    }
}

impl From<safecopy_chunks::ChunkError> for ResumeError {
    fn from(error: safecopy_chunks::ChunkError) -> Self {
        match error {
            safecopy_chunks::ChunkError::CutShort { .. } => Self::WrongSize,
            safecopy_chunks::ChunkError::Io(kind) => Self::Io(kind),
            _ => Self::Damaged,
        }
    }
}

impl From<safecopy_journal::JournalError> for ResumeError {
    fn from(error: safecopy_journal::JournalError) -> Self {
        Self::Journal(error)
    }
}

#[cfg(test)]
mod tests;
