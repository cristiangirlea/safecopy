//! How much room a disk has left.

use safecopy_folder::Directory;

/// The bytes this user may still write on the disk `dir` is on; `None`
/// when the system does not say.
pub(crate) fn free(dir: &Directory) -> Option<u64> {
    #[cfg(test)]
    if let Some(free) = crate::tests::seams::FREE.get() {
        return Some(free);
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        // SAFETY: the descriptor is the folder's, open; `stat` is written before it is read.
        let done = unsafe { libc::fstatvfs(dir.handle().as_raw_fd(), stat.as_mut_ptr()) };
        if done != 0 {
            return None;
        }
        // SAFETY: fstatvfs filled `stat` when it returned 0.
        let stat = unsafe { stat.assume_init() };
        #[allow(clippy::unnecessary_cast)]
        Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        use windows::core::PCWSTR;
        let path = dir.final_path().ok()?;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let mut free = 0u64;
        // SAFETY: `wide` is a NUL-terminated path; `free` outlives the call.
        unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut free), None, None) }.ok()?;
        Some(free)
    }
}
