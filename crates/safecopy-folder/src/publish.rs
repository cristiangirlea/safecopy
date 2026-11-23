//! Giving a finished file its name without replacing anything.

use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use safecopy_names::candidates;
use tokio::io::AsyncWriteExt;

use crate::{Applied, Directory, Meta, MetaRules, Refusal, refusal};

/// How a finished file is published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishRules {
    /// Mark the file as downloaded before it gets its name, so it is never
    /// there unmarked ([`Directory::mark`]). Off by default: this crate
    /// marks nothing unless asked.
    pub mark: bool,
    /// How often a publish refused because something else holds the file a
    /// moment (a virus scanner, an indexer) is tried again.
    pub retries: u32,
    /// How long to wait before each of those tries.
    pub retry_wait: Duration,
}

impl Default for PublishRules {
    fn default() -> Self {
        Self {
            mark: false,
            retries: 5,
            retry_wait: Duration::from_millis(50),
        }
    }
}

/// What [`Directory::publish_with`] does to a file besides naming it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finish {
    /// The sender's modified time and execute bit.
    pub meta: Meta,
    /// Which of them are set.
    pub meta_rules: MetaRules,
    /// Flush a copy's bytes to the disk before it is final, where the disk
    /// makes the file be copied into its name. The file itself is the
    /// caller's to flush before publishing. On by default.
    pub sync: bool,
}

impl Default for Finish {
    fn default() -> Self {
        Self {
            meta: Meta::default(),
            meta_rules: MetaRules::default(),
            sync: true,
        }
    }
}

/// Flushes `file`'s bytes to the disk on the blocking pool, through a
/// handle of its own: a cancel leaves the flush running and the caller's
/// handle free.
async fn sync_data(file: &File, cancel: impl Future<Output = ()>) -> Result<(), PublishError> {
    #[cfg(test)]
    crate::seams::SYNCS.set(crate::seams::SYNCS.get() + 1);
    let handle = file.try_clone()?;
    let syncing = tokio::task::spawn_blocking(move || handle.sync_data());
    tokio::select! {
        biased;
        () = cancel => Err(PublishError::Cancelled),
        synced = syncing => match synced {
            Ok(synced) => synced.map_err(PublishError::Io),
            Err(_) => Err(PublishError::Io(io::ErrorKind::Other.into())),
        },
    }
}

/// A copy created under its final name, removed on drop unless kept.
struct Claim<'a> {
    dir: &'a Directory,
    file: File,
    leaf: &'a OsStr,
    kept: bool,
}

impl Claim<'_> {
    fn keep(mut self) {
        self.kept = true;
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        if !self.kept {
            let _ = self.dir.discard(&self.file, self.leaf);
        }
    }
}

/// Whether a file was marked as downloaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Marked {
    /// The rules did not ask for it.
    NotAsked,
    /// Marked.
    Marked,
    /// This system has no such mark.
    NotOnThisSystem,
    /// The mark could not be written (a FAT or exFAT disk has no streams
    /// to keep it in). The file is kept unmarked, as browsers do.
    Failed(io::ErrorKind),
}

/// A file published.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    /// Where it is: the name asked for, or a numbered one when that was
    /// taken.
    pub path: PathBuf,
    /// Whether it was marked as downloaded.
    pub marked: Marked,
}

/// Why [`Directory::publish`] or [`publish_path`] did not publish.
#[derive(Debug)]
#[non_exhaustive]
pub enum PublishError {
    /// The caller's cancel finished first. Nothing was published; the
    /// partial file is still there for the caller to discard.
    Cancelled,
    /// The system refused before any name was tried: the file's time or
    /// execute bit could not be set.
    Io(io::Error),
    /// The system refused while the file was being given a name.
    Name {
        /// The name being tried, as [`Published::path`] would give it: the
        /// name asked for, or the numbered one reached when that and the
        /// ones before it were taken.
        path: PathBuf,
        /// What the system said, with its kind and its own code.
        error: io::Error,
    },
}

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The kind alone: the caller words it, and names the file itself.
        match self {
            Self::Cancelled => f.write_str("Cancelled"),
            Self::Io(e) => write!(f, "Io({:?})", e.kind()),
            Self::Name { error, .. } => write!(f, "Name({:?})", error.kind()),
        }
    }
}

impl std::error::Error for PublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Cancelled => None,
            Self::Io(e) | Self::Name { error: e, .. } => Some(e),
        }
    }
}

