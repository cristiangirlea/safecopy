//! A whole file saved into a held folder, verified before it is published.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::LazyLock;

use safecopy_folder::{
    Applied, Directory, Finish, Marked, Meta, MetaRules, PublishError, PublishRules, Refusal,
    refusal,
};
use tokio::io::AsyncRead;

use crate::{ChunkError, Digest, Pace, Progress, Settings, copy_exact, read_end, read_trailer};

/// The caller's steps around [`save_verified`], each with a default that
/// does nothing. An error from any removes the partial file and ends the
/// save with [`ChunkError::Hook`].
pub trait Hooks: Send + Sync {
    /// Before the partial file is created: the last moment to refuse with
    /// nothing on the disk.
    fn before_create(&self) -> io::Result<()> {
        Ok(())
    }

    /// The partial file `partial` was created as `file`: for a crash record.
    fn created(&self, file: &File, partial: &OsStr) -> io::Result<()> {
        let _ = (file, partial);
        Ok(())
    }

    /// Its hash matched; it is not yet marked and has no name.
    fn before_publish(&self, file: &File, partial: &OsStr) -> io::Result<()> {
        let _ = (file, partial);
        Ok(())
    }

    /// The save ended, with what became of the partial file: published,
    /// or after a failure removed, or left where the system refused to
    /// remove it, for the caller's crash cleanup to find. Called once for
    /// a save that runs to its end, whatever that end is; not called when
    /// the save's future is dropped, and the partial file then stays for
    /// the crash cleanup.
    fn ended(&self, outcome: Result<&Saved, &ChunkError>, partial: PartialFate) {
        let _ = (outcome, partial);
    }
}

/// What became of the partial file when a save ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PartialFate {
    /// The save ended before one was created.
    NotCreated,
    /// The save succeeded: it has its name. Where the file was linked or
    /// copied into that name, a partial name the system then refused to
    /// remove stays behind; [`Directory::publish_with`] does not report
    /// that, so a cleanup still looks for it.
    Published,
    /// The save failed, and it was removed.
    Removed,
    /// The save failed, and another file was under its name by then
    /// (Linux and macOS, which remove by name): that file is not the
    /// partial and was left untouched.
    Replaced,
    /// The save failed, and the system refused to remove it: what
    /// [`Hooks::created`] was given is still there, unless the kind says
    /// otherwise (one already gone is `NotFound`).
    Left {
        /// What the system said.
        kind: io::ErrorKind,
        /// The system's own code for it (`errno`, a Windows error code),
        /// when it has one.
        raw: Option<i32>,
    },
}

/// No hooks.
impl Hooks for () {}

/// How [`save_verified`] saves.
#[derive(Clone, Copy)]
pub struct SaveOptions<'a> {
    /// How many boundaries the hasher keeps.
    pub settings: Settings,
    /// The downloaded-file mark and the publish's retries.
    pub publish: &'a PublishRules,
    /// The sender's modified time and execute bit.
    pub meta: Meta,
    /// Which of them are set.
    pub meta_rules: MetaRules,
    /// Hears of every slice written.
    pub progress: &'a dyn Progress,
    /// Awaited before every slice written.
    pub pace: &'a dyn Pace,
    /// Flush the file's bytes to the disk before it gets its name, so a
    /// power cut never leaves a name on bytes that were not written; where
    /// the disk makes the file be copied into its name, the copy is flushed
    /// too before the save succeeds. The flush runs on tokio's blocking
    /// pool, never on a worker. On by default.
    pub sync: bool,
}

static DEFAULT_PUBLISH: LazyLock<PublishRules> = LazyLock::new(PublishRules::default);

impl Default for SaveOptions<'_> {
    fn default() -> Self {
        Self {
            settings: Settings::default(),
            publish: &*DEFAULT_PUBLISH,
            meta: Meta::default(),
            meta_rules: MetaRules::default(),
            progress: &(),
            pace: &(),
            sync: true,
        }
    }
}

/// A file saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    /// Where it is: the name asked for, or a numbered one.
    pub path: PathBuf,
    /// Whether it was marked as downloaded.
    pub marked: Marked,
    /// What of its time and execute bit was set.
    pub applied: Applied,
    /// Its hash, equal to the trailer's.
    pub digest: Digest,
}

/// The partial file's name: `name.part`, or `name.N.part` when taken.
fn partial_name(name: &str, n: u64) -> OsString {
    OsString::from(if n == 1 {
        format!("{name}.part")
    } else {
        format!("{name}.{n}.part")
    })
}

/// Saves the file that `reader` carries into `dir` as `name`: exactly
/// `size` bytes, then the 32-byte trailer, then the end of the stream.
///
/// In order: [`Hooks::before_create`]; the partial file created new
/// (`name.part`) and [`Hooks::created`]; the bytes written and hashed; the
/// trailer read and the stream's end checked; the hash compared; the
/// bytes flushed to the disk ([`SaveOptions::sync`]);
/// [`Hooks::before_publish`]; then [`Directory::publish_with`]: the
/// downloaded-file mark when `options.publish` asks, the time and the
/// execute bit as `options.meta_rules` allow, and the name, never
/// replacing a file. A file that fails anywhere, its hash included, is
/// removed before it gets a name. [`Hooks::ended`] hears the outcome, and
/// whether that removal failed ([`PartialFate::Left`]).
///
/// `cancel` is watched while the bytes come and while publishing.
pub async fn save_verified<R: AsyncRead + Unpin + ?Sized>(
    reader: &mut R,
    dir: &Directory,
    name: &str,
    size: u64,
    options: &SaveOptions<'_>,
    hooks: &dyn Hooks,
    cancel: impl Future<Output = ()>,
) -> Result<Saved, ChunkError> {
    let (outcome, partial) = save(reader, dir, name, size, options, hooks, cancel).await;
    hooks.ended(outcome.as_ref(), partial);
    outcome
}

