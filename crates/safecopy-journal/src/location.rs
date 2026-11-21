//! Where a partial file is, found again by identity after a crash.

use std::ffi::OsStr;
use std::io;
use std::path::Component;

use safecopy_folder::{Directory, FileId, identity};

use crate::log::Root;
use crate::{JournalError, Settings, removable};

/// One folder below the root, on the way to a partial file: its name in
/// the folder above and its identity.
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
/// the way. On Windows the last folder's file ID opens it; on Unix, which
/// has no open by ID, the root is opened by its path and each level below
/// it by name, no link followed, each one's device and inode compared.
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
    /// A recorded name that cleanup never removes.
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
    /// Any other error of the system's: worth another try.
    Io,
}

impl Left {
    /// Whether another cleanup could still remove the partial: its journal
    /// is kept for it.
    pub(crate) fn may_change(self) -> bool {
        matches!(self, Self::Unreachable | Self::Io)
    }
}

/// An error opening the recorded folder or file: what it says of it.
pub(crate) fn left(error: &io::Error) -> Left {
    if safecopy_folder::refusal(error).is_some() {
        // A link or a folder under the name, a file under the folder's ID.
        return Left::Replaced;
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
/// above with no link followed: refused with [`JournalError::NotBelow`]
/// when `dir` is not the folder its path below `root` names.
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
            let dir = crate::win::open_folder(&volume, id).map_err(|e| left(&e))?;
            let got = dir.identity().map_err(|_| Left::Io)?;
            if got.volume != *serial || got.file != id {
                return Err(Left::Replaced);
            }
            Ok(dir)
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
            let path = std::path::Path::new(OsStr::from_bytes(path));
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
    /// recorded with its identity, as a journal records it.
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

    /// Removes the partial file, only through a handle checked to be it:
    /// its folder found by identity, the name ending in
    /// [`Settings::partial_ending`], its file ID the recorded one. Windows
    /// deletes through that handle; Unix removes the name after checking
    /// it still is that file. Anything else is left.
    pub fn remove(&self, settings: &Settings) -> bool {
        self.remove_checked(settings).is_ok()
    }

    pub(crate) fn remove_checked(&self, settings: &Settings) -> Result<(), Left> {
        if !removable(&self.partial, settings) {
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
    /// its folder: `None` when it is gone or another file has its name.
    pub fn reopen(&self) -> Option<(Directory, std::fs::File)> {
        let dir = self.location.find().ok()?;
        let file = dir.reopen(OsStr::new(&self.partial)).ok()?;
        let id = identity(&file).ok()?;
        (id.file == self.file && dir.identity().ok()?.volume == id.volume).then_some((dir, file))
    }
}

/// Whether `found` is `name` or a numbered form of it ("name (2).ext").
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
    n >= 2
        && !digits.starts_with('0')
        && safecopy_names::numbered(name, n, &settings.names).as_deref() == Some(found)
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
    fn a_numbered_form_is_only_the_one_the_names_crate_makes() {
        let settings = Settings::default();
        assert!(named_as("report.pdf", "report.pdf", &settings));
        assert!(named_as("report (2).pdf", "report.pdf", &settings));
        assert!(named_as("report (17).pdf", "report.pdf", &settings));
        assert!(!named_as("report (1).pdf", "report.pdf", &settings));
        assert!(!named_as("report (02).pdf", "report.pdf", &settings));
        assert!(!named_as("report (2).pdf.part", "report.pdf", &settings));
        assert!(!named_as("other (2).pdf", "report.pdf", &settings));
        assert!(!named_as("report (x).pdf", "report.pdf", &settings));
    }
}
