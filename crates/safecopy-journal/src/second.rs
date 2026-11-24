//! A second identity of a partial file, beside its file ID or inode: what
//! a new file given a removed file's inode does not share with it.

use std::fs::File;

use crate::location::Left;

/// What a partial file is known by beside its file ID or inode, read when
/// it is recorded and compared when it is found again. Unix may give a removed
/// file's inode to the next file made, so the inode alone can name another
/// file than the one recorded.
///
/// What the system and the file system give, with no privilege, and both
/// where both are given:
///
/// - **Linux:** the birth time from `statx`, on a kernel that has the call
///   (4.11) and a file system that keeps one and reports it: ext4 with
///   inodes of 256 bytes or more, btrfs, f2fs made with inode creation
///   times, and on newer kernels xfs (version 5 inodes) and tmpfs. An
///   overlay reports its upper file system's. And the inode's generation
///   (`FS_IOC_GETVERSION`), which ext2, ext3, ext4, xfs, btrfs and f2fs
///   answer: so an older kernel, or ext4 with 128-byte inodes, is still
///   told apart. None where a file system gives neither, as tmpfs on an
///   older kernel and many network and FUSE file systems.
/// - **Android:** the same, the birth time only through the C library's
///   `statx`, which Android 11 brought: the system call itself is refused
///   to an app before that. A generation only where the system's rules let
///   the app ask.
/// - **macOS and the BSDs:** the birth time of `stat` (APFS, HFS+, UFS2,
///   ZFS). One that reads as zero, or as -1, is none: a file system that
///   keeps no birth time. The generation is the superuser's there.
/// - **Windows:** none. NTFS and ReFS keep a file ID for the life of the
///   file, and never give it to another.
///
/// The change time is never used: the journal records a partial before its
/// first byte, and every write to the file moves its change time.
///
/// What the journal and its caller do to a partial leaves these alone:
/// writing, syncing, cutting and renaming change neither. One thing does:
/// macOS and the BSDs move a birth time back when a file's modified time
/// is set to an earlier one, as a publish that carries the sender's time
/// sets it. So there a file born earlier than its record says is still
/// the file recorded, and only one born later is another: a file made
/// since. (A clock set back between the two is not seen.)
///
/// A birth time is as fine as the clock the file system stamps files
/// with, a few milliseconds on many Linux kernels: a file removed and
/// another made with its inode within one step of that clock have one
/// birth time, and only a generation tells them apart. A generation is as good as the
/// file system makes it: btrfs numbers generations by transaction, so two
/// files of one transaction share one.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Second {
    /// None was read: the file is known as before, by its file ID or inode.
    #[default]
    None,
    /// When the file was made, from the Unix epoch.
    Born {
        /// The seconds.
        secs: i64,
        /// The nanoseconds of the second, below 1,000,000,000.
        nanos: u32,
    },
    /// The inode's generation: another number each time the inode is given
    /// to a new file.
    Generation(u32),
    /// Both, where both are given: each is compared.
    Both {
        /// The seconds of its birth time.
        secs: i64,
        /// The nanoseconds of the second, below 1,000,000,000.
        nanos: u32,
        /// The inode's generation.
        generation: u32,
    },
}

impl Second {
    /// The second identity of the open `file`: its birth time and its
    /// inode's generation, each where it is given. [`Second::None`] where
    /// neither is, or the reading fails: the file is then known as before.
    pub fn of(file: &File) -> Self {
        Self::from_parts(born(file), generation(file))
    }

    fn from_parts(born: Option<(i64, u32)>, generation: Option<u32>) -> Self {
        match (born, generation) {
            (None, None) => Self::None,
            (Some((secs, nanos)), None) => Self::Born { secs, nanos },
            (None, Some(generation)) => Self::Generation(generation),
            (Some((secs, nanos)), Some(generation)) => Self::Both {
                secs,
                nanos,
                generation,
            },
        }
    }

    /// The birth time and the generation it holds.
    fn parts(self) -> (Option<(i64, u32)>, Option<u32>) {
        match self {
            Self::None => (None, None),
            Self::Born { secs, nanos } => (Some((secs, nanos)), None),
            Self::Generation(generation) => (None, Some(generation)),
            Self::Both {
                secs,
                nanos,
                generation,
            } => (Some((secs, nanos)), Some(generation)),
        }
    }

