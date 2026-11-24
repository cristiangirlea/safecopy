//! Where a partial file is, found again by identity after a crash.

use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};

use safecopy_folder::{Directory, FileId, Refusal, identity};

use crate::log::Root;
use crate::{JournalError, Settings, removable};

/// One folder below the root, on the way to a partial file: its name in
/// the folder above and its identity. An empty name, in a level directly
/// below the root and the only one, is a folder known by its identity
/// alone: Windows opens it by its ID and checks it is still below the root,
/// and Unix cannot find it again. A segment that holds an empty name
/// anywhere else is read as damaged.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Level {
    /// Its name in the folder above.
    pub name: String,
    /// Its volume or device.
    pub device: u64,
    /// Its file ID or inode.
    pub id: u128,
}

/// A folder found again by identity: the root, and each folder below it on
/// the way. On Windows the root's and the last folder's file IDs open them,
/// and the folder is found only while it is below the root: by its recorded
/// names, or by where both are now, walked from the root with no link
/// followed. One moved out of the root, or whose place cannot be read, is
/// not found. On Unix, which has no open by ID, the root is opened by its
/// path and each level below it by name, no link followed, each one's
/// device and inode compared, so a folder renamed since is not found.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Location {
    /// The root.
    pub root: Root,
    /// The folders below it, the root's child first.
    pub levels: Vec<Level>,
}

/// A partial file's place: its folder, its name there and its own ID.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PartialAt {
    /// Its folder.
    pub location: Location,
    /// Its name there.
    pub partial: String,
    /// Its own file ID or inode.
    pub file: u128,
}

/// Why a recorded folder or partial is not found again, and is left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Left {
    /// A recorded name that is not a partial's
    /// ([`PartialAt::is_partial`]): never removed, never opened.
    Name,
    /// A root recorded on another system, a volume path that is not a
    /// volume's, or another disk under it.
    Volume,
    /// Another file, folder or link under the recorded name, or another
    /// folder on the way.
    Replaced,
    /// The folder or the file is not there.
    Gone,
    /// The recorded volume or root could not be opened: a disk not plugged
    /// in, not mounted.
    Unreachable,
    /// The file is there, open elsewhere in a way that shuts this open
    /// out: a transfer is writing it. Only Windows says so.
    Busy,
    /// Any other error of the system's: worth another try.
    Io,
}

impl Left {
    /// Whether another cleanup could still remove the partial: its journal
    /// is kept for it.
    pub(crate) fn may_change(self) -> bool {
        matches!(self, Self::Unreachable | Self::Busy | Self::Io)
    }
}

/// An error opening the recorded folder or file: what it says of it.
pub(crate) fn left(error: &io::Error) -> Left {
    if safecopy_folder::refusal(error).is_some() {
        // A link or a folder under the name, a file under the folder's ID.
        return Left::Replaced;
    }
    if crate::files::busy(error) {
        return Left::Busy;
    }
    match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => Left::Gone,
        // A folder ID no longer in use reads as a bad parameter.
        io::ErrorKind::InvalidInput => Left::Gone,
        io::ErrorKind::IsADirectory => Left::Replaced,
        _ => Left::Io,
    }
}

/// The root a journal records for `root`.
pub(crate) fn root_of(root: &Directory) -> Result<Root, JournalError> {
    let id = root.identity()?;
    #[cfg(windows)]
    {
        Ok(Root::Windows {
            volume: root.volume_path()?,
            serial: id.volume,
            folder: id.file,
        })
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        // Where it really is now, as an absolute path a later process opens.
        let at = root.final_path()?;
        let at = std::path::absolute(&at)?;
        Ok(Root::Unix {
            path: at.as_os_str().as_bytes().to_vec(),
            device: id.volume,
            inode: id.file,
        })
    }
}

