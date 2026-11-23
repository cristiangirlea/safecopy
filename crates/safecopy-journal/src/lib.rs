//! Crash records for partial files, and cleanup that removes only the file
//! it recorded.
//!
//! A [`Journal`] is kept in a folder the caller gives, one per transfer (a
//! folder, a paste, a session's single files): an append-only log of what
//! the transfer has done, in short segment files. From the moment each
//! partial file is created, and before it gets a byte, the journal records
//! the file's folder, its name, its own ID and the entry it is for, synced
//! to the disk. For a transfer that goes by groups (`safecopy-manifest`'s
//! [`groups`](safecopy_manifest::groups)) it also records the groups it
//! opens and the entries it settles, so its [`Progress`] stays bounded and
//! can be read back after a crash ([`recover`]).
//!
//! After a crash, [`clean_up`] removes a recorded partial only through a
//! handle checked to be that very file, never by its name alone: a file
//! renamed since, another file under the name, a folder or a disk that is
//! gone, is left alone. A journal still held by a running transfer, in this
//! process or another, is left alone too.
//!
//! # A folder renamed while its transfer runs
//!
//! A folder is recorded as its levels below the root, each by name and
//! identity, walked from the root's handle with no link followed. When the
//! path a held folder was opened at no longer reaches it (renamed, moved,
//! or another folder or a link put under its name), the journal reads where
//! the handle is now and walks that instead: the folder is recorded under
//! its present names, and the transfer carries on. One that is no longer
//! below the root is refused ([`JournalError::NotBelow`]), as is one never
//! below it. A folder opened through a link below the root's path is judged
//! the same way, by where it really is: recorded under its real names when
//! that is below the root, refused when the link leads outside.
//!
//! What happens when the present place is not read depends on why:
//!
//! - **Windows, Linux and Android read it.** When a reading fails (a file
//!   system or a volume that gives a handle no path, a Linux folder whose
//!   name ends in " (deleted)", procfs mounted and refusing), nothing
//!   shows the folder is still below the root: it is refused with
//!   [`JournalError::NotBelow`], renamed inside the root or moved out of
//!   it alike. An error of the first walk that is no sign of a rename (a
//!   link under the name, say) stands as it is.
//! - **macOS and the BSDs cannot read it, nor Linux and Android with no
//!   procfs mounted.** There a folder whose name is gone, or names a file
//!   or another folder now, is recorded by its identity alone: one level
//!   with an empty name, directly below the root. One moved out of the
//!   root cannot be told from one renamed inside it, and is recorded so
//!   too; cleanup never finds such a folder on these systems, so nothing
//!   is removed there. A link under its name is refused
//!   ([`Refusal::Link`]): it could be how the folder was opened.
//!
//! After a crash, [`clean_up`] removes a partial recorded after the rename
//! on Windows, Linux and Android: its folder is found under its present
//! names. A partial recorded before the rename is removed on Windows and
//! left on Unix. Windows opens the root and the folder by their IDs, which
//! open them anywhere on the volume, and so checks the folder is still
//! below the root before it touches anything: by its recorded names, or by
//! where both are now, walked from the root with no link followed. A
//! folder moved out of the root since is left as one that is gone, and its
//! record ends with the journal's; one whose place cannot be read then is
//! left and its journal kept for a later cleanup. A move in the moment
//! between that check and the removal is not caught.
//! Unix cannot open a folder by its ID: recorded names that no longer
//! reach the folder, or an empty one, leave the partial. The partial's own
//! name and ID are checked as ever, and a file of the same name where the
//! folder used to be is another file in another folder: it is left.
//!
//! # The log
//!
//! Each segment starts with a header (the format, its sequence number, the
//! destination's root and a snapshot of the progress: the mark, the open
//! groups' settled entries, the live partials and the folders they are in),
//! then entries, each framed with its length and a CRC-32C. A reader stops
//! at the first entry cut short, damaged, or that could not follow the ones
//! before it, so a torn append loses only that entry. When the mark moves,
//! or an append would pass [`Settings::segment_bytes`], the journal starts
//! its next segment: the snapshot is written and synced there first (on
//! Unix its folder too), and only then is the old segment deleted. A reader takes
//! the highest-numbered segment whose header is whole.
//!
//! A journal syncs once per partial started (once for a bundle's, with
//! [`Journal::started_many`]) and once per mark move; settling and ending
//! are written without a sync of their own and reach the disk with the
//! next.
//!
//! # Systems
//!
//! - **Windows** finds a folder again by its volume (its
//!   `\\?\Volume{...}\` path and serial number) and its file ID, and
//!   only while it is still below the root, which is found the same way. A
//!   folder on a network share has no volume path, and [`Journal::start`]
//!   refuses it.
//! - **Linux, macOS and Android** cannot open a folder by its ID: the
//!   journal records the root's path, device and inode, and each folder
//!   below it by name with its device and inode. Cleanup opens the root,
//!   compares it, then each level by name relative to the one above with
//!   no link followed, comparing each; a level renamed, replaced or turned
//!   into a link stops the walk. A partial is removed by its name after its
//!   inode is compared, so a file swapped in at that moment is not caught.
//!   Its inode can be reused as soon as a file is deleted, so a file found
//!   under a published name is only a candidate there, never settled
//!   ([`Recovered::candidates`]).
//! - Journals in the whole-record format of before ([`Record`], extension
//!   [`EXTENSION`]) are still read and cleaned up; only Windows wrote them.

