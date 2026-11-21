//! Verified copying of one file.
//!
//! Copies exactly the declared number of bytes over any async byte stream
//! (tokio's [`AsyncRead`] and [`AsyncWrite`]), hashes them with SHA-256 on
//! both sides, carries the whole-file hash after the last byte (the
//! trailer), and verifies the file before it gets its name.
//!
//! - [`Hasher`]: SHA-256 over a whole file, keeping the hash state at the
//!   last few chunk boundaries, so the hash of a prefix is at hand after a
//!   cut ([`Hasher::at`], [`Hasher::rewound`]); [`hash_prefix`] and
//!   [`Hasher::read`] read it from the disk when not.
//! - [`copy_exact`]: the bytes from `from` to `size`, in slices of at most
//!   [`SLICE`] bytes, each written only once the caller's [`Pace`] lets it.
//! - [`write_trailer`] and [`read_trailer`]: the 32-byte hash after the
//!   bytes; [`read_end`] checks nothing follows.
//! - [`save_verified`]: a whole file into a held folder: write, hash,
//!   compare, mark, time and execute bit, publish; the partial file is
//!   removed after any failure.
//! - [`CandidateHash::hash`]: the hash of a file already at the
//!   destination, checked unchanged before and after.
//!
//! What both sides must agree on is in the framing: [`FRAMING`], the
//! chunk size ([`CHUNK`]), the trailer. Everything else is a setting
//! ([`Settings`]).
//!
//! [`AsyncRead`]: tokio::io::AsyncRead
//! [`AsyncWrite`]: tokio::io::AsyncWrite

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::io;
use std::num::NonZeroU64;
use std::pin::Pin;

use ring::digest::{Context, SHA256};

mod candidate;
mod copy;
mod save;

pub use candidate::{CandidateError, CandidateHash};
pub use copy::{
    FRAMING, copy_exact, hash_prefix, read_end, read_meta, read_trailer, read_version, write_meta,
    write_trailer, write_version,
};
pub use save::{Hooks, SaveOptions, Saved, save_verified};

/// The chunk: a resumed file continues from a multiple of it. 4 MiB.
pub const CHUNK: u64 = 4 << 20;

/// The most bytes [`copy_exact`] writes at once: 64 KiB.
pub const SLICE: usize = 64 << 10;

/// The length of a [`Digest`] and of the trailer.
pub const DIGEST_LEN: usize = 32;

/// A SHA-256 hash.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest(pub [u8; DIGEST_LEN]);

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Digest(")?;
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        f.write_str(")")
    }
}

fn digest(context: Context) -> Digest {
    let mut bytes = [0; DIGEST_LEN];
    bytes.copy_from_slice(context.finish().as_ref());
    Digest(bytes)
}

/// The settings both [`save_verified`] and a sender use. The defaults are
/// the ones the framing was designed with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// How many chunk boundaries a receiver's [`Hasher`] keeps: its last
    /// whole chunk is what it can say it has after a cut.
    pub receiver_keep: usize,
    /// How many a sender's keeps: it may be ahead of the receiver by what
    /// was in flight, about two chunks; four leave a margin.
    pub sender_keep: usize,
    /// The most bytes of the caller's metadata block [`read_meta`] takes.
    pub meta_bytes: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            receiver_keep: 1,
            sender_keep: 4,
            meta_bytes: 4096,
        }
    }
}

impl Settings {
    /// A [`Hasher`] for the receiving side.
    pub fn receiving(&self) -> Hasher {
        Hasher::new(self.receiver_keep)
    }

    /// A [`Hasher`] for the sending side.
    pub fn sending(&self) -> Hasher {
        Hasher::new(self.sender_keep)
    }
}

/// SHA-256 over a whole file, keeping the state at its last chunk
/// boundaries.
///
/// A SHA-256 state cannot be rewound, but it can be copied: the hasher
/// copies it at every chunk boundary it passes and keeps the last `keep`
/// copies. [`Self::at`] finishes the copy taken at a boundary; the hash of
/// any older prefix has to be read again from the disk ([`hash_prefix`]).
#[derive(Clone)]
pub struct Hasher {
    context: Context,
    position: u64,
    chunk: u64,
    keep: usize,
    kept: VecDeque<(u64, Context)>,
}

impl fmt::Debug for Hasher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hasher")
            .field("position", &self.position)
            .field("chunk", &self.chunk)
            .field("kept", &self.kept.iter().map(|k| k.0).collect::<Vec<_>>())
            .finish()
    }
}

impl Hasher {
    /// A hasher at byte 0 keeping the last `keep` boundaries of [`CHUNK`].
    pub fn new(keep: usize) -> Self {
        Self::with_chunk(keep, NonZeroU64::new(CHUNK).expect("not zero"))
    }

