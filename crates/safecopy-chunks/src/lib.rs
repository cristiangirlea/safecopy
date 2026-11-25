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
//!   removed after any failure, and [`Hooks::ended`] hears what became of
//!   it ([`PartialFate`]). How it is published is a [`Publish`]: the
//!   mark, the retries and the names tried, and no file held open.
//! - [`save_resumable`]: the same save from a new partial file or from one
//!   kept before ([`Start`]), which keeps the partial file, cut to its last
//!   whole chunk, when it stops on an error the caller's rule names
//!   ([`Stopped`], [`Kept`]); [`Hooks::whole`] hears of each whole chunk.
//! - [`CandidateHash::hash`]: the hash of a file already at the
//!   destination, checked unchanged before and after.
//!
//! **What a matching hash proves.** The hash compared with the trailer is
//! of the bytes as they came off the stream and were handed to the system
//! to write; a save never reads its own file back, before or after it
//! gets its name. So a match says the stream was whole and unchanged, and
//! that those bytes were written: not what a disk that fails without an
//! error holds, nor what another program wrote into the partial file
//! while it had no name. For a resumed file the hash covers the kept
//! bytes through the hasher handed in ([`Kept::hasher`]): the caller
//! answers for it being the hash of what the partial file holds at that
//! moment. `safecopy-resume` gives one read from the disk through the
//! very handle the save then writes; with it, a resumed file's kept
//! bytes are proven as read at that reopening, and the rest as written.
//! Bytes changed on the disk after they were hashed are not seen again
//! in either case.
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
use safecopy_folder::Refusal;

mod candidate;
mod copy;
// What the fuzz target runs: for the fuzzer's build and the tests alone.
#[cfg(any(test, fuzzing))]
pub mod fuzzing;
mod save;

pub use candidate::{CandidateError, CandidateHash};
pub use copy::{
    FRAMING, copy_exact, hash_prefix, read_end, read_meta, read_trailer, read_version, write_meta,
    write_trailer, write_version,
};
pub use save::{
    Hooks, Kept, PartialFate, Publish, SaveOptions, Saved, Start, Stopped, Target, save_resumable,
    save_verified,
};

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

    /// The chunk whose boundaries it keeps.
    pub(crate) fn chunk(&self) -> u64 {
        self.chunk
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
    /// A partial file handed to [`save_resumable`] holds fewer bytes than
    /// its hasher has hashed.
    PartialShort {
        /// The bytes the hasher has hashed.
        have: u64,
        /// The bytes the file holds.
        len: u64,
    },
    /// Every name tried for a new partial file was taken
    /// ([`SaveOptions::partial_tries`]): none was created.
    NoPartialName {
        /// The names tried.
        tried: usize,
    },
    /// Every name tried for the finished file was taken
    /// ([`Publish::name_tries`]): it got none, and
    /// its partial file was removed.
    NoNameLeft {
        /// The names tried.
        tried: usize,
    },
    /// One of the caller's [`Hooks`] refused, with its error's kind.
    Hook(io::ErrorKind),
    /// `safecopy-folder` refused, itself and not the system: the partial
    /// file's name when it was created, or the file on its way to its
    /// name. A name that is not one part of a path or names a stream, a
    /// link in the way, and the rest of its `Refusal`. The two searches
    /// that run out of names are not here: [`ChunkError::NoPartialName`]
    /// is this crate's own, and [`ChunkError::NoNameLeft`] the publish's.
    Folder(Refusal),
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

/// The one place an error of a file, a folder or a stream becomes this
/// crate's: a refusal of `safecopy-folder` keeps its kind.
impl From<io::Error> for ChunkError {
    fn from(e: io::Error) -> Self {
        match safecopy_folder::refusal(&e) {
            Some(why) => Self::Folder(why),
            None => Self::Io(e.kind()),
        }
    }
}

/// Switches tests use to act out a system's refusals.
#[cfg(test)]
// Android's emulated thread locals make Clippy ask for the `const` already there.
#[allow(clippy::missing_const_for_thread_local)]
mod seams {
    use std::cell::{Cell, RefCell};
    use std::sync::Arc;

    thread_local! {
        /// The next removal of a partial file fails with this system code.
        pub static DISCARD_ERROR: Cell<Option<i32>> = const { Cell::new(None) };
        /// The write that would pass this byte of the file fails, with the
        /// bytes before it written.
        pub static WRITE_ERROR_AT: Cell<Option<u64>> = const { Cell::new(None) };
        /// The next cut of a kept partial to its whole chunks fails.
        pub static CUT_ERROR: Cell<bool> = const { Cell::new(false) };
        /// The next flush of a checked file to the disk fails.
        pub static SYNC_ERROR: Cell<bool> = const { Cell::new(false) };
        /// The next publish of a checked file fails.
        pub static PUBLISH_ERROR: Cell<bool> = const { Cell::new(false) };
        /// The next publish of a checked file is refused so, at its name.
        pub static PUBLISH_REFUSAL: Cell<Option<safecopy_folder::Refusal>> =
            const { Cell::new(None) };
        /// The next removal of a partial file is refused so.
        pub static DISCARD_REFUSAL: Cell<Option<safecopy_folder::Refusal>> =
            const { Cell::new(None) };
        /// Told how many partial names a save of this thread found taken
        /// so far, after each, before its cancel is looked at.
        pub static NAME_TAKEN: RefCell<Option<Taken>> = const { RefCell::new(None) };
        /// Told of each step of the writes of the next save this thread
        /// starts, on the thread that takes the step.
        pub static WRITES: RefCell<Option<Writes>> = const { RefCell::new(None) };
    }

    /// What hears of each taken partial name.
    pub type Taken = Box<dyn Fn(usize)>;

    pub fn name_taken(taken: usize) {
        NAME_TAKEN.with_borrow(|seam| {
            if let Some(seam) = seam {
                seam(taken);
            }
        });
    }

    /// What hears of a save's writes: it may hold a step for as long as a
    /// test needs.
    pub type Writes = Arc<dyn Fn(Step) + Send + Sync>;

    /// A step of a save's writer.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Step {
        /// A write is on the blocking pool and has not taken the gate.
        Queued,
        /// It holds the gate and is about to write.
        Writing,
        /// It has let the gate go, written or not.
        Ended,
        /// The writer is being dropped and is about to take the gate.
        Closing,
    }
}

#[cfg(test)]
mod tests;
