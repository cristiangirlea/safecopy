//! Listing what a person picked, and opening it again for sending.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use safecopy_folder::{Directory, FileId, Refusal, Unfit, identity, is_link, refusal};
use safecopy_names::Siblings;

use crate::{Entry, Kind, Limits, Manifest, ManifestError, add_within, check};

// Windows' attributes of a file kept by a cloud or storage service.
pub(crate) const OFFLINE: u32 = 0x0000_1000;
pub(crate) const RECALL_ON_OPEN: u32 = 0x0004_0000;
pub(crate) const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// What is kept of one entry to find it again.
#[derive(Debug)]
struct Source {
    /// Which of the held folders its names start from.
    folder: usize,
    /// Its names on disk below that folder, the picked file or folder first.
    names: Vec<OsString>,
    /// The identity of each folder on the way, the picked one first.
    ancestors: Vec<FileId>,
    kind: Kind,
    id: FileId,
    size: u64,
    modified: Option<SystemTime>,
    online_only: bool,
}

/// What is kept to send the files of a [`Manifest`]: the folders the
/// picked files and folders are in, held open, and what each entry was
/// when it was listed. Nothing else can be opened through it.
#[derive(Debug)]
pub struct Sources {
    id: u64,
    pub(crate) folders: Vec<Directory>,
    entries: Vec<Source>,
}

/// A listed file, opened to be sent.
#[derive(Debug)]
pub struct Opened {
    /// The file, open for reading.
    pub file: File,
    size: u64,
    modified: Option<SystemTime>,
}

impl Opened {
    /// The size it had when it was listed, and still has.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Whether the file still has the size and modified time it was listed
    /// with: check it again while and after it is read, since another
    /// program can write to it meanwhile.
    pub fn unchanged(&self) -> Result<(), ManifestError> {
        let meta = self.file.metadata().map_err(|_| ManifestError::Changed)?;
        if meta.len() != self.size || meta.modified().ok() != self.modified {
            return Err(ManifestError::Changed);
        }
        Ok(())
    }
}

impl Sources {
    /// Lists the files and folders at `roots`, as a person picked them, as
    /// list `id`. Folders are walked by handle. A link, a junction or
    /// anything unreadable among what was picked refuses the whole list;
    /// links on the way to the folders they are in are followed. An
    /// online-only file is listed from its metadata, and fetched only when
    /// [`Self::open`] opens it.
    pub fn list<P: AsRef<Path>>(
        id: u64,
        roots: &[P],
        limits: &Limits,
    ) -> Result<(Manifest, Self), ManifestError> {
        list_within(id, roots, limits, None)
    }

    /// As [`Self::list`], refused with [`ManifestError::KeptOut`] when
    /// anything picked is one of the folders `kept_out`, lies inside one,
    /// holds one or is anything in them. Judged by the identity of all
    /// they hold (at most [`Limits::kept_entries`]) and by where the
    /// folders the picks are in really are, so a link or a hard link to
    /// them counts too. A plain copy made elsewhere has an identity of its
    /// own and is not recognised: this guards against mistakes, not against
    /// someone in control of the computer. A kept-out folder that cannot be
    /// opened or walked refuses every list.
    pub fn list_keeping_out<P: AsRef<Path>, K: AsRef<Path>>(
        id: u64,
        roots: &[P],
        kept_out: &[K],
        limits: &Limits,
    ) -> Result<(Manifest, Self), ManifestError> {
        let kept = Kept::open(kept_out, limits)?;
        list_within(id, roots, limits, Some(&kept))
    }

    /// The list's id.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Whether entry `n` is a file a cloud service keeps online only, whose
    /// contents are fetched when it is opened.
    pub fn online_only(&self, n: u32) -> bool {
        self.entries
            .get(n as usize)
            .is_some_and(|source| source.online_only)
    }

