//! Windows: every name opened with `NtCreateFile` relative to the held
//! folder's handle, as the reparse point itself, then checked.

use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::io::{self, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_DISPOSITION_FLAG_DELETE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX, FILE_DISPOSITION_INFO_EX_FLAGS,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_RENAME_INFO, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileAttributeTagInfo,
    FileDispositionInfo, FileDispositionInfoEx, FileIdInfo, FileStandardInfo,
    GETFINALPATHNAMEBYHANDLE_FLAGS, GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
    GetVolumeInformationByHandleW, SYNCHRONIZE, SetFileInformationByHandle, VOLUME_NAME_DOS,
    VOLUME_NAME_GUID,
};
use windows::Win32::System::SystemServices::FILE_NAMED_STREAMS;

use crate::{FileId, Marked, Refusal, Unfit, refused};

const FILE_OPEN: u32 = 1;
const FILE_CREATE: u32 = 2;
const FILE_OVERWRITE_IF: u32 = 5;
const FILE_DIRECTORY_FILE: u32 = 1;
const FILE_NON_DIRECTORY_FILE: u32 = 0x40;
const FILE_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
const OBJ_CASE_INSENSITIVE: u32 = 0x40;
const FILE_NAMES_INFORMATION: u32 = 12;
const FILE_RENAME_INFORMATION: u32 = 10;
const STATUS_NO_MORE_FILES: i32 = 0x8000_0006_u32 as i32;
const STATUS_NO_SUCH_FILE: i32 = 0xc000_000f_u32 as i32;
const SHARE_READ_WRITE: u32 = FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0;
const SHARE_ALL: u32 = SHARE_READ_WRITE | FILE_SHARE_DELETE.0;

/// What Windows reads from a downloaded file's `Zone.Identifier`: it came
/// from the Internet zone.
pub(crate) const INTERNET_ZONE: &str = "[ZoneTransfer]\r\nZoneId=3\r\n";

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum: u16,
    buffer: *const u16,
}

#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root: *mut c_void,
    name: *const UnicodeString,
    attributes: u32,
    security: *mut c_void,
    quality: *mut c_void,
}

#[repr(C)]
struct IoStatus {
    status: usize,
    information: usize,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateFile(
        handle: *mut *mut c_void,
        access: u32,
        attributes: *const ObjectAttributes,
        status: *mut IoStatus,
        size: *const i64,
        file_attributes: u32,
        share: u32,
        disposition: u32,
        options: u32,
        ea: *const c_void,
        ea_length: u32,
    ) -> i32;
    fn NtQueryDirectoryFile(
        handle: *mut c_void,
        event: *mut c_void,
        apc: *const c_void,
        context: *const c_void,
        status: *mut IoStatus,
        buffer: *mut c_void,
        length: u32,
        class: u32,
        single: u8,
        name: *const UnicodeString,
        restart: u8,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
    fn NtSetInformationFile(
        handle: *mut c_void,
        status: *mut IoStatus,
        information: *const c_void,
        length: u32,
        class: u32,
    ) -> i32;
}

fn error(status: i32) -> io::Error {
    // SAFETY: a pure lookup of a status code.
    io::Error::from_raw_os_error(unsafe { RtlNtStatusToDosError(status) } as i32)
}

/// Reparse tags of filter drivers (deduplication, compression, cloud
/// files), whose files are opened through their driver; links are not.
fn filter_tag(tag: u32) -> bool {
    tag & 0x2000_0000 == 0
        && (matches!(tag, 0x8000_0013 | 0x8000_0017) || tag & !0x0000_f000 == 0x9000_001a)
}

/// Whether `file` is a reparse point of a filter (true), none (false), or
/// a link (refused).
fn checked(file: &File) -> io::Result<bool> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: `info` is the buffer the class needs, of its size.
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileAttributeTagInfo,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of_val(&info) as u32,
        )
    }?;
    if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0 {
        return Ok(false);
    }
    if !filter_tag(info.ReparseTag) {
        return Err(refused(Refusal::Link));
    }
    Ok(true)
}

pub fn identity(file: &File) -> io::Result<FileId> {
    let (volume, id) = file_id(file)?;
    Ok(FileId {
        volume,
        file: u128::from_le_bytes(id),
    })
}

fn file_id(file: &File) -> io::Result<(u64, [u8; 16])> {
    let mut info = FILE_ID_INFO::default();
    // SAFETY: `info` is the buffer the class needs, of its size.
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            size_of_val(&info) as u32,
        )
    }?;
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

fn final_path(file: &File) -> io::Result<PathBuf> {
    final_path_as(file, VOLUME_NAME_DOS)
}