use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use safecopy_folder::{Directory, Refusal, identity};
use safecopy_manifest::{Group, GroupLimits, Progress, ProgressError};
use safecopy_names::NameRules;

pub mod files;
mod location;
mod log;
mod record;
#[cfg(windows)]
mod win;

pub use location::{Left, Level, Location, PartialAt};
pub use log::{Folder, Root, Segment, Started, read_segment};

use files::Held;
use log::{Entry, State, frame};

/// The ending of a journal file of the whole-record format of before,
/// which [`clean_up`] still reads.
pub const EXTENSION: &str = "journal";

/// The ending of a segment of a journal's log.
pub const LOG_EXTENSION: &str = "jlog";

/// What a journal may hold and which names its cleanup may remove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The longest record of the whole-record format a cleanup reads, in
    /// bytes: a longer file is not read. 1 MiB.
    pub record_bytes: usize,
    /// The longest a segment of the log may be, in bytes: an append that
    /// would pass it starts the next segment, and is refused only if it
    /// passes it there too; a longer file is read only this far.
    /// 4 MiB: two open groups' entries in the deepest folders fit in about
    /// 2 MiB.
    pub segment_bytes: usize,
    /// For a transfer that does not go by groups, how many partials may end
    /// before the journal starts its next segment, keeping it short: 256.
    pub roll_every: usize,
    /// The groups a transfer's progress is held to.
    pub groups: GroupLimits,
    /// The most partial files a journal holds at once: 134, six single
    /// files and two bundles of 64.
    pub partials: usize,
    /// The ending a recorded name must have for cleanup to remove the
    /// file: the partial name's ending. `.part`, as
    /// [`safecopy_folder::part`] names it.
    pub partial_ending: String,
    /// The rules a published file's numbered name was made by.
    pub names: NameRules,
    /// The most names of a folder [`recover`] reads, looking for a file
    /// whose settling was lost: 10,000.
    pub list_most: usize,
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
            segment_bytes: 4 << 20,
            roll_every: 256,
            groups: GroupLimits::default(),
            partials: 134,
            partial_ending: ".part".to_string(),
            names: NameRules::default(),
            list_most: 10_000,
            keep_for: Duration::from_secs(30 * 24 * 60 * 60),
            keep_most: 64,
        }
    }
}

/// Why a journal could not be written or read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum JournalError {
    /// A name is not Unicode text; a journal keeps names as text.
    NameNotText,
    /// A record or a segment would be longer than its limit, or more
    /// partials than [`Settings::partials`].
    TooLarge {
        /// The limit it passed.
        most: usize,
    },
    /// The bytes are not a record or a segment: cut short, a number or
    /// text that does not read, or bytes after the end.
    Damaged,
    /// The partial file, or its folder, is not on the volume the journal
    /// records: its file ID would mean another file there.
    OtherVolume,
    /// The folder is not below the journal's root: by the path it was
    /// opened at, or by where it is now.
    NotBelow,
    /// The change does not follow the transfer's groups: see
    /// [`ProgressError`].
    Progress(ProgressError),
    /// A write failed and could not be undone: the journal takes no more.
    Broken,
    /// `safecopy-folder` refused: a folder on no volume of its own (a
    /// network share), a name it would not open.
    Folder(Refusal),
    /// The system's error.
    Io {
        /// Its kind.
        kind: io::ErrorKind,
        /// The system's own code for it (`errno`, a Windows error code),
        /// when it has one.
        raw: Option<i32>,
    },
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
            None => Self::Io {
                kind: error.kind(),
                raw: error.raw_os_error(),
            },
        }
    }
}

/// One partial file of a journal of the whole-record format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partial {
    /// The file ID of the folder it is in.
    pub folder: u128,
    /// Its name in that folder.
    pub name: String,
    /// Its own file ID.
    pub file: u128,
}

/// A journal of the whole-record format of before: the volume and the
/// partial files on it. Only read now, by [`clean_up`].
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

