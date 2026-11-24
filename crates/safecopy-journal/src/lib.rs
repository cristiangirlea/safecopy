//! Crash records for partial files, and cleanup that removes only the file
//! it recorded.
//!
//! A [`Journal`] is kept in a folder the caller gives, one per transfer (a
//! folder, a paste, a session's single files): an append-only log of what
//! the transfer has done, in short segment files. From the moment each
//! partial file is created, and before it gets a byte, the journal records
//! the file's folder, its name, its own ID, its second identity where the
//! system gives one, and the entry it is for, synced to the disk. A partial kept from an earlier transfer and opened again
//! to continue is recorded the same way, with the bytes it has. For a transfer that goes by groups (`safecopy-manifest`'s
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
//! # A partial kept to continue later
//!
//! A caller that keeps a partial file when its stream is cut
//! (`safecopy-resume`'s records) calls nothing for it then: neither
//! [`Journal::done`] nor [`Journal::ended`]. The journal goes on listing
//! it, and is left on the disk when it is dropped. [`clean_up_keeping`]
//! then leaves the partial while the caller's rule keeps it, and keeps the
//! journal with it, so the cleanup after the rule lets go (the resume
//! record expired, was torn or is gone) still knows the file and removes
//! it, and the journal after it. A transfer that continues the partial in
//! a later run lists it in that run's journal ([`Journal::started`] takes
//! a file that has bytes); in the same run it is still listed, and needs
//! no second call. Once the caller removes a kept partial itself, it calls
//! [`Journal::ended`] if that journal still runs.
//!
//! Journals kept this way count toward [`Settings::keep_most`] and
//! [`Settings::keep_for`] with the ones kept for a disk that is away, so
//! both must fit what the caller keeps: at least as many journals as it
//! keeps partials (each may be alone in its journal), for at least as long
//! as it keeps one. A partial cut and continued in `n` runs is listed in
//! `n` kept journals, one per run, so the bound is reached with fewer
//! partials than it counts. Nothing is orphaned by that: the oldest
//! journals go first, and the newest one that lists the file stays. The
//! defaults fit `safecopy-resume`'s (256 files, 24
//! hours) with as many journals again to spare. A journal past either
//! bound is removed and its partials left: nothing is removed that a
//! journal does not prove was written by it.
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
//! # A folder on another file system
//!
//! Every partial of a journal is on the root's own volume or device, on
//! every system. [`Journal::started`], [`Journal::started_many`] and
//! [`PartialAt::of`] refuse with [`JournalError::OtherVolume`] a folder
//! that is on another one, and a folder reached through one that is:
//! another volume on Windows, a file system mounted below the root on
//! Unix. A partial is found again by its file ID or its inode, which names
//! a file on one volume or device only. A caller that writes below such a
//! folder starts a journal whose root is on that file system.
//!
//! The device is the one the system reports for the open folder (`st_dev`
//! on Unix, the volume's serial number on Windows), compared with the
//! root's at each level of the walk down from it. Nothing else is asked,
//! so what counts as another file system is the system's answer: a folder
//! of the root's own file system bound to a second place below the root
//! reports the root's device where a bind mount keeps it (Linux does) and
//! is taken; a btrfs subvolume reports a device number of its own and is
//! refused like any mount, as is an overlay, a network or a memory file
//! system mounted below the root; a way that leaves the root's device and
//! comes back to it is refused at the level that leaves.
//!
//! Finding a partial again goes by the same rule ([`Location::open`],
//! [`PartialAt::reopen`], [`PartialAt::try_reopen`],
//! [`PartialAt::present`], [`PartialAt::remove`], [`clean_up`] and
//! [`recover`]):
//!
//! - **A recorded level on another device than the root's** stops the
//!   walk before anything is opened at it: [`Left::Volume`]. Only a
//!   journal written on Unix before this rule holds one, or a damaged one. Its partial is
//!   never opened or removed: nothing crosses the boundary. Cleanup counts
//!   it as left, and since no later cleanup would find it either, the
//!   entry keeps no journal: the partial file stays where it is.
//! - **A file system mounted since over a recorded folder** puts another
//!   device and inode under the folder's name: [`Left::Replaced`], as for
//!   any folder put in its place. The partial below the mount is left.
//! - **Windows** opens a recorded folder by its ID on the root's volume
//!   only: one recorded with another volume's serial number is
//!   [`Left::Volume`].
//!
//! # A second identity
//!
//! Unix may give a removed file's inode to the next file made (ext4 does
//! at once; APFS and btrfs hardly ever). A journal's
//! entry can outlive its partial (its ending could not be written, an old
//! segment was left under its name, the person removed the file by hand),
//! and a new partial of the same name in the same folder can be given the
//! same inode. By the inode alone, a cleanup in another process would
//! take that new file, still being written, for the one recorded.
//!
//! So a partial is recorded with a second identity where the system and
//! the file system give one ([`Second`], which says what each gives): its
//! birth time and its inode's generation, each where it is given. Wherever a recorded partial is
//! found again, the second identity is compared after the ID:
//! [`clean_up`], [`PartialAt::remove`], [`PartialAt::reopen`],
//! [`PartialAt::try_reopen`], [`PartialAt::present`], and the published
//! file [`recover`] looks for.
//!
//! - **Another second identity** under the recorded name and ID is
//!   another file: [`Left::Replaced`]. It is not opened for writing, cut
//!   or removed, and its journal ends. On macOS and the BSDs a birth time
//!   earlier than the one recorded is not another: a file's birth time
//!   moves back there when its modified time is set to an earlier one,
//!   as a publish sets it, and a file made since is born later.
//! - **One that cannot be read now**, where one was recorded (a system or
//!   a sandbox that no longer answers), shows nothing either way: the
//!   file is left, [`Left::Io`], and its journal kept for a later cleanup.
//! - **None recorded** compares nothing more, and the partial is known by
//!   its ID alone, as before: a journal of before, a file system that
//!   gives none, Windows.
//!
//! What stays of the limit: a file system that gives no second identity,
//! and, where only a birth time is given, two files made within one step
//! of the clock a file system stamps its files with (see [`Second`]).
//!
//! **Windows on FAT and exFAT.** NTFS and ReFS keep a file ID for the
//! life of its file. A FAT or exFAT volume makes one from where the
//! file's entry sits in its folder, and gives it to another file later.
//! [`Journal::start`] takes such a volume, which has a volume path, and
//! its partials are recorded. A cleanup finds a recorded folder only by
//! opening it by its ID, and the FAT driver refuses every open by ID: no
//! folder is found, and no file there is removed by a cleanup, the one
//! recorded or another. Where a volume does open a folder by such an ID,
//! a file under the recorded name with the recorded ID is taken for the
//! one recorded: Windows reads no second identity.
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
//! the highest-numbered segment whose header is whole. When a file is
//! already under the next segment's name, the journal leaves it and takes
//! the number after: the numbers of a journal's segments grow, and need
//! not follow one another.
//!
//! There are two formats, and this version reads both. In format 2 each
//! started partial, in the snapshot or in an entry, ends with its second
//! identity; format 1 has none and is otherwise the same. A journal is
//! written in format 2 on Unix, whether its file system gives a second
//! identity or not, and in format 1 on Windows, byte for byte as before.
//! A version that knows only format 1 reads a format 2 segment as one with
//! no whole header: its cleanup removes the journal and touches no file,
//! so the partials listed stay on the disk, and its [`recover`] gives
//! nothing for that journal.
//!
//! A journal syncs once per partial started (once for a bundle's, with
//! [`Journal::started_many`]) and once per mark move; settling and ending
//! are written without a sync of their own and reach the disk with the
//! next.
//!
//! # A cleanup beside a running journal
//!
//! A cleanup may run in one process while a journal is written in
//! another. It goes by one rule: a running journal holds every segment it
//! made, from before the segment has its name until after that name is
//! gone. So a segment a cleanup holds under its name is no running
//! journal's, and what it lists was left behind. The journal keeps its
//! side while it starts its next segment: the new one is held before it is
//! named, the old one's name is removed before the old one is let go, and
//! an old one whose name cannot be removed stays held. The journal starts
//! no further segment until it can be: entries still go into the segment
//! it has, and one that needs the next segment to fit fails with the
//! system's error for the removal.
//!
//! - **Windows** makes or opens a file and holds it in one step (an open
//!   that shares no writing or deleting), so a name is never there with
//!   its file not yet held, and a cleanup's open of a held segment is
//!   refused: neither side has a moment between finding a file and
//!   holding it, so no temporary name is used there. What a roll does
//!   with a taken next name and with an old segment it cannot remove
//!   holds on every system.
//! - **Linux, macOS and Android** lock a file after opening it, and the
//!   lock is on the file, not on its name. A cleanup therefore checks,
//!   once it has the lock, that the file is still the one its name
//!   reaches: a segment its journal let go of since the cleanup opened it
//!   has lost its name, and is left. A journal makes a segment under a
//!   temporary name (ending `jnew`), locks it, gives it its own name
//!   beside the first without replacing anything, and removes the
//!   temporary one. [`clean_up`] removes a temporary name it can lock
//!   under that name: a crash left it, and it is never read as a journal.
//!   A journal that loses its temporary name to a cleanup before its lock
//!   makes another.
//! - **Where a folder gives a file no second name** (a file system
//!   without hard links, or a system whose rules refuse them, as Android's
//!   do to an app), a segment is made under its own name and then locked.
//!   A cleanup may lock the empty file in between: it lists nothing, so
//!   nothing is removed for it, and the journal takes the next name (a
//!   first segment's) or the next number (a later one's). The empty file
//!   stays until a cleanup takes it with no journal running under its
//!   name.
//! - **A child process started just as a journal is let go** (Unix). The
//!   lock is the open file's, and a child a program starts holds a copy
//!   of every open file of that program from the moment it is made until
//!   it becomes its own program, the files marked to be closed then
//!   among them. A journal dropped in that moment is closed here and
//!   still locked there. [`clean_up`] and [`recover`] then read it as a
//!   running journal's: they remove nothing and report nothing for it,
//!   and leave it whole. That is safe, as it is for any running journal:
//!   no partial is removed and no record is lost, and a call after the
//!   child has become its own program finds the journal let go. That is
//!   a moment after the start is back in the program: the system closes
//!   the child's copies as the last step of the change, and Linux hands
//!   the start back before it (seen in 7 of 5000 starts). A caller that
//!   needs it at once, to continue its transfer, calls again. A child
//!   that never becomes another program (a fork that goes on as the same
//!   one) keeps its copies, and the journals with them, until it closes
//!   them or ends. Windows gives a child no handle of a journal.
//! - **A journals folder on a network file system** has weaker locks on
//!   Unix: Linux turns this lock into a lock a process holds. A cleanup
//!   in the process that runs the journal is given it too, and the
//!   process loses it when it closes any handle of that file, as a
//!   cleanup or a recovery does with each segment it looks at. Whether
//!   another computer sees the lock is the server's matter. Keep the
//!   journals on a local disk, or run no cleanup and no recovery in a
//!   process that has a journal open there.
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
//!   into a link stops the walk, and so does one recorded on another device
//!   than the root's. A partial is removed by its name after its inode
//!   and its second identity are compared, so a file swapped in at that
//!   moment is not caught. Its inode can be reused as soon as a file is
//!   deleted, and not every file system gives a second identity, so a
//!   file found under a published name is only a candidate there, never
//!   settled ([`Recovered::candidates`]).
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
mod second;
#[cfg(windows)]
mod win;