async fn save<R: AsyncRead + Unpin + ?Sized>(
    reader: &mut R,
    dir: &Directory,
    name: &str,
    size: u64,
    options: &SaveOptions<'_>,
    hooks: &dyn Hooks,
    cancel: impl Future<Output = ()>,
) -> (Result<Saved, ChunkError>, PartialFate) {
    tokio::pin!(cancel);
    if let Err(e) = hooks.before_create() {
        return (Err(ChunkError::Hook(e.kind())), PartialFate::NotCreated);
    }
    let (file, partial) = match create(dir, name) {
        Ok(created) => created,
        Err(e) => return (Err(e), PartialFate::NotCreated),
    };
    let target = Target {
        dir,
        file: &file,
        partial: &partial,
        name,
        size,
    };
    let saved = fill(reader, &target, options, hooks, &mut cancel).await;
    let fate = if saved.is_ok() {
        PartialFate::Published
    } else {
        // A file that is not whole and checked never gets a name. One that
        // cannot be removed now is left to the caller's crash cleanup; the
        // error that stopped the save is still the one returned.
        match discard(dir, &file, &partial) {
            Ok(()) => PartialFate::Removed,
            Err(e) if refusal(&e) == Some(Refusal::Replaced) => PartialFate::Replaced,
            Err(e) => PartialFate::Left {
                kind: e.kind(),
                raw: e.raw_os_error(),
            },
        }
    };
    (saved, fate)
}

fn discard(dir: &Directory, file: &File, partial: &OsStr) -> io::Result<()> {
    #[cfg(test)]
    if let Some(error) = crate::seams::DISCARD_ERROR.take() {
        return Err(io::Error::from_raw_os_error(error));
    }
    dir.discard(file, partial)
}

fn create(dir: &Directory, name: &str) -> Result<(File, OsString), ChunkError> {
    for n in 1.. {
        let partial = partial_name(name, n);
        match dir.create(&partial) {
            Ok(file) => return Ok((file, partial)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    unreachable!("the numbers never run out")
}

/// The file being saved: where, as what, and how long.
struct Target<'a> {
    dir: &'a Directory,
    file: &'a File,
    partial: &'a OsStr,
    name: &'a str,
    size: u64,
}

/// Flushes `file`'s bytes to the disk on the blocking pool, through a
/// handle of its own: a large flush never holds a worker, and a cancel
/// leaves it running there and returns at once.
async fn sync_data(
    file: &File,
    cancel: &mut Pin<&mut impl Future<Output = ()>>,
) -> Result<(), ChunkError> {
    let handle = file.try_clone()?;
    let syncing = tokio::task::spawn_blocking(move || handle.sync_data());
    tokio::select! {
        biased;
        () = cancel.as_mut() => Err(ChunkError::Cancelled),
        synced = syncing => match synced {
            Ok(synced) => synced.map_err(ChunkError::from),
            Err(_) => Err(ChunkError::Io(io::ErrorKind::Other)),
        },
    }
}

async fn fill<R: AsyncRead + Unpin + ?Sized>(
    reader: &mut R,
    target: &Target<'_>,
    options: &SaveOptions<'_>,
    hooks: &dyn Hooks,
    cancel: &mut Pin<&mut impl Future<Output = ()>>,
) -> Result<Saved, ChunkError> {
    let Target {
        dir,
        file,
        partial,
        name,
        size,
    } = *target;
    hooks
        .created(file, partial)
        .map_err(|e| ChunkError::Hook(e.kind()))?;
    let mut hasher = options.settings.receiving();
    let mut writer = tokio::fs::File::from_std(file.try_clone()?);
    let receiving = async {
        copy_exact(
            reader,
            &mut writer,
            size,
            0,
            &mut hasher,
            options.progress,
            options.pace,
        )
        .await?;
        let trailer = read_trailer(reader).await?;
        read_end(reader).await?;
        Ok(trailer)
    };
    let trailer = tokio::select! {
        biased;
        () = cancel.as_mut() => Err(ChunkError::Cancelled),
        trailer = receiving => trailer,
    };
    // Every write is done before the handle is used for anything else.
    drop(writer.into_std().await);
    let trailer = trailer?;
    let digest = hasher.finish();
    if digest != trailer {
        return Err(ChunkError::Damaged);
    }
    if options.sync {
        sync_data(file, cancel).await?;
    }
    hooks
        .before_publish(file, partial)
        .map_err(|e| ChunkError::Hook(e.kind()))?;
    let (published, applied) = dir
        .publish_with(
            file,
            partial,
            name,
            options.publish,
            &Finish {
                meta: options.meta,
                meta_rules: options.meta_rules,
                sync: options.sync,
            },
            cancel.as_mut(),
        )
        .await
        .map_err(|e| match e {
            PublishError::Cancelled => ChunkError::Cancelled,
            PublishError::Io(e) | PublishError::Name { error: e, .. } => e.into(),
            _ => ChunkError::Io(io::ErrorKind::Other),
        })?;
    Ok(Saved {
        path: published.path,
        marked: published.marked,
        applied,
        digest,
    })
}
