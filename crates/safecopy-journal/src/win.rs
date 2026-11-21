//! Windows: journal files held without write sharing, and folders opened
//! again by their file ID.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Seek, Write};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use safecopy_folder::{Directory, identity};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    DELETE, ExtendedFileIdType, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_ID_128,
    FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OpenFileById,
};

use crate::{EXTENSION, JournalError, Partial, Record, Settings, removable};

/// How many taken names a new journal steps past before it gives up.
const NAME_TRIES: u32 = 64;

static NEXT: AtomicU64 = AtomicU64::new(1);

/// A journal being written.
pub(crate) struct Open {
    file: Mutex<File>,
    record: Mutex<Record>,
    settings: Settings,
    /// Left for cleanup even with nothing recorded: a crash acted out.
    keep: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic elsewhere leaves the record as whole as it was.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Open {
    pub(crate) fn start(
        dir: &Path,
        root: &Directory,
        settings: Settings,
    ) -> Result<Self, JournalError> {
        let volume = root.volume_path()?;
        let serial = root.identity()?.volume;
        let file = create_named(dir, &NEXT)?;
        let open = Self {
            file: Mutex::new(file),
            record: Mutex::new(Record {
                volume,
                serial,
                partials: Vec::new(),
            }),
            settings,
            keep: AtomicBool::new(false),
        };
        // Dropped on failure with nothing recorded: the file goes again.
        open.write(&lock(&open.record))?;
        Ok(open)
    }

    pub(crate) fn started(
        &self,
        dir: &Directory,
        file: &File,
        name: &OsStr,
    ) -> Result<(), JournalError> {
        let (folder, id) = (dir.identity()?, identity(file)?);
        let partial = Partial {
            folder: folder.file,
            name: name.to_str().ok_or(JournalError::NameNotText)?.to_string(),
            file: id.file,
        };
        let mut record = lock(&self.record);
        // A file ID is unique on its volume only: cleanup looks on the recorded one.
        if folder.volume != record.serial || id.volume != record.serial {
            return Err(JournalError::OtherVolume);
        }
        let before = record.clone();
        record.partials.push(partial);
        if let Err(e) = self.replace(&record, &before) {
            // Not on the disk, so not recorded: the caller removes the file.
            *record = before;
            return Err(e);
        }
        Ok(())
    }

    pub(crate) fn ended(&self, dir: &Directory, name: &OsStr) -> Result<(), JournalError> {
        let folder = dir.identity()?.file;
        let mut record = lock(&self.record);
        let before = record.clone();
        record
            .partials
            .retain(|p| !(p.folder == folder && OsStr::new(&p.name) == name));
        self.replace(&record, &before)
    }

    /// Writes `record` over `before`; when that fails, puts `before` back as
    /// well as it can, so the partials it held are still found.
    fn replace(&self, record: &Record, before: &Record) -> Result<(), JournalError> {
        self.write(record).inspect_err(|_| {
            let _ = self.write(before);
        })
    }

    #[cfg(test)]
    pub(crate) fn keep(&self) {
        self.keep.store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn record(&self) -> MutexGuard<'_, Record> {
        lock(&self.record)
    }

    pub(crate) fn write(&self, record: &Record) -> Result<(), JournalError> {
        let bytes = record.encode(&self.settings)?;
        let mut file = lock(&self.file);
        // The room first, so a full disk fails before the old bytes change.
        if file.metadata()?.len() < bytes.len() as u64 {
            file.set_len(bytes.len() as u64)?;
        }
        file.rewind()?;
        #[cfg(test)]
        if seams::FAIL_WRITES.get() > 0 {
            seams::FAIL_WRITES.set(seams::FAIL_WRITES.get() - 1);
            file.write_all(&bytes[..bytes.len() / 2])?;
            return Err(io::Error::other("a write cut short").into());
        }
        file.write_all(&bytes)?;
        file.set_len(bytes.len() as u64)?;
        file.sync_data()?;
        Ok(())
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // A partial file still recorded is left for cleanup.
        let kept = !lock(&self.record).partials.is_empty() || self.keep.load(Ordering::SeqCst);
        if !kept {
            let _ = safecopy_folder::delete_by_handle(&lock(&self.file));
        }
    }
}

/// A new journal file in `dir`, under a name no other has: the process ID
/// and a count, stepping past a name left by an earlier process.
pub(crate) fn create_named(dir: &Path, next: &AtomicU64) -> io::Result<File> {
    let mut tries = 0;
    loop {
        let n = next.fetch_add(1, Ordering::Relaxed);
        let name = format!("{}-{n}.{EXTENSION}", std::process::id());
        match create(&dir.join(name)) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && tries + 1 < NAME_TRIES => {
                tries += 1;
            }
            other => return other,
        }
    }
}

/// Switches tests use to act out a failing disk.
#[cfg(test)]
// Android's emulated thread locals make Clippy ask for the `const` already there.
#[allow(clippy::missing_const_for_thread_local)]
pub(crate) mod seams {
    use std::cell::Cell;