/// One partial file of a bundle, for [`Journal::started_many`].
#[derive(Debug, Clone, Copy)]
pub struct Start<'a> {
    /// The entry's place in its list.
    pub place: u32,
    /// The partial file, just created, with no byte yet.
    pub file: &'a File,
    /// The partial file's name.
    pub partial: &'a OsStr,
    /// The name the entry is published under.
    pub name: &'a str,
    /// The entry's declared size.
    pub size: u64,
}

static NEXT: AtomicU64 = AtomicU64::new(1);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic elsewhere leaves the journal as whole as it was.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One transfer's journal: written as each partial file is created, gone
/// when it is dropped with none left. One still recorded (a transfer
/// dropped midway, a removal that failed) keeps the journal for
/// [`clean_up`].
pub struct Journal {
    inner: Option<Open>,
}

impl fmt::Debug for Journal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Journal").finish_non_exhaustive()
    }
}

/// A journal being written.
struct Open {
    dir: PathBuf,
    name: String,
    root: Directory,
    settings: Settings,
    log: Mutex<Log>,
}

struct Log {
    segment: Held,
    len: u64,
    state: State,
    /// Partials ended or settled since the last segment began, for a
    /// transfer without groups.
    since_roll: usize,
    /// A write failed and could not be undone, or a crash was acted out.
    broken: bool,
    /// Left for cleanup even with nothing recorded: a crash acted out.
    keep: bool,
    /// Groups opened, applied to `state`, written with the next append.
    pending: Vec<Entry>,
    /// The next segment is due but could not be begun: tried again.
    roll_due: bool,
}

fn segment_name(name: &str, seq: u64) -> String {
    format!("{name}.{seq}.{LOG_EXTENSION}")
}

impl Journal {
    /// A journal that records nothing.
    pub fn none() -> Self {
        Self { inner: None }
    }

    /// A new journal in the folder `dir`, for partial files written below
    /// `root`. Fails with [`Refusal::NoVolume`] on Windows for a folder on
    /// a network share.
    pub fn start(dir: &Path, root: &Directory, settings: &Settings) -> Result<Self, JournalError> {
        let state = State::new(location::root_of(root)?, settings);
        let header = frame(&state.header());
        // A process ID is reused: the salt keeps a later journal from taking
        // the name of one kept from before, whose later segments it would read as its own.
        let salt = format!("-{:016x}", salt());
        let (name, file) = files::create_named(dir, &NEXT, &salt, &format!(".1.{LOG_EXTENSION}"))?;
        let segment = Held {
            file,
            path: dir.join(segment_name(&name, 1)),
        };
        let written = (&segment.file)
            .write_all(&header)
            .and_then(|()| sync(&segment.file))
            .and_then(|()| sync_dir(dir));
        if let Err(e) = written {
            let _ = segment.delete();
            return Err(e.into());
        }
        Ok(Self {
            inner: Some(Open {
                dir: dir.to_path_buf(),
                name,
                root: root.try_clone()?,
                settings: settings.clone(),
                log: Mutex::new(Log {
                    segment,
                    len: header.len() as u64,
                    state,
                    since_roll: 0,
                    broken: false,
                    keep: false,
                    pending: Vec::new(),
                    roll_due: false,
                }),
            }),
        })
    }

    /// Group `index` of the transfer's list opens. Written with the next
    /// entry, in the same append: nothing of the group is on the disk
    /// before something in it is.
    pub fn group(&self, index: u32, group: &Group) -> Result<(), JournalError> {
        let Some(open) = &self.inner else {
            return Ok(());
        };
        let entry = Entry::Group {
            index,
            first: group.first,
            len: group.len,
        };
        let mut log = lock(&open.log);
        if log.broken {
            return Err(JournalError::Broken);
        }
        log.state.apply(entry.clone())?;
        log.pending.push(entry);
        Ok(())
    }

    /// The partial file `file` was created as `partial` in `dir`, with no
    /// byte yet, for the entry at `place`, which is published as `name` and
    /// declared `size` bytes. Call it before writing to the file: one that
    /// cannot be recorded is not, and the caller removes it. `dir` must be
    /// `root` or a folder opened below it by name; one renamed or moved
    /// since, and still below `root`, is recorded where it is now (see the
    /// [crate] docs).
    pub fn started(
        &self,
        place: u32,
        dir: &Directory,
        file: &File,
        partial: &OsStr,
        name: &str,
        size: u64,
    ) -> Result<(), JournalError> {
        self.started_many(
            dir,
            &[Start {
                place,
                file,
                partial,
                name,
                size,
            }],
        )
    }

