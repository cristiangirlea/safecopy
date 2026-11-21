//! Windows: volumes opened by their GUID path, folders by their file ID,
//! and the whole-record journals of before.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::PathBuf;

use safecopy_folder::{Directory, identity};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    ExtendedFileIdType, FILE_FLAG_BACKUP_SEMANTICS, FILE_GENERIC_READ, FILE_ID_128,
    FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OpenFileById,
};

use crate::location::{Left, left};
use crate::{Partial, Record, Settings, removable};

fn open_volume(volume: &str) -> io::Result<File> {
    #[cfg(test)]
    if crate::seams::NO_VOLUME.get() {
        return Err(io::ErrorKind::NotFound.into());
    }
    use std::os::windows::fs::OpenOptionsExt;
    let share = FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0;
    std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode(share)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(volume)
}

/// The volume at `volume`, a `\\?\Volume{GUID}\` path only, with the
/// serial number recorded: never a share, a pipe or a device, nor another
/// disk under the same path.
pub(crate) fn checked_volume(volume: &str, serial: u64) -> Result<File, Left> {
    if !volume_guid_path(volume) {
        return Err(Left::Volume);
    }
    let file = open_volume(volume).map_err(|_| Left::Unreachable)?;
    if identity(&file).map_err(|_| Left::Unreachable)?.volume != serial {
        return Err(Left::Volume);
    }
    Ok(file)
}

pub(crate) fn open_folder(volume: &File, id: u128) -> io::Result<Directory> {
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

/// Removes one partial file a journal of the whole-record format recorded,
/// if it is still that file.
pub(crate) fn remove(record: &Record, partial: &Partial, settings: &Settings) -> Result<(), Left> {
    if !removable(&partial.name, settings) {
        return Err(Left::Name);
    }
    let volume = checked_volume(&record.volume, record.serial)?;
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