/// The folders from `root` down to `dir`, each opened by name from the one
/// above with no link followed.
///
/// The path `dir` was opened at is walked first. When it no longer reaches
/// `dir` (renamed, moved, another folder or a link under its name), the
/// place its handle has now is walked the same way, so a folder still below
/// `root` is recorded under its present names. Not below `root` by the
/// path it was opened at, or by where it is now:
/// [`JournalError::NotBelow`]. So is one whose place the system can read
/// and did not: nothing shows it is still below `root`. Only where the
/// system has no way to read it is a folder whose name is gone, or names a
/// file or another folder, one level of its identity alone, with an empty
/// name.
pub(crate) fn levels(
    root: &Directory,
    dir: &Directory,
) -> Result<Vec<(Level, FileId)>, JournalError> {
    let want = dir.identity()?;
    if want == root.identity()? {
        return Ok(Vec::new());
    }
    let below = dir
        .path()
        .strip_prefix(root.path())
        .map_err(|_| JournalError::NotBelow)?;
    let opened = walk(root, below, want);
    if opened.is_ok() {
        return opened;
    }
    // What a rename leaves: the name gone, or a file or another folder under it.
    let renamed = matches!(
        opened,
        Err(JournalError::NotBelow
            | JournalError::Folder(Refusal::NotFolder)
            | JournalError::Io {
                kind: io::ErrorKind::NotFound | io::ErrorKind::NotADirectory,
                ..
            })
    );
    match (place(root), place(dir)) {
        (Place::At(root_now), Place::At(dir_now)) => {
            let below = dir_now
                .strip_prefix(&root_now)
                .map_err(|_| JournalError::NotBelow)?;
            // Renamed again in this moment: the first walk's error stands.
            walk(root, below, want).or(opened)
        }
        // It could be anywhere: a failed reading is never taken for a rename.
        (Place::Unreadable, _) | (_, Place::Unreadable) => {
            if renamed {
                Err(JournalError::NotBelow)
            } else {
                opened
            }
        }
        _ => {
            // Nothing can say where it is now. A link under the name could
            // be how the folder was opened, and a path of no names or with
            // `..` in it never led below the root.
            let names = !below.as_os_str().is_empty()
                && below
                    .components()
                    .all(|c| matches!(c, Component::Normal(_)));
            if !(names && renamed) {
                return opened;
            }
            let alone = Level {
                name: String::new(),
                device: want.volume,
                id: want.file,
            };
            Ok(vec![(alone, want)])
        }
    }
}

/// Where a held folder is now, as far as its handle says.
enum Place {
    /// Read from the handle: it follows a rename.
    At(PathBuf),
    /// The system reads it, and this reading failed.
    Unreadable,
    /// The system has no way to read it: macOS and the BSDs, Linux and
    /// Android with no procfs mounted.
    Unknown,
}

fn place(dir: &Directory) -> Place {
    #[cfg(test)]
    match crate::seams::PLACE.get() {
        crate::seams::Place::Read => {}
        crate::seams::Place::Fails => return Place::Unreadable,
        crate::seams::Place::Unknown => return Place::Unknown,
    }
    // Elsewhere the path it was opened at is all there is.
    if cfg!(not(any(
        windows,
        target_os = "linux",
        target_os = "android"
    ))) {
        return Place::Unknown;
    }
    match dir.final_path() {
        Ok(at) => Place::At(at),
        Err(_) if no_procfs() => Place::Unknown,
        Err(_) => Place::Unreadable,
    }
}

/// Whether Linux or Android has no procfs to read a handle's place from: a
/// process always finds its own handles there when it is mounted, so only
/// their folder being absent says so. Any other error is a failed reading.
fn no_procfs() -> bool {
    cfg!(any(target_os = "linux", target_os = "android"))
        && std::fs::symlink_metadata("/proc/self/fd")
            .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
}

/// The folders `below` names from `root` down, each opened by name from
/// the one above with no link followed: [`JournalError::NotBelow`] when the
/// last is not the folder `want`.
fn walk(
    root: &Directory,
    below: &Path,
    want: FileId,
) -> Result<Vec<(Level, FileId)>, JournalError> {
    let mut at = root.try_clone()?;
    let mut levels = Vec::new();
    for part in below.components() {
        let Component::Normal(name) = part else {
            return Err(JournalError::NotBelow);
        };
        let text = name.to_str().ok_or(JournalError::NameNotText)?;
        at = at.directory(name)?;
        let id = at.identity()?;
        levels.push((
            Level {
                name: text.to_string(),
                device: id.volume,
                id: id.file,
            },
            id,
        ));
    }
    match levels.last() {
        Some((_, id)) if *id == want => Ok(levels),
        _ => Err(JournalError::NotBelow),
    }
}

