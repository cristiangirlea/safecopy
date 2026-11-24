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
//!   bytes and expire, never while their session runs. Several processes
//!   may share one folder: each holds the records it writes, and reads
//!   the others'. What is kept is listed without reading it
//!   ([`Store::kept_by`]); a kept partial is checked when it is
//!   continued ([`Store::reopen_hashed`]), and the hasher of that check
//!   handed on, so the save (`safecopy_chunks::save_resumable`) reads
//!   the prefix once. A save holds a guard on its file
//!   ([`Store::saving`], [`Saving`]): one save per file, and a save that
//!   is dropped or panics lets its record go.
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
//! # What a matching hash proves
//!
//! A file gets its name only when the hash of its bytes equals the
//! sender's. Those bytes are hashed where they pass, never read back:
//!
//! - **A new file:** each byte as it is written.
//! - **A resumed file:** the kept bytes as they are read from the disk at
//!   [`Store::reopen_hashed`], through the handle the save then appends
//!   through, so the partial is held from the check on; the rest as it
//!   is written.
//!
//! So a match proves the stream was whole and unchanged, and that the
//! kept bytes were the sender's when they were read. It does not prove
//! what the disk holds afterwards: nothing is read again before or after
//! the file is published, and a byte changed later, by a disk that fails
//! without an error or by another program writing the partial, is not
//! seen. A hasher from [`Store::held_hashed`] proves less: no handle is
//! on the partial between that check and the reopening, and bytes
//! changed in between are published under a hash that matches. Use it
//! only where nothing else can write the folder.
//!
//! # The store's folder is its program's alone
//!
//! Records hold no file contents and are not sealed, and nothing in one
//! proves that the store wrote it. The folder passed to [`Store::open`]
//! must be writable only by the program that owns it, as its journals'
//! folder must: a program that can write there can write the journals
//! too, and no check here stands against it. What a record file planted
//! in that folder can and cannot do:
//!
//! - **It cannot** have a byte written through a link, nor have a file
//!   opened, cut, written or removed that is not named as a partial
//!   (`safecopy_journal::PartialAt::is_partial`), nor be written over a
//!   file that is not that record (see [`Store`]). One that names a file
//!   which is no partial counts toward no cap, and goes at the next
//!   [`Store::expire`].
//! - **It can** name any file whose name ends in the partial ending, in
//!   any folder the person can write, by that folder's identity, the name
//!   and the file ID: another program's unfinished file, or on Unix this
//!   store's own running save under another key. [`Store::reopen`] cuts
//!   that file to the record's `have` and hands it out to be continued;
//!   [`Store::reopen_hashed`] does once the file's first `have` bytes
//!   hash to the record's `prefix`, both of which whoever planted the
//!   record chose. Whatever removes the record removes that file with it
//!   (an expiry, [`Store::forget`], [`Store::remove_all`], a check that
//!   finds it changed), and until then the record counts toward the caps
//!   with the `have` it claims, so an expiry may remove older kept files
//!   for it.
//!
//! What is published is still decided by the hashes alone: of the kept
//! bytes as read at [`Store::reopen_hashed`], and of the whole file at
//! the end, against the sender's. A planted record gets a file named as
//! a partial cut, continued or removed; it gets no byte published that
//! the sender did not send.
//!
//! # A save's hooks, and how it ends
//!
//! A save takes its file first ([`Store::saving`]), before a check's
//! answer is acted on, and holds that guard until it has ended.
//!
//! - **A first save** (`safecopy_chunks::Start::New`): its `whole` hook
//!   calls [`Store::receiving`] for the first whole chunk and
//!   [`Store::keep`] (not at a drop) for each later one.
//! - **A continued save** (`Start::Resume`): its `whole` hook calls
//!   [`Store::keep`] only, since the record is there; `before_create`
//!   and `created` are not called for it, and the caller lists the
//!   reopened partial in its journal itself.
//! - **It ends** with its partial kept: [`Saving::kept`]. Published, or
//!   stopped with nothing kept for any reason: [`Saving::forget`].
//!   Dropped or panicked: the guard lets the record go.
//!
//! [`Store::forget`] is [`ResumeError::Busy`] for a file being received,
//! so the same key on a second connection cannot have the first's record
//! and partial removed, whatever a check answered for it. A sending
//! record it forgets whatever is being received under its key.
//!
//! A save whose future is dropped leaves no write behind
//! (`safecopy_chunks::save_resumable`): when its drop returns, no write
//! of it is running and none can start, so nothing of the dropped stream
//! lands in the partial once a later save has reopened and hashed it.
//! That drop waits for a write already under way, for as long as that
//! one write takes: 64 KiB at most, and on a disk that stalls, as long
//! as it stalls.
//!
//! # Blocking
//!
//! Every function of [`Store`] that reads or changes records, but the
//! three async ones, is synchronous and does file work, most of it under
//! the store's lock: none belongs on a thread that must not wait for a
//! disk. Each of them lists the store's folder at least once (some two
//! or three times) and opens the record files a listing shows as new or
//! changed. What each does besides:
//!
//! - [`Store::received`], [`Store::sent`], [`Store::saving`],
//!   [`Store::keeps`], [`Store::kept_bytes`], [`Store::release`] and
//!   [`Store::release_peer`]: nothing more. Dropping a [`Saving`] lists
//!   nothing.
//! - [`Store::open`]: makes the folder and reads every record in it.
//! - [`Store::kept_by`]: inspects each kept partial's name in its folder.
//! - [`Store::reopen`]: takes the record file, finds the partial's folder
//!   by identity, opens the partial and cuts it.
//! - [`Store::receiving`]: flushes the partial to the disk, then writes
//!   the record and flushes it. [`Store::sending`] writes and flushes
//!   its record. [`Store::keep`] does as `receiving` when a rewrite is
//!   due ([`Limits::rewrite_bytes`], [`Limits::rewrite_every`]) or at a
//!   drop, and so [`Saving::kept`]; otherwise it only looks the record
//!   up.
//! - [`Store::forget`], [`Saving::forget`], [`Store::remove_all`],
//!   [`Store::expire`] and [`Store::clean_up`]: remove record files and
//!   partials; `expire` reads each disk's free space, and `clean_up` the
//!   journals.
//!
//! On an async runtime, run them on its blocking pool
//! (`tokio::task::spawn_blocking`, with the store in an `Arc`) or inside
//! `tokio::task::block_in_place`. A [`Saving`] borrows its store and does
//! not move to another thread's task: take it ([`Store::saving`]) and end
//! it ([`Saving::kept`], [`Saving::forget`]) inside `block_in_place`. A
//! save's `whole` hook runs on the thread that polls the save: the
//! [`Store::keep`] in it lists the folder each time and flushes the
//! partial and the record when a rewrite is due; on a multi-threaded
//! runtime wrap it in `tokio::task::block_in_place`, or give the save a
//! thread of its own.
//!
//! The three async functions ([`Store::reopen_hashed`],
//! [`Store::held_hashed`], [`Store::held_from`]) do the same file work
//! on the thread that polls them, before, between and after their
//! waits: the folder listed, a record taken, each partial opened, cut or
//! removed with its record. Only a partial's bytes are read on the
//! blocking pool. They hold the lock across no wait, and their futures
//! are `Send`.

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
pub use store::{Expired, Saving, Store};

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
    /// The most sending records kept: 256. Past it, the oldest goes.
    pub sending: usize,
    /// The most record files a store's index remembers: 1024, twice what
    /// the caps above keep. Any past it are read at each lookup.
    pub indexed: usize,
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
            sending: 256,
            indexed: 1024,
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
    /// The partial file or its folder is not there any more, another
    /// file has its name, or the record names a file that is no partial
    /// (never opened).
    Gone,
    /// The partial file's disk or folder cannot be read now: a disk away,
    /// a place the system would not show. Nothing is removed for it.
    Unreachable,
    /// The partial file's first `have` bytes do not hash to the record's
    /// `prefix`: changed on the disk since they were recorded.
    PrefixChanged,
    /// The record or its partial file is in use: another store on the
    /// same folder holds the record, this store holds it and is asked to
    /// reopen its partial again, or the partial is open elsewhere. It is
    /// not written, reopened or removed here.
    Busy,
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
    /// A receiving record's file is not named as a partial under the
    /// store's journal settings: the record is not written.
    NotPartial,
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