impl From<io::Error> for PublishError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// The staging name of `path`: its name with `.part` after it.
pub fn part(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

/// `name` in `dir`, or "name (2).ext" and so on when that or its
/// [`part`] exists. Only a wish: another file may take it before it is
/// published, which [`publish_path`] allows for.
pub fn unique(dir: &Path, name: &str) -> PathBuf {
    candidates(name)
        .map(|name| dir.join(name))
        .find(|p| !p.exists() && !part(p).exists())
        .expect("the numbers never run out")
}

/// Marks the file at `path` as downloaded, as [`Directory::mark`] does.
pub fn mark_path(path: &Path) -> Marked {
    #[cfg(windows)]
    return crate::platform::mark_path(path);
    #[cfg(not(windows))]
    {
        let _ = path;
        Marked::NotOnThisSystem
    }
}

/// Whether publishing failed because the file system cannot link or
/// rename by handle at all (FAT, exFAT, some network shares, no procfs),
/// rather than for this file.
fn no_hard_links(e: &io::Error) -> bool {
    if e.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    let code = e.raw_os_error();
    #[cfg(windows)]
    {
        // ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED, ERROR_NOT_SAME_DEVICE
        matches!(code, Some(1 | 50 | 17))
    }
    #[cfg(unix)]
    {
        // procfs can be absent or denied in Android apps and containers.
        matches!(
            code,
            Some(
                libc::EPERM
                    | libc::ENOENT
                    | libc::EACCES
                    | libc::EOPNOTSUPP
                    | libc::EXDEV
                    | libc::ENOSYS
            )
        ) || code == Some(libc::ENOTSUP)
    }
}

/// Whether a publish failed because something holds the file a moment.
fn held(e: &io::Error) -> bool {
    // 32 is Windows' ERROR_SHARING_VIOLATION; on Unix it is EPIPE.
    e.kind() == io::ErrorKind::PermissionDenied || cfg!(windows) && e.raw_os_error() == Some(32)
}

/// Whether `path` is, without following a link, the file `meta` was read
/// from. Unix only: Windows' file ID is not in std's metadata, and
/// [`Directory::publish`] removes by a checked handle there.
fn still(path: &Path, meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(path)
            .is_ok_and(|m| m.dev() == meta.dev() && m.ino() == meta.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, meta);
        true
    }
}

impl Directory {
    /// Whether linking `file` failed for this folder rather than for the
    /// file, so it would fail for every file here.
    fn cannot_link(&self, file: &File, e: &io::Error) -> bool {
        #[cfg(windows)]
        {
            let _ = file;
            // ERROR_NOT_SAME_DEVICE: the file is on another volume.
            e.raw_os_error() != Some(17)
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let _ = e;
            // A deleted file (ENOENT through procfs) or one on another disk
            // (EXDEV) says nothing of the folder.
            match (file.metadata(), self.file.metadata()) {
                (Ok(file), Ok(folder)) => file.nlink() > 0 && file.dev() == folder.dev(),
                _ => false,
            }
        }
    }

    /// Gives `file`, created here as `partial` ([`Self::create`]), its
    /// final name: `name`, or "name (2).ext" and so on when that is taken.
    /// A file that took a name is never replaced, however late it came.
    /// With [`PublishRules::mark`] the file is marked before it gets a name.
    ///
    /// The name is claimed with [`Self::finish`], else [`Self::rename`];
    /// where the disk can do neither, the file is copied from its open
    /// handle into the name created new, and `partial` is discarded. When
    /// linking fails for the folder (no procfs, procfs denied), this handle
    /// remembers it and later names go straight to the rename. A file that
    /// took `partial`'s name meanwhile is never removed.
    /// `cancel` is checked before each step and while waiting. After an
    /// error or a cancel, `partial` is left for the caller to discard. An
    /// error at a name says which ([`PublishError::Name`]).
    ///
    /// Once the file has its name, the publish succeeds: when Linux or
    /// Android linked the name but could not remove `partial` (the system
    /// refused), the file is published and `partial` is left behind, a
    /// second name of the same file, for the caller's crash cleanup (the
    /// journal) to remove.
    pub async fn publish(
        &self,
        file: &File,
        partial: &OsStr,
        name: &str,
        rules: &PublishRules,
        cancel: impl Future<Output = ()>,
    ) -> Result<Published, PublishError> {
        let nothing = Finish {
            meta: Meta::default(),
            meta_rules: MetaRules {
                times: false,
                executable: false,
            },
            sync: false,
        };
        let (published, _) = self
            .publish_with(file, partial, name, rules, &nothing, cancel)
            .await?;
        Ok(published)
    }