    thread_local! {
        /// How many of this thread's next record writes fail halfway.
        pub static FAIL_WRITES: Cell<u32> = const { Cell::new(0) };
        /// No recorded volume opens on this thread: a disk unplugged.
        pub static NO_VOLUME: Cell<bool> = const { Cell::new(false) };
    }
}

/// A new journal, open for writing with others only reading it.
fn create(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .access_mode((FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE).0)
        .share_mode(FILE_SHARE_READ.0)
        .open(path)
}

/// A journal no running transfer holds, opened for this process alone,
/// and its bytes: at most one past the record limit, so a longer one is
/// told apart. A link is opened as itself and refused, never followed: its
/// target is not a journal, and would be deleted with it.
pub(crate) fn take(path: &Path, settings: &Settings) -> io::Result<(File, Vec<u8>)> {
    let file = std::fs::OpenOptions::new()
        .access_mode((FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE).0)
        .share_mode(0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut bytes = Vec::new();
    let most = settings.record_bytes.saturating_add(1) as u64;
    (&file).take(most).read_to_end(&mut bytes)?;
    Ok((file, bytes))
}

fn open_volume(volume: &str) -> io::Result<File> {
    #[cfg(test)]
    if seams::NO_VOLUME.get() {
        return Err(io::ErrorKind::NotFound.into());
    }
    let share = FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0;
    std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode(share)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(volume)
}

fn open_folder(volume: &File, id: u128) -> io::Result<Directory> {
    let descriptor = FILE_ID_DESCRIPTOR {
        dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
        Type: ExtendedFileIdType,
        Anonymous: FILE_ID_DESCRIPTOR_0 {
            ExtendedFileId: FILE_ID_128 {
                Identifier: id.to_le_bytes(),
            },
        },
    };
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
    // SAFETY: `descriptor` is a whole, sized descriptor; the handle returned
    // is owned by the `File` made from it at once.
    let handle = unsafe {
        OpenFileById(
            HANDLE(volume.as_raw_handle()),
            &descriptor,
            FILE_GENERIC_READ.0,
            share,
            None,
            FILE_FLAG_BACKUP_SEMANTICS,
        )
    }
    .map_err(win32)?;
    // SAFETY: the handle was just opened and nothing else owns it.
    let file = unsafe { File::from_raw_handle(handle.0) };
    Directory::from_file(file, PathBuf::new())
}

/// The system's error as its Win32 code, which `io::ErrorKind` reads: the
/// `windows` crate gives it as an HRESULT, which it does not.
fn win32(error: windows::core::Error) -> io::Error {
    let code = error.code().0 as u32;
    if code & 0xFFFF_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((code & 0xFFFF) as i32)
    } else {
        io::Error::other(error)
    }
}

/// The reasons a recorded partial is left alone.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Left {
    /// A recorded name that cleanup never removes.
    Name,
    /// A volume path that is not a volume's, or another disk under it.
    Volume,
    /// Another file, folder or link under the recorded name, or another
    /// file under the folder's ID.
    Replaced,
    /// The folder or the file is not there.
    Gone,
    /// The recorded volume could not be opened: a disk not plugged in.
    Unreachable,
    /// Any other error of the system's: worth another try.
    Io,
}

impl Left {
    /// Whether another cleanup could still remove the partial: its journal
    /// is kept for it.
    pub(crate) fn may_change(&self) -> bool {
        matches!(self, Self::Unreachable | Self::Io)
    }
}

/// An error opening the recorded folder or file: what it says of it.
fn left(error: &io::Error) -> Left {
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

/// Removes one recorded partial file, if it is still that file.
pub(crate) fn remove(record: &Record, partial: &Partial, settings: &Settings) -> Result<(), Left> {
    if !removable(&partial.name, settings) {
        return Err(Left::Name);
    }
    // Only a volume by its GUID: never a share, a pipe or a device.
    if !volume_guid_path(&record.volume) {
        return Err(Left::Volume);
    }
    let volume = open_volume(&record.volume).map_err(|_| Left::Unreachable)?;
    // Another disk under the same path is not the one recorded.
    if identity(&volume).map_err(|_| Left::Unreachable)?.volume != record.serial {
        return Err(Left::Volume);
    }
    let folder = open_folder(&volume, partial.folder).map_err(|e| left(&e))?;
    let file = folder
        .open_for_delete(OsStr::new(&partial.name))
        .map_err(|e| left(&e))?;
    // Opened by its recorded name, so the name still has the ending.
    let id = identity(&file).map_err(|_| Left::Io)?;
    if id.volume != record.serial || id.file != partial.file {
        return Err(Left::Replaced);
    }
    safecopy_folder::delete_by_handle(&file).map_err(|_| Left::Io)?;
    Ok(())
}

/// Whether `path` is exactly `\\?\Volume{GUID}\`.
pub(crate) fn volume_guid_path(path: &str) -> bool {
    let Some(guid) = path
        .strip_prefix(r"\\?\Volume{")
        .and_then(|rest| rest.strip_suffix(r"}\"))
    else {
        return false;
    };
    guid.len() == 36
        && guid.char_indices().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}