/// Whether the folder `dir`, opened by its ID, is below `root` now: reached
/// by its recorded names, or by where both are now, each folder opened by
/// name from the one above with no link followed. A place that cannot be
/// read shows nothing, and a folder not shown to be below is not.
#[cfg(windows)]
fn still_below(root: &Directory, dir: &Directory, recorded: &[Level]) -> io::Result<bool> {
    let want = dir.identity()?;
    let reaches = |names: &mut dyn Iterator<Item = &OsStr>| -> io::Result<bool> {
        let mut at = root.try_clone()?;
        for name in names {
            at = at.directory(name)?;
        }
        Ok(at.identity()? == want)
    };
    // The recorded names first: they need no place read.
    if recorded.iter().all(|l| !l.name.is_empty())
        && matches!(
            reaches(&mut recorded.iter().map(|l| OsStr::new(&l.name))),
            Ok(true)
        )
    {
        return Ok(true);
    }
    let (Place::At(root_now), Place::At(dir_now)) = (place(root), place(dir)) else {
        // A reading that fails may succeed later: the journal is kept for it.
        return Err(io::ErrorKind::Other.into());
    };
    let Ok(below) = dir_now.strip_prefix(&root_now) else {
        return Ok(false);
    };
    let mut names = Vec::new();
    for part in below.components() {
        let Component::Normal(name) = part else {
            return Ok(false);
        };
        names.push(name);
    }
    if names.is_empty() {
        return Ok(false);
    }
    reaches(&mut names.into_iter())
}

impl Location {
    /// The folder, opened again by identity.
    /// Fails with why it is not found: gone, replaced, on another disk or
    /// system, or not reachable now.
    pub fn open(&self) -> Result<Directory, Left> {
        self.find()
    }

    pub(crate) fn find(&self) -> Result<Directory, Left> {
        #[cfg(windows)]
        {
            let Root::Windows {
                volume,
                serial,
                folder,
            } = &self.root
            else {
                return Err(Left::Volume);
            };
            let volume = crate::win::checked_volume(volume, *serial)?;
            let (device, id) = self
                .levels
                .last()
                .map_or((*serial, *folder), |l| (l.device, l.id));
            if device != *serial {
                return Err(Left::Volume);
            }
            let open = |id: u128| {
                let dir = crate::win::open_folder(&volume, id).map_err(|e| left(&e))?;
                let got = dir.identity().map_err(|_| Left::Io)?;
                if got.volume != *serial || got.file != id {
                    return Err(Left::Replaced);
                }
                Ok(dir)
            };
            let dir = open(id)?;
            if self.levels.is_empty() {
                return Ok(dir);
            }
            // An ID opens a folder anywhere on the volume: only one still
            // below the root is the journal's to clean.
            let root = open(*folder)?;
            match still_below(&root, &dir, &self.levels) {
                Ok(true) => Ok(dir),
                Ok(false) => Err(Left::Gone),
                Err(e) => Err(left(&e)),
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let Root::Unix {
                path,
                device,
                inode,
            } = &self.root
            else {
                return Err(Left::Volume);
            };
            let path = Path::new(OsStr::from_bytes(path));
            let mut dir = Directory::open(path).map_err(|e| match left(&e) {
                // A disk not mounted: the path missing, or another folder there.
                Left::Gone => Left::Unreachable,
                other => other,
            })?;
            let id = dir.identity().map_err(|_| Left::Io)?;
            if id.volume != *device || id.file != *inode {
                return Err(Left::Unreachable);
            }
            for level in &self.levels {
                // Known by its identity alone: no name finds it again.
                if level.name.is_empty() {
                    return Err(Left::Gone);
                }
                dir = dir
                    .directory(OsStr::new(&level.name))
                    .map_err(|e| left(&e))?;
                let id = dir.identity().map_err(|_| Left::Io)?;
                if id.volume != level.device || id.file != level.id {
                    return Err(Left::Replaced);
                }
            }
            Ok(dir)
        }
    }
}

impl PartialAt {
    /// Where the partial file `file`, created as `partial` in `dir`, is:
    /// `dir` is `root` or a folder opened below it by name, each level
    /// recorded with its identity, as a journal records it: under its
    /// present names when it was renamed since it was opened.
    pub fn of(
        root: &Directory,
        dir: &Directory,
        file: &std::fs::File,
        partial: &OsStr,
    ) -> Result<Self, JournalError> {
        let id = identity(file)?;
        if id.volume != dir.identity()?.volume {
            return Err(JournalError::OtherVolume);
        }
        Ok(Self {
            location: Location {
                root: root_of(root)?,
                levels: levels(root, dir)?.into_iter().map(|(l, _)| l).collect(),
            },
            partial: partial
                .to_str()
                .ok_or(JournalError::NameNotText)?
                .to_string(),
            file: id.file,
        })
    }

