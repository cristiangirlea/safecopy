//! A bounded list of files to send.
//!
//! - A [`Manifest`] is a checked list of files and folders: every path
//!   safe for the other side ([`safecopy_names::entry_name`] for each
//!   part), each folder before what it holds, no two names the same apart
//!   from case or Unicode form, and all of it within the caller's
//!   [`Limits`]. It encodes to bytes and decodes from them, checked whole
//!   before it is used.
//! - [`Sources`] lists files and folders a person picked, by handle, and
//!   keeps what it needs to open each again: the folders they are in,
//!   held open, and the identity, size, modified time and execute bit of
//!   every entry. [`Sources::meta`] gives the last two as they were read
//!   while listing, with no file opened. A source put in another's place,
//!   grown, shrunk, touched or with another execute bit since it was
//!   listed is [`ManifestError::Changed`], never read; one that is no
//!   longer a regular file is [`ManifestError::NotAFile`].
//! - [`Listing`] walks one folder by handle for sending it whole, skipping
//!   (and saying so) what it cannot send; each entry carries its time and
//!   execute bit ([`Listed::meta`]). An entry that does not open
//!   again names the part of its path that failed: the folder on the way
//!   that became a link ([`ManifestError::Link`]), or the entry itself.
//! - What `safecopy-folder` refuses is never
//!   [`ManifestError::Unreadable`]: a link and a special file have kinds
//!   of their own, and every other [`safecopy_folder::Refusal`] is
//!   [`ManifestError::Refused`] with the refusal in it.
//! - [`groups`] cuts a list into groups, which a transfer goes through in
//!   order, at most [`GroupLimits::open`] at once; [`Progress`] counts what
//!   is done in them, bounded whatever the list's length.
//!
//! A list that cannot be offered whole is refused whole: nothing is left
//! out quietly. Nothing here reads a setting, a path or an environment
//! variable of its own, or logs a name: the caller passes every folder and
//! every limit, and words every [`ManifestError`].
//!
//! # Systems
//!
//! - **Windows**: [`Sources`] lists an online-only file (one a cloud
//!   service keeps) from its metadata alone, its time included, while
//!   [`Listing::list`] opens each file as it walks and so fetches one
//!   marked to be fetched when opened. `Sources` refuses one a storage
//!   service took offline ([`ManifestError::Offline`]). A source folder on
//!   no volume of this computer (a network share) is refused
//!   ([`ManifestError::Network`]). Other systems have neither attribute
//!   nor check. No file is executable there: the execute bit is the
//!   owner's on Linux and macOS, as [`safecopy_folder::Meta::read`] has it.
//! - **macOS** cannot read where a folder held open is now, so a source
//!   inside a kept-out folder is found by the identity of everything in the
//!   kept-out folders, and by the path the folder was opened at.

// What the fuzz target runs: for the fuzzer's build and the tests alone.
#[cfg(any(test, fuzzing))]
pub mod fuzzing;
mod groups;
mod listing;
mod sources;
mod wire;

pub use groups::{Group, GroupLimits, Groups, OpenGroup, Progress, ProgressError, groups};
pub use listing::{Listed, Listing, Skip, Skipped};
pub use sources::{Opened, Sources};

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io;
use std::path::PathBuf;

use safecopy_folder::Refusal;
use safecopy_names::{NameError, NameRules, PathError, entry_name, fold, reserved};

/// What an entry of a list is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A file, with its size.
    File,
    /// A folder; its size is 0.
    Folder,
}

/// One file or folder of a list. Its number is its place in the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its path in the list, parts joined by `/`, each part a safe name.
    pub path: String,
    /// A file or a folder.
    pub kind: Kind,
    /// A file's size in bytes; a folder's is 0.
    pub size: u64,
}

/// A check of the caller's own on each path of a list, such as whether it
/// fits the caller's framing: `false` refuses the list with
/// [`ManifestError::DoesNotFit`].
pub type Fits = fn(&str, Kind) -> bool;