pub use location::{Left, Level, Location, PartialAt};
pub use log::{Folder, Root, Segment, Started, read_segment};
pub use second::Second;

use files::Held;
use log::{Entry, State, frame};

/// The ending of a journal file of the whole-record format of before,
/// which [`clean_up`] still reads.
pub const EXTENSION: &str = "journal";

/// The ending of a segment of a journal's log.
pub const LOG_EXTENSION: &str = "jlog";

/// The ending of the name a new segment is made under on Unix before it
/// has its own: never read as a journal, and removed by [`clean_up`] when
/// a crash left it.
#[cfg(unix)]
const NEW_EXTENSION: &str = "jnew";

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
    /// volume could not be opened, a partial could not be removed, a
    /// partial the caller still keeps) for a later cleanup, from the
    /// journal's last change. Older, it is removed and its partials left.
    /// At least as long as the caller keeps a partial. 30 days.
    pub keep_for: Duration,
    /// The most journals [`clean_up`] keeps so; past it the oldest are
    /// removed and their partials left. At least the most partials the
    /// caller keeps, each of which may be alone in its journal, and more
    /// where partials are continued often: each run that continues one
    /// lists it in a journal of its own, and all of them are kept with
    /// it. Past the bound the oldest go, never the newest that lists a
    /// file. 512: the
    /// 256 files a `safecopy-resume` store keeps, and as many journals
    /// again waiting for a disk.
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
            keep_most: 512,
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
    /// The partial file, its folder, or a folder on the way to it from
    /// the root, is not on the root's volume or device as the system
    /// reports it (a disk or a file system mounted below the root): its
    /// file ID or inode would mean another file there. On every system.
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
    /// The partial file: just created, with no byte yet, or a kept one
    /// opened again with the bytes it has.
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
    /// A segment this journal is done with, whose name could not be
    /// removed: held until it can be.
    stale: Option<Held>,
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
        let state = State::new(location::root_of(root)?, settings, written(root));
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
                    stale: None,
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
    /// cannot be recorded is not, and the caller removes it. A partial kept
    /// from an earlier transfer and opened again is recorded the same way,
    /// before it is appended to, with the bytes it has: this journal's
    /// cleanup then knows it too. `dir` must be
    /// `root` or a folder opened below it by name; one renamed or moved
    /// since, and still below `root`, is recorded where it is now (see the
    /// [crate] docs). One on another volume or device than `root`'s, or
    /// reached through a folder that is, is refused with
    /// [`JournalError::OtherVolume`], on every system.
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

    /// A bundle's partial files were created in `dir`, with no byte yet
    /// (or kept ones opened again): recorded in one append and one sync.
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
    /// settled: removed, to come again from zero or not at all. Not for a
    /// partial kept to continue later, which stays listed (see the [crate]
    /// docs).
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

