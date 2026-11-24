//! Writing through a held folder.
//!
//! A [`Directory`] is a folder held open by handle. Everything below it is
//! opened relative to that handle, one name at a time, so a folder renamed
//! or replaced on the way cannot redirect a write:
//!
//! - links and junctions are never followed below the held folder (a link
//!   is refused with [`Refusal::Link`], see [`is_link`]);
//! - files are only ever created new, never opened over one already there;
//! - a finished file gets its name with [`Directory::publish`], which never
//!   replaces a file that took the name meanwhile, and takes the next
//!   numbered name instead;
//! - removal never goes through a link or outside the held folder; a file
//!   removed for an open handle is checked to be that file first.
//!
//! Errors are [`std::io::Error`]s. A refusal of this crate's own carries a
//! [`Refusal`], read back with [`refusal`].
//!
//! # Systems
//!
//! - **Linux and Android** publish by linking the open file through
//!   `/proc/self/fd`, so a staging name swapped meanwhile cannot supply the
//!   content. Without procfs they rename with `RENAME_NOREPLACE`, checking
//!   the staging name is still the open file before and after.
//! - **macOS** has no procfs: it renames with `RENAME_EXCL`, with the same
//!   checks.
//! - **Windows** renames the open handle itself, which refuses a taken
//!   name; the staging file is open without delete sharing, so it cannot
//!   be swapped. Files can carry the downloaded-file mark
//!   ([`Directory::mark`]); other systems have no such mark.
//! - Where none of these work (FAT, exFAT, some network shares), the file
//!   is copied from its open handle into a name created new.
//! - **Windows** removes a file by a handle opened without following a
//!   link, so a file that took the name after it was opened stays. **Unix**
//!   has no delete by handle: where an open file is meant
//!   ([`Directory::finish`], [`Directory::discard`]) it compares the name's
//!   device and inode with the open file's first, and leaves another file
//!   with [`Refusal::Replaced`]. A swap in the moment between that check
//!   and the removal is not caught. [`Directory::remove`] has no open file
//!   to compare: it removes the name.
//! - Where linking through procfs fails for the folder (procfs absent or
//!   denied, as in Android apps and some containers),
//!   [`Directory::publish`] remembers it on that handle and goes straight
//!   to the rename for every later name.
//! - Linux and Android read a folder's current place from procfs; macOS
//!   cannot, so [`Directory::final_path`] gives the path it was opened at,
//!   [`Directory::removed`] is always false there, and
//!   [`Directory::unfit`] finds a folder inside a kept-out one by that
//!   path.
//! - **Modified times** are set by handle ([`Directory::apply`]); a time
//!   the disk cannot hold leaves the arrival time. The **execute bit**
//!   exists only on Linux and macOS, where only the owner's is ever set,
//!   and only when the caller asks; Windows ignores it.
//! - A file's **exact name** ([`Directory::candidate`],
//!   [`Directory::existing`]) is read back from the handle on Windows; on
//!   Unix the folder is read for an entry of exactly those bytes, since
//!   macOS and casefolded Linux folders open other spellings of a name.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

mod candidate;
mod meta;
mod publish;
#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
mod win;
#[cfg(windows)]
use win as platform;

pub use candidate::Candidate;
pub use meta::{Applied, BitApplied, Meta, MetaRules, Time, TimeApplied};
pub use publish::{
    Finish, Marked, PublishError, PublishRules, Published, mark_path, part, publish_path, unique,
};

/// How many names a search for a free one tries where the caller sets no
/// other limit ([`PublishRules::name_tries`], [`Directory::claim`]): the
/// name asked for and its numbers up to 10 001, as far as the default
/// `safecopy_names::NameRules` number a name.
pub const NAME_TRIES: usize = 10_001;

