//! Crash records for partial files, and cleanup that removes only the file
//! it recorded.
//!
//! A [`Journal`] is kept in a folder the caller gives, one file per
//! transfer. It records the volume of the folder files are written into
//! (its `\\?\Volume{...}\` path and serial number) and, from the moment
//! each partial file is created and before it gets a byte, that file's
//! folder, name and file ID. [`clean_up`], run after a crash, removes a
//! recorded partial only through a handle checked to be that very file:
//! never by its name alone. A file renamed since, another file under the
//! name, a folder or a disk that is gone, is left alone.
//!
//! The journal file stays open without write sharing while its transfer
//! runs, so a cleanup in another process meanwhile leaves it alone.
//!
//! # Systems
//!
//! - **Windows** records and cleans up as above: it opens the volume by
//!   its GUID path, checks its serial number, opens the folder by its file
//!   ID and the partial by its name in that folder, compares the file ID
//!   and deletes through that handle.
//! - **Other systems** cannot open a folder by its ID: a journal there
//!   records nothing and writes no file, and [`clean_up`] only removes the
//!   journal files it finds.
//!
//! Each record is rewritten whole on every change, at most
//! [`Settings::record_bytes`] long.

use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::path::Path;
use std::time::Duration;

use safecopy_folder::{Directory, Refusal};

mod record;
#[cfg(windows)]
mod win;

/// The ending of a journal file's name, which [`clean_up`] looks for.
pub const EXTENSION: &str = "journal";

/// What a journal may hold and which names its cleanup may remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The longest a record may be, in bytes: a record that would grow
    /// past it is not written, and a longer file is not read. 1 MiB.
    pub record_bytes: usize,
    /// The ending a recorded name must have for cleanup to remove the
    /// file: the partial name's ending. `.part`, as
    /// [`safecopy_folder::part`] names it.
    pub partial_ending: String,
    /// How long [`clean_up`] keeps a journal it could not finish (its
    /// volume could not be opened, a partial could not be removed) for a
    /// later cleanup, from the journal's last change. Older, it is removed
    /// and its partials left. 30 days.
    pub keep_for: Duration,
    /// The most journals [`clean_up`] keeps so; past it the oldest are
    /// removed and their partials left. 64.
    pub keep_most: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            record_bytes: 1 << 20,
            partial_ending: ".part".to_string(),
            keep_for: Duration::from_secs(30 * 24 * 60 * 60),
            keep_most: 64,
        }
    }
}

/// Why a journal could not be written or read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum JournalError {
    /// The partial's name is not Unicode text; a record keeps names as text.
    NameNotText,
    /// The record would be longer than [`Settings::record_bytes`].
    TooLarge {
        /// The limit it passed.
        most: usize,
    },
    /// The bytes are not a record: cut short, a number or text that does
    /// not read, or bytes after the last partial.
    Damaged,
    /// The partial file, or its folder, is not on the volume the journal
    /// records: its file ID would mean another file there.
    OtherVolume,
    /// `safecopy-folder` refused: a folder on no volume of its own (a
    /// network share), a name it would not open.
    Folder(Refusal),
    /// The system's error.
    Io(io::ErrorKind),
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The caller words it; this is the kind for logs and tests.
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for JournalError {}

impl From<io::Error> for JournalError {
    fn from(error: io::Error) -> Self {
        match safecopy_folder::refusal(&error) {
            Some(why) => Self::Folder(why),
            None => Self::Io(error.kind()),
        }
    }
}

/// One recorded partial file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partial {
    /// The file ID of the folder it is in.
    pub folder: u128,
    /// Its name in that folder.
    pub name: String,
    /// Its own file ID.
    pub file: u128,
}

/// One journal's record: the volume and the partial files on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The volume as `\\?\Volume{...}\`.
    pub volume: String,
    /// The volume's serial number: another disk under the same path has
    /// another.
    pub serial: u64,
    /// The partial files not yet published or removed.
    pub partials: Vec<Partial>,
}

impl Record {
    /// The record's bytes, as a journal file holds them.
    pub fn encode(&self, settings: &Settings) -> Result<Vec<u8>, JournalError> {
        record::encode(self, settings)
    }

    /// A record read from a journal file's bytes.
    pub fn decode(bytes: &[u8], settings: &Settings) -> Result<Self, JournalError> {
        record::decode(bytes, settings)
    }
}

/// One transfer's journal: written as each partial file is created, gone
/// when it is dropped with none left. One still recorded (a transfer
/// dropped midway, a removal that failed) keeps the journal for
/// [`clean_up`].
pub struct Journal {
    #[cfg(windows)]
    inner: Option<win::Open>,
}

impl fmt::Debug for Journal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Journal").finish_non_exhaustive()
    }
}

impl Journal {
    /// A journal that records nothing.
    pub fn none() -> Self {
        Self {
            #[cfg(windows)]
            inner: None,
        }
    }

    /// A new journal in the folder `dir`, for partial files written below
    /// `root`. Only Windows can find a folder again by its ID; elsewhere it
    /// records nothing and creates no file. Fails with
    /// [`Refusal::NoVolume`] for a folder on a network share.
    pub fn start(dir: &Path, root: &Directory, settings: &Settings) -> Result<Self, JournalError> {
        #[cfg(windows)]
        {
            let open = win::Open::start(dir, root, settings.clone())?;
            Ok(Self { inner: Some(open) })
        }
        #[cfg(not(windows))]
        {
            let _ = (dir, root, settings);
            Ok(Self::none())
        }
    }