    /// Whether the recorded name is a partial's under `settings`: it ends
    /// in [`Settings::partial_ending`] and is more than the ending, one
    /// part of a path, naming no stream of a file. Only such a name is
    /// ever opened, cut or removed. A record read from a disk may name
    /// any file: [`Self::remove`], [`Self::reopen`], [`Self::try_reopen`]
    /// and [`Self::present`] each ask this first, and take the settings
    /// so that no caller can leave it out.
    pub fn is_partial(&self, settings: &Settings) -> bool {
        removable(&self.partial, settings)
    }

    /// Removes the partial file, only through a handle checked to be it:
    /// its folder found by identity, the name ending in
    /// [`Settings::partial_ending`], its file ID the recorded one. Windows
    /// deletes through that handle; Unix removes the name after checking
    /// it still is that file. Anything else is left.
    pub fn remove(&self, settings: &Settings) -> bool {
        self.remove_checked(settings).is_ok()
    }

    pub(crate) fn remove_checked(&self, settings: &Settings) -> Result<(), Left> {
        if !self.is_partial(settings) {
            return Err(Left::Name);
        }
        let dir = self.location.find()?;
        let name = OsStr::new(&self.partial);
        #[cfg(windows)]
        {
            let file = dir.open_for_delete(name).map_err(|e| left(&e))?;
            let id = identity(&file).map_err(|_| Left::Io)?;
            if id.file != self.file || dir.identity().ok().map(|d| d.volume) != Some(id.volume) {
                return Err(Left::Replaced);
            }
            safecopy_folder::delete_by_handle(&file).map_err(|_| Left::Io)
        }
        #[cfg(unix)]
        {
            let file = dir.entry(name).map_err(|e| left(&e))?;
            let meta = file.metadata().map_err(|_| Left::Io)?;
            let id = identity(&file).map_err(|_| Left::Io)?;
            if !meta.is_file() || id.file != self.file {
                return Err(Left::Replaced);
            }
            dir.discard(&file, name).map_err(|e| left(&e))
        }
    }

    /// The partial file opened by identity for reading and writing, with
    /// its folder: `None` when its name is not a partial's under
    /// `settings` ([`Self::is_partial`]), it is gone, another file has its
    /// name, or it could not be opened. [`Self::try_reopen`] says which.
    pub fn reopen(&self, settings: &Settings) -> Option<(Directory, std::fs::File)> {
        self.try_reopen(settings).ok()
    }

    /// [`Self::reopen`], with why it failed: [`Left::Gone`] and
    /// [`Left::Replaced`] say the recorded file is not under its name any
    /// more; [`Left::Busy`] that it is there and open elsewhere (Windows,
    /// where a file being written is not shared for this open); the rest
    /// that nothing is known of it now. [`Left::Name`] is a recorded name
    /// that is not a partial's under `settings`: nothing is opened for it,
    /// not its folder either.
    pub fn try_reopen(&self, settings: &Settings) -> Result<(Directory, std::fs::File), Left> {
        // The caller may cut and write what this opens.
        if !self.is_partial(settings) {
            return Err(Left::Name);
        }
        let dir = self.location.find()?;
        let file = dir
            .reopen(OsStr::new(&self.partial))
            .map_err(|e| left(&e))?;
        let id = identity(&file).map_err(|_| Left::Io)?;
        let volume = dir.identity().map_err(|_| Left::Io)?.volume;
        if id.file != self.file || volume != id.volume {
            return Err(Left::Replaced);
        }
        Ok((dir, file))
    }