/// Why this crate refused to open, create or name something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// A link or junction, which is never followed.
    Link,
    /// Neither a file nor a folder: a device, a pipe or a socket.
    NotFileOrFolder,
    /// A file where a folder was asked for.
    NotFolder,
    /// A name that is not one part (empty, `.`, `..`, or with a separator).
    LeavesFolder,
    /// A name that names a stream of a file (`name:stream`, Windows).
    Stream,
    /// A name with a NUL in it.
    Nul,
    /// A name longer than the system can pass in one call.
    TooLong,
    /// The folder returned a name it could not have (Windows).
    BadListing,
    /// The folder is on no volume of its own (a network share, Windows).
    NoVolume,
    /// The name no longer refers to the open file it was given for: another
    /// file took it, and was left as it is.
    Replaced,
    /// Every name tried for a new folder was taken
    /// ([`Directory::claim_within`]).
    NoNameLeft {
        /// The names tried.
        tried: usize,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The caller words it; this is the kind for logs and tests.
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for Refusal {}

fn refused(why: Refusal) -> io::Error {
    io::Error::other(why)
}

/// The [`Refusal`] in `error`, when this crate refused something.
pub fn refusal(error: &io::Error) -> Option<Refusal> {
    error.get_ref()?.downcast_ref::<Refusal>().copied()
}

/// Whether `error` is a link or junction this crate would not follow.
pub fn is_link(error: &io::Error) -> bool {
    refusal(error) == Some(Refusal::Link)
}

/// Which file an open handle is: its volume and its number there. A file
/// put in another's place has another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId {
    /// The volume (Windows' serial number, Unix's device).
    pub volume: u64,
    /// The file on that volume (Windows' 128-bit ID, Unix's inode).
    pub file: u128,
}

/// The identity of an open file or folder.
pub fn identity(file: &File) -> io::Result<FileId> {
    platform::identity(file)
}

/// Deletes the file this handle is, and no other: never a file that took
/// its name. The handle needs delete access, as
/// [`Directory::open_for_delete`] gives it. Windows only: Unix removes by
/// name ([`Directory::remove`]).
#[cfg(windows)]
pub fn delete_by_handle(file: &File) -> io::Result<()> {
    platform::delete_by_handle(file)
}

/// A folder held open. See the [crate] docs.
#[derive(Debug)]
pub struct Directory {
    file: File,
    path: PathBuf,
    /// Linking an open file failed here for the folder, not the file (no
    /// procfs, procfs denied, no hard links): later names skip it.
    no_link: AtomicBool,
}

/// Refuses a name that is not exactly one part of a path.
fn component(name: &OsStr) -> io::Result<()> {
    let mut parts = Path::new(name).components();
    if !matches!(parts.next(), Some(Component::Normal(_))) || parts.next().is_some() {
        return Err(refused(Refusal::LeavesFolder));
    }
    // "name:stream" opens a stream of the file, not a file in the folder.
    if cfg!(windows) && name.as_encoded_bytes().contains(&b':') {
        return Err(refused(Refusal::Stream));
    }
    Ok(())
}