    /// A bundle's partial files were created in `dir`, with no byte yet:
    /// recorded in one append and one sync.
    pub fn started_many(&self, dir: &Directory, starts: &[Start<'_>]) -> Result<(), JournalError> {
        let Some(open) = &self.inner else {
            return Ok(());
        };
        open.started(dir, starts)
    }

    /// The entry at `place` is settled: published after its hash matched,
    /// found already there, or ended with an outcome the person was shown.
    /// Its partial, if any, is no longer the journal's.
    pub fn done(&self, place: u32) -> Result<(), JournalError> {
        self.done_many(&[place])
    }

    /// A bundle's entries are settled, in one entry.
    pub fn done_many(&self, places: &[u32]) -> Result<(), JournalError> {
        let Some(open) = &self.inner else {
            return Ok(());
        };
        let entry = match places {
            [place] => Entry::Done { place: *place },
            _ => Entry::DoneMany(places.to_vec()),
        };
        open.append(vec![entry], false)
    }

    /// The partial of the entry at `place` went without the entry being
    /// settled: cut, to come again, or removed.
    pub fn ended(&self, place: u32) -> Result<(), JournalError> {
        let Some(open) = &self.inner else {
            return Ok(());
        };
        open.append(vec![Entry::Ended { place }], false)
    }

    /// What the journal says now; `None` for one that records nothing.
    pub fn progress(&self) -> Option<Progress> {
        let open = self.inner.as_ref()?;
        Some(lock(&open.log).state.progress.clone())
    }

    /// The partials recorded and not yet settled nor ended.
    pub fn partials(&self) -> Vec<Started> {
        self.inner
            .as_ref()
            .map(|open| lock(&open.log).state.live.clone())
            .unwrap_or_default()
    }

    /// Ends as a crash would: the journal is closed and left for
    /// [`clean_up`].
    #[cfg(test)]
    fn crash(self) {
        if let Some(open) = &self.inner {
            let mut log = lock(&open.log);
            log.keep = true;
            log.broken = true;
        }
    }

    #[cfg(test)]
    fn segment_path(&self) -> PathBuf {
        let open = self.inner.as_ref().expect("a journal that records");
        lock(&open.log).segment.path.clone()
    }
}

/// A number unlike any earlier process's: the standard library's hasher
/// keys come from the system's random source.
fn salt() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    hasher.finish()
}

fn sync(file: &File) -> io::Result<()> {
    #[cfg(test)]
    seams::SYNCS.set(seams::SYNCS.get() + 1);
    file.sync_data()
}

/// Syncs the folder `dir`, so a segment just created there is found after
/// a crash (see [`files::sync_dir`]).
fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(test)]
    seams::DIR_SYNCS.set(seams::DIR_SYNCS.get() + 1);
    files::sync_dir(dir)
}

impl Open {
    /// The volume of the journal's root: a file ID names a file there only.
    #[cfg(windows)]
    fn root_volume(&self) -> Result<u64, JournalError> {
        let volume = self.root.identity()?.volume;
        #[cfg(test)]
        if seams::OTHER_VOLUME.get() {
            return Ok(volume ^ 1);
        }
        Ok(volume)
    }

