//! Record files: created new and held by their transfer, taken whole by a
//! cleanup only when no transfer holds them, never through a link. Used for
//! a journal's segments, and by `safecopy-resume` for its records.
//!
//! On Windows a held file is open without write or delete sharing; on Unix
//! it holds an exclusive lock (`flock`), which only other programs that
//! lock see. Either way another process still reads it ([`open_read`]),
//! and none takes it ([`take`]) or holds it too ([`hold`]).
//!
//! On Windows the open that makes or finds a file is what holds it, in
//! one step. On Unix the lock comes after the open, and is on the file,
//! not on its name: [`take`] and [`hold`] therefore check, under the lock,
//! that the file is still the one its name reaches: a journal removes a
//! segment's name before letting go of the segment. [`create`] makes the
//! name and then locks it, so a file it made may be held by another for
//! that moment; a journal's segments are locked before they are named.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// How many taken names a new journal, or the next segment of one, steps
/// past before it gives up.
pub(crate) const NAME_TRIES: u32 = 64;

/// A new journal file, held so no other cleanup takes it while its
/// transfer runs: on Windows open without write or delete sharing, on Unix
/// under an exclusive lock. On Unix the file is made and then locked:
/// another process may hold it in between, and then this fails with the
/// error [`busy`] tells, the empty file left under its name.
pub fn create(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            DELETE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
        };
        options
            .access_mode((FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE).0)
            .share_mode(FILE_SHARE_READ.0);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    file.try_lock().map_err(io::Error::from)?;
    Ok(file)
}

/// A new segment of a journal at `path`, held from before its name is
/// there: whoever finds the name finds the file held.
///
/// Windows makes and holds it in one open. On Unix it is made under a
/// temporary name (ending `jnew`, which no cleanup takes for a segment),
/// locked, given its own name as a second one, never in place of
/// a file already there, and the temporary name removed. A cleanup removes
/// a temporary name it can lock, so one made here and lost to a cleanup
/// before its lock is given up for another.
///
/// A folder that gives a file no second name (a file system without hard
/// links, a system whose rules refuse them) leaves only [`create`]'s way:
/// the name, then the lock. A cleanup that had the empty file in between
/// removed it or left it: either way this fails with the error [`busy`]
/// tells, and the caller takes the next name.
pub(crate) fn create_held(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        create_then_name(path)
    }
    #[cfg(not(unix))]
    {
        create(path)
    }
}