    /// The partial file `file` was created as `name` in `dir`, with no byte
    /// yet. Call it before writing to the file: a record that cannot be
    /// written leaves the file unrecorded, and the caller removes it. A
    /// file or folder on another volume than `root`'s is
    /// [`JournalError::OtherVolume`].
    pub fn started(
        &self,
        dir: &Directory,
        file: &std::fs::File,
        name: &OsStr,
    ) -> Result<(), JournalError> {
        #[cfg(windows)]
        if let Some(open) = &self.inner {
            return open.started(dir, file, name);
        }
        let _ = (dir, file, name);
        Ok(())
    }

    /// The partial file `name` in `dir` was published or removed: it is no
    /// longer the journal's.
    pub fn ended(&self, dir: &Directory, name: &OsStr) -> Result<(), JournalError> {
        #[cfg(windows)]
        if let Some(open) = &self.inner {
            return open.ended(dir, name);
        }
        let _ = (dir, name);
        Ok(())
    }

    /// Ends as a crash would: the journal file is closed and left for
    /// [`clean_up`].
    #[cfg(all(test, windows))]
    fn crash(self) {
        if let Some(open) = &self.inner {
            open.keep();
        }
    }
}

/// What a cleanup did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Cleaned {
    /// Partial files removed.
    pub removed: usize,
    /// Recorded files left alone: renamed, replaced, gone, on another
    /// disk, or on a volume that could not be opened.
    pub left: usize,
    /// Journals kept for a later cleanup: a volume that could not be
    /// opened (a removable disk not plugged in), or a partial that could
    /// not be removed and may yet be.
    pub kept: usize,
}

/// Removes the partial files the journals in `dir` recorded, each only
/// through a handle checked to be that file, and the journals with them.
///
/// A journal is removed once each of its partials is removed, gone or
/// replaced. One whose volume could not be opened, or with a partial that
/// failed otherwise, is kept for a later cleanup, at most for
/// [`Settings::keep_for`] and [`Settings::keep_most`] journals: past
/// either, the oldest is removed and its partials left.
///
/// A journal still open in a running transfer, in this process or another,
/// is left alone; so is one that cannot be opened, and a link or other
/// reparse point named as a journal. A damaged or too long journal is
/// removed and touches nothing. A missing `dir` has nothing to clean. Only
/// Windows counts what it removed: elsewhere the journals hold nothing, and
/// are removed.
pub fn clean_up(dir: &Path, settings: &Settings) -> Result<Cleaned, JournalError> {
    // Only Windows counts.
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut cleaned = Cleaned::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(cleaned),
        Err(e) => return Err(e.into()),
    };
    // Journals kept, newest change first, each held until the end.
    #[cfg(windows)]
    let mut kept: Vec<(std::time::SystemTime, std::fs::File)> = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension() != Some(OsStr::new(EXTENSION)) {
            continue;
        }
        #[cfg(windows)]
        {
            let Ok((file, bytes)) = win::take(&path, settings) else {
                // In use by a running transfer, a link, or unreadable: left.
                continue;
            };
            let mut keep = false;
            if let Ok(record) = Record::decode(&bytes, settings) {
                let (done, again) = remove_all(&record, settings);
                cleaned.removed += done.removed;
                cleaned.left += done.left;
                keep = again;
            }
            if keep {
                // A time it cannot read counts as old: the bound still holds.
                let changed = file.metadata().and_then(|m| m.modified()).ok();
                let fresh = changed
                    .filter(|&at| at.elapsed().unwrap_or(Duration::ZERO) <= settings.keep_for);
                if let Some(at) = fresh {
                    let place = kept.partition_point(|(other, _)| *other >= at);
                    kept.insert(place, (at, file));
                    if kept.len() > settings.keep_most
                        && let Some((_, oldest)) = kept.pop()
                    {
                        let _ = safecopy_folder::delete_by_handle(&oldest);
                    }
                    continue;
                }
            }
            let _ = safecopy_folder::delete_by_handle(&file);
        }
        #[cfg(not(windows))]
        {
            let _ = settings;
            let _ = std::fs::remove_file(&path);
        }
    }
    #[cfg(windows)]
    {
        cleaned.kept = kept.len();
    }
    Ok(cleaned)
}

/// Removes each partial file `record` lists, only through a handle checked
/// to be that file: on the recorded volume (its serial checked), in the
/// folder of the recorded ID, under the recorded name, which must end in
/// [`Settings::partial_ending`] and name no stream, with the recorded file
/// ID. Anything else is left. Other systems leave every one.
pub fn remove_by_identity(record: &Record, settings: &Settings) -> Cleaned {
    remove_all(record, settings).0
}

/// What [`remove_by_identity`] does, and whether a later try could remove
/// a partial left now: the journal is kept for it.
fn remove_all(record: &Record, settings: &Settings) -> (Cleaned, bool) {
    let mut cleaned = Cleaned::default();
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut again = false;
    for partial in &record.partials {
        #[cfg(windows)]
        let removed = match win::remove(record, partial, settings) {
            Ok(()) => true,
            Err(why) => {
                again |= why.may_change();
                false
            }
        };
        #[cfg(not(windows))]
        let removed = {
            let _ = (partial, settings);
            false
        };
        if removed {
            cleaned.removed += 1;
        } else {
            cleaned.left += 1;
        }
    }
    (cleaned, again)
}

/// Whether cleanup may remove a file of this recorded name: a partial's
/// ending, and one part of a path that names no stream.
#[cfg_attr(not(windows), allow(dead_code))]
fn removable(name: &str, settings: &Settings) -> bool {
    // "name:stream" would open a stream of a file, not the file.
    !settings.partial_ending.is_empty()
        && name.ends_with(settings.partial_ending.as_str())
        && !name.contains(['\\', '/', ':'])
}

#[cfg(test)]
mod tests;