impl Directory {
    /// `path` opened as a folder, refused when its last part is a link or
    /// junction. Links before the last part are followed.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = platform::root(path, false)?;
        Self::from_file(file, path.to_path_buf())
    }

    /// `path` opened through any links, its own last part included: for a
    /// folder the caller chose, which only anchors what is opened below it.
    pub fn followed(path: &Path) -> io::Result<Self> {
        let file = platform::root(path, true)?;
        Self::from_file(file, path.to_path_buf())
    }

    /// A destination folder: [`Self::followed`].
    pub fn destination(path: &Path) -> io::Result<Self> {
        Self::followed(path)
    }

    /// `path` opened through links, as [`Self::followed`], asking for the
    /// right to add files there (and folders, with `folders`). Windows
    /// refuses the open when that right is missing; Unix checks write
    /// access. Nothing is created to find out.
    pub fn writable(path: &Path, folders: bool) -> io::Result<Self> {
        let file = platform::writable(path, folders)?;
        Self::from_file(file, path.to_path_buf())
    }

    /// A folder already open, known by `path`; refused when it is no folder.
    pub fn from_file(file: File, path: PathBuf) -> io::Result<Self> {
        if !file.metadata()?.is_dir() {
            return Err(refused(Refusal::NotFolder));
        }
        Ok(Self {
            file,
            path,
            no_link: AtomicBool::new(false),
        })
    }

    /// A second handle to the same folder.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            path: self.path.clone(),
            no_link: AtomicBool::new(self.no_link.load(Ordering::Relaxed)),
        })
    }

    /// The path the folder was opened at. It may have moved since; see
    /// [`Self::final_path`].
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The handle the folder is held by.
    pub fn handle(&self) -> &File {
        &self.file
    }

    /// The file or folder `name` here, opened for reading. A link, a
    /// junction, a device or a pipe is refused.
    pub fn entry(&self, name: &OsStr) -> io::Result<File> {
        component(name)?;
        let file = platform::open(&self.file, name, false, false)?;
        file_or_folder(file)
    }

    /// `name` opened for its metadata alone, never through a cloud
    /// provider: listing must not download an online-only file. Links,
    /// junctions, devices and pipes are refused.
    pub fn inspect(&self, name: &OsStr) -> io::Result<File> {
        component(name)?;
        let file = platform::inspect(&self.file, name)?;
        file_or_folder(file)
    }

    /// The identity of this folder, as [`identity`] gives it.
    pub fn identity(&self) -> io::Result<FileId> {
        identity(&self.file)
    }

    /// The folder `name` here; a link or junction is refused.
    pub fn directory(&self, name: &OsStr) -> io::Result<Self> {
        component(name)?;
        let file = platform::open(&self.file, name, true, false)?;
        Self::from_file(file, self.path.join(name))
    }

    /// Creates the folder `name` here, new: "name (2)" and so on when the
    /// name is taken, by a file or a folder. It is created in one step that
    /// fails when it exists, so two folders of one name created at once
    /// each get their own. A folder's name has no extension: the number
    /// goes last. At most [`NAME_TRIES`] names are tried:
    /// [`Self::claim_within`] takes another limit.
    pub fn claim(&self, name: &str) -> io::Result<Self> {
        self.claim_within(name, NAME_TRIES)
    }

    /// [`Self::claim`], trying at most `tries` names: `name` and its
    /// numbers up to `tries`. When all are taken nothing is created and it
    /// fails with [`Refusal::NoNameLeft`].
    pub fn claim_within(&self, name: &str, tries: usize) -> io::Result<Self> {
        for n in 1..=tries {
            let name = if n == 1 {
                name.to_string()
            } else {
                format!("{name} ({n})")
            };
            component(OsStr::new(&name))?;
            match platform::create_dir(&self.file, OsStr::new(&name)) {
                Ok(file) => return Self::from_file(file, self.path.join(name)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(refused(Refusal::NoNameLeft { tried: tries }))
    }

    /// The names in this folder, at most `limit`, without `.` and `..`. A
    /// name that could not be read is an error in its place, after which
    /// the listing stops.
    pub fn names(&self, limit: usize) -> io::Result<Vec<io::Result<OsString>>> {
        platform::names(&self.file, limit)
    }

    /// Creates the file `name` here, new, for reading and writing; it fails
    /// with [`io::ErrorKind::AlreadyExists`] when the name is taken. On
    /// Windows it is open without delete sharing, so nothing can rename or
    /// replace it while it is held. On Unix it is created `0o666` under the
    /// umask: never executable.
    pub fn create(&self, name: &OsStr) -> io::Result<File> {
        component(name)?;
        platform::open(&self.file, name, false, true)
    }

    /// The regular file `name` here, already there, opened for reading and
    /// writing: a kept partial file a transfer appends to. A link, a
    /// junction, a folder, a device or a pipe is refused; it is never
    /// created. On Windows it is open without delete sharing, as
    /// [`Self::create`] opens a new one, and with the same rights: it is
    /// published ([`Self::publish`], [`Self::finish`]) and removed
    /// ([`Self::discard`]) through this handle as a new one is.
    pub fn reopen(&self, name: &OsStr) -> io::Result<File> {
        component(name)?;
        let file = platform::reopen(&self.file, name)?;
        if !file.metadata()?.is_file() {
            return Err(refused(Refusal::NotFileOrFolder));
        }
        Ok(file)
    }

    /// Gives `file`, created here as `partial`, the name `name` too, in one
    /// step that fails with [`io::ErrorKind::AlreadyExists`] when the name
    /// is taken. The content comes from the open `file`, never from
    /// whatever `partial` names now. Linux and Android link it through
    /// procfs, then remove `partial` when it still names `file`; when
    /// another file took it, that file is left and the call fails with
    /// [`Refusal::Replaced`], `name` given all the same. Windows renames the
    /// handle. macOS fails with [`io::ErrorKind::Unsupported`]; Linux
    /// without procfs, or with procfs denied, fails with the system's error
    /// (`ENOENT`, `EACCES`, `EPERM`). Use [`Self::rename`] then.
    /// [`Self::publish`] does all of that, and once linking failed for the
    /// folder this handle fails with [`io::ErrorKind::Unsupported`] at once.
    pub fn finish(&self, file: &File, partial: &OsStr, name: &OsStr) -> io::Result<()> {
        // Linux removes `partial` after linking: it must not name anything outside.
        component(partial)?;
        component(name)?;
        if self.no_link.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::Unsupported.into());
        }
        #[cfg(test)]
        {
            if let Some(error) = seams::FINISH_ERRORS.with_borrow_mut(|e| e.pop_front()) {
                return Err(io::Error::from_raw_os_error(error));
            }
            if seams::NO_HARD_LINKS.get() {
                return Err(io::ErrorKind::Unsupported.into());
            }
        }
        platform::finish(&self.file, file, partial, name)
    }

    /// Renames `partial` to `name` without replacing anything, after
    /// checking `partial` is still the open `file`; and checks again after,
    /// giving another file swapped in meanwhile its old name back (or
    /// leaving it, when that name was taken again). Either swap fails with
    /// [`Refusal::Replaced`]. Unix only (`RENAME_NOREPLACE`, or `RENAME_EXCL` on
    /// macOS); Windows fails with [`io::ErrorKind::Unsupported`], since
    /// [`Self::finish`] renames the handle itself there.
    pub fn rename(&self, file: &File, partial: &OsStr, name: &OsStr) -> io::Result<()> {
        component(partial)?;
        component(name)?;
        #[cfg(test)]
        if seams::NO_RENAME.get() {
            return Err(io::ErrorKind::Unsupported.into());
        }
        platform::rename(&self.file, file, partial, name)
    }

    /// Removes the file `name` here, never through a link. On Windows the
    /// file is opened first, without following a link, and deleted by that
    /// handle; a link or junction is refused. Unix removes the name: a link
    /// is removed itself, not what it points to.
    pub fn remove(&self, name: &OsStr) -> io::Result<()> {
        component(name)?;
        platform::remove(&self.file, name)
    }

    /// Removes `file`, created here as `name`. Windows deletes it by its
    /// handle, so only that file goes; Unix removes the name after checking
    /// it is still `file`, and leaves another file there with
    /// [`Refusal::Replaced`].
    pub fn discard(&self, file: &File, name: &OsStr) -> io::Result<()> {
        component(name)?;
        platform::discard(&self.file, file, name)
    }

    /// Marks the file `name` here as downloaded from the Internet (on
    /// Windows its `Zone.Identifier` stream, the Mark of the Web), so the
    /// system warns before it runs. A link or a rename keeps the mark.
    /// Other systems have no such mark: [`Marked::NotOnThisSystem`].
    pub fn mark(&self, name: &OsStr) -> Marked {
        if let Err(e) = component(name) {
            return Marked::Failed(e.kind());
        }
        platform::mark(&self.file, name)
    }

    /// Where the folder is now, read from its handle: it follows a rename.
    /// Windows gives it without the `\\?\` prefix. macOS gives the path it
    /// was opened at.
    pub fn final_path(&self) -> io::Result<PathBuf> {
        platform::final_path_shown(&self.file, &self.path)
    }

    /// Whether the folder was deleted, or moved to the Recycle Bin, since it
    /// was opened. Only a sure sign counts; a path that cannot be read says
    /// nothing. Always false on macOS, which cannot read a folder's place
    /// from its handle.
    pub fn removed(&self) -> bool {
        removed_from(platform::delete_pending(&self.file), self.final_path())
    }

    /// Why files written here could not be found again after a crash or
    /// marked as downloaded, or why the caller keeps them out of here; `None`
    /// when nothing stands in the way. `kept_out` lists folders nothing may
    /// be written into, the folders inside them included. A folder in it
    /// that cannot be opened counts as holding this one, unless it does not
    /// exist (and an empty path always counts). A folder counts as inside
    /// when its place starts with a kept-out folder's place; macOS compares
    /// the paths the folders were opened at (see [`Self::final_path`]).
    pub fn unfit<P: AsRef<Path>>(&self, kept_out: &[P]) -> Option<Unfit> {
        platform::unfit(&self.file).or_else(|| self.inside(kept_out).then_some(Unfit::KeptOut))
    }

    /// Whether this folder is one of `kept_out` or inside one, by its
    /// identity or where it really is; where that is unsure, it is.
    fn inside<P: AsRef<Path>>(&self, kept_out: &[P]) -> bool {
        let (Ok(id), Ok(at)) = (self.identity(), self.final_path()) else {
            return true;
        };
        for path in kept_out {
            let path = path.as_ref();
            let folder = match Self::followed(path) {
                Ok(folder) => folder,
                // A folder not made yet holds nothing.
                Err(e) if e.kind() == io::ErrorKind::NotFound && !path.as_os_str().is_empty() => {
                    continue;
                }
                Err(_) => return true,
            };
            match folder.identity() {
                Ok(kept) if kept != id => {}
                _ => return true,
            }
            match folder.final_path() {
                Ok(kept) if !at.starts_with(&kept) => {}
                _ => return true,
            }
        }
        false
    }

    /// The volume this folder is on as `\\?\Volume{...}\`, which opens it
    /// again whatever letter or mount point it has then. A network share
    /// has none: [`Refusal::NoVolume`].
    #[cfg(windows)]
    pub fn volume_path(&self) -> io::Result<String> {
        platform::volume_path(&self.file)
    }

    /// The file `name` here opened to delete it with [`delete_by_handle`],
    /// never through a link.
    #[cfg(windows)]
    pub fn open_for_delete(&self, name: &OsStr) -> io::Result<File> {
        component(name)?;
        platform::open_for_delete(&self.file, name)
    }
}

