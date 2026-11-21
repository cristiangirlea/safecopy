//! Many small files in one stream.
//!
//! A bundle carries several small files over any async byte stream
//! (tokio's [`AsyncRead`] and [`AsyncWrite`]): the framing version, then a
//! **table** of every file in it (its size, its [`TransferKey`] and the
//! caller's metadata block), then each file's bytes followed by a mark byte
//! and its SHA-256. The table comes first, so a receiver decides on every
//! file and creates all its partial files at once, before any byte, and
//! always knows where the next file starts: one bad file never spoils the
//! others.
//!
//! - [`plan`]: which files of a list go bundled, overlapped or alone.
//! - [`BundleWriter`]: writes a table, then each file or [`BundleWriter::give_up`]
//!   for one that changed or could not be read (its bytes are padding).
//! - [`BundleReader::open`] reads and checks the whole table;
//!   [`Table::read`] saves the files with one [`Take`] per entry, each
//!   verified before it is published, and reports one [`Outcome`] each.
//! - [`Gate`]: at most [`BundleLimits::at_once`] bundles of one transfer
//!   read at a time.
//! - With the `quic` feature, `quic`: stopping a quinn stream with a
//!   program's code and reading the code back.
//!
//! What both sides must agree on is the framing (below); every bound is a
//! setting ([`BundleLimits`]).
//!
//! # The framing
//!
//! All numbers little-endian.
//!
//! 1. The framing version, one byte ([`safecopy_chunks::FRAMING`]).
//! 2. The table's length in bytes, `u32`, then the table: the number of
//!    files, `u32`; for each, its size (`u64`), its transfer key (16
//!    bytes), its metadata block's length (`u32`) and the block.
//! 3. For each file, in the table's order: exactly its size in bytes, a
//!    mark byte ([`WHOLE`] or [`GIVEN_UP`]), and 32 bytes: the file's
//!    SHA-256 when whole, zeros when given up.
//! 4. The end of the stream.
//!
//! [`AsyncRead`]: tokio::io::AsyncRead
//! [`AsyncWrite`]: tokio::io::AsyncWrite

use std::fmt;
use std::io;
use std::sync::Arc;

pub use safecopy_chunks::Saved;
pub use safecopy_resume::TransferKey;

mod reader;
mod wire;
mod writer;

#[cfg(feature = "quic")]
pub mod quic;

pub use reader::{BundleHooks, BundleReader, Partial, ReadOptions, Report, Table, Take, Target};
pub use writer::{BundleWriter, WriteOptions, Written};

/// The mark after a whole file's bytes.
pub const WHOLE: u8 = 0;
/// The mark after a file given up at the sender: its bytes are padding.
pub const GIVEN_UP: u8 = 1;

/// The bounds of a bundle. The reader refuses a table past any of them
/// before reading a byte of any file; the writer never writes one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BundleLimits {
    /// The largest file bundled: 1 MiB.
    pub small: u64,
    /// The most files in one bundle: 64.
    pub files: u32,
    /// The most bytes of files in one bundle: 8 MiB.
    pub bytes: u64,
    /// The longest table, in bytes: 128 KiB. The writer ends a bundle
    /// early rather than pass it.
    pub table_bytes: u32,
    /// The longest metadata block of one entry: 4 KiB.
    pub meta_bytes: u32,
    /// How many bundles of one transfer a receiver reads at a time: 2
    /// ([`Gate`]).
    pub at_once: usize,
    /// Files over this size go alone, one at a time; those over
    /// [`Self::small`] up to it overlap ([`plan`]): 8 MiB.
    pub alone_over: u64,
}

impl Default for BundleLimits {
    fn default() -> Self {
        Self {
            small: 1 << 20,
            files: 64,
            bytes: 8 << 20,
            table_bytes: 128 << 10,
            meta_bytes: 4 << 10,
            at_once: 2,
            alone_over: 8 << 20,
        }
    }
}

/// One file of a bundle, as the table holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableEntry {
    /// Its transfer key, so a file sent again on its own later is matched
    /// to this entry.
    pub key: TransferKey,
    /// The caller's metadata block: this crate does not read it.
    pub meta: Vec<u8>,
    /// Its size in bytes.
    pub size: u64,
}

/// How a file of a list is sent ([`plan`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Group {
    /// These files, by their index in the list, in one bundle.
    Bundle(Vec<usize>),
    /// This file on a stream of its own, alongside others.
    Overlapped(usize),
    /// This file on a stream of its own, alone.
    Alone(usize),
}

/// Cuts a list of file sizes into bundles and single files: a file of at
/// most [`BundleLimits::small`] bytes is bundled, in the list's order, up
/// to [`BundleLimits::files`] files or [`BundleLimits::bytes`] bytes per
/// bundle, whichever comes first; one up to [`BundleLimits::alone_over`]
/// overlaps; a larger one goes alone. Each group is placed where its first
/// file is in the list. A pure function of the sizes; the metadata blocks
/// can end a bundle earlier ([`BundleWriter::new`]).
pub fn plan(sizes: &[u64], limits: &BundleLimits) -> Vec<Group> {
    let mut groups = Vec::new();
    // The open bundle: its place in `groups` and its bytes.
    let mut open: Option<(usize, u64)> = None;
    for (index, &size) in sizes.iter().enumerate() {
        let bundled = size <= limits.small && size <= limits.bytes && limits.files > 0;
        if !bundled {
            groups.push(if size > limits.alone_over {
                Group::Alone(index)
            } else {
                Group::Overlapped(index)
            });
            continue;
        }
        if let Some((at, bytes)) = open {
            let Group::Bundle(files) = &mut groups[at] else {
                unreachable!("an open bundle is a bundle")
            };
            let total = bytes.checked_add(size).filter(|t| *t <= limits.bytes);
            if let Some(total) = total.filter(|_| files.len() < limits.files as usize) {
                files.push(index);
                open = Some((at, total));
                continue;
            }
        }
        open = Some((groups.len(), size));
        groups.push(Group::Bundle(vec![index]));
    }
    groups
}