fn final_path_as(file: &File, form: GETFINALPATHNAMEBYHANDLE_FLAGS) -> io::Result<PathBuf> {
    let mut buffer = vec![0; 512];
    loop {
        // SAFETY: the call writes at most `buffer.len()` units.
        let length =
            unsafe { GetFinalPathNameByHandleW(HANDLE(file.as_raw_handle()), &mut buffer, form) }
                as usize;
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        if length < buffer.len() {
            return Ok(PathBuf::from(OsString::from_wide(&buffer[..length])));
        }
        buffer.resize(length + 1, 0);
    }
}

pub fn root(path: &Path, follow: bool) -> io::Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(SHARE_ALL)
        .custom_flags(
            FILE_FLAG_BACKUP_SEMANTICS.0
                | if follow {
                    0
                } else {
                    FILE_FLAG_OPEN_REPARSE_POINT.0
                },
        )
        .open(path)?;
    if follow || !checked(&file)? {
        return Ok(file);
    }
    // A filter's file opens through its driver, which must give the same object.
    let reopened = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)?;
    if file_id(&file)? != file_id(&reopened)? || final_path(&file)? != final_path(&reopened)? {
        return Err(refused(Refusal::Link));
    }
    checked(&reopened)?;
    Ok(reopened)
}

/// `name` in `parent` opened as its reparse point, refused when that is a
/// link, and reopened through its driver when it is a filter's.
fn relative(
    parent: &File,
    name: &OsStr,
    access: u32,
    disposition: u32,
    options: u32,
    share: u32,
) -> io::Result<File> {
    let file = relative_open(
        parent,
        name,
        access,
        disposition,
        options | FILE_OPEN_REPARSE_POINT,
        share,
    )?;
    if !checked(&file)? {
        return Ok(file);
    }
    // Filter files need their driver, but reopening must keep the same object and parent.
    let original = file_id(&file)?;
    drop(file);
    let reopened = relative_open(parent, name, access, FILE_OPEN, options, share)?;
    if original != file_id(&reopened)?
        || final_path(&reopened)?.parent() != Some(final_path(parent)?.as_path())
    {
        return Err(refused(Refusal::Link));
    }
    checked(&reopened)?;
    Ok(reopened)
}

fn relative_open(
    parent: &File,
    name: &OsStr,
    access: u32,
    disposition: u32,
    options: u32,
    share: u32,
) -> io::Result<File> {
    let wide: Vec<u16> = name.encode_wide().collect();
    let length = u16::try_from(wide.len() * 2).map_err(|_| refused(Refusal::TooLong))?;
    if wide.contains(&0) {
        return Err(refused(Refusal::Nul));
    }
    let name = UnicodeString {
        length,
        maximum: length,
        buffer: wide.as_ptr(),
    };
    let attributes = ObjectAttributes {
        length: size_of::<ObjectAttributes>() as u32,
        root: parent.as_raw_handle(),
        name: &name,
        attributes: OBJ_CASE_INSENSITIVE,
        security: std::ptr::null_mut(),
        quality: std::ptr::null_mut(),
    };
    let mut status = IoStatus {
        status: 0,
        information: 0,
    };
    let mut handle = std::ptr::null_mut();
    // SAFETY: every pointer is to a live value of the layout the call expects.
    let result = unsafe {
        NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE.0,
            &attributes,
            &mut status,
            std::ptr::null(),
            0,
            share,
            disposition,
            options | FILE_SYNCHRONOUS_IO_NONALERT,
            std::ptr::null(),
            0,
        )
    };
    if result < 0 {
        return Err(error(result));
    }
    // SAFETY: `handle` is a new handle nothing else owns.
    Ok(unsafe { File::from_raw_handle(handle) })
}

pub fn open(parent: &File, name: &OsStr, directory: bool, create: bool) -> io::Result<File> {
    let access = if create {
        (FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE).0
    } else {
        FILE_GENERIC_READ.0
    };
    let options = if directory {
        FILE_DIRECTORY_FILE
    } else if create {
        FILE_NON_DIRECTORY_FILE
    } else {
        0
    };
    relative(
        parent,
        name,
        access,
        if create { FILE_CREATE } else { FILE_OPEN },
        options,
        // A file being written is shared without delete, so it cannot be swapped.
        if create { SHARE_READ_WRITE } else { SHARE_ALL },
    )
}

pub fn inspect(parent: &File, name: &OsStr) -> io::Result<File> {
    // The reparse point itself: a cloud file's provider is not asked for its contents.
    let file = relative_open(
        parent,
        name,
        FILE_READ_ATTRIBUTES.0,
        FILE_OPEN,
        FILE_OPEN_REPARSE_POINT,
        SHARE_ALL,
    )?;
    checked(&file)?;
    Ok(file)
}