/// A new file at `path`, for this process to read and write: never one
/// already there, never through a link.
#[cfg(unix)]
fn create_new(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Whether the open file `file` is the one `path` names now, no link
/// followed. A file with no name left is not: its holder removed it.
#[cfg(unix)]
fn named(file: &File, path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let held = file.metadata()?;
    if held.nlink() == 0 {
        return Ok(false);
    }
    match std::fs::symlink_metadata(path) {
        Ok(at) => Ok(at.dev() == held.dev() && at.ino() == held.ino()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Counts the temporary names this process makes.
#[cfg(unix)]
pub(crate) static NEW: AtomicU64 = AtomicU64::new(1);

#[cfg(unix)]
fn create_then_name(path: &Path) -> io::Result<File> {
    let dir = path.parent().unwrap_or(Path::new(""));
    let mut tries = 0;
    loop {
        let n = NEW.fetch_add(1, Ordering::Relaxed);
        let temp = dir.join(format!(
            "{}-{n}.{}",
            std::process::id(),
            crate::NEW_EXTENSION
        ));
        let file = match create_new(&temp) {
            // One an earlier process of this number left.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && tries + 1 < NAME_TRIES => {
                tries += 1;
                continue;
            }
            made => made?,
        };
        #[cfg(test)]
        crate::seams::at(crate::seams::Point::TempMade);
        // A cleanup locks a temporary name to see whether it was left, and
        // removes the one it locked: held here under its name, it is past
        // that. Otherwise it is the cleanup's, and another is made.
        let ours = match file.try_lock() {
            Ok(()) => named(&file, &temp)?,
            Err(std::fs::TryLockError::WouldBlock) => false,
            Err(std::fs::TryLockError::Error(e)) => {
                let _ = delete(&file, &temp);
                return Err(e);
            }
        };
        if !ours {
            if tries + 1 < NAME_TRIES {
                tries += 1;
                continue;
            }
            return Err(io::ErrorKind::WouldBlock.into());
        }
        // The file's own name, never in place of one already there.
        #[cfg(test)]
        let linked = if crate::seams::NO_LINKS.get() {
            Err(io::Error::from_raw_os_error(libc::EPERM))
        } else {
            std::fs::hard_link(&temp, path)
        };
        #[cfg(not(test))]
        let linked = std::fs::hard_link(&temp, path);
        // A temporary name that stays is a second name of a held file: no
        // cleanup takes it, and one removes it once the file is let go.
        let _ = delete(&file, &temp);
        return match linked {
            Ok(()) => Ok(file),
            Err(e) if no_second_name(&e) => create_in_place(path),
            Err(e) => Err(e),
        };
    }
}

/// Whether `error`, from giving a file of this process a second name
/// beside its first, says the folder gives none: a file system without
/// hard links, or a system whose rules refuse them to this process.
#[cfg(unix)]
fn no_second_name(error: &io::Error) -> bool {
    // Compared one by one: two of them are one number on some systems.
    [
        libc::EPERM,
        libc::EACCES,
        libc::ENOTSUP,
        libc::EOPNOTSUPP,
        libc::ENOSYS,
    ]
    .iter()
    .any(|code| error.raw_os_error() == Some(*code))
}

/// A new segment made under its own name and then locked, where it can
/// have no other first. A cleanup that locked the empty file in between
/// has it, or removed it as a journal that says nothing: [`busy`].
#[cfg(unix)]
fn create_in_place(path: &Path) -> io::Result<File> {
    let file = create_new(path)?;
    #[cfg(test)]
    crate::seams::at(crate::seams::Point::Made);
    file.try_lock().map_err(io::Error::from)?;
    if !named(&file, path)? {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    Ok(file)
}

/// A new journal's first segment in `dir`, named `<process>-<count><salt>`
/// and `tail`, held as [`create_held`] holds it, under a name no other
/// has: one an earlier process left, or a cleanup has, is stepped past.
/// Its name without `tail`, and the file.
pub(crate) fn create_named(
    dir: &Path,
    next: &AtomicU64,
    salt: &str,
    tail: &str,
) -> io::Result<(String, File)> {
    let mut tries = 0;
    loop {
        let n = next.fetch_add(1, Ordering::Relaxed);
        let name = format!("{}-{n}{salt}", std::process::id());
        match create_held(&dir.join(format!("{name}{tail}"))) {
            Err(e) if taken(&e) && tries + 1 < NAME_TRIES => tries += 1,
            Err(e) => return Err(e),
            Ok(file) => return Ok((name, file)),
        }
    }
}

/// Whether `error`, from [`create_held`], says the name is not this
/// journal's to have: a file is under it, or a cleanup has the one just
/// made there. The next name is tried.
pub(crate) fn taken(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::AlreadyExists || busy(error)
}

/// A journal file no running transfer holds, opened for this process
/// alone, and its first `most` bytes and one more, so a longer one is told
/// apart. A link is opened as itself and refused, never followed: its
/// target is not a journal, and would be deleted with it.
///
/// On Unix the lock comes after the open, and a holder removes a file's
/// name before it lets go of the file. So a file that is locked here and
/// is no longer the one `path` names was let go in between, removed or
/// replaced: it is not taken ([`io::ErrorKind::NotFound`]), and says
/// nothing of the journal it was part of. Windows has no such moment: the
/// open is refused while another holds the file.
pub fn take(path: &Path, most: usize) -> io::Result<(File, Vec<u8>)> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
        };
        options
            .access_mode((FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE).0)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
    }
    if !meta.is_file() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    #[cfg(test)]
    crate::seams::at(crate::seams::Point::TakeOpened);
    #[cfg(unix)]
    {
        // Held by a running transfer, in this process or another.
        file.try_lock().map_err(io::Error::from)?;
        // Let go since it was opened: its name went first.
        if !named(&file, path)? {
            return Err(io::ErrorKind::NotFound.into());
        }
    }
    let mut bytes = Vec::new();
    (&file)
        .take(most.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    Ok((file, bytes))
}

/// A record file that is already there, held as [`create`] holds a new
/// one: for this process to write, while others still read it
/// ([`open_read`]) and none takes it or holds it too. On Windows it is
/// open with read sharing only; on Unix under an exclusive lock, which
/// readers do not ask for. A link is opened as itself and refused, and so
/// is a file with more than one name (a hard link): what is written
/// through the handle would land in a file some other name shows, which
/// nothing says is a record ([`refused`] tells those errors from others).
/// One already held or taken fails ([`busy`] tells that error), and one
/// removed in the moment it was opened is [`io::ErrorKind::NotFound`].
pub fn hold(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{
            DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
            FILE_SHARE_READ,
        };
        options
            .access_mode((FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE).0)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
    }
    if !meta.is_file() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    #[cfg(test)]
    if let Some(hook) = crate::seams::HOLD_OPENED.take() {
        hook();
    }
    // Counted through the handle: the file opened, whatever its name shows
    // next. None left: removed in the moment it was opened.
    match links(&file, &meta)? {
        0 => return Err(io::ErrorKind::NotFound.into()),
        1 => {}
        _ => return Err(io::ErrorKind::InvalidInput.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        file.try_lock().map_err(io::Error::from)?;
        // Its last holder may have removed it since it was opened: a lock
        // on a file with no name holds nothing.
        let named = std::fs::symlink_metadata(path)?;
        if named.dev() != meta.dev() || named.ino() != meta.ino() {
            return Err(io::ErrorKind::NotFound.into());
        }
    }
    Ok(file)
}

/// How many names the open file `file` has.
fn links(file: &File, meta: &std::fs::Metadata) -> io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Read now, not when the file was opened.
        let _ = meta;
        Ok(file.metadata()?.nlink())
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let _ = meta;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: the handle is open for as long as `file` is borrowed, and
        // `info` is a whole structure of the size the call fills.
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
            .map_err(crate::win::win32)?;
        Ok(info.nNumberOfLinks.into())
    }
}

