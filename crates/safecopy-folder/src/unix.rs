//! Linux, Android and macOS: every name opened with `openat` relative to the
//! held folder, with `O_NOFOLLOW`.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::{FileId, Marked, Refusal, Unfit, refused};

fn c_name(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| refused(Refusal::Nul))
}

/// Whether `name` in `parent` is a symbolic link, without following it.
fn is_symlink_at(parent: &File, name: &CStr) -> bool {
    let mut entry = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `name` is a valid C string and `entry` is written before it is read.
    let read = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            entry.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    // SAFETY: fstatat filled `entry` when it returned 0.
    read == 0 && unsafe { entry.assume_init() }.st_mode & libc::S_IFMT == libc::S_IFLNK
}

pub fn root(path: &Path, follow: bool) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            libc::O_DIRECTORY | libc::O_CLOEXEC | if follow { 0 } else { libc::O_NOFOLLOW },
        )
        .open(path)
        .map_err(|error| {
            // A link refused by O_NOFOLLOW reads as ELOOP, or as ENOTDIR with O_DIRECTORY.
            let link = match error.raw_os_error() {
                Some(libc::ELOOP) => true,
                Some(libc::ENOTDIR) => {
                    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
                }
                _ => false,
            };
            if !follow && link {
                refused(Refusal::Link)
            } else {
                error
            }
        })
}