pub fn create_dir(parent: &File, name: &OsStr) -> io::Result<File> {
    relative(
        parent,
        name,
        FILE_GENERIC_READ.0,
        FILE_CREATE,
        FILE_DIRECTORY_FILE,
        SHARE_ALL,
    )
}

pub fn names(parent: &File, limit: usize) -> io::Result<Vec<io::Result<OsString>>> {
    let mut names = Vec::new();
    let mut restart = 1;
    loop {
        let mut buffer = [0u32; 1024];
        let mut status = IoStatus {
            status: 0,
            information: 0,
        };
        // SAFETY: `buffer` is writable for its whole size, given as the length.
        let result = unsafe {
            NtQueryDirectoryFile(
                parent.as_raw_handle(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                &mut status,
                buffer.as_mut_ptr().cast(),
                size_of_val(&buffer) as u32,
                FILE_NAMES_INFORMATION,
                1,
                std::ptr::null(),
                restart,
            )
        };
        if result == STATUS_NO_MORE_FILES || result == STATUS_NO_SUCH_FILE {
            break;
        }
        if result < 0 {
            if restart == 1 {
                return Err(error(result));
            }
            names.push(Err(error(result)));
            break;
        }
        restart = 0;
        // FILE_NAMES_INFORMATION: next offset, index, name length in bytes, name.
        let bytes = buffer[2] as usize;
        if !bytes.is_multiple_of(2)
            || status.information < 12 + bytes
            || bytes > size_of_val(&buffer) - 12
        {
            return Err(refused(Refusal::BadListing));
        }
        // SAFETY: the checks above keep the name inside what the call wrote.
        let wide =
            unsafe { std::slice::from_raw_parts(buffer.as_ptr().add(3).cast::<u16>(), bytes / 2) };
        let name = OsString::from_wide(wide);
        if name == "." || name == ".." {
            continue;
        }
        names.push(Ok(name));
        if names.len() >= limit {
            break;
        }
    }
    Ok(names)
}

/// Renames the open `file` itself to `name` in `parent`: what `partial`
/// names now does not matter, and a taken name is refused.
pub fn finish(parent: &File, file: &File, _: &OsStr, name: &OsStr) -> io::Result<()> {
    let wide: Vec<u16> = name.encode_wide().collect();
    let bytes = (std::mem::offset_of!(FILE_RENAME_INFO, FileName) + wide.len() * 2)
        .max(size_of::<FILE_RENAME_INFO>());
    let mut buffer = vec![0usize; bytes.div_ceil(size_of::<usize>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: `buffer` is aligned for and large enough to hold the header and the name.
    unsafe {
        (*info).RootDirectory = HANDLE(parent.as_raw_handle());
        (*info).FileNameLength = (wide.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(
            wide.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast(),
            wide.len(),
        );
        let mut status = IoStatus {
            status: 0,
            information: 0,
        };
        // ReplaceIfExists stays false: a taken name fails.
        let result = NtSetInformationFile(
            file.as_raw_handle(),
            &mut status,
            info.cast(),
            bytes as u32,
            FILE_RENAME_INFORMATION,
        );
        if result < 0 {
            Err(error(result))
        } else {
            Ok(())
        }
    }
}

pub fn remove(parent: &File, name: &OsStr) -> io::Result<()> {
    let file = open_for_delete(parent, name)?;
    discard(parent, &file, name)
}

pub fn rename(_: &File, _: &File, _: &OsStr, _: &OsStr) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

pub fn discard(_: &File, file: &File, _: &OsStr) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: `info` is the buffer the class needs, of its size.
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            size_of_val(&info) as u32,
        )
    }
    .map_err(io::Error::from)
}

pub fn writable(path: &Path, folders: bool) -> io::Result<File> {
    let add = FILE_ADD_FILE.0 | if folders { FILE_ADD_SUBDIRECTORY.0 } else { 0 };
    std::fs::OpenOptions::new()
        .access_mode(FILE_GENERIC_READ.0 | add)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)
}

/// The handle's path as people read it: without the `\\?\` prefix.
pub fn final_path_shown(file: &File, _: &Path) -> io::Result<PathBuf> {
    let path = final_path(file)?;
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    let unc: Vec<u16> = r"\\?\UNC\".encode_utf16().collect();
    let local: Vec<u16> = r"\\?\".encode_utf16().collect();
    Ok(if let Some(rest) = wide.strip_prefix(&unc[..]) {
        let mut shown: Vec<u16> = r"\\".encode_utf16().collect();
        shown.extend_from_slice(rest);
        PathBuf::from(OsString::from_wide(&shown))
    } else if let Some(rest) = wide.strip_prefix(&local[..]) {
        PathBuf::from(OsString::from_wide(rest))
    } else {
        path
    })
}