/// Whether `error`, from [`hold`], [`take`] or [`open_read`], says what is
/// under the name is not a file they open: a link, anything but a regular
/// file or, for [`hold`], a file with more than one name. The system's own
/// errors are not it, but for the one a link gives where links are not
/// followed.
pub fn refused(error: &io::Error) -> bool {
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return true;
    }
    error.kind() == io::ErrorKind::InvalidInput && error.raw_os_error().is_none()
}

/// Whether `error`, from [`hold`] or [`take`], says another holder has
/// the file: a sharing or lock violation on Windows, a lock that would
/// wait on Unix.
pub fn busy(error: &io::Error) -> bool {
    #[cfg(windows)]
    {
        // ERROR_SHARING_VIOLATION and ERROR_LOCK_VIOLATION.
        matches!(error.raw_os_error(), Some(32 | 33))
    }
    #[cfg(not(windows))]
    {
        error.kind() == io::ErrorKind::WouldBlock
    }
}

/// A record file opened for reading only, as itself: a link, a junction, a
/// pipe or anything but a regular file is refused, and nothing waits.
pub fn open_read(path: &Path) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let meta = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
    }
    if !meta.is_file() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(file)
}

/// Deletes the journal file `file`, open at `path`: on Windows by its
/// handle; on Unix by its name, only while that name is still this file.
pub fn delete(file: &File, path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        let _ = path;
        safecopy_folder::delete_by_handle(file)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let held = file.metadata()?;
        let named = std::fs::symlink_metadata(path)?;
        if held.dev() != named.dev() || held.ino() != named.ino() {
            return Err(io::ErrorKind::NotFound.into());
        }
        std::fs::remove_file(path)
    }
}

/// Makes a name just created in `dir` last through a crash, before
/// anything relies on it: on Unix the folder itself is synced, as a file's
/// sync does not reach its folder's entry. Windows has no such call for a
/// folder; there it does nothing, and NTFS's own log keeps its folders
/// whole.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// A file held with the path it was opened at.
#[derive(Debug)]
pub(crate) struct Held {
    pub(crate) file: File,
    pub(crate) path: PathBuf,
}

impl Held {
    pub(crate) fn delete(&self) -> io::Result<()> {
        #[cfg(test)]
        if crate::seams::FAIL_DELETES.get() > 0 {
            crate::seams::FAIL_DELETES.set(crate::seams::FAIL_DELETES.get() - 1);
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        delete(&self.file, &self.path)
    }

    /// Removes its name, then lets go of it. `Err` with the file still
    /// held when the name could not be removed; a name that is no longer
    /// this file's is gone already.
    pub(crate) fn retire(self) -> Result<(), (Self, io::Error)> {
        let removed = self.delete();
        #[cfg(test)]
        crate::seams::at(crate::seams::Point::Unlinked);
        match removed {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err((self, e)),
            _ => Ok(()),
        }
    }
}
