//! What is already at the destination: a file that may be the one coming,
//! and a folder to continue into.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::time::SystemTime;

use crate::{Directory, FileId, component, identity, platform};

/// A regular file already in a folder, of the name and size of a file
/// coming in: [`Directory::candidate`]. It is only ever read.
///
/// Its identity, size and modified time are taken when it is opened;
/// [`Self::unchanged`] compares them again, and the name must still lead
/// to the same file. A hash of it counts only when it was unchanged before
/// and after (`safecopy-chunks` hashes it that way).
#[derive(Debug)]
pub struct Candidate {
    folder: Directory,
    name: OsString,
    file: File,
    id: FileId,
    size: u64,
    modified: Option<SystemTime>,
}

impl Candidate {
    /// The open file, for reading.
    pub fn file(&self) -> &File {
        &self.file
    }

    /// Its size when it was opened, the size asked for.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Its name in the folder.
    pub fn name(&self) -> &OsStr {
        &self.name
    }

    /// Whether the file is as it was when opened: the same size and
    /// modified time, and its name still leads, without a link, to this
    /// same file. Anything that cannot be read counts as changed.
    pub fn unchanged(&self) -> bool {
        let Ok(now) = self.file.metadata() else {
            return false;
        };
        if now.len() != self.size || now.modified().ok() != self.modified {
            return false;
        }
        // A file put in its place, or the name gone: the hash would not be of what is there.
        let Ok(there) = self.folder.inspect(&self.name) else {
            return false;
        };
        identity(&there).is_ok_and(|id| id == self.id)
    }
}

impl Directory {
    /// The file `name` here, when it could be the file coming in: exactly
    /// that name (not another case of it, nor another form of its
    /// characters, nor a short name), a regular file, no link followed, and
    /// `size` bytes long. Linux and Android check that by a path handle
    /// before opening anything for reading, and then that the file opened
    /// is the one checked. Opened for reading by handle, sharing with every
    /// other user; never written, renamed or locked. `None` when there is
    /// no such file or it cannot be opened (locked against reading, gone).
    pub fn candidate(&self, name: &OsStr, size: u64) -> Option<Candidate> {
        component(name).ok()?;
        // Linux and Android look first, by a path handle: opening a device,
        // a pipe or a FUSE file for reading can have effects of its own.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let looked = {
            let path = platform::inspect(&self.file, name).ok()?;
            let meta = path.metadata().ok()?;
            if !meta.is_file() || meta.len() != size {
                return None;
            }
            identity(&path).ok()?
        };
        let file = platform::open(&self.file, name, false, false).ok()?;
        let meta = file.metadata().ok()?;
        if !meta.is_file() || meta.len() != size {
            return None;
        }
        let id = identity(&file).ok()?;
        // Another file put in its place between the look and the open.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if id != looked {
            return None;
        }
        if !platform::exact_name(&self.file, &file, name).ok()? {
            return None;
        }
        Some(Candidate {
            folder: self.try_clone().ok()?,
            name: name.to_os_string(),
            id,
            size,
            modified: meta.modified().ok(),
            file,
        })
    }

    /// The folder `name` here, to continue into: exactly that name, no link
    /// or junction followed, and open with the rights to add files and
    /// folders to it (on Windows the handle holds them; on Unix the
    /// folder's write and search permissions are checked). `None` for
    /// anything else: no such name, a file, a link, a folder this user
    /// cannot add to.
    pub fn existing(&self, name: &OsStr) -> Option<Self> {
        component(name).ok()?;
        let file = platform::open_existing(&self.file, name).ok()?;
        if !platform::exact_name(&self.file, &file, name).ok()? {
            return None;
        }
        Self::from_file(file, self.path.join(name)).ok()
    }
}