/// The bounds a list is held to. `Default` gives the values below.
#[derive(Debug, Clone)]
pub struct Limits {
    /// The most files and folders in one list: 10,000.
    pub entries: usize,
    /// The most parts a path may have, the first one included: 32. It also
    /// bounds how deep the kept-out folders are walked: one holding folders
    /// nested deeper than this refuses every list.
    pub depth: usize,
    /// The longest path, in characters: 1,024.
    pub path_chars: usize,
    /// The most bytes the files of one list may hold together: 1 TiB.
    pub bytes: u64,
    /// The most bytes a list may take encoded: 16 MiB.
    pub encoded: usize,
    /// The most folders the picked files and folders may come from, each
    /// held open while the [`Sources`] last: 256.
    pub source_folders: usize,
    /// The most files and folders the kept-out folders are walked for; past
    /// it nothing is listed: 10,000.
    pub kept_entries: usize,
    /// The rules for each name.
    pub names: NameRules,
    /// The caller's own check on each path; `None` takes every path.
    pub fits: Option<Fits>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            entries: 10_000,
            depth: 32,
            path_chars: 1024,
            bytes: 1 << 40,
            encoded: 16 << 20,
            source_folders: 256,
            kept_entries: 10_000,
            names: NameRules::default(),
            fits: None,
        }
    }
}

impl Limits {
    /// The entry limit as the encoding can count it.
    fn most_entries(&self) -> usize {
        self.entries.min(u32::MAX as usize)
    }
}

/// Why a list is refused, or a source is not opened.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ManifestError {
    /// The list holds no files or folders.
    Empty,
    /// More than [`Limits::entries`] files and folders.
    TooMany {
        /// The limit it passed.
        most: usize,
    },
    /// The files hold more than [`Limits::bytes`] together.
    TooLarge {
        /// The limit it passed.
        most: u64,
    },
    /// The encoded list takes more than [`Limits::encoded`] bytes.
    TooLong {
        /// The limit it passed.
        most: usize,
    },
    /// The bytes are not a list: cut short, a number or text that does not
    /// read, an unknown kind, or bytes after the last entry.
    Damaged,
    /// A path in the list is refused.
    Path(PathError),
    /// A name in the list is not one [`safecopy_names::entry_name`] keeps
    /// as it is.
    Name,
    /// The caller's [`Limits::fits`] refused a path.
    DoesNotFit,
    /// A file in the list is also a folder: something is listed below it.
    FileAsFolder,
    /// An entry comes before the folder it is in, or that folder is not in
    /// the list.
    Orphan,
    /// Two paths in the list differ only in case or Unicode form.
    SameName,
    /// A folder in the list has a size.
    FolderSize,
    /// A picked path is the top of a drive, or has no folder.
    WholeDrive,
    /// The picked files and folders come from more than
    /// [`Limits::source_folders`] folders.
    TooManyFolders {
        /// The limit it passed.
        most: usize,
    },
    /// Something picked is one of the caller's kept-out folders, inside
    /// one, holds one or is anything in them; or where they are or what
    /// they hold could not be read.
    KeptOut,
    /// A picked file or folder is in a network folder.
    Network,
    /// A link or junction among what was picked, or on the way to a listed
    /// entry opened again, which is not followed.
    Link {
        /// The link itself, not what was being reached through it: for
        /// [`Listing::open_entry`] the entry's own path when the entry
        /// became a link, and a folder above it when that folder did.
        at: PathBuf,
    },
    /// Neither a file nor a folder: a device, a pipe or a socket.
    NotFileOrFolder {
        /// Where it is.
        at: PathBuf,
    },
    /// A name, or what is under it, that `safecopy-folder` refused to open
    /// or list for a reason with no kind of its own here. A link is
    /// [`Self::Link`] and a device, a pipe or a socket
    /// [`Self::NotFileOrFolder`] or [`Self::NotAFile`], never this.
    Refused {
        /// Why.
        why: Refusal,
        /// Which of the picked paths it was for, counted from 0. A
        /// [`Listing`] has one folder: 0.
        root: usize,
        /// How many names lead to what was refused, from the folder held
        /// open. For a [`Listing`] that is the listed folder: 0 is the
        /// folder itself, and otherwise that many of [`Listed::parts`].
        /// For [`Sources::list`] it is the folder the picked path is in:
        /// 0 is that folder, 1 the picked file or folder, 2 a name in it.
        part: usize,
    },
    /// A file or folder the system could not open or read.
    Unreadable {
        /// Where it is: for [`Listing::open_entry`], the part of the
        /// entry's path that could not be opened.
        at: PathBuf,
        /// What the system said.
        kind: io::ErrorKind,
        /// The system's own code for it (`errno`, a Windows error code),
        /// when it has one.
        raw: Option<i32>,
    },
    /// A file or folder more than [`Limits::depth`] parts deep.
    TooDeep {
        /// Where it is.
        at: PathBuf,
        /// The limit it passed.
        most: usize,
    },
    /// A path longer than [`Limits::path_chars`].
    PathTooLong {
        /// Where it is.
        at: PathBuf,
        /// The limit it passed.
        most: usize,
    },
    /// A file a storage service took offline, which cannot be fetched.
    Offline {
        /// Where it is.
        at: PathBuf,
    },
    /// A name that cannot go in a list.
    BadName {
        /// Where it is.
        at: PathBuf,
        /// Why.
        why: NameError,
    },
    /// A folder was put in another's place while it was listed.
    ChangedWhileListed {
        /// Where it is.
        at: PathBuf,
    },
    /// The list has no entry of that number.
    NotListed,
    /// The source is no longer what was listed: another file in its place,
    /// another folder in a folder's, gone, a link, or another size,
    /// modified time or execute bit.
    Changed,
    /// A listed file is no longer a regular file: a folder is under its
    /// name now, or a device, a pipe or a socket.
    NotAFile {
        /// Where it is.
        at: PathBuf,
    },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The caller words it; this is the kind for logs and tests.
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for ManifestError {}

