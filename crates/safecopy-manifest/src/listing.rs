//! One folder walked by handle, to be sent whole.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use safecopy_folder::{Directory, is_link, refusal};

use crate::sources::unreadable;
use crate::{Limits, ManifestError};

/// What a folder to send holds, and the folder held open to send it from.
#[derive(Debug)]
pub struct Listing {
    /// The folder's own name, as the system gave it (any part that is not
    /// Unicode replaced).
    pub name: String,
    /// Its files, and the folders with nothing in them to send, in the
    /// order they go: depth first, each folder's files before its folders.
    pub entries: Vec<Listed>,
    /// What is not sent, and why.
    pub skipped: Vec<Skipped>,
    /// How many files.
    pub files: u32,
    /// The sum of the files' sizes when they were listed.
    pub bytes: u64,
    root: Directory,
}

/// A file, or a folder with nothing in it to send, of a [`Listing`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// Its path in the folder, the folder's name first, with `/`; a
    /// folder's ends in `/`.
    pub name: String,
    /// A folder, not a file.
    pub folder: bool,
    /// A file's size when it was listed; a folder's is 0.
    pub size: u64,
    /// Its names on disk below the listed folder.
    parts: Vec<OsString>,
}

impl Listed {
    /// Its names on disk below the listed folder.
    pub fn parts(&self) -> &[OsString] {
        &self.parts
    }
}

/// Something in the folder that is not sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// Its path in the folder, as [`Listed::name`] gives one.
    pub path: String,
    /// Why.
    pub why: Skip,
}

/// Why something in a folder is not sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Skip {
    /// It could not be read. A folder that could not be read still goes,
    /// empty.
    Unreadable,
    /// A link or junction, which is not followed.
    Link,
    /// Deeper than [`Limits::depth`], or its path is not shorter than
    /// [`Limits::path_chars`].
    TooDeep,
    /// Neither a file nor a folder: a device, a pipe or a socket.
    NotFileOrFolder,
    /// This many names in the folder could not be read.
    Unlisted {
        /// How many.
        count: usize,
    },
}

impl Listing {
    /// Walks the folder at `root` by handle: what goes, and what is
    /// skipped. Links and junctions inside are skipped, not followed; one
    /// at `root` itself is refused. More than [`Limits::entries`] names in
    /// all refuses the folder as soon as the one past it is read.
    pub fn list(root: &Path, limits: &Limits) -> Result<Self, ManifestError> {
        let name = root
            .file_name()
            .ok_or(ManifestError::WholeDrive)?
            .to_string_lossy()
            .into_owned();
        let opened = Directory::open(root).map_err(|e| unreadable(root, &e))?;
        let most = limits.entries;
        let mut listing = Listing {
            name: name.clone(),
            entries: Vec::new(),
            skipped: Vec::new(),
            files: 0,
            bytes: 0,
            root: opened,
        };
        let mut found = 0usize;
        // Depth first, each folder's files before what is below them.
        let mut folders = vec![(Vec::<OsString>::new(), name, 1usize)];
        while let Some((parts, at, depth)) = folders.pop() {
            // A folder with nothing in it to send still goes, as a folder of
            // its own: one that cannot be read, or holds only links.
            let own = Listed {
                name: format!("{at}/"),
                folder: true,
                size: 0,
                parts: parts.clone(),
            };
            let read = open_directory(&listing.root, &parts).and_then(|directory| {
                let read = directory.names(most.saturating_sub(found).saturating_add(1))?;
                Ok((directory, read))
            });
            let (directory, read) = match read {
                Ok(read) => read,
                Err(e) if depth == 1 => return Err(unreadable(root, &e)),
                Err(_) => {
                    listing.skipped.push(Skipped {
                        path: format!("{at}/"),
                        why: Skip::Unreadable,
                    });
                    listing.entries.push(own);
                    continue;
                }
            };
            let (mut children, unlisted) = taken(read.into_iter(), &mut found, most)?;
            if unlisted > 0 {
                listing.skipped.push(Skipped {
                    path: format!("{at}/"),
                    why: Skip::Unlisted { count: unlisted },
                });
            }
            children.sort();
            let mut below = Vec::new();
            let mut kept = 0;
            for child in children {
                let path = format!("{at}/{}", child.to_string_lossy());
                let mut child_parts = parts.clone();
                child_parts.push(child);
                let child = child_parts.last().expect("a name");
                let file = match directory.entry(child) {
                    Ok(file) => file,
                    Err(e) => {
                        let why = skip_for(&e);
                        listing.skipped.push(Skipped { path, why });
                        continue;
                    }
                };
                let meta = file
                    .metadata()
                    .map_err(|e| unreadable(&source(root, &child_parts), &e))?;
                if depth + 1 > limits.depth || path.chars().count() >= limits.path_chars {
                    listing.skipped.push(Skipped {
                        path,
                        why: Skip::TooDeep,
                    });
                } else if meta.is_dir() {
                    kept += 1;
                    below.push((child_parts, path, depth + 1));
                } else if meta.is_file() {
                    kept += 1;
                    listing.files += 1;
                    let size = meta.len();
                    listing.bytes = add_size(listing.bytes, size);
                    listing.entries.push(Listed {
                        name: path,
                        folder: false,
                        size,
                        parts: child_parts,
                    });
                } else {
                    listing.skipped.push(Skipped {
                        path,
                        why: Skip::NotFileOrFolder,
                    });
                }
            }
            if kept == 0 && depth > 1 {
                listing.entries.push(own);
            }
            folders.extend(below.into_iter().rev());
        }
        Ok(listing)
    }