    /// Opens entry `n` from the held folders, following no link: a file to
    /// read, or `None` for a folder that is still there. A file that is no
    /// longer a regular file (a folder, a device, a pipe or a socket under
    /// its name) is [`ManifestError::NotAFile`]; anything else not as it
    /// was listed is [`ManifestError::Changed`].
    pub fn open(&self, n: u32) -> Result<Option<Opened>, ManifestError> {
        let changed = |_| ManifestError::Changed;
        let source = self
            .entries
            .get(n as usize)
            .ok_or(ManifestError::NotListed)?;
        let (last, within) = source.names.split_last().ok_or(ManifestError::Changed)?;
        let mut dir = self.folders[source.folder].try_clone().map_err(changed)?;
        for (name, id) in within.iter().zip(&source.ancestors) {
            dir = dir.directory(name).map_err(changed)?;
            if dir.identity().ok() != Some(*id) {
                return Err(ManifestError::Changed);
            }
        }
        match source.kind {
            Kind::Folder => {
                let folder = dir.directory(last).map_err(changed)?;
                if folder.identity().ok() != Some(source.id) {
                    return Err(ManifestError::Changed);
                }
                Ok(None)
            }
            Kind::File => {
                let not_a_file = || {
                    let mut at = self.folders[source.folder].path().to_path_buf();
                    at.extend(&source.names);
                    ManifestError::NotAFile { at }
                };
                let file = dir.entry(last).map_err(|e| match refusal(&e) {
                    Some(Refusal::NotFileOrFolder) => not_a_file(),
                    _ => ManifestError::Changed,
                })?;
                let meta = file.metadata().map_err(changed)?;
                if !meta.is_file() {
                    return Err(not_a_file());
                }
                if identity(&file).ok() != Some(source.id) {
                    return Err(ManifestError::Changed);
                }
                let opened = Opened {
                    file,
                    size: source.size,
                    modified: source.modified,
                };
                opened.unchanged()?;
                Ok(Some(opened))
            }
        }
    }
}

/// Folders no list may reach: where each really is, and the identity of
/// every file and folder in them.
pub(crate) struct Kept {
    at: Vec<PathBuf>,
    ids: HashSet<FileId>,
    most: usize,
    depth: usize,
}

impl Kept {
    pub(crate) fn open<K: AsRef<Path>>(
        paths: &[K],
        limits: &Limits,
    ) -> Result<Self, ManifestError> {
        let kept_out = |_| ManifestError::KeptOut;
        let mut kept = Self {
            at: Vec::new(),
            ids: HashSet::new(),
            most: limits.kept_entries,
            depth: limits.depth,
        };
        for path in paths {
            // Unsure where it is or what it holds: nothing is listed rather than it.
            let folder = Directory::followed(path.as_ref()).map_err(kept_out)?;
            kept.at.push(final_path(&folder).map_err(kept_out)?);
            kept.ids.insert(folder.identity().map_err(kept_out)?);
            kept.walk(&folder, 1)?;
        }
        Ok(kept)
    }

    fn walk(&mut self, folder: &Directory, depth: usize) -> Result<(), ManifestError> {
        let kept_out = |_| ManifestError::KeptOut;
        let room = self.most.saturating_sub(self.ids.len());
        let names = folder.names(room.saturating_add(1)).map_err(kept_out)?;
        if names.len() > room || depth > self.depth {
            return Err(ManifestError::KeptOut);
        }
        for name in names {
            let name = name.map_err(kept_out)?;
            #[cfg(test)]
            gone_before(&crate::seams::GONE_BEFORE_INSPECT, folder, &name);
            let file = match inspect(folder, &name) {
                Ok(file) => file,
                // A link there is not followed by a list either.
                Err(e) if is_link(&e) => continue,
                // A device, pipe or socket holds nothing; a list refuses one picked anyway.
                Err(e) if refusal(&e) == Some(Refusal::NotFileOrFolder) => continue,
                // Files saved by renaming over them: one gone since the listing holds nothing.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => return Err(ManifestError::KeptOut),
            };
            self.ids.insert(identity(&file).map_err(kept_out)?);
            if file.metadata().map_err(kept_out)?.is_dir() {
                drop(file);
                #[cfg(test)]
                gone_before(&crate::seams::GONE_BEFORE_OPEN, folder, &name);
                let below = match folder.directory(&name) {
                    Ok(below) => below,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(_) => return Err(ManifestError::KeptOut),
                };
                self.walk(&below, depth + 1)?;
            }
        }
        Ok(())
    }

    /// Whether `folder`, held open, is one of these folders, inside one,
    /// or anything in them; where that is unsure, it is.
    pub(crate) fn holds(&self, folder: &Directory) -> bool {
        if folder.identity().map_or(true, |id| self.ids.contains(&id)) {
            return true;
        }
        match final_path(folder) {
            Ok(at) => self.at.iter().any(|kept| at.starts_with(kept)),
            Err(_) => true,
        }
    }
}

