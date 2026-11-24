//! Record files: created new and held by their transfer, taken whole by a
//! cleanup only when no transfer holds them, never through a link. Used for
//! a journal's segments, and by `safecopy-resume` for its records.
//!
//! On Windows a held file is open without write or delete sharing; on Unix
//! it holds an exclusive lock (`flock`), which only other programs that
//! lock see. Either way another process still reads it ([`open_read`]),
//! and none takes it ([`take`]) or holds it too ([`hold`]).

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// How many taken names a new journal steps past before it gives up.
const NAME_TRIES: u32 = 64;

/// A new journal file, held so no other cleanup takes it while its
/// transfer runs: on Windows open without write or delete sharing, on Unix
/// under an exclusive lock.
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

/// A new journal file in `dir` named `<process>-<count><salt>` and `tail`,
/// under a name no other has, stepping past one an earlier process left.
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
        match create(&dir.join(format!("{name}{tail}"))) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && tries + 1 < NAME_TRIES => {
                tries += 1;
            }
            Err(e) => return Err(e),
            Ok(file) => return Ok((name, file)),
        }
    }
}

/// A journal file no running transfer holds, opened for this process
/// alone, and its first `most` bytes and one more, so a longer one is told
/// apart. A link is opened as itself and refused, never followed: its
/// target is not a journal, and would be deleted with it.
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
    // Held by a running transfer, in this process or another.
    #[cfg(unix)]
    file.try_lock().map_err(io::Error::from)?;
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
/// readers do not ask for. A link is opened as itself and refused; one
/// already held or taken fails ([`busy`] tells that error from others),
/// and one removed in the moment it was opened is
/// [`io::ErrorKind::NotFound`].
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
        delete(&self.file, &self.path)
    }
}