    fn started(&self, dir: &Directory, starts: &[Start<'_>]) -> Result<(), JournalError> {
        let mut many = Vec::with_capacity(starts.len());
        for start in starts {
            let id = identity(start.file)?;
            let dir_id = dir.identity()?;
            // A file ID is unique on its volume only: cleanup looks on the folder's.
            if id.volume != dir_id.volume {
                return Err(JournalError::OtherVolume);
            }
            #[cfg(windows)]
            if id.volume != self.root_volume()? {
                return Err(JournalError::OtherVolume);
            }
            many.push(Started {
                place: start.place,
                folder: 0,
                name: start.name.to_string(),
                partial: start
                    .partial
                    .to_str()
                    .ok_or(JournalError::NameNotText)?
                    .to_string(),
                size: start.size,
                file: id.file,
            });
        }
        // Folders are numbered under the same lock as the write, so two
        // streams into new folders never take one number.
        self.append_with(
            |state| {
                let mut entries = Vec::new();
                let folder = self.folder_of(state, dir, &mut entries)?;
                let mut many = many.clone();
                for started in &mut many {
                    started.folder = folder;
                }
                entries.push(match many.len() {
                    1 => Entry::Started(many.pop().expect("one")),
                    _ => Entry::StartedMany(many),
                });
                Ok(entries)
            },
            true,
        )
    }

    /// The number of `dir` in the journal, adding a `Folder` entry to
    /// `entries` for each level below the root not yet named.
    fn folder_of(
        &self,
        state: &State,
        dir: &Directory,
        entries: &mut Vec<Entry>,
    ) -> Result<u32, JournalError> {
        let id = dir.identity()?;
        if id == self.root.identity()? {
            return Ok(0);
        }
        #[cfg(windows)]
        if id.volume != self.root_volume()? {
            return Err(JournalError::OtherVolume);
        }
        let find = |parent: u32, level: &Level| {
            state
                .folders
                .values()
                .find(|f| {
                    f.parent == parent
                        && f.name == level.name
                        && f.device == level.device
                        && f.id == level.id
                })
                .map(|f| f.n)
        };
        let mut next = state.folders.keys().next_back().map_or(1, |n| n + 1);
        let mut parent = 0;
        for (level, _) in location::levels(&self.root, dir)? {
            parent = match find(parent, &level) {
                Some(n) => n,
                None => {
                    let n = next;
                    next += 1;
                    entries.push(Entry::Folder(Folder {
                        n,
                        parent,
                        name: level.name,
                        device: level.device,
                        id: level.id,
                    }));
                    n
                }
            };
        }
        Ok(parent)
    }

    /// Appends `entries` as one write, synced when `sync`; when the mark
    /// moves, or a transfer without groups has ended enough partials, the
    /// next segment begins. An append that would pass
    /// [`Settings::segment_bytes`] begins the next segment first, whose
    /// snapshot holds only what is live, and is tried once more there.
    fn append(&self, entries: Vec<Entry>, sync_now: bool) -> Result<(), JournalError> {
        self.append_with(|_| Ok(entries.clone()), sync_now)
    }

    /// [`Self::append`] with the entries made from the state, under the lock:
    /// made again from the new segment's state when it rolls first.
    fn append_with(
        &self,
        make: impl Fn(&State) -> Result<Vec<Entry>, JournalError>,
        sync_now: bool,
    ) -> Result<(), JournalError> {
        let mut log = lock(&self.log);
        if log.broken {
            return Err(JournalError::Broken);
        }
        let most = self.settings.segment_bytes;
        let mut rolled = false;
        let (next, bytes, moved, settled) = loop {
            let entries = make(&log.state)?;
            let mut next = log.state.clone();
            let mut moved = 0;
            let mut settled = 0;
            let mut bytes = Vec::new();
            // The groups opened since, already in the state.
            for entry in &log.pending {
                bytes.extend_from_slice(&frame(&entry.encode()));
            }
            for entry in entries {
                settled += match &entry {
                    Entry::Done { .. } | Entry::Ended { .. } => 1,
                    Entry::DoneMany(places) => places.len(),
                    _ => 0,
                };
                bytes.extend_from_slice(&frame(&entry.encode()));
                moved += next.apply(entry)?;
            }
            if log.len + bytes.len() as u64 <= most as u64 {
                break (next, bytes, moved, settled);
            }
            // Entries that settle nothing (a group's churn of starts and
            // ends) fill a segment no mark move rolls: a fresh one holds
            // only what is live. Once, so a segment stays bounded.
            if rolled {
                return Err(JournalError::TooLarge { most });
            }
            // A failed roll is reported as itself: a full disk is not a large entry.
            self.roll(&mut log)?;
            log.roll_due = false;
            rolled = true;
        };
        let at = log.len;
        let written = (|| -> io::Result<()> {
            let mut file = &log.segment.file;
            file.seek(SeekFrom::Start(at))?;
            #[cfg(test)]
            if seams::FAIL_WRITES.get() > 0 {
                seams::FAIL_WRITES.set(seams::FAIL_WRITES.get() - 1);
                file.write_all(&bytes[..bytes.len() / 2])?;
                return Err(io::Error::other("a write cut short"));
            }
            file.write_all(&bytes)?;
            if sync_now {
                sync(file)?;
            }
            Ok(())
        })();
        if let Err(e) = written {
            // Cut back to the entries before, or a later append would follow a torn one.
            if log.segment.file.set_len(at).is_err() {
                log.broken = true;
            }
            return Err(e.into());
        }
        log.len += bytes.len() as u64;
        log.state = next;
        log.pending.clear();
        log.since_roll += settled;
        let due = log.roll_due
            || if log.state.grouped {
                moved > 0
            } else {
                log.since_roll >= self.settings.roll_every.max(1)
            };
        if due {
            // The entries are written; a roll that fails is tried again at
            // the next append, and the old segment still holds everything.
            match self.roll(&mut log) {
                Ok(()) => log.roll_due = false,
                Err(JournalError::Broken) => return Err(JournalError::Broken),
                Err(_) => log.roll_due = true,
            }
        }
        Ok(())
    }

    /// Starts the next segment: its snapshot written and synced first, then
    /// the old segment deleted.
    fn roll(&self, log: &mut Log) -> Result<(), JournalError> {
        let mut state = log.state.clone();
        state.seq += 1;
        // The new segment names only the folders its partials need.
        let needed = state.needed_folders();
        state.folders = needed.into_iter().map(|f| (f.n, f)).collect();
        let header = frame(&state.header());
        let most = self.settings.segment_bytes;
        if header.len() > most {
            return Err(JournalError::TooLarge { most });
        }
        let path = self.dir.join(segment_name(&self.name, state.seq));
        #[cfg(test)]
        if seams::FAIL_ROLL.replace(false) {
            return Err(io::Error::from(io::ErrorKind::Other).into());
        }
        #[cfg(test)]
        if seams::CRASH.get() == Some(seams::Crash::BeforeSnapshot) {
            return Err(log.crash());
        }
        let file = files::create(&path)?;
        let segment = Held { file, path };
        #[cfg(test)]
        if seams::CRASH.get() == Some(seams::Crash::TornSnapshot) {
            let _ = (&segment.file).write_all(&header[..header.len() / 2]);
            return Err(log.crash());
        }
        // The new name reaches the disk before the old segment goes.
        let written = (&segment.file)
            .write_all(&header)
            .and_then(|()| sync(&segment.file))
            .and_then(|()| sync_dir(&self.dir));
        if let Err(e) = written {
            // The old segment still holds everything.
            let _ = segment.delete();
            return Err(e.into());
        }
        #[cfg(test)]
        if seams::CRASH.get() == Some(seams::Crash::BeforeOldDeleted) {
            return Err(log.crash());
        }
        // A deletion that fails leaves an older segment, which a reader passes over.
        let _ = log.segment.delete();
        log.segment = segment;
        log.len = header.len() as u64;
        log.state = state;
        log.since_roll = 0;
        // The snapshot holds the groups opened since.
        log.pending.clear();
        Ok(())
    }
}

impl Log {
    /// A crash acted out by a test: nothing more is written, and the files
    /// are left as they are.
    #[cfg(test)]
    fn crash(&mut self) -> JournalError {
        self.broken = true;
        self.keep = true;
        JournalError::Broken
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        let log = lock(&self.log);
        // A partial file still recorded is left for cleanup.
        if log.state.live.is_empty() && !log.keep {
            let _ = log.segment.delete();
        }
    }
}

/// What a cleanup did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Cleaned {
    /// Partial files removed.
    pub removed: usize,
    /// Recorded files left alone: renamed, replaced, gone, on another
    /// disk, on a volume that could not be opened, or kept by the caller.
    pub left: usize,
    /// Journals kept for a later cleanup: a volume that could not be
    /// opened (a removable disk not plugged in), or a partial that could
    /// not be removed and may yet be.
    pub kept: usize,
}