    /// Whether this and `other`, two records of a file of one ID, can be
    /// of one file: no birth time of the one differs from the other's,
    /// and no generation. What only one of them holds says nothing.
    pub fn agrees(self, other: Self) -> bool {
        let ((born, generation), (other_born, other_generation)) = (self.parts(), other.parts());
        fn differ<T: PartialEq>(a: Option<T>, b: Option<T>) -> bool {
            a.zip(b).is_some_and(|(a, b)| a != b)
        }
        !differ(born, other_born) && !differ(generation, other_generation)
    }

    /// Whether it holds a generation, which a path-only handle does not
    /// answer for: it is read from the file itself.
    pub(crate) fn has_generation(self) -> bool {
        self.parts().1.is_some()
    }

    /// Checks that `file`, found under a recorded name with the recorded
    /// ID, is the file this was read from: each of the two that was
    /// recorded is compared. With none recorded there is nothing to
    /// compare. [`Left::Replaced`] when the file has another;
    /// [`Left::Io`] when one recorded cannot be read from it now: nothing
    /// shows it is the file, and nothing that it is not.
    pub(crate) fn check(self, file: &File) -> Result<(), Left> {
        // A file shown to be another is that, whatever else is unread.
        match (self.check_born(file), self.check_generation(file)) {
            (Err(Left::Replaced), _) | (_, Err(Left::Replaced)) => Err(Left::Replaced),
            (born, generation) => born.and(generation),
        }
    }

    /// [`Self::check`] of the birth time alone, which a path-only handle
    /// answers for too.
    pub(crate) fn check_born(self, file: &File) -> Result<(), Left> {
        let Some(recorded) = self.parts().0 else {
            return Ok(());
        };
        match born(file) {
            Some(now) if now == recorded => Ok(()),
            // Its modified time was set to an earlier one since, as a
            // publish sets it: a file made after the record is born later.
            Some(now) if moves_back() && now < recorded => Ok(()),
            Some(_) => Err(Left::Replaced),
            None => Err(Left::Io),
        }
    }

    /// [`Self::check`] of the generation alone.
    pub(crate) fn check_generation(self, file: &File) -> Result<(), Left> {
        let Some(recorded) = self.parts().1 else {
            return Ok(());
        };
        match generation(file) {
            Some(now) if now == recorded => Ok(()),
            Some(_) => Err(Left::Replaced),
            None => Err(Left::Io),
        }
    }
}

/// Whether the system moves a file's birth time back when its modified
/// time is set to an earlier one: macOS and the BSDs do, Linux and Android
/// never change one.
fn moves_back() -> bool {
    #[cfg(test)]
    if crate::seams::BIRTH_MOVES_BACK.get() {
        return true;
    }
    cfg!(not(any(
        windows,
        target_os = "linux",
        target_os = "android"
    )))
}

/// The birth time of `file`, where the system and its file system give one.
fn born(file: &File) -> Option<(i64, u32)> {
    // What a test makes every file of its thread report.
    #[cfg(test)]
    if let Some(acted) = crate::seams::SECOND.get() {
        return acted.parts().0;
    }
    let (secs, nanos) = birth(file)?;
    // A time that is no time is a file system's way of having none.
    (nanos < 1_000_000_000 && !(nanos == 0 && matches!(secs, 0 | -1))).then_some((secs, nanos))
}

/// The generation of `file`'s inode, where the file system answers.
fn generation(file: &File) -> Option<u32> {
    #[cfg(test)]
    if let Some(acted) = crate::seams::SECOND.get() {
        return acted.parts().1;
    }
    // A file system that keeps none answers 0.
    version(file).filter(|&n| n != 0)
}

/// The fields of `statx` that hold a file's birth time, when it says it
/// filled them.
#[cfg(any(
    all(target_os = "linux", any(target_env = "gnu", target_env = "musl")),
    target_os = "android"
))]
fn filled(stat: &libc::statx) -> Option<(i64, u32)> {
    (stat.stx_mask & libc::STATX_BTIME != 0)
        .then_some((stat.stx_btime.tv_sec, stat.stx_btime.tv_nsec))
}