fn inspect(folder: &Directory, name: &OsStr) -> io::Result<File> {
    #[cfg(test)]
    if let Some(kind) = crate::seams::INSPECT_FAILS.with_borrow(|fails| fails.get(name).copied()) {
        return Err(kind.into());
    }
    folder.inspect(name)
}

/// Tests take an entry away while the kept-out folders are walked.
#[cfg(test)]
fn gone_before(
    names: &'static std::thread::LocalKey<std::cell::RefCell<HashSet<OsString>>>,
    folder: &Directory,
    name: &OsStr,
) {
    if names.with_borrow_mut(|gone| gone.remove(name)) {
        let path = folder.final_path().unwrap().join(name);
        if path.is_dir() {
            std::fs::remove_dir_all(&path).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
    }
}

/// Whether `folder` is on no volume of this computer's (Windows).
fn on_the_network(folder: &Directory) -> bool {
    #[cfg(test)]
    if crate::seams::NO_VOLUME.get() {
        return true;
    }
    folder.unfit::<&Path>(&[]) == Some(Unfit::Network)
}

/// Where a folder held open really is, read from its handle, so two
/// folders' paths compare in one form.
fn final_path(folder: &Directory) -> io::Result<PathBuf> {
    #[cfg(test)]
    if crate::seams::FINAL_PATH_FAILS.get() {
        return Err(io::Error::other("final path failed"));
    }
    folder.final_path()
}

pub(crate) fn list_within<P: AsRef<Path>>(
    id: u64,
    roots: &[P],
    limits: &Limits,
    kept: Option<&Kept>,
) -> Result<(Manifest, Sources), ManifestError> {
    let mut folders = Vec::new();
    let mut opened: HashMap<&Path, usize> = HashMap::new();
    let mut walk = Walk {
        kept: kept.map(|k| &k.ids),
        limits,
        entries: Vec::new(),
        sources: Vec::new(),
        bytes: 0,
        pending: 0,
    };
    let mut top = Siblings::new(limits.names.clone());
    for root in roots {
        let root = root.as_ref();
        let name = root.file_name().ok_or(ManifestError::WholeDrive)?;
        let parent = root
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or(ManifestError::WholeDrive)?;
        let folder = match opened.get(parent) {
            Some(&folder) => folder,
            None => {
                if folders.len() >= limits.source_folders {
                    return Err(ManifestError::TooManyFolders {
                        most: limits.source_folders,
                    });
                }
                // Not part of the list, and held open from here: links to it are fine.
                let dir = Directory::followed(parent).map_err(|e| unreadable(parent, &e))?;
                if kept.is_some_and(|kept| kept.holds(&dir)) {
                    return Err(ManifestError::KeptOut);
                }
                if on_the_network(&dir) {
                    return Err(ManifestError::Network);
                }
                folders.push(dir);
                opened.insert(parent, folders.len() - 1);
                folders.len() - 1
            }
        };
        let path = top.take(name).map_err(|why| ManifestError::BadName {
            at: root.to_path_buf(),
            why,
        })?;
        walk.add(
            &folders[folder],
            folder,
            vec![name.to_os_string()],
            Vec::new(),
            path,
            root,
        )?;
    }
    let manifest = check(id, walk.entries, limits)?;
    manifest.encode(limits)?;
    Ok((
        manifest,
        Sources {
            id,
            folders,
            entries: walk.sources,
        },
    ))
}

/// Why `at` could not be listed.
pub(crate) fn unreadable(at: &Path, e: &io::Error) -> ManifestError {
    let at = at.to_path_buf();
    match refusal(e) {
        Some(Refusal::Link) => ManifestError::Link { at },
        Some(Refusal::NotFileOrFolder) => ManifestError::NotFileOrFolder { at },
        _ => ManifestError::Unreadable {
            at,
            kind: e.kind(),
            raw: e.raw_os_error(),
        },
    }
}

struct Walk<'a> {
    /// What the kept-out folders hold, when the list must not reach it.
    kept: Option<&'a HashSet<FileId>>,
    limits: &'a Limits,
    entries: Vec<Entry>,
    sources: Vec<Source>,
    bytes: u64,
    /// Names read from folders above and not listed yet: they count too.
    pending: usize,
}