/// Removes the partial files the journals in `dir` recorded, each only
/// through a handle checked to be that file, and the journals with them.
/// [`clean_up_keeping`] with nothing kept.
pub fn clean_up(dir: &Path, settings: &Settings) -> Result<Cleaned, JournalError> {
    clean_up_keeping(dir, settings, &|_| false)
}

/// One journal file taken by a cleanup.
struct Taken {
    held: Held,
    bytes: Vec<u8>,
}

/// Removes the partial files the journals in `dir` recorded, each only
/// through a handle checked to be that file, except those `keep` names (a
/// partial a valid resume record still keeps), and the journals with them.
///
/// Of a journal's log, every segment whose header is whole is read, the
/// highest-numbered first: a crash between two segments leaves both, and
/// each partial either lists is removed once, the second look counted as
/// left. A journal of the whole-record format of before is read too.
///
/// A journal is removed once each of its partials is removed, gone,
/// replaced or kept. One whose volume could not be opened, or with a
/// partial that failed otherwise, is kept for a later cleanup, at most for
/// [`Settings::keep_for`] and [`Settings::keep_most`] journals: past
/// either, the oldest is removed and its partials left.
///
/// A journal still open in a running transfer, in this process or another,
/// is left alone; so is one that cannot be opened, and a link or other
/// reparse point named as a journal. A damaged journal is removed and
/// touches nothing. A missing `dir` has nothing to clean.
pub fn clean_up_keeping(
    dir: &Path,
    settings: &Settings,
    keep: &dyn Fn(&PartialAt) -> bool,
) -> Result<Cleaned, JournalError> {
    let mut cleaned = Cleaned::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(cleaned),
        Err(e) => return Err(e.into()),
    };
    let mut old = Vec::new();
    let mut logs: std::collections::BTreeMap<String, Vec<(u64, PathBuf)>> = Default::default();
    for entry in entries {
        let path = entry?.path();
        match path.extension().and_then(OsStr::to_str) {
            Some(EXTENSION) => old.push(path),
            Some(LOG_EXTENSION) => {
                let Some((name, seq)) = log_name(&path) else {
                    continue;
                };
                logs.entry(name).or_default().push((seq, path));
            }
            _ => {}
        }
    }
    // Journals kept, newest change first, each held until the end.
    let mut kept: Vec<(std::time::SystemTime, Vec<Held>)> = Vec::new();
    let mut settle = |held: Vec<Held>, again: bool| {
        if again {
            // A time it cannot read counts as old: the bound still holds.
            let changed = held
                .first()
                .and_then(|h| h.file.metadata().and_then(|m| m.modified()).ok());
            let fresh =
                changed.filter(|&at| at.elapsed().unwrap_or(Duration::ZERO) <= settings.keep_for);
            if let Some(at) = fresh {
                let place = kept.partition_point(|(other, _)| *other >= at);
                kept.insert(place, (at, held));
                if kept.len() > settings.keep_most
                    && let Some((_, oldest)) = kept.pop()
                {
                    for h in oldest {
                        let _ = h.delete();
                    }
                }
                return;
            }
        }
        for h in held {
            let _ = h.delete();
        }
    };
    for path in old {
        let Ok((file, bytes)) = files::take(&path, settings.record_bytes) else {
            // In use by a running transfer, a link, or unreadable: left.
            continue;
        };
        let mut again = false;
        if let Ok(record) = Record::decode(&bytes, settings) {
            let (done, more) = remove_all(&record, settings);
            cleaned.removed += done.removed;
            cleaned.left += done.left;
            again = more;
        }
        settle(vec![Held { file, path }], again);
    }
    for (_, mut segments) in logs {
        // The highest-numbered first.
        segments.sort_by_key(|s| std::cmp::Reverse(s.0));
        let mut taken = Vec::new();
        let mut in_use = false;
        for (_, path) in segments {
            match files::take(&path, settings.segment_bytes) {
                Ok((file, bytes)) => taken.push(Taken {
                    held: Held { file, path },
                    bytes,
                }),
                Err(_) => in_use = true,
            }
        }
        if in_use {
            // One segment held: the transfer runs, or is not whole to read.
            continue;
        }
        let mut again = false;
        for one in &taken {
            let Ok(segment) = read_segment(&one.bytes, settings) else {
                continue;
            };
            for at in segment.places() {
                let Some(at) = at else {
                    cleaned.left += 1;
                    continue;
                };
                if keep(&at) {
                    cleaned.left += 1;
                    continue;
                }
                match at.remove_checked(settings) {
                    Ok(()) => cleaned.removed += 1,
                    Err(why) => {
                        again |= why.may_change();
                        cleaned.left += 1;
                    }
                }
            }
        }
        settle(taken.into_iter().map(|t| t.held).collect(), again);
    }
    cleaned.kept = kept.len();
    Ok(cleaned)
}