fn file_or_folder(file: File) -> io::Result<File> {
    let meta = file.metadata()?;
    if !meta.is_file() && !meta.is_dir() {
        return Err(refused(Refusal::NotFileOrFolder));
    }
    Ok(file)
}

/// Why files should not be written into a folder: see [`Directory::unfit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Unfit {
    /// No `\\?\Volume{...}\` path: a network folder (Windows). A file there
    /// cannot be found again by its ID after a crash.
    Network,
    /// The volume keeps no alternate data streams (FAT, exFAT), so no
    /// downloaded-file mark (Windows).
    NoMarks,
    /// One of the caller's kept-out folders, or inside one.
    KeptOut,
}

/// Only a sure sign says a folder is gone: a pending delete, a path in the
/// Recycle Bin, or the system saying it is not there.
fn removed_from(pending: bool, now: io::Result<PathBuf>) -> bool {
    pending
        || match now {
            Ok(path) => in_recycle_bin(&path),
            Err(e) => e.kind() == io::ErrorKind::NotFound,
        }
}

/// Whether `path` is inside a `$Recycle.Bin` folder, where Windows moves
/// what it deletes.
pub fn in_recycle_bin(path: &Path) -> bool {
    path.components().any(|part| {
        part.as_os_str()
            .to_str()
            .is_some_and(|part| part.eq_ignore_ascii_case("$Recycle.Bin"))
    })
}