/// What became of one file of a bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// Its hash matched and it was published.
    Saved(Saved),
    /// [`Take::SaveUnlessSame`] and its hash equalled the given one: its
    /// partial file was removed and nothing was published.
    AlreadyThere,
    /// Its hash was not the one that came after it: its partial file was
    /// removed. The sender may send it once more on its own.
    Damaged,
    /// The receiver took it with [`Take::Skip`]: its bytes were read and
    /// thrown away.
    Refused,
    /// The sender gave it up: its bytes were padding, thrown away.
    GivenUp,
    /// The bundle ended before this file was whole: the stream was cut,
    /// disagreed with the table, or was cancelled. Nothing of it is kept.
    NotReceived,
    /// This side failed it: the system refused its partial file, a write,
    /// the publish, or a hook refused (its error's kind). Its bytes were
    /// read past; nothing of it is kept.
    Failed(io::ErrorKind),
}

/// Why a bundle was refused or ended early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BundleError {
    /// A framing version this crate does not read.
    Version {
        /// The version byte that came.
        got: u8,
    },
    /// The table names more files than [`BundleLimits::files`].
    TooMany {
        /// The files it names.
        count: u32,
        /// The most taken.
        most: u32,
    },
    /// An entry larger than [`BundleLimits::small`].
    TooLarge {
        /// Its place in the table.
        entry: u32,
        /// Its declared size.
        size: u64,
        /// The most taken.
        most: u64,
    },
    /// The entries' sizes add up past [`BundleLimits::bytes`].
    TooManyBytes {
        /// The bytes of the entries up to the one that passed it.
        bytes: u64,
        /// The most taken.
        most: u64,
    },
    /// A table longer than [`BundleLimits::table_bytes`], or one entry
    /// that alone would make it so.
    TableTooLong {
        /// Its length in bytes.
        len: u64,
        /// The most taken.
        most: u64,
    },
    /// A metadata block longer than [`BundleLimits::meta_bytes`].
    MetaTooLong {
        /// Its entry's place in the table.
        entry: u32,
        /// Its declared length.
        len: u64,
        /// The most taken.
        most: u64,
    },
    /// The table's count does not match the entries it holds: they end
    /// before the count, or bytes are left after them.
    CountMismatch,
    /// The stream ended inside the table.
    TableCutShort,
    /// A mark other than [`WHOLE`] or [`GIVEN_UP`].
    BadMark {
        /// Its entry's place in the table.
        entry: u32,
        /// The mark that came.
        mark: u8,
    },
    /// The stream ended inside this entry's bytes, mark or hash.
    CutShort {
        /// Its place in the table.
        entry: u32,
    },
    /// More came after the last file's hash.
    TooLong,
    /// [`Table::read`] got a number of decisions other than the table's
    /// entries.
    Decisions {
        /// The decisions given.
        got: usize,
        /// The table's entries.
        entries: usize,
    },
    /// [`BundleWriter::write`] or [`BundleWriter::give_up`] after the last
    /// entry.
    NoEntryLeft,
    /// A [`BundleWriter`] call after a write that failed or was dropped
    /// part way through an entry: the stream can only be ended short.
    Torn,
    /// One of the caller's [`BundleHooks`] refused the bundle, with its
    /// error's kind.
    Hook(io::ErrorKind),
    /// The caller's cancel finished first.
    Cancelled,
    /// The stream failed.
    Io(io::ErrorKind),
}

impl fmt::Display for BundleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The caller words it; this is the kind for logs and tests.
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for BundleError {}

impl From<io::Error> for BundleError {
    fn from(e: io::Error) -> Self {
        Self::Io(e.kind())
    }
}

/// Lets at most [`BundleLimits::at_once`] bundles of one transfer be read
/// at a time: a receiver enters before it reads a bundle's table and holds
/// the [`Pass`] until the bundle ends; a further bundle waits unread.
#[derive(Debug, Clone)]
pub struct Gate(Arc<tokio::sync::Semaphore>);

/// A place in a [`Gate`], given back when dropped.
#[derive(Debug)]
pub struct Pass {
    _place: tokio::sync::OwnedSemaphorePermit,
}

impl Gate {
    /// A gate for one transfer.
    pub fn new(limits: &BundleLimits) -> Self {
        Self(Arc::new(tokio::sync::Semaphore::new(limits.at_once)))
    }

    /// Waits for a place.
    pub async fn enter(&self) -> Pass {
        Pass {
            _place: Arc::clone(&self.0)
                .acquire_owned()
                .await
                .expect("the gate is never closed"),
        }
    }

    /// A place now, or none when every place is held.
    pub fn try_enter(&self) -> Option<Pass> {
        Arc::clone(&self.0)
            .try_acquire_owned()
            .ok()
            .map(|place| Pass { _place: place })
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(test, feature = "quic"))]
mod quic_tests;