/// The checked list of files and folders. It does not change once made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    id: u64,
    entries: Vec<Entry>,
    files: u32,
    bytes: u64,
}

impl Manifest {
    /// Checks `entries` as a list; `id` is the caller's number for it,
    /// carried in its encoding.
    pub fn new(id: u64, entries: Vec<Entry>, limits: &Limits) -> Result<Self, ManifestError> {
        check(id, entries, limits)
    }

    /// The caller's number for this list.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Every entry, folders before what they hold.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Entry number `n`, if the list has it.
    pub fn entry(&self, n: u32) -> Option<&Entry> {
        self.entries.get(n as usize)
    }

    /// How many files.
    pub fn files(&self) -> u32 {
        self.files
    }

    /// The sum of the files' sizes.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The list as bytes: the id, the count, then each entry's path, kind
    /// and size. Numbers are unsigned LEB128 varints and text is its
    /// length then UTF-8, so the bytes match the `postcard` encoding of
    /// `(u64, u32)` followed by each entry as a struct. Refused past
    /// [`Limits::encoded`].
    pub fn encode(&self, limits: &Limits) -> Result<Vec<u8>, ManifestError> {
        wire::encode(self, limits.encoded)
    }

    /// A list from the other side, read an entry at a time and checked
    /// whole before it is used. The count is checked before any room is
    /// set aside for it.
    pub fn decode(bytes: &[u8], limits: &Limits) -> Result<Self, ManifestError> {
        wire::decode(bytes, limits)
    }
}

fn check(id: u64, entries: Vec<Entry>, limits: &Limits) -> Result<Manifest, ManifestError> {
    if entries.is_empty() {
        return Err(ManifestError::Empty);
    }
    let most = limits.most_entries();
    if entries.len() > most {
        return Err(ManifestError::TooMany { most });
    }
    let (mut files, mut bytes) = (0u32, 0u64);
    {
        let mut kinds: HashMap<&str, Kind> = HashMap::with_capacity(entries.len());
        let mut folded = HashSet::with_capacity(entries.len());
        for entry in &entries {
            path_problem(&entry.path, entry.kind, limits)?;
            if let Some((parent, _)) = entry.path.rsplit_once('/') {
                match kinds.get(parent) {
                    Some(Kind::Folder) => {}
                    Some(Kind::File) => return Err(ManifestError::FileAsFolder),
                    None => return Err(ManifestError::Orphan),
                }
            }
            if !folded.insert(fold(&entry.path)) {
                return Err(ManifestError::SameName);
            }
            kinds.insert(&entry.path, entry.kind);
            match entry.kind {
                Kind::Folder if entry.size != 0 => return Err(ManifestError::FolderSize),
                Kind::Folder => {}
                Kind::File => {
                    files += 1;
                    bytes = add_within(bytes, entry.size, limits.bytes)?;
                }
            }
        }
    }
    Ok(Manifest {
        id,
        entries,
        files,
        bytes,
    })
}