    /// [`Self::publish`], setting `finish.meta` as `finish.meta_rules`
    /// allow ([`Self::apply`]) after the mark and before the name: the file
    /// has its time and execute bit before anyone can see it. When the disk
    /// makes the file be copied into its name, the copy gets them too, and
    /// with [`Finish::sync`] its bytes are flushed to the disk before the
    /// publish succeeds; a copy that fails any of that is removed.
    pub async fn publish_with(
        &self,
        file: &File,
        partial: &OsStr,
        name: &str,
        rules: &PublishRules,
        finish: &Finish,
        cancel: impl Future<Output = ()>,
    ) -> Result<(Published, Applied), PublishError> {
        let Finish {
            meta, meta_rules, ..
        } = finish;
        tokio::pin!(cancel);
        let mut marked = if rules.mark {
            self.mark(partial)
        } else {
            Marked::NotAsked
        };
        // After the mark: writing it changes the time on Windows.
        let mut applied = self.apply(file, meta, meta_rules)?;
        for target in candidates(name) {
            let leaf = OsStr::new(&target);
            let published = |marked, applied| {
                (
                    Published {
                        path: self.path.join(&target),
                        marked,
                    },
                    applied,
                )
            };
            // The caller knows only its first choice: the error says which name this is.
            let at_name = |error: io::Error| PublishError::Name {
                path: self.path.join(&target),
                error,
            };
            let mut tries = 0;
            let finished = loop {
                tokio::select! {
                    biased;
                    () = &mut cancel => return Err(PublishError::Cancelled),
                    () = std::future::ready(()) => {}
                }
                match self.finish(file, partial, leaf) {
                    Err(e) if held(&e) && tries < rules.retries => {
                        tries += 1;
                        tokio::select! {
                            biased;
                            () = &mut cancel => return Err(PublishError::Cancelled),
                            () = tokio::time::sleep(rules.retry_wait) => {}
                        }
                    }
                    result => break result,
                }
            };
            match finished {
                Ok(()) => return Ok(published(marked, applied)),
                // Named all the same; the file that took `partial` is not ours to remove.
                Err(e) if refusal(&e) == Some(Refusal::Replaced) => {
                    return Ok(published(marked, applied));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) if no_hard_links(&e) => {
                    // Retrying every name for a folder that cannot link costs the waits again.
                    if self.cannot_link(file, &e) {
                        self.no_link
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                Err(e) => return Err(at_name(e)),
            }
            match self.rename(file, partial, leaf) {
                Ok(()) => return Ok(published(marked, applied)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e)
                    if no_hard_links(&e)
                        || e.kind() == io::ErrorKind::InvalidInput
                        || refusal(&e) == Some(Refusal::Replaced) => {}
                Err(e) => return Err(at_name(e)),
            }
            let source = file.try_clone().map_err(at_name)?;
            let output = match self.create(leaf) {
                Ok(output) => output,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(at_name(e)),
            };
            // Until the publish succeeds the copy goes, even with a future dropped midway.
            let claim = Claim {
                dir: self,
                file: output,
                leaf,
                kept: false,
            };
            let output = claim.file.try_clone().map_err(at_name)?;
            // Copied from the open file, not from what `partial` names now.
            if rules.mark {
                marked = self.mark(leaf);
            }
            let mut output = tokio::fs::File::from_std(output);
            let mut source = tokio::fs::File::from_std(source);
            let copying = async {
                use tokio::io::AsyncSeekExt;
                source.rewind().await?;
                tokio::io::copy(&mut source, &mut output).await?;
                output.flush().await?;
                io::Result::Ok(())
            };
            let copied = tokio::select! {
                biased;
                () = &mut cancel => Err(PublishError::Cancelled),
                copied = copying => copied.map_err(PublishError::Io),
            };
            let output = output.into_std().await;
            // A new file: the time and the bit are set on it again, after its last byte.
            let copied = copied.and_then(|()| {
                applied = self.apply(&output, meta, meta_rules)?;
                Ok(())
            });
            // The copy is a file of its own: the caller's flush of `file` did not reach it.
            let copied = match copied {
                Ok(()) if finish.sync => sync_data(&output, &mut cancel).await,
                copied => copied,
            };
            copied.map_err(|e| match e {
                PublishError::Io(e) => at_name(e),
                other => other,
            })?;
            claim.keep();
            let _ = self.discard(file, partial);
            return Ok(published(marked, applied));
        }
        unreachable!("the numbers never run out")
    }
}

/// Gives the closed file at `partial` its final name, by path: `wanted` if
/// still free, else the next free name for `name` in `dir`. A file that took
/// the name meanwhile is never replaced. With [`PublishRules::mark`] the
/// file is marked first.
///
/// A rename would replace a file that took the name (one whose partial was
/// created before this one's name was chosen, and finished first). A hard
/// link refuses a taken name instead, so the name is claimed in the one
/// step that makes it visible. Where the file system has no hard links, the
/// name is claimed by creating it new, and the rename then replaces only
/// that claim.
///
/// By path, links on the way are followed: prefer [`Directory::publish`].
///
/// Every error is a [`PublishError::Name`]: the name being tried when the
/// system refused, which is `wanted` or a numbered one after it.
pub async fn publish_path(
    partial: &Path,
    wanted: &Path,
    dir: &Path,
    name: &str,
    rules: &PublishRules,
) -> Result<Published, PublishError> {
    let marked = if rules.mark {
        mark_path(partial)
    } else {
        Marked::NotAsked
    };
    let own = wanted.to_path_buf();
    let others = candidates(name)
        .map(|name| dir.join(name))
        .filter(move |p| *p != own);
    for target in std::iter::once(wanted.to_path_buf()).chain(others) {
        let at_name = |error: io::Error| PublishError::Name {
            path: target.clone(),
            error,
        };
        match hard_link(partial, &target).await {
            Ok(()) => {
                #[cfg(test)]
                crate::seams::at("linked");
                // The content is in place; a partial that will not go only clutters the
                // folder. One swapped in meanwhile is not the file linked, and stays.
                if let Ok(linked) = tokio::fs::symlink_metadata(&target).await
                    && still(partial, &linked)
                {
                    let _ = tokio::fs::remove_file(partial).await;
                }
                return Ok(Published {
                    path: target,
                    marked,
                });
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) if no_hard_links(&e) => {}
            Err(e) => return Err(at_name(e)),
        }
        let claimed = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .await;
        let claim = match claimed {
            // Dropped here: Windows cannot rename over a file held open. Without
            // its identity the claim cannot be told from a file put in its
            // place, so it is left.
            Ok(claim) => claim.metadata().await.map_err(at_name)?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(at_name(e)),
        };
        #[cfg(test)]
        crate::seams::at("claimed");
        let mut tries = 0;
        let renamed = loop {
            match tokio::fs::rename(partial, &target).await {
                // An indexer or scanner may hold the new claim open a moment.
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied && tries < rules.retries => {
                    tries += 1;
                    tokio::time::sleep(rules.retry_wait).await;
                }
                done => break done,
            }
        };
        if let Err(e) = renamed {
            // Only the empty claim goes, not a file put in its place.
            if still(&target, &claim) {
                let _ = tokio::fs::remove_file(&target).await;
            }
            return Err(at_name(e));
        }
        return Ok(Published {
            path: target,
            marked,
        });
    }
    unreachable!("the numbers never run out")
}

async fn hard_link(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(test)]
    if crate::seams::NO_HARD_LINKS.get() {
        return Err(io::ErrorKind::Unsupported.into());
    }
    tokio::fs::hard_link(from, to).await
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn restricted_procfs_errors_allow_publishing_without_a_hard_link() {
        for code in [
            libc::EACCES,
            libc::ENOENT,
            libc::EPERM,
            libc::EOPNOTSUPP,
            libc::ENOTSUP,
            libc::EXDEV,
            libc::ENOSYS,
        ] {
            assert!(no_hard_links(&io::Error::from_raw_os_error(code)));
        }
        assert!(!no_hard_links(&io::Error::from_raw_os_error(libc::EIO)));
    }

    #[cfg(windows)]
    #[test]
    fn disks_without_hard_links_are_told_apart_from_other_failures() {
        for code in [1, 17, 50] {
            assert!(no_hard_links(&io::Error::from_raw_os_error(code)));
        }
        assert!(no_hard_links(&io::ErrorKind::Unsupported.into()));
        for code in [5, 32, 183] {
            assert!(!no_hard_links(&io::Error::from_raw_os_error(code)));
        }
    }

    #[test]
    fn the_default_rules_mark_nothing_and_retry_five_times_fifty_milliseconds_apart() {
        let rules = PublishRules::default();
        assert!(!rules.mark);
        assert_eq!(rules.retries, 5);
        assert_eq!(rules.retry_wait, Duration::from_millis(50));
    }

    #[test]
    fn a_partial_file_is_named_after_its_file() {
        assert_eq!(part(Path::new("a/b.txt")), Path::new("a/b.txt.part"));
        assert_eq!(part(Path::new(".hidden")), Path::new(".hidden.part"));
    }
}