    /// Opens `entry` from the held folder, each folder on the way from its
    /// opened parent and no link followed: a file to read with its size
    /// now, or `None` for a folder that is still there. A file larger than
    /// it was listed is [`ManifestError::Changed`], so nothing that grew is
    /// sent; a smaller one opens, with its smaller size.
    pub fn open_entry(&self, entry: &Listed) -> Result<Option<(File, u64)>, ManifestError> {
        let at = || self.root.path().join(source(Path::new(""), &entry.parts));
        let failed = |e: io::Error| unreadable(&at(), &e);
        if entry.folder {
            open_directory(&self.root, &entry.parts).map_err(failed)?;
            return Ok(None);
        }
        let (last, within) = entry.parts.split_last().ok_or(ManifestError::Changed)?;
        let parent = open_directory(&self.root, within).map_err(failed)?;
        let file = parent.entry(last).map_err(failed)?;
        let meta = file.metadata().map_err(failed)?;
        if !meta.is_file() || meta.len() > entry.size {
            return Err(ManifestError::Changed);
        }
        Ok(Some((file, meta.len())))
    }
}

/// The listing's total with one more file: it stops at the largest
/// number rather than wrapping.
pub(crate) fn add_size(bytes: u64, size: u64) -> u64 {
    bytes.saturating_add(size)
}

/// `parts` below `root` as a path.
fn source(root: &Path, parts: &[OsString]) -> PathBuf {
    let mut path = root.to_path_buf();
    path.extend(parts);
    path
}

fn skip_for(e: &io::Error) -> Skip {
    if is_link(e) {
        Skip::Link
    } else if refusal(e) == Some(safecopy_folder::Refusal::NotFileOrFolder) {
        Skip::NotFileOrFolder
    } else {
        Skip::Unreadable
    }
}

/// The folder `parts` below `root`, each part opened from its opened
/// parent, so a replacement on the way cannot redirect it.
fn open_directory(root: &Directory, parts: &[OsString]) -> io::Result<Directory> {
    let mut dir = root.try_clone()?;
    for part in parts {
        dir = dir.directory(part)?;
    }
    Ok(dir)
}

/// The names of one folder as they are read, each counted in `found`: past
/// `most` the folder is refused at once, without reading the rest. Also how
/// many could not be read.
pub(crate) fn taken<T>(
    entries: impl Iterator<Item = io::Result<T>>,
    found: &mut usize,
    most: usize,
) -> Result<(Vec<T>, usize), ManifestError> {
    let (mut read, mut unreadable) = (Vec::new(), 0);
    for entry in entries {
        *found += 1;
        if *found > most {
            return Err(ManifestError::TooMany { most });
        }
        match entry {
            Ok(entry) => read.push(entry),
            Err(_) => unreadable += 1,
        }
    }
    Ok((read, unreadable))
}