    /// The partial file's length, when it is still under its name: its
    /// folder found as [`Self::reopen`] finds it, the name inspected with
    /// no link followed, and its identity and volume compared as there.
    /// No byte of it is read, and the handle conflicts with no other, a
    /// transfer writing the file included: Windows opens it for its
    /// attributes alone, sharing everything, and Linux and Android as a
    /// path only; macOS and the BSDs, which have neither, open it for
    /// reading without waiting, and read nothing. `None` when its name is
    /// not a partial's under `settings` ([`Self::is_partial`], nothing
    /// opened), it is gone, another file has its name, or nothing is
    /// known of it now.
    pub fn present(&self, settings: &Settings) -> Option<u64> {
        if !self.is_partial(settings) {
            return None;
        }
        let dir = self.location.find().ok()?;
        let file = dir.inspect(OsStr::new(&self.partial)).ok()?;
        let meta = file.metadata().ok()?;
        let id = identity(&file).ok()?;
        let same = id.file == self.file && dir.identity().ok()?.volume == id.volume;
        (same && meta.is_file()).then_some(meta.len())
    }
}

/// Whether `found` is `name` or a numbered form of it ("name (2).ext"):
/// the one the rules cut to fit, or the whole name with the number, as a
/// publish that cuts nothing writes it.
fn named_as(found: &str, name: &str, settings: &Settings) -> bool {
    if found == name {
        return true;
    }
    // The number is the last " (n)" in the name, before any extension.
    let Some(open) = found.rfind(" (") else {
        return false;
    };
    let Some(close) = found[open..].find(')') else {
        return false;
    };
    let digits = &found[open + 2..open + close];
    let Ok(n) = digits.parse::<usize>() else {
        return false;
    };
    let (stem, ext) = safecopy_names::stem_and_ext(name);
    n >= 2
        && !digits.starts_with('0')
        && (safecopy_names::numbered(name, n, &settings.names).as_deref() == Some(found)
            || found == format!("{stem} ({n}){ext}"))
}

/// A published file in `dir` that is the partial `file` named as it
/// would be published: its ID, a name that is `name` or a numbered form of
/// it and does not end in the partial ending, and `size` bytes. Its name.
pub(crate) fn find_published(
    dir: &Directory,
    name: &str,
    file: u128,
    size: u64,
    settings: &Settings,
) -> Option<String> {
    let names = dir.names(settings.list_most).ok()?;
    for found in names.into_iter().map_while(Result::ok) {
        let Some(found) = found.to_str() else {
            continue;
        };
        if found.ends_with(settings.partial_ending.as_str()) || !named_as(found, name, settings) {
            continue;
        }
        let Some(candidate) = dir.candidate(OsStr::new(found), size) else {
            continue;
        };
        if identity(candidate.file()).is_ok_and(|id| id.file == file) {
            return Some(found.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_numbered_form_is_the_name_with_its_number_cut_or_whole() {
        let settings = Settings::default();
        assert!(named_as("report.pdf", "report.pdf", &settings));
        assert!(named_as("report (2).pdf", "report.pdf", &settings));
        assert!(named_as("report (17).pdf", "report.pdf", &settings));
        assert!(!named_as("report (1).pdf", "report.pdf", &settings));
        assert!(!named_as("report (02).pdf", "report.pdf", &settings));
        assert!(!named_as("report (2).pdf.part", "report.pdf", &settings));
        assert!(!named_as("other (2).pdf", "report.pdf", &settings));
        assert!(!named_as("report (x).pdf", "report.pdf", &settings));
        // A name at the limit is published with its number after it, uncut;
        // the name the rules would cut to make room is one too.
        for name in [
            "я".repeat(100),
            "文".repeat(66),
            "x".repeat(200),
            format!("{}.txt", "я".repeat(98)),
        ] {
            let (stem, ext) = safecopy_names::stem_and_ext(&name);
            let whole = format!("{stem} (2){ext}");
            assert!(named_as(&whole, &name, &settings), "{whole}");
            let cut = safecopy_names::numbered(&name, 2, &settings.names).unwrap();
            assert!(cut != whole && named_as(&cut, &name, &settings), "{cut}");
            assert!(!named_as(&format!("{stem} (02){ext}"), &name, &settings));
            assert!(!named_as(&format!("{stem}x (2){ext}"), &name, &settings));
        }
    }
}
