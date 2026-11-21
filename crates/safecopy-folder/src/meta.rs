//! A file's modified time and execute bit: read at the source, set on the
//! partial file before it is published.

use std::fs::{File, FileTimes};
use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::Directory;

/// A point in time: seconds and nanoseconds from the Unix epoch, UTC.
/// Before the epoch, `secs` is negative and `nanos` still counts forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Time {
    /// Whole seconds from 1970-01-01 00:00:00 UTC.
    pub secs: i64,
    /// Nanoseconds after `secs`, below 1,000,000,000.
    pub nanos: u32,
}

impl Time {
    /// `time` as seconds and nanoseconds; `None` past what an `i64` holds.
    pub fn from_system(time: SystemTime) -> Option<Self> {
        match time.duration_since(UNIX_EPOCH) {
            Ok(after) => Some(Self {
                secs: i64::try_from(after.as_secs()).ok()?,
                nanos: after.subsec_nanos(),
            }),
            Err(before) => {
                let before = before.duration();
                let secs = i64::try_from(before.as_secs()).ok()?;
                Some(if before.subsec_nanos() == 0 {
                    Self {
                        secs: -secs,
                        nanos: 0,
                    }
                } else {
                    Self {
                        secs: (-secs).checked_sub(1)?,
                        nanos: 1_000_000_000 - before.subsec_nanos(),
                    }
                })
            }
        }
    }

    /// This time as the system holds times; `None` when it cannot (on
    /// Windows, anything before 1601) or `nanos` is out of range.
    pub fn to_system(self) -> Option<SystemTime> {
        if self.nanos >= 1_000_000_000 {
            return None;
        }
        let nanos = Duration::from_nanos(self.nanos.into());
        if self.secs >= 0 {
            UNIX_EPOCH
                .checked_add(Duration::from_secs(self.secs.unsigned_abs()))?
                .checked_add(nanos)
        } else {
            UNIX_EPOCH
                .checked_sub(Duration::from_secs(self.secs.unsigned_abs()))?
                .checked_add(nanos)
        }
    }
}

/// What travels with a file: its modified time and whether its owner may
/// run it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Meta {
    /// When the file was last modified; `None` when that could not be read.
    pub modified: Option<Time>,
    /// The owner's execute bit, on a regular file (Linux and macOS). Always
    /// false on Windows, which has no such bit.
    pub executable: bool,
}

impl Meta {
    /// What `file` says of itself, read through its handle.
    pub fn read(file: &File) -> io::Result<Self> {
        let meta = file.metadata()?;
        let modified = meta.modified().ok().and_then(Time::from_system);
        #[cfg(unix)]
        let executable = {
            use std::os::unix::fs::PermissionsExt;
            meta.is_file() && meta.permissions().mode() & 0o100 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        Ok(Self {
            modified,
            executable,
        })
    }
}

/// What [`Directory::apply`] may set. The caller turns the execute bit on;
/// this crate never sets it by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetaRules {
    /// Set the modified time.
    pub times: bool,
    /// Set the owner's execute bit when [`Meta::executable`] says so (Linux
    /// and macOS only; ignored on Windows).
    pub executable: bool,
}

impl Default for MetaRules {
    fn default() -> Self {
        Self {
            times: true,
            executable: false,
        }
    }
}

/// What became of the modified time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TimeApplied {
    /// The rules did not ask for it.
    NotAsked,
    /// [`Meta::modified`] was `None`: the arrival time stays.
    NoTime,
    /// Set.
    Set,
    /// The file system cannot hold that time (on Windows, before 1601; on
    /// Linux, past what the disk stores): the arrival time stays.
    CannotHold,
    /// The system refused to set it (a disk that keeps no times, a share
    /// that denies it): the arrival time stays, or whatever the disk kept.
    Failed(io::ErrorKind),
}

/// What became of the execute bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BitApplied {
    /// The rules did not ask for it, or the sender's file had no bit.
    NotAsked,
    /// The owner's execute bit is set.
    Set,
    /// Windows, or a file that is not a regular file: nothing to set.
    NotHere,
    /// The system refused (a disk without Unix permissions, such as exFAT):
    /// the file is not executable.
    Failed(io::ErrorKind),
}

/// What [`Directory::apply`] set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    /// The modified time.
    pub time: TimeApplied,
    /// The owner's execute bit.
    pub executable: BitApplied,
}