pub fn delete_pending(file: &File) -> bool {
    let mut info = FILE_STANDARD_INFO::default();
    // SAFETY: `info` is the buffer the class needs, of its size.
    let read = unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileStandardInfo,
            (&mut info as *mut FILE_STANDARD_INFO).cast(),
            size_of_val(&info) as u32,
        )
    };
    read.is_ok() && info.DeletePending
}

pub fn unfit(file: &File) -> Option<Unfit> {
    if volume_path(file).is_err() {
        return Some(Unfit::Network);
    }
    let mut flags = 0u32;
    // SAFETY: only the flags are asked for, into a live u32.
    let read = unsafe {
        GetVolumeInformationByHandleW(
            HANDLE(file.as_raw_handle()),
            None,
            None,
            None,
            Some(&mut flags),
            None,
        )
    };
    // Unreadable flags say nothing; the mark is tried as before.
    let streams = read.is_err() || flags & FILE_NAMED_STREAMS != 0;
    (!streams).then_some(Unfit::NoMarks)
}

pub fn open_for_delete(parent: &File, name: &OsStr) -> io::Result<File> {
    relative(
        parent,
        name,
        (DELETE | FILE_READ_ATTRIBUTES).0,
        FILE_OPEN,
        FILE_NON_DIRECTORY_FILE,
        SHARE_ALL,
    )
}

pub fn volume_path(file: &File) -> io::Result<String> {
    let path = final_path_as(file, VOLUME_NAME_GUID)?;
    let path = path.to_string_lossy();
    let end = path
        .find("}\\")
        .filter(|_| path.starts_with(r"\\?\Volume{"))
        .ok_or_else(|| refused(Refusal::NoVolume))?;
    Ok(path[..end + 2].to_string())
}

pub fn delete_by_handle(file: &File) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_INFO_EX_FLAGS(
            FILE_DISPOSITION_FLAG_DELETE.0 | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS.0,
        ),
    };
    // SAFETY: `info` is the buffer the class needs, of its size.
    let done = unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileDispositionInfoEx,
            (&info as *const FILE_DISPOSITION_INFO_EX).cast(),
            size_of_val(&info) as u32,
        )
    };
    // Disks without the newer form (FAT) take the older one.
    match done {
        Ok(()) => Ok(()),
        Err(_) => discard(file, file, OsStr::new("")),
    }
}

pub fn mark(parent: &File, name: &OsStr) -> Marked {
    let mut zone = name.to_os_string();
    zone.push(":Zone.Identifier");
    // The staging file stays open without delete sharing until its mark and rename finish.
    let marked = relative(
        parent,
        &zone,
        (FILE_GENERIC_WRITE | FILE_READ_ATTRIBUTES).0,
        FILE_OVERWRITE_IF,
        FILE_NON_DIRECTORY_FILE,
        SHARE_ALL,
    )
    .and_then(|mut file| file.write_all(INTERNET_ZONE.as_bytes()));
    match marked {
        Ok(()) => Marked::Marked,
        Err(e) => Marked::Failed(e.kind()),
    }
}

/// Marks the file at `path` as [`mark`] does, by path.
pub fn mark_path(path: &Path) -> Marked {
    let mut zone = path.as_os_str().to_os_string();
    zone.push(":Zone.Identifier");
    match std::fs::write(&zone, INTERNET_ZONE) {
        Ok(()) => Marked::Marked,
        Err(e) => Marked::Failed(e.kind()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_filter_tags_can_be_reopened() {
        for tag in [0x8000_0013, 0x8000_0017] {
            assert!(filter_tag(tag));
        }
        for variant in 0..16 {
            assert!(filter_tag(0x9000_001a | (variant << 12)));
        }
        for tag in [
            0,
            0xa000_0003,
            0xa000_000c,
            0xb000_001a,
            0x8000_9999,
            0x9001_001a,
        ] {
            assert!(!filter_tag(tag), "{tag:08x}");
        }
    }

    #[test]
    fn a_directory_without_list_access_returns_an_error() {
        let path = crate::tests::temp("no-list-access");
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(&path)
            .unwrap();
        assert!(names(&file, 10).is_err());
        drop(file);
        std::fs::remove_dir_all(path).unwrap();
    }
}