/// Linux: the system call itself, which a C library older than the kernel
/// does not wrap. A kernel without it, or a sandbox that refuses it, fails
/// the call, and no birth time is read.
#[cfg(all(target_os = "linux", any(target_env = "gnu", target_env = "musl")))]
fn birth(file: &File) -> Option<(i64, u32)> {
    use std::os::fd::AsRawFd;
    let mut stat = std::mem::MaybeUninit::<libc::statx>::zeroed();
    // SAFETY: the path is an empty C string, so the open descriptor itself
    // is asked; `stat` is a whole `statx` for the kernel to fill.
    let read = unsafe {
        libc::syscall(
            libc::SYS_statx,
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_STATX_SYNC_AS_STAT,
            libc::STATX_BTIME,
            stat.as_mut_ptr(),
        )
    };
    if read != 0 {
        return None;
    }
    // SAFETY: it was zeroed, all of it numbers, and the call returned 0.
    filled(&unsafe { stat.assume_init() })
}

/// Android: only through the C library's `statx`, looked up as the program
/// runs. Before the library had it (Android 11), the system refuses the
/// call to an app by ending the app, and a program built for an older
/// Android would not link against it.
#[cfg(target_os = "android")]
fn birth(file: &File) -> Option<(i64, u32)> {
    use std::os::fd::AsRawFd;
    type Statx = unsafe extern "C" fn(
        libc::c_int,
        *const libc::c_char,
        libc::c_int,
        libc::c_uint,
        *mut libc::statx,
    ) -> libc::c_int;
    // SAFETY: the name is a valid C string; nothing is called yet.
    let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"statx".as_ptr()) };
    if found.is_null() {
        return None;
    }
    // SAFETY: not null, and the C library's `statx` has this signature.
    let statx = unsafe { std::mem::transmute::<*mut libc::c_void, Statx>(found) };
    let mut stat = std::mem::MaybeUninit::<libc::statx>::zeroed();
    // SAFETY: the path is an empty C string, so the open descriptor itself
    // is asked; `stat` is a whole `statx` for the call to fill.
    let read = unsafe {
        statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_STATX_SYNC_AS_STAT,
            libc::STATX_BTIME,
            stat.as_mut_ptr(),
        )
    };
    if read != 0 {
        return None;
    }
    // SAFETY: it was zeroed, all of it numbers, and the call returned 0.
    filled(&unsafe { stat.assume_init() })
}

/// macOS and the BSDs: `stat` has it, and the standard library reads it.
/// Any other system has none the library knows of.
#[cfg(not(any(
    all(target_os = "linux", any(target_env = "gnu", target_env = "musl")),
    target_os = "android"
)))]
fn birth(file: &File) -> Option<(i64, u32)> {
    // Linux with another C library has `statx` under no name used here,
    // and Windows needs none.
    if cfg!(any(windows, target_os = "linux")) {
        return None;
    }
    let made = file.metadata().ok()?.created().ok()?;
    match made.duration_since(std::time::UNIX_EPOCH) {
        Ok(after) => Some((i64::try_from(after.as_secs()).ok()?, after.subsec_nanos())),
        // Before the epoch: the seconds round down, the nanoseconds count up.
        Err(before) => {
            let before = before.duration();
            let secs = i64::try_from(before.as_secs()).ok()?;
            match before.subsec_nanos() {
                0 => Some((-secs, 0)),
                nanos => Some((-secs - 1, 1_000_000_000 - nanos)),
            }
        }
    }
}

/// Linux and Android: the generation the file system keeps for the inode.
/// Asked of the file itself: a path-only handle is refused, as is a file
/// system that keeps none, and any refusal reads as none.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn version(file: &File) -> Option<u32> {
    use std::os::fd::AsRawFd;
    // The request is named for a long, and the kernel writes an int. A
    // file system that wrote the whole long would be read by its first
    // half: wrong where that is the high one, and then one number for
    // every file or none, never another file's.
    let mut answer: libc::c_long = 0;
    // SAFETY: `answer` is a long this call may write to, whole or in part.
    let read = unsafe { libc::ioctl(file.as_raw_fd(), libc::FS_IOC_GETVERSION, &mut answer) };
    if read != 0 {
        return None;
    }
    // The int is the first four bytes, whichever end the system starts at.
    let first = answer.to_ne_bytes();
    Some(u32::from_ne_bytes([first[0], first[1], first[2], first[3]]))
}

/// Elsewhere a generation is read by the superuser alone, or not kept.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn version(_: &File) -> Option<u32> {
    None
}