/// A segment file's journal name and sequence number:
/// `<name>.<seq>.jlog`.
fn log_name(path: &Path) -> Option<(String, u64)> {
    let stem = path.file_stem()?.to_str()?;
    let (name, seq) = stem.rsplit_once('.')?;
    Some((name.to_string(), seq.parse().ok()?))
}

impl Segment {
    /// Where each live partial is; `None` for one whose folder the segment
    /// does not name.
    pub fn places(&self) -> Vec<Option<PartialAt>> {
        self.partials()
            .iter()
            .map(|started| {
                let chain = self.chain(started.folder)?;
                Some(PartialAt {
                    location: Location {
                        root: self.root().clone(),
                        levels: chain
                            .into_iter()
                            .map(|f| Level {
                                name: f.name.clone(),
                                device: f.device,
                                id: f.id,
                            })
                            .collect(),
                    },
                    partial: started.partial.clone(),
                    file: started.file,
                })
            })
            .collect()
    }
}

/// A journal read back after a crash, to continue its transfer.
#[derive(Debug, Clone)]
pub struct Recovered {
    /// The journal's name in its folder.
    pub name: String,
    /// The highest-numbered segment whose header is whole, with every
    /// entry read applied, and the entries whose settling was lost and
    /// were found published (Windows) settled too.
    pub segment: Segment,
    /// The places settled because their published file was found: its
    /// recorded ID, under its expected or a numbered name, with its size.
    /// Windows only, where a file ID is never reused for another file.
    pub settled: Vec<u32>,
    /// Places whose partial was found published under the name here, but
    /// not settled: on Unix an inode is reused as soon as a file is
    /// deleted, so the file is only a candidate, to be settled only when
    /// its whole-file hash equals the sender's. Until then the entry is
    /// fetched again, and the file is never removed.
    pub candidates: Vec<(u32, String)>,
}