impl Walk<'_> {
    /// Lists the last of `names` in `parent`, a folder with all it holds,
    /// as `path`; `at` is where it is on disk.
    fn add(
        &mut self,
        parent: &Directory,
        folder: usize,
        names: Vec<OsString>,
        ancestors: Vec<FileId>,
        path: String,
        at: &Path,
    ) -> Result<(), ManifestError> {
        let limits = self.limits;
        if names.len() > limits.depth {
            return Err(ManifestError::TooDeep {
                at: at.to_path_buf(),
                most: limits.depth,
            });
        }
        if path.chars().count() > limits.path_chars {
            return Err(ManifestError::PathTooLong {
                at: at.to_path_buf(),
                most: limits.path_chars,
            });
        }
        let most = limits.most_entries();
        if self.entries.len() >= most {
            return Err(ManifestError::TooMany { most });
        }
        let name = names.last().expect("a name");
        let file = parent.inspect(name).map_err(|e| unreadable(at, &e))?;
        let meta = file.metadata().map_err(|e| unreadable(at, &e))?;
        let id = identity(&file).map_err(|e| unreadable(at, &e))?;
        if self.kept.is_some_and(|kept| kept.contains(&id)) {
            return Err(ManifestError::KeptOut);
        }
        let modified = meta.modified().ok();
        if meta.is_file() {
            let online_only = online_only(attributes(&meta, name), at)?;
            let size = meta.len();
            self.bytes = add_within(self.bytes, size, limits.bytes)?;
            self.entries.push(Entry {
                path,
                kind: Kind::File,
                size,
            });
            self.sources.push(Source {
                folder,
                names,
                ancestors,
                kind: Kind::File,
                id,
                size,
                modified,
                online_only,
            });
            return Ok(());
        }
        drop(file);
        let dir = parent.directory(name).map_err(|e| unreadable(at, &e))?;
        // Put in another's place between the look and the open.
        if dir.identity().ok() != Some(id) {
            return Err(ManifestError::ChangedWhileListed {
                at: at.to_path_buf(),
            });
        }
        self.entries.push(Entry {
            path: path.clone(),
            kind: Kind::Folder,
            size: 0,
        });
        self.sources.push(Source {
            folder,
            names: names.clone(),
            ancestors: ancestors.clone(),
            kind: Kind::Folder,
            id,
            size: 0,
            modified,
            online_only: false,
        });
        let room = most.saturating_sub(self.entries.len() + self.pending);
        let mut children = Vec::new();
        for child in dir
            .names(room.saturating_add(1))
            .map_err(|e| unreadable(at, &e))?
        {
            children.push(child.map_err(|e| unreadable(at, &e))?);
        }
        if children.len() > room {
            return Err(ManifestError::TooMany { most });
        }
        children.sort();
        self.pending += children.len();
        let mut siblings = Siblings::new(limits.names.clone());
        for child in children {
            self.pending -= 1;
            let below_at = at.join(&child);
            let safe = siblings
                .take(&child)
                .map_err(|why| ManifestError::BadName {
                    at: below_at.clone(),
                    why,
                })?;
            let mut below = names.clone();
            below.push(child);
            let mut parents = ancestors.clone();
            parents.push(id);
            self.add(
                &dir,
                folder,
                below,
                parents,
                format!("{path}/{safe}"),
                &below_at,
            )?;
        }
        Ok(())
    }
}

/// Whether a file with these Windows attributes is kept online only, its
/// contents fetched when it is opened or read. One a storage service took
/// offline, with no way to ask for it, is refused.
pub(crate) fn online_only(attributes: u32, at: &Path) -> Result<bool, ManifestError> {
    if attributes & (RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) != 0 {
        return Ok(true);
    }
    if attributes & OFFLINE != 0 {
        return Err(ManifestError::Offline {
            at: at.to_path_buf(),
        });
    }
    Ok(false)
}

fn attributes(meta: &std::fs::Metadata, name: &OsStr) -> u32 {
    #[cfg(windows)]
    let found = std::os::windows::fs::MetadataExt::file_attributes(meta);
    #[cfg(not(windows))]
    let found = {
        let _ = meta;
        0
    };
    #[cfg(test)]
    let found =
        found | crate::seams::ATTRIBUTES.with_borrow(|given| given.get(name).copied().unwrap_or(0));
    #[cfg(not(test))]
    let _ = name;
    found
}