/// Switches tests use to act out other file systems.
#[cfg(test)]
// Android's emulated thread locals make Clippy ask for the `const` already there.
#[allow(clippy::missing_const_for_thread_local)]
mod seams {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    type Hook = Box<dyn FnOnce()>;

    thread_local! {
        pub static NO_HARD_LINKS: Cell<bool> = const { Cell::new(false) };
        pub static NO_RENAME: Cell<bool> = const { Cell::new(false) };
        pub static FINISH_ERRORS: RefCell<VecDeque<i32>> = const { RefCell::new(VecDeque::new()) };
        pub static REMOVE_ERRORS: RefCell<VecDeque<i32>> = const { RefCell::new(VecDeque::new()) };
        pub static SET_TIMES_ERROR: Cell<Option<i32>> = const { Cell::new(None) };
        pub static CHMOD_ERROR: Cell<Option<i32>> = const { Cell::new(None) };
        /// One entry per `apply`: `Some` fails that call with the error.
        pub static APPLY_ERRORS: RefCell<VecDeque<Option<i32>>> = const { RefCell::new(VecDeque::new()) };
        /// How many times a copy was flushed to the disk.
        pub static SYNCS: Cell<u32> = const { Cell::new(0) };
        static HOOKS: RefCell<Vec<(&'static str, Hook)>> = const { RefCell::new(Vec::new()) };
        /// Told of each step of the slices of the next copy into a name
        /// this thread starts, on the thread that takes the step.
        pub static COPIES: RefCell<Option<Copies>> = const { RefCell::new(None) };
    }

    /// What hears of a copy's slices: it may hold a step for as long as a
    /// test needs.
    pub type Copies = std::sync::Arc<dyn Fn(Step) + Send + Sync>;

    /// A step of a copy into a name.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Step {
        /// A slice is on the blocking pool and has not taken the gate.
        Queued,
        /// It holds the gate and is about to copy.
        Copying,
        /// It has copied, or found the gate shut, and still holds it.
        Ended,
        /// The copy's claim is being dropped and is about to take the gate.
        Closing,
    }

    /// Runs `hook` once, when the code reaches `point`: a swap acted out
    /// between two steps.
    pub fn on(point: &'static str, hook: impl FnOnce() + 'static) {
        HOOKS.with_borrow_mut(|hooks| hooks.push((point, Box::new(hook))));
    }

    pub fn at(point: &str) {
        let hook = HOOKS.with_borrow_mut(|hooks| {
            let i = hooks.iter().position(|(p, _)| *p == point)?;
            Some(hooks.remove(i).1)
        });
        if let Some(hook) = hook {
            hook();
        }
    }
}

#[cfg(test)]
mod meta_tests;
#[cfg(test)]
mod tests;