/// The format a new journal is written in: the second wherever a partial
/// may have a second identity to record, which is every Unix system. On
/// Windows none is read, and the format of before says all there is to
/// say, to a version of before too.
fn written(root: &Directory) -> u8 {
    // Nothing is read here but on Windows, where a test acts one out.
    if cfg!(unix) || Second::of(root.handle()) != Second::None {
        log::FORMAT_SECOND
    } else {
        log::FORMAT
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
                second: Second::of(start.file),
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
            let format = log.state.format;
            for entry in &log.pending {
                bytes.extend_from_slice(&frame(&entry.encode(format)));
            }
            for entry in entries {
                settled += match &entry {
                    Entry::Done { .. } | Entry::Ended { .. } => 1,
                    Entry::DoneMany(places) => places.len(),
                    _ => 0,
                };
                bytes.extend_from_slice(&frame(&entry.encode(format)));
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
    ///
    /// What a cleanup relies on is kept here: from before a segment has
    /// its name until after that name is gone, this journal holds it. The
    /// next segment is held before it is named, so it is named and held
    /// before the old one's name goes, and the old one's name goes before
    /// the old one is let go. A segment whose name cannot be removed stays
    /// held, and the journal starts no further segment until it can: this
    /// fails with that removal's error.
    fn roll(&self, log: &mut Log) -> Result<(), JournalError> {
        log.retire_stale()?;
        let mut state = log.state.clone();
        // The new segment names only the folders its partials need.
        let needed = state.needed_folders();
        state.folders = needed.into_iter().map(|f| (f.n, f)).collect();
        let most = self.settings.segment_bytes;
        #[cfg(test)]
        if seams::FAIL_ROLL.replace(false) {
            return Err(io::Error::from(io::ErrorKind::Other).into());
        }
        #[cfg(test)]
        if seams::CRASH.get() == Some(seams::Crash::BeforeSnapshot) {
            return Err(log.crash());
        }
        let mut tries = 0;
        let (segment, header) = loop {
            state.seq += 1;
            let header = frame(&state.header());
            if header.len() > most {
                return Err(JournalError::TooLarge { most });
            }
            let path = self.dir.join(segment_name(&self.name, state.seq));
            match files::create_held(&path) {
                Ok(file) => break (Held { file, path }, header),
                // A file under the next name is none this journal wrote:
                // it holds every one it did. Another's, or its own empty
                // one that a cleanup had, is left as it is for the cleanup
                // after this journal, and the number after is taken: a
                // reader goes by the highest.
                Err(e) if files::taken(&e) && tries + 1 < files::NAME_TRIES => tries += 1,
                Err(e) => return Err(e.into()),
            }
        };
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
        #[cfg(test)]
        let written = match seams::FAIL_SNAPSHOT.replace(false) {
            true => Err(io::Error::from(io::ErrorKind::StorageFull)),
            false => written,
        };
        if let Err(e) = written {
            // The old segment still holds everything.
            log.retire(segment);
            return Err(e.into());
        }
        #[cfg(test)]
        if seams::CRASH.get() == Some(seams::Crash::BeforeOldDeleted) {
            return Err(log.crash());
        }
        #[cfg(test)]
        seams::at(seams::Point::Rolled);
        let old = std::mem::replace(&mut log.segment, segment);
        // An older segment that stays is one a reader passes over.
        log.retire(old);
        log.len = header.len() as u64;
        log.state = state;
        log.since_roll = 0;
        // The snapshot holds the groups opened since.
        log.pending.clear();
        Ok(())
    }
}

impl Log {
    /// Lets go of `segment`, which this journal is done with: its name
    /// removed first. Named and let go, it would read to a cleanup as an
    /// ended journal's, and the partials it lists as left behind: one whose
    /// name cannot be removed stays held.
    fn retire(&mut self, segment: Held) {
        if let Err((segment, _)) = segment.retire() {
            // None is waiting: a roll begins only once the last is gone.
            self.stale = Some(segment);
        }
    }

    /// Removes the name of the segment still held from before, or says
    /// why it stays.
    fn retire_stale(&mut self) -> io::Result<()> {
        match self.stale.take().map(Held::retire) {
            Some(Err((segment, e))) => {
                self.stale = Some(segment);
                Err(e)
            }
            _ => Ok(()),
        }
    }

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
            if let Some(stale) = &log.stale {
                let _ = stale.delete();
            }
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
    /// opened (a removable disk not plugged in), a partial that could
    /// not be removed and may yet be, or a partial the caller keeps.
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
/// A journal is removed once each of its partials is removed, gone or
/// replaced. One that lists a partial `keep` names is kept with it, so a
/// later cleanup removes that partial once it is kept no more; so is one
/// whose volume could not be opened, or with a partial that failed
/// otherwise. A journal is kept at most for [`Settings::keep_for`], and
/// [`Settings::keep_most`] journals are: past either, the oldest is
/// removed and its partials left. A journal so lives as long as a partial
/// it lists is kept: on Unix, where an inode is reused once its file is
/// deleted, that is as long as it can meet a new partial of the same name
/// and inode in the same folder. It takes that one for the one it lists
/// only where neither has a second identity to tell them apart
/// ([`Second`]).
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
            // The name a segment was being made under. Taken, it is held
            // by nobody under that name: left by a crash, or a segment in
            // the making that will take another name on losing this one.
            // Held, it is in use and stays. It may be the second name of
            // a journal's segment: let go at once, before that is taken.
            #[cfg(unix)]
            Some(NEW_EXTENSION) if new_name(&path) => {
                if let Ok((file, _)) = files::take(&path, 0) {
                    let _ = files::delete(&file, &path);
                }
            }
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
            // One segment held, or let go since this folder was read: the
            // transfer runs, or is not whole to read.
            continue;
        }
        // Every segment taken is held here under its name. A running
        // journal holds each segment it made from before the segment has
        // its name until after that name is gone (see `Open::roll`): so
        // none of these is a running journal's, whatever it did since
        // this folder was read, and what they list is left behind.
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
                    // The journal stays to remove it once it is kept no more.
                    again = true;
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

/// Whether `path` is named as a segment in the making is:
/// `<process>-<count>.jnew`. No other name is a cleanup's to remove.
#[cfg(unix)]
fn new_name(path: &Path) -> bool {
    path.file_stem()
        .and_then(OsStr::to_str)
        .and_then(|stem| stem.split_once('-'))
        .is_some_and(|(process, count)| {
            process.parse::<u32>().is_ok() && count.parse::<u64>().is_ok()
        })
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
                    second: started.second,
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
            let Some(found) = location::find_published(&dir, &started, settings) else {
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

    /// A seam as it stands until a test sets it. Not a `const` initialiser:
    /// where thread locals are emulated (Android), Clippy asks for the
    /// `const` that is already there.
    fn unset<T: Default>() -> T {
        T::default()
    }

    thread_local! {
        /// How many of this thread's next appends fail halfway.
        pub static FAIL_WRITES: Cell<u32> = unset();
        /// No recorded volume opens on this thread: a disk unplugged.
        pub static NO_VOLUME: Cell<bool> = unset();
        /// How many syncs this thread made.
        pub static SYNCS: Cell<u32> = unset();
        /// How many folder syncs this thread made.
        pub static DIR_SYNCS: Cell<u32> = unset();
        /// The journal's root acted out as on another volume (Windows).
        pub static OTHER_VOLUME: Cell<bool> = unset();
        /// A folder, by its file ID or inode, acted out as a mount point:
        /// the walks read it as on this device.
        pub static OTHER_DEVICE: Cell<Option<(u128, u64)>> = unset();
        /// The next segment of this thread's journal fails to begin, once.
        pub static FAIL_ROLL: Cell<bool> = unset();
        /// A crash to act out at the next segment.
        pub static CRASH: Cell<Option<Crash>> = unset();
        /// How this thread reads a held folder's present place.
        pub static PLACE: Cell<Place> = unset();
        /// What every file and folder reports as its second identity on
        /// this thread, in place of what the system gives: `Some(None)`
        /// is a system that gives none.
        pub static SECOND: Cell<Option<crate::Second>> = unset();
        /// This thread's system moves a file's birth time back with its
        /// modified time, as macOS does.
        pub static BIRTH_MOVES_BACK: Cell<bool> = unset();
    }

    thread_local! {
        /// Run once, when this thread's next `files::hold` has opened its
        /// file and not yet locked it.
        pub static HOLD_OPENED: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = unset();
    }

    /// Where a test steps in while a segment is made, taken or let go.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Point {
        /// A cleanup has opened a file and not yet locked it (Unix).
        TakeOpened,
        /// A new segment's temporary name is made and not yet locked.
        #[cfg(unix)]
        TempMade,
        /// A new segment is made under its own name and not yet locked:
        /// in a folder that gives a file no second name.
        #[cfg(unix)]
        Made,
        /// The next segment is written and the old one still has its name.
        Rolled,
        /// The old segment's name is gone and the file is still held.
        Unlinked,
    }

    /// A point and what a test does there.
    type Hook = (Point, Box<dyn FnOnce()>);

    thread_local! {
        /// Run once, when this thread next comes to the point.
        pub static AT: std::cell::RefCell<Option<Hook>> = unset();
        /// The journals' folder acted out as one that gives a file no
        /// second name.
        #[cfg(unix)]
        pub static NO_LINKS: Cell<bool> = unset();
        /// The next segment's snapshot fails to be written, once.
        pub static FAIL_SNAPSHOT: Cell<bool> = unset();
        /// How many of this thread's next deletions of a segment fail.
        pub static FAIL_DELETES: Cell<u32> = unset();
    }

    /// Runs `hook` once, when this thread next comes to `point`.
    pub fn when(point: Point, hook: impl FnOnce() + 'static) {
        AT.set(Some((point, Box::new(hook))));
    }

    /// This thread is at `point`: the hook set for it runs.
    pub fn at(point: Point) {
        let hook = AT.with_borrow_mut(|at| match at {
            Some((set, _)) if *set == point => at.take().map(|(_, hook)| hook),
            _ => None,
        });
        if let Some(hook) = hook {
            hook();
        }
    }

    /// A reading of a held folder's present place, acted out.
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
    pub enum Place {
        /// As the system reads it.
        #[default]
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
mod meet_tests;
#[cfg(test)]
mod second_tests;
#[cfg(test)]
mod tests;