impl Default for Applied {
    fn default() -> Self {
        Self {
            time: TimeApplied::NotAsked,
            executable: BitApplied::NotAsked,
        }
    }
}

/// The furthest two readings of one time may differ and still be that
/// time: FAT keeps modified times in two-second steps.
const COARSEST: Duration = Duration::from_secs(2);

fn near(a: SystemTime, b: SystemTime) -> bool {
    let apart = a
        .duration_since(b)
        .unwrap_or_else(|before| before.duration());
    apart <= COARSEST
}

/// Whether setting a time failed because no disk could hold it, rather
/// than for this file.
fn cannot_hold(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::InvalidInput {
        return true;
    }
    #[cfg(unix)]
    {
        matches!(
            e.raw_os_error(),
            Some(libc::EINVAL | libc::EOVERFLOW | libc::ERANGE)
        )
    }
    #[cfg(windows)]
    {
        // ERROR_INVALID_PARAMETER
        e.raw_os_error() == Some(87)
    }
}

fn set_modified(file: &File, time: SystemTime) -> io::Result<()> {
    #[cfg(test)]
    if let Some(error) = crate::seams::SET_TIMES_ERROR.get() {
        return Err(io::Error::from_raw_os_error(error));
    }
    file.set_times(FileTimes::new().set_modified(time))
}

/// Sets the owner's execute bit on `file`, by handle (`fchmod`).
#[cfg(unix)]
fn set_executable(file: &File) -> io::Result<BitApplied> {
    use std::os::unix::fs::PermissionsExt;
    let now = file.metadata()?;
    if !now.is_file() {
        return Ok(BitApplied::NotHere);
    }
    let mode = now.permissions().mode();
    if mode & 0o100 == 0 {
        #[cfg(test)]
        if let Some(error) = crate::seams::CHMOD_ERROR.get() {
            return Err(io::Error::from_raw_os_error(error));
        }
        file.set_permissions(std::fs::Permissions::from_mode(mode | 0o100))?;
    }
    Ok(BitApplied::Set)
}

impl Directory {
    /// Sets `meta` on `file`, a partial file of this folder, through its
    /// handle, as far as `rules` allow: the owner's execute bit (Linux and
    /// macOS only, never the group's or others'), then the modified time.
    /// Call it after the file's hash matched and after the downloaded-file
    /// mark, which changes the time on Windows; [`Self::publish_with`] does
    /// both in that order.
    ///
    /// Both are best effort: what could not be set is reported in
    /// [`Applied`], never an error, so a disk that keeps no times or no
    /// permissions still takes the file. A time the file system cannot hold
    /// leaves the arrival time; a time the disk changed as it stored it
    /// (more than two seconds off) is undone the same way. Only a handle
    /// whose metadata cannot be read is an error.
    pub fn apply(&self, file: &File, meta: &Meta, rules: &MetaRules) -> io::Result<Applied> {
        #[cfg(test)]
        if let Some(Some(error)) = crate::seams::APPLY_ERRORS.with_borrow_mut(|e| e.pop_front()) {
            return Err(io::Error::from_raw_os_error(error));
        }
        let mut applied = Applied::default();
        if rules.executable && meta.executable {
            #[cfg(unix)]
            {
                applied.executable =
                    set_executable(file).unwrap_or_else(|e| BitApplied::Failed(e.kind()));
            }
            #[cfg(not(unix))]
            {
                applied.executable = BitApplied::NotHere;
            }
        }
        if !rules.times {
            return Ok(applied);
        }
        let Some(time) = meta.modified else {
            applied.time = TimeApplied::NoTime;
            return Ok(applied);
        };
        let Some(wanted) = time.to_system() else {
            applied.time = TimeApplied::CannotHold;
            return Ok(applied);
        };
        let arrival = file.metadata()?.modified()?;
        applied.time = match set_modified(file, wanted) {
            Err(e) if cannot_hold(&e) => TimeApplied::CannotHold,
            Err(e) => TimeApplied::Failed(e.kind()),
            Ok(()) => {
                let stored = file.metadata()?.modified()?;
                if near(stored, wanted) {
                    TimeApplied::Set
                } else {
                    // Clamped by the disk: a wrong time is worse than the arrival time.
                    match set_modified(file, arrival) {
                        Ok(()) => TimeApplied::CannotHold,
                        Err(e) => TimeApplied::Failed(e.kind()),
                    }
                }
            }
        };
        Ok(applied)
    }
}