    /// A hasher at byte 0 keeping the last `keep` boundaries of `chunk`.
    /// For tests and fuzzing only, which reach boundaries with less data:
    /// the chunk is a constant of the framing, never a setting.
    #[doc(hidden)]
    pub fn with_chunk(keep: usize, chunk: NonZeroU64) -> Self {
        Self {
            context: Context::new(&SHA256),
            position: 0,
            chunk: chunk.get(),
            keep,
            kept: VecDeque::with_capacity(keep.min(16)),
        }
    }

    /// How many bytes it has hashed.
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Hashes the next bytes of the file.
    pub fn update(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let to_boundary = self.chunk - self.position % self.chunk;
            let take = usize::try_from(to_boundary).map_or(bytes.len(), |b| b.min(bytes.len()));
            self.context.update(&bytes[..take]);
            self.position += take as u64;
            bytes = &bytes[take..];
            if self.position.is_multiple_of(self.chunk) && self.keep > 0 {
                if self.kept.len() == self.keep {
                    self.kept.pop_front();
                }
                self.kept.push_back((self.position, self.context.clone()));
            }
        }
    }

    /// The hash of the first `have` bytes, when `have` is 0 or a boundary
    /// still kept. `None` otherwise: off a boundary, past what was hashed,
    /// or older than the kept boundaries; hash the prefix from the disk.
    pub fn at(&self, have: u64) -> Option<Digest> {
        if have == 0 {
            return Some(digest(Context::new(&SHA256)));
        }
        let (_, context) = self.kept.iter().find(|(at, _)| *at == have)?;
        Some(digest(context.clone()))
    }

    /// A hasher standing at `have`, to continue a cut file from there:
    /// from the state kept at that boundary, keeping the boundaries up to
    /// it. `None` when [`Self::at`] would be; then start a new hasher and
    /// [`Self::read`] the prefix from the disk.
    pub fn rewound(&self, have: u64) -> Option<Self> {
        let context = if have == 0 {
            Context::new(&SHA256)
        } else {
            self.kept.iter().find(|(at, _)| *at == have)?.1.clone()
        };
        Some(Self {
            context,
            position: have,
            chunk: self.chunk,
            keep: self.keep,
            kept: self
                .kept
                .iter()
                .filter(|(at, _)| *at <= have)
                .cloned()
                .collect(),
        })
    }

    /// The hash of everything hashed.
    pub fn finish(self) -> Digest {
        digest(self.context)
    }
}

/// How many bytes have moved, for the caller's progress.
pub trait Progress: Send + Sync {
    /// `bytes` more were written (or read, when hashing).
    fn advanced(&self, bytes: u64);
}

/// No progress kept.
impl Progress for () {
    fn advanced(&self, _: u64) {}
}

/// The hook a program uses to hold its file bytes back: behind other
/// traffic, or for a pause. [`copy_exact`] awaits
/// [`Pace::before_write`] before each slice it writes, and holds the copy
/// until it returns. This crate never sets a stream's priority itself.
pub trait Pace: Send + Sync {
    /// Called before `len` bytes are written; the write waits for it. The
    /// default goes on at once.
    fn before_write(&self, len: usize) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let _ = len;
        Box::pin(std::future::ready(()))
    }
}

/// No pacing: every slice goes at once.
impl Pace for () {}

/// Why a copy, a read of the framing or a save failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChunkError {
    /// The stream ended after `got` of the `size` bytes it should carry:
    /// a file's bytes, the version byte or the metadata block.
    CutShort {
        /// The bytes that came.
        got: u64,
        /// The bytes that should have.
        size: u64,
    },
    /// The stream ended after the file's bytes, with `got` of the
    /// trailer's 32.
    TrailerCutShort {
        /// The bytes of the trailer that came.
        got: u64,
    },
    /// More came after the trailer.
    TooLong,
    /// The bytes' hash is not the trailer's.
    Damaged,
    /// The caller's cancel finished first.
    Cancelled,
    /// A framing version this crate does not read.
    Version {
        /// The version byte that came.
        got: u8,
    },
    /// A metadata block longer than [`Settings::meta_bytes`].
    MetaTooLong {
        /// Its declared length.
        len: u64,
        /// The most taken.
        most: u64,
    },
    /// `from` past `size`, or a [`Hasher`] not at `from`.
    Offset {
        /// Where the copy was asked to start.
        from: u64,
        /// The file's size.
        size: u64,
        /// Where the hasher stood.
        hashed: u64,
    },
    /// One of the caller's [`Hooks`] refused, with its error's kind.
    Hook(io::ErrorKind),
    /// The system refused.
    Io(io::ErrorKind),
}

impl fmt::Display for ChunkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The caller words it; this is the kind for logs and tests.
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for ChunkError {}

impl From<io::Error> for ChunkError {
    fn from(e: io::Error) -> Self {
        Self::Io(e.kind())
    }
}

#[cfg(test)]
mod tests;