pub fn open(parent: &File, name: &OsStr, directory: bool, create: bool) -> io::Result<File> {
    let name = c_name(name)?;
    let flags = libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 }
        | if create {
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL
        } else {
            libc::O_RDONLY
        };
    // SAFETY: `name` is a valid C string; the descriptor is owned below.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags,
            0o666 as libc::c_uint,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        let link = match error.raw_os_error() {
            Some(libc::ELOOP) => true,
            Some(libc::ENOTDIR) => directory && is_symlink_at(parent, &name),
            _ => false,
        };
        return Err(if link { refused(Refusal::Link) } else { error });
    }
    // SAFETY: `fd` is a new descriptor nothing else owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub fn reopen(parent: &File, name: &OsStr) -> io::Result<File> {
    let c = c_name(name)?;
    // Non-blocking, so a pipe put under the name never holds the open.
    let flags = libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    // SAFETY: `c` is a valid C string; the descriptor is owned below.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), c.as_ptr(), flags) };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return Err(if error.raw_os_error() == Some(libc::ELOOP) {
            refused(Refusal::Link)
        } else {
            error
        });
    }
    // SAFETY: `fd` is a new descriptor nothing else owns.
    let file = unsafe { File::from_raw_fd(fd) };
    // Back to blocking I/O for the file's bytes.
    // SAFETY: plain fcntl calls on a descriptor this function owns.
    unsafe {
        let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
    }
    Ok(file)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn inspect(parent: &File, name: &OsStr) -> io::Result<File> {
    // A path handle: nothing is opened for reading, not a device nor a FUSE file.
    let name = c_name(name)?;
    // SAFETY: `name` is a valid C string; the descriptor is owned below.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a new descriptor nothing else owns.
    let file = unsafe { File::from_raw_fd(fd) };
    // O_PATH with O_NOFOLLOW opens the link itself rather than failing.
    if file.metadata()?.file_type().is_symlink() {
        return Err(refused(Refusal::Link));
    }
    Ok(file)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn inspect(parent: &File, name: &OsStr) -> io::Result<File> {
    open(parent, name, false, false)
}

pub fn identity(file: &File) -> io::Result<FileId> {
    let meta = file.metadata()?;
    Ok(FileId {
        volume: meta.dev(),
        file: meta.ino().into(),
    })
}

pub fn create_dir(parent: &File, name: &OsStr) -> io::Result<File> {
    let c = c_name(name)?;
    // SAFETY: `c` is a valid C string.
    if unsafe { libc::mkdirat(parent.as_raw_fd(), c.as_ptr(), 0o777) } != 0 {
        return Err(io::Error::last_os_error());
    }
    open(parent, name, true, false)
}

/// Clears `errno`, so a null from `readdir` can be told apart from an error.
fn clear_errno() {
    // SAFETY: each returns this thread's errno.
    unsafe {
        #[cfg(target_os = "android")]
        let errno = libc::__errno();
        #[cfg(target_vendor = "apple")]
        let errno = libc::__error();
        #[cfg(not(any(target_os = "android", target_vendor = "apple")))]
        let errno = libc::__errno_location();
        *errno = 0;
    }
}

pub fn names(parent: &File, limit: usize) -> io::Result<Vec<io::Result<OsString>>> {
    // A fresh description keeps enumeration independent of other uses of the handle.
    let fd = open(parent, OsStr::new("."), true, false)?.into_raw_fd();
    // SAFETY: `fd` is an open directory; on success the stream owns it.
    let dir = unsafe { libc::fdopendir(fd) };
    if dir.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: the stream did not take `fd`.
        unsafe { libc::close(fd) };
        return Err(error);
    }
    let mut names = Vec::new();
    loop {
        clear_errno();
        // SAFETY: `dir` is an open stream.
        let entry = unsafe { libc::readdir(dir) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(0) {
                names.push(Err(error));
            }
            break;
        }
        // SAFETY: `d_name` is a NUL-terminated name valid until the next readdir.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        names.push(Ok(OsString::from_vec(name.to_vec())));
        if names.len() >= limit {
            break;
        }
    }
    // SAFETY: `dir` is open and closed once.
    unsafe { libc::closedir(dir) };
    Ok(names)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn finish(parent: &File, file: &File, partial: &OsStr, name: &OsStr) -> io::Result<()> {
    // Linking the open file prevents a replaced staging name from supplying the content.
    let source =
        CString::new(format!("/proc/self/fd/{}", file.as_raw_fd())).expect("a number has no NUL");
    let name = c_name(name)?;
    // SAFETY: both are valid C strings.
    if unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            source.as_ptr(),
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::AT_SYMLINK_FOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // The content is published: the staging name goes only while it is still ours,
    // and a failure to remove it is only clutter, not a failed finish.
    match discard(parent, file, partial) {
        Err(e) if crate::refusal(&e) == Some(Refusal::Replaced) => Err(e),
        _ => Ok(()),
    }
}

/// No procfs to link an open file by: [`rename`] publishes instead.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn finish(_: &File, _: &File, _: &OsStr, _: &OsStr) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

pub fn remove(parent: &File, name: &OsStr) -> io::Result<()> {
    let name = c_name(name)?;
    #[cfg(test)]
    if let Some(error) = crate::seams::REMOVE_ERRORS.with_borrow_mut(|e| e.pop_front()) {
        return Err(io::Error::from_raw_os_error(error));
    }
    // SAFETY: `name` is a valid C string.
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Whether `name` in `parent` is the open `file`, without following a link.
// `st_dev` is a u64 on Linux but an i32 on macOS.
#[allow(clippy::unnecessary_cast)]
fn matches_file(parent: &File, file: &File, name: &CStr) -> io::Result<bool> {
    let mut entry = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `name` is a valid C string and `entry` is written before it is read.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            entry.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fstatat filled `entry`.
    let entry = unsafe { entry.assume_init() };
    let meta = file.metadata()?;
    Ok(entry.st_dev as u64 == meta.dev() && entry.st_ino as u64 == meta.ino())
}

/// Renames `from` to `to` in `parent`, failing with `EEXIST` when `to` is taken.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_no_replace(parent: &File, from: &CStr, to: &CStr) -> io::Result<()> {
    // The syscall also works before Android's C library supplied renameat2.
    // SAFETY: both are valid C strings.
    if unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// macOS' own no-replace rename: `renameatx_np` with `RENAME_EXCL`.
#[cfg(target_vendor = "apple")]
fn rename_no_replace(parent: &File, from: &CStr, to: &CStr) -> io::Result<()> {
    // SAFETY: both are valid C strings.
    if unsafe {
        libc::renameatx_np(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn rename(parent: &File, file: &File, partial: &OsStr, name: &OsStr) -> io::Result<()> {
    let partial = c_name(partial)?;
    let target = c_name(name)?;
    if !matches_file(parent, file, &partial)? {
        return Err(refused(Refusal::Replaced));
    }
    #[cfg(test)]
    crate::seams::at("rename");
    rename_no_replace(parent, &partial, &target)?;
    #[cfg(test)]
    crate::seams::at("renamed");
    // A swap between the check and the rename must not supply the finished
    // content; the file swapped in is not ours to delete, so it goes back.
    if !matches_file(parent, file, &target)? {
        let _ = rename_no_replace(parent, &target, &partial);
        return Err(refused(Refusal::Replaced));
    }
    Ok(())
}

pub fn mark(_: &File, _: &OsStr) -> Marked {
    Marked::NotOnThisSystem
}

/// The folder `name` in `parent`, never through a link, when this user may
/// add to it (write and search permission).
pub fn open_existing(parent: &File, name: &OsStr) -> io::Result<File> {
    let file = open(parent, name, true, false)?;
    // SAFETY: "." is a valid C string.
    if unsafe { libc::faccessat(file.as_raw_fd(), c".".as_ptr(), libc::W_OK | libc::X_OK, 0) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

/// Whether a name in `dir` opens only a file of exactly its bytes: a file
/// system that never folds case or the form of characters, and a folder
/// not casefolded. Unsure is false, and the folder is read instead.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn names_are_bytes(dir: &File) -> bool {
    // ext2/3/4, Btrfs, tmpfs, F2FS, whose only folding is the flag below.
    // Not XFS (ASCII case folding at mkfs), ZFS (casesensitivity per
    // dataset), overlayfs (its layers' rules), nor FAT, exFAT, NTFS, SMB or
    // FUSE (Android's shared storage): their folders are read.
    const EXACT: [u64; 4] = [0xef53, 0x9123_683e, 0x0102_1994, 0xf2f5_2010];
    // ext4, F2FS and tmpfs fold case in a folder with this flag.
    const FS_CASEFOLD_FL: libc::c_int = 0x4000_0000;
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: `stat` is written before it is read.
    if unsafe { libc::fstatfs(dir.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return false;
    }
    // SAFETY: fstatfs filled it. Its type's width differs between systems.
    #[allow(clippy::unnecessary_cast)]
    let kind = unsafe { stat.assume_init() }.f_type as u64 & 0xffff_ffff;
    if !EXACT.contains(&kind) {
        return false;
    }
    let mut flags: libc::c_int = 0;
    // SAFETY: the call writes one int of flags into `flags`.
    if unsafe { libc::ioctl(dir.as_raw_fd(), libc::FS_IOC_GETFLAGS, &mut flags) } != 0 {
        // A file system without flags has no casefolding either.
        return io::Error::last_os_error().raw_os_error() == Some(libc::ENOTTY);
    }
    flags & FS_CASEFOLD_FL == 0
}

/// macOS folds case and the form of characters on most disks; the folder
/// is always read.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn names_are_bytes(_: &File) -> bool {
    false
}

/// Whether `file`, opened as `name` in `parent`, has exactly that name. A
/// folder that ignores case or the form of characters (macOS by default,
/// casefolded folders and FAT or FUSE disks on Linux and Android) opens
/// another spelling too, so unless the folder is known to match bytes
/// only, it is read for an entry of exactly these bytes, which must then
/// be `file`.
pub fn exact_name(parent: &File, file: &File, name: &OsStr) -> io::Result<bool> {
    if names_are_bytes(parent) {
        // `openat` found exactly these bytes.
        return Ok(true);
    }
    let fd = open(parent, OsStr::new("."), true, false)?.into_raw_fd();
    // SAFETY: `fd` is an open directory; on success the stream owns it.
    let dir = unsafe { libc::fdopendir(fd) };
    if dir.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: the stream did not take `fd`.
        unsafe { libc::close(fd) };
        return Err(error);
    }
    let wanted = name.as_bytes();
    let found = loop {
        clear_errno();
        // SAFETY: `dir` is an open stream.
        let entry = unsafe { libc::readdir(dir) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            break if error.raw_os_error() == Some(0) {
                Ok(false)
            } else {
                Err(error)
            };
        }
        // SAFETY: `d_name` is a NUL-terminated name valid until the next readdir.
        if unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes() == wanted {
            break Ok(true);
        }
    };
    // SAFETY: `dir` is open and closed once.
    unsafe { libc::closedir(dir) };
    Ok(found? && matches_file(parent, file, &c_name(name)?)?)
}

pub fn writable(path: &Path, _folders: bool) -> io::Result<File> {
    let file = root(path, true)?;
    // SAFETY: "." is a valid C string.
    if unsafe { libc::faccessat(file.as_raw_fd(), c".".as_ptr(), libc::W_OK | libc::X_OK, 0) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn final_path_shown(file: &File, _: &Path) -> io::Result<PathBuf> {
    let path = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
    if path.as_os_str().as_bytes().ends_with(b" (deleted)") {
        return Err(io::ErrorKind::NotFound.into());
    }
    Ok(path)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn final_path_shown(_: &File, path: &Path) -> io::Result<PathBuf> {
    Ok(path.to_path_buf())
}

pub fn delete_pending(_: &File) -> bool {
    false
}

pub fn unfit(_: &File) -> Option<Unfit> {
    None
}

pub fn discard(parent: &File, file: &File, name: &OsStr) -> io::Result<()> {
    // No delete by handle here: a file that took the name meanwhile is left.
    if !matches_file(parent, file, &c_name(name)?)? {
        return Err(refused(Refusal::Replaced));
    }
    remove(parent, name)
}

/// The published file `file` removed from `name`, only while the name is
/// still that file: `file` is open, so no other file has its inode.
pub fn take_back(
    parent: &File,
    folder: Option<&File>,
    file: &File,
    name: &OsStr,
) -> io::Result<()> {
    // Both folders are open, so neither number can be another folder's:
    // a second name of the file in some other folder is not the one published.
    let published_here = match folder {
        Some(folder) => {
            let (here, there) = (parent.metadata()?, folder.metadata()?);
            here.dev() == there.dev() && here.ino() == there.ino()
        }
        None => false,
    };
    if !published_here {
        return Err(refused(Refusal::OtherFolder));
    }
    discard(parent, file, name)
}

/// The folder a held file was published in, held too: [`take_back`]
/// knows it again by it.
pub fn hold_folder(parent: &File) -> io::Result<Option<File>> {
    parent.try_clone().map(Some)
}