impl Recovered {
    /// The progress: what a resume skips.
    pub fn progress(&self) -> &Progress {
        self.segment.progress()
    }

    /// The partials still being written when the journal stopped.
    pub fn partials(&self) -> &[Started] {
        self.segment.partials()
    }
}

/// Reads back the journals in `dir` no running transfer holds, each from
/// its highest-numbered whole segment, without changing anything: what
/// each transfer settled and which partials it was writing. A partial
/// whose settling was lost is looked for under its published name (see
/// [`Recovered::settled`] and [`Recovered::candidates`]).
pub fn recover(dir: &Path, settings: &Settings) -> Result<Vec<Recovered>, JournalError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut logs: std::collections::BTreeMap<String, Vec<(u64, PathBuf)>> = Default::default();
    for entry in entries {
        let path = entry?.path();
        if path.extension() == Some(OsStr::new(LOG_EXTENSION))
            && let Some((name, seq)) = log_name(&path)
        {
            logs.entry(name).or_default().push((seq, path));
        }
    }
    let mut out = Vec::new();
    'journals: for (name, mut segments) in logs {
        segments.sort_by_key(|s| std::cmp::Reverse(s.0));
        let mut best = None;
        for (_, path) in &segments {
            let Ok((file, bytes)) = files::take(path, settings.segment_bytes) else {
                continue 'journals;
            };
            drop(file);
            if best.is_none()
                && let Ok(segment) = read_segment(&bytes, settings)
            {
                best = Some(segment);
            }
        }
        let Some(mut segment) = best else {
            continue;
        };
        let mut settled = Vec::new();
        let mut candidates = Vec::new();
        for (started, at) in segment
            .partials()
            .to_vec()
            .into_iter()
            .zip(segment.places())
        {
            let Some(dir) = at.and_then(|at| at.location.find().ok()) else {
                continue;
            };
            let Some(found) =
                location::find_published(&dir, &started.name, started.file, started.size, settings)
            else {
                continue;
            };
            if cfg!(windows) {
                // A file ID never names another file on the volume.
                if segment
                    .state
                    .apply(Entry::Done {
                        place: started.place,
                    })
                    .is_ok()
                {
                    settled.push(started.place);
                }
            } else {
                candidates.push((started.place, found));
            }
        }
        out.push(Recovered {
            name,
            segment,
            settled,
            candidates,
        });
    }
    Ok(out)
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
fn removable(name: &str, settings: &Settings) -> bool {
    // "name:stream" would open a stream of a file, not the file.
    !settings.partial_ending.is_empty()
        && name.ends_with(settings.partial_ending.as_str())
        && !name.contains(['\\', '/', ':'])
        && name != settings.partial_ending
}

/// Switches tests use to act out a failing disk or a crash.
#[cfg(test)]
// Android's emulated thread locals make Clippy ask for the `const` already there.
#[allow(clippy::missing_const_for_thread_local)]
pub(crate) mod seams {
    use std::cell::Cell;

    /// Where a crash is acted out while the next segment begins.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Crash {
        /// After the entry that moved the mark, before the new segment.
        BeforeSnapshot,
        /// Halfway through the new segment's snapshot.
        TornSnapshot,
        /// After the new segment's sync, before the old one is deleted.
        BeforeOldDeleted,
    }

    thread_local! {
        /// How many of this thread's next appends fail halfway.
        pub static FAIL_WRITES: Cell<u32> = const { Cell::new(0) };
        /// No recorded volume opens on this thread: a disk unplugged.
        pub static NO_VOLUME: Cell<bool> = const { Cell::new(false) };
        /// How many syncs this thread made.
        pub static SYNCS: Cell<u32> = const { Cell::new(0) };
        /// How many folder syncs this thread made.
        pub static DIR_SYNCS: Cell<u32> = const { Cell::new(0) };
        /// The journal's root acted out as on another volume (Windows).
        pub static OTHER_VOLUME: Cell<bool> = const { Cell::new(false) };
        /// The next segment of this thread's journal fails to begin, once.
        pub static FAIL_ROLL: Cell<bool> = const { Cell::new(false) };
        /// A crash to act out at the next segment.
        pub static CRASH: Cell<Option<Crash>> = const { Cell::new(None) };
        /// How this thread reads a held folder's present place.
        pub static PLACE: Cell<Place> = const { Cell::new(Place::Read) };
    }

    /// A reading of a held folder's present place, acted out.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Place {
        /// As the system reads it.
        Read,
        /// The system reads it and this reading fails.
        Fails,
        /// The system has no way to read it, as on macOS.
        Unknown,
    }
}

#[cfg(test)]
mod log_tests;
#[cfg(test)]
mod tests;