/// `bytes + size`, refused past `most` and never wrapping.
fn add_within(bytes: u64, size: u64, most: u64) -> Result<u64, ManifestError> {
    bytes
        .checked_add(size)
        .filter(|&bytes| bytes <= most)
        .ok_or(ManifestError::TooLarge { most })
}

/// What is wrong with a path in a list, if anything: it may not be too
/// long or deep, start at the top of a drive, climb out, name a drive, a
/// stream or a device, or hold a name that making it safe would change.
fn path_problem(path: &str, kind: Kind, limits: &Limits) -> Result<(), ManifestError> {
    if path.chars().count() > limits.path_chars {
        return Err(ManifestError::Path(PathError::TooLong {
            most: limits.path_chars,
        }));
    }
    if limits.fits.is_some_and(|fits| !fits(path, kind)) {
        return Err(ManifestError::DoesNotFit);
    }
    if path.starts_with(['/', '\\']) {
        return Err(ManifestError::Path(PathError::AtTop));
    }
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() > limits.depth {
        return Err(ManifestError::Path(PathError::TooDeep {
            most: limits.depth,
        }));
    }
    for part in parts {
        if part.is_empty() {
            return Err(ManifestError::Path(PathError::EmptyPart));
        }
        if matches!(part.trim(), "." | "..") {
            return Err(ManifestError::Path(PathError::LeavesFolder));
        }
        if part.contains(':') {
            return Err(ManifestError::Path(PathError::DriveOrStream));
        }
        if reserved(part) {
            return Err(ManifestError::Path(PathError::Reserved));
        }
        // Also refuses `\`, which entry_name turns into `_`.
        if entry_name(part, &limits.names).as_deref() != Some(part) {
            return Err(ManifestError::Name);
        }
    }
    Ok(())
}

/// Switches tests use to act out other systems and races.
#[cfg(test)]
// Android's emulated thread locals make Clippy ask for the `const` already there.
#[allow(clippy::missing_const_for_thread_local)]
mod seams {
    use std::cell::{Cell, RefCell};
    use std::collections::{HashMap, HashSet};
    use std::ffi::OsString;

    thread_local! {
        /// Every folder counts as one on the network.
        pub static NO_VOLUME: Cell<bool> = const { Cell::new(false) };
        /// Reading a folder's final path fails.
        pub static FINAL_PATH_FAILS: Cell<bool> = const { Cell::new(false) };
        /// These names are deleted just before the kept-out walk inspects them.
        pub static GONE_BEFORE_INSPECT: RefCell<HashSet<OsString>> = RefCell::new(HashSet::new());
        /// These folders are deleted after they are inspected, before they are opened.
        pub static GONE_BEFORE_OPEN: RefCell<HashSet<OsString>> = RefCell::new(HashSet::new());
        /// Inspecting these names in the kept-out walk fails with this kind.
        pub static INSPECT_FAILS: RefCell<HashMap<OsString, std::io::ErrorKind>> =
            RefCell::new(HashMap::new());
        /// Opening these names while listing, or again for a listed entry, is refused so.
        pub static REFUSED: RefCell<HashMap<OsString, safecopy_folder::Refusal>> =
            RefCell::new(HashMap::new());
        /// Files of these names get these Windows attributes.
        pub static ATTRIBUTES: RefCell<HashMap<OsString, u32>> = RefCell::new(HashMap::new());
    }
}

#[cfg(test)]
mod groups_tests;
#[cfg(test)]
mod tests;
