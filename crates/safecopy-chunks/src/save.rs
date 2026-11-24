//! A whole file saved into a held folder, verified before it is published.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, ready};

use safecopy_folder::{
    Applied, Directory, Finish, Marked, Meta, MetaRules, PublishError, PublishRules, Refusal,
    refusal,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::task::JoinHandle;

use crate::{
    ChunkError, Digest, Hasher, Pace, Progress, Settings, copy_exact, read_end, read_trailer,
};

/// The caller's steps around [`save_verified`] and [`save_resumable`], each
/// with a default that does nothing. An error from any ends the save with
/// [`ChunkError::Hook`], and the partial file is removed unless the
/// caller's rule keeps it.
pub trait Hooks: Send + Sync {
    /// Before the partial file is created: the last moment to refuse with
    /// nothing on the disk. Not called for [`Start::Resume`].
    fn before_create(&self) -> io::Result<()> {
        Ok(())
    }

    /// The partial file `partial` was created as `file`: for a crash record.
    /// Not called for [`Start::Resume`], whose caller holds the file
    /// already. `file` is the save's own handle: reading or seeking it,
    /// here or later through a clone kept, on any thread, moves no byte
    /// the save writes or publishes, since every byte is written at its
    /// own offset in the file and never at the handle's position, and a
    /// file that is copied into its name is read the same way.
    fn created(&self, file: &File, partial: &OsStr) -> io::Result<()> {
        let _ = (file, partial);
        Ok(())
    }

    /// The first `have` bytes of `file`, a whole number of chunks, are
    /// written, and `prefix` is their hash: for a resume record, from the
    /// first chunk on. Called after each whole chunk is written and before
    /// the next byte is read; a file that ends on a chunk hears of its
    /// last one too. The bytes are written, not yet flushed to the disk.
    ///
    /// An error ends the save. When the caller's rule keeps that error, the
    /// partial file is kept with this chunk in it: its bytes are written
    /// and hashed, whatever the hook did not get to record.
    ///
    /// `file` is the save's own handle, as in [`Self::created`]: its
    /// position is nothing to the save.
    ///
    /// It is called on the thread that polls the save. A hook that
    /// flushes to the disk here (a resume record's rewrite) holds that
    /// thread meanwhile: see `safecopy-resume` for where to run it.
    fn whole(&self, file: &File, have: u64, prefix: Digest) -> io::Result<()> {
        let _ = (file, have, prefix);
        Ok(())
    }

    /// Its hash matched; it is not yet marked and has no name.
    fn before_publish(&self, file: &File, partial: &OsStr) -> io::Result<()> {
        let _ = (file, partial);
        Ok(())
    }

    /// The save ended, with what became of the partial file: published,
    /// or after a failure removed, kept for a later save, or left where
    /// the system refused to remove it, for the caller's crash cleanup to
    /// find. Called once for
    /// a save that runs to its end, whatever that end is; not called when
    /// the save's future is dropped, and the partial file then stays for
    /// the crash cleanup, with no write of the save still to come.
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
    /// The save failed with an error the caller's rule keeps on: it is
    /// still there, cut back to its first `have` bytes, and
    /// [`Stopped::kept`] holds it.
    Kept {
        /// The bytes kept: a whole number of chunks, at least one.
        have: u64,
    },
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

/// How [`save_verified`] and [`save_resumable`] save.
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

/// The file a save makes: where, under which name, and how long.
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    /// The folder it is saved into.
    pub dir: &'a Directory,
    /// Its name there; a taken one is numbered, never replaced. Nothing is
    /// cut here: the number (" (N)") and the partial file's ending
    /// (`.part`, `.N.part`) come after it, so the caller's limit on a name
    /// leaves room for them below what the disk holds in one. The default
    /// rules of `safecopy-names` leave 55, in bytes and in UTF-16 units.
    pub name: &'a str,
    /// The whole file's bytes, a resumed one's kept bytes included.
    pub size: u64,
}

/// The partial file's name: `name.part`, or `name.N.part` when taken.
fn partial_name(name: &str, n: u64) -> OsString {
    OsString::from(if n == 1 {
        format!("{name}.part")
    } else {
        format!("{name}.{n}.part")
    })
}

/// Where a save starts.
#[derive(Debug)]
pub enum Start {
    /// A new partial file (`name.part`), from byte 0.
    New,
    /// A partial file kept from an earlier save, continued where it
    /// stopped: it is cut to its hasher's position before the first byte
    /// is appended.
    Resume(Box<Kept>),
}

/// A partial file kept from a save that stopped, and what a save that
/// continues it starts from ([`Start::Resume`]).
#[derive(Debug)]
pub struct Kept {
    /// The partial file, open for reading and writing. One a save kept is
    /// cut to the bytes kept.
    pub file: File,
    /// Its name in the folder.
    pub partial: OsString,
    /// The hash of the bytes kept, standing after the last of them: a
    /// whole number of chunks within the file's size. Whoever makes a
    /// `Kept` answers for it matching the file's bytes; the whole-file
    /// hash catches one that does not.
    pub hasher: Hasher,
}

impl Kept {
    /// The bytes kept: a whole number of chunks.
    pub fn have(&self) -> u64 {
        self.hasher.position()
    }

    /// The hash of those bytes.
    pub fn prefix(&self) -> Digest {
        self.hasher.clone().finish()
    }
}

/// A save that did not finish.
#[derive(Debug)]
pub struct Stopped {
    /// Why.
    pub error: ChunkError,
    /// The partial file, when the caller's rule kept it; `None` when it
    /// was removed, or left where the system refused to remove it.
    pub kept: Option<Box<Kept>>,
}

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for Stopped {}

/// Saves the file that `reader` carries into `dir` as `name`: exactly
/// `size` bytes, then the 32-byte trailer, then the end of the stream.
/// [`save_resumable`] from [`Start::New`], keeping nothing.
///
/// In order: [`Hooks::before_create`]; the partial file created new
/// (`name.part`) and [`Hooks::created`]; the bytes written and hashed,
/// with [`Hooks::whole`] at each whole chunk; the trailer read and the
/// stream's end checked; the hash compared; the bytes flushed to the disk
/// ([`SaveOptions::sync`]); [`Hooks::before_publish`]; then
/// [`Directory::publish_with`]: the downloaded-file mark when
/// `options.publish` asks, the time and the execute bit as
/// `options.meta_rules` allow, and the name, never replacing a file. A
/// file that fails anywhere, its hash included, is removed before it gets
/// a name. [`Hooks::ended`] hears the outcome, and whether that removal
/// failed ([`PartialFate::Left`]).
///
/// `cancel` is watched while the bytes come and while publishing.
///
/// Dropping the save's future ends its writing of the partial file: when
/// the drop returns, no write to it is running and none can start, and
/// the handle the save wrote through is closed. The drop waits for a
/// write already under way on the blocking pool, for as long as that
/// one write takes: one slice ([`crate::SLICE`]) at most, and on a disk
/// that stalls, as long as it stalls.
pub async fn save_verified<R: AsyncRead + Unpin + ?Sized>(
    reader: &mut R,
    dir: &Directory,
    name: &str,
    size: u64,
    options: &SaveOptions<'_>,
    hooks: &dyn Hooks,
    cancel: impl Future<Output = ()>,
) -> Result<Saved, ChunkError> {
    let keep = |_: &ChunkError| false;
    let target = Target { dir, name, size };
    save_resumable(reader, target, Start::New, &keep, options, hooks, cancel)
        .await
        .map_err(|stopped| stopped.error)
}

/// Saves the file that `reader` carries into `target.dir` as
/// `target.name`, from a new partial file or from one kept before, and
/// keeps the partial file when it stops on an error `keep` names.
///
/// From [`Start::New`] the stream carries all `target.size` bytes and the
/// save goes as [`save_verified`] does. From [`Start::Resume`] it carries
/// the bytes from the hasher's position (`have`) to the size: the partial
/// file is cut to `have` and the bytes appended from there, wherever the
/// handle's position stood, and the hash compared with
/// the trailer is of the whole file, the hasher handed in and the new
/// bytes. A stream that brings more or fewer bytes fails as ever
/// ([`ChunkError::CutShort`] with the bytes of the whole file that are
/// there, [`ChunkError::TooLong`]). A hasher past `size` or off a chunk is
/// [`ChunkError::Offset`], a partial file shorter than `have`
/// [`ChunkError::PartialShort`]. The file is published exactly as
/// [`save_verified`] publishes: verified first, then the mark, the time
/// and the execute bit, never replacing a file.
///
/// When the save stops, `keep` is asked of the error. If it says yes, the
/// partial file is not removed: it is cut back to its last whole chunk
/// and [`Stopped::kept`] holds it, open, with its name and the running
/// hash at the bytes kept ([`Kept::have`], [`Kept::prefix`]), and
/// [`Hooks::ended`] hears [`PartialFate::Kept`]. It is removed
/// all the same, whatever `keep` says, when the error is
/// [`ChunkError::Cancelled`] or [`ChunkError::Damaged`], when not one
/// whole chunk is written, when it cannot be cut back, or when the save
/// failed after every byte came and the hash matched (the flush, the
/// hook before publishing, the publish itself): `keep` is asked only of
/// errors that came while the stream was read. A write that fails within
/// a chunk keeps the whole chunks before it; an error from
/// [`Hooks::whole`] keeps the chunk it was told of too.
///
/// **Dropped.** A save whose future is dropped leaves its partial file
/// as its writes left it, and no write behind: each write runs on the
/// blocking pool behind a gate the save's writer shares with it, and the
/// drop shuts that gate. When the drop returns, no write of the save is
/// running and none can start, whatever the pool still holds, so a save
/// that continues the partial afterwards never has bytes of the dropped
/// one land over its own. The drop waits for a write already under way,
/// for as long as that one write takes: one slice ([`crate::SLICE`]) at
/// most, and on a disk that stalls, as long as it stalls. A copy into
/// the name that a publish had begun ends the same way: the drop waits
/// for the slice of it under way, and removes the copy; only a flush of
/// the copy still on the pool ends there
/// ([`Directory::publish_with`]). Every
/// other end goes through the
/// same gate first: it is shut before the partial is cut back, handed
/// back in [`Stopped::kept`], flushed, published or removed, and
/// [`Hooks::whole`] is called with no write under way.
///
/// What the matching hash proves: see the [crate] docs.
pub async fn save_resumable<R: AsyncRead + Unpin + ?Sized>(
    reader: &mut R,
    target: Target<'_>,
    start: Start,
    keep: &(dyn Fn(&ChunkError) -> bool + Sync),
    options: &SaveOptions<'_>,
    hooks: &dyn Hooks,
    cancel: impl Future<Output = ()>,
) -> Result<Saved, Stopped> {
    tokio::pin!(cancel);
    let Target { dir, name, .. } = target;
    let (file, partial, hasher, resumed) = match start {
        Start::New => {
            if let Err(e) = hooks.before_create() {
                let error = ChunkError::Hook(e.kind());
                hooks.ended(Err(&error), PartialFate::NotCreated);
                return Err(Stopped { error, kept: None });
            }
            match create(dir, name) {
                Ok((file, partial)) => (file, partial, options.settings.receiving(), false),
                Err(error) => {
                    hooks.ended(Err(&error), PartialFate::NotCreated);
                    return Err(Stopped { error, kept: None });
                }
            }
        }
        Start::Resume(kept) => (kept.file, kept.partial, kept.hasher, true),
    };
    let open = Open {
        target,
        file: &file,
        partial: &partial,
    };
    let mut run = Run {
        hasher,
        whole: None,
        resumed,
        verified: false,
    };
    let error = match fill(reader, &open, &mut run, options, hooks, &mut cancel).await {
        Ok(saved) => {
            hooks.ended(Ok(&saved), PartialFate::Published);
            return Ok(saved);
        }
        Err(error) => error,
    };
    // A cancel is the person's no, and a wrong hash leaves no byte to trust:
    // neither is the rule's to keep. Nor is a whole, checked file that
    // failed on its way to its name: it may carry the mark and the
    // execute bit by then, and there is no stream left to continue.
    let may_keep = !run.verified && !matches!(error, ChunkError::Cancelled | ChunkError::Damaged);
    if may_keep
        && keep(&error)
        && let Some(hasher) = run.whole.take()
        && cut_back(&file, hasher.position()).is_ok()
    {
        let have = hasher.position();
        hooks.ended(Err(&error), PartialFate::Kept { have });
        let kept = Box::new(Kept {
            file,
            partial,
            hasher,
        });
        return Err(Stopped {
            error,
            kept: Some(kept),
        });
    }
    // A file that is not whole and checked never gets a name. One that
    // cannot be removed now is left to the caller's crash cleanup; the
    // error that stopped the save is still the one returned.
    let fate = match discard(dir, &file, &partial) {
        Ok(()) => PartialFate::Removed,
        Err(e) if refusal(&e) == Some(Refusal::Replaced) => PartialFate::Replaced,
        Err(e) => PartialFate::Left {
            kind: e.kind(),
            raw: e.raw_os_error(),
        },
    };
    hooks.ended(Err(&error), fate);
    Err(Stopped { error, kept: None })
}

/// Cuts a partial that is kept to its whole chunks.
fn cut_back(file: &File, have: u64) -> io::Result<()> {
    #[cfg(test)]
    if crate::seams::CUT_ERROR.replace(false) {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    file.set_len(have)
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

/// The file being saved, with its partial file open.
struct Open<'a> {
    target: Target<'a>,
    file: &'a File,
    partial: &'a OsStr,
}

/// The hashing of one save.
struct Run {
    /// Stands after the last byte written.
    hasher: Hasher,
    /// As it stood at the last whole chunk known written: what a kept
    /// partial is cut back to.
    whole: Option<Hasher>,
    /// From a kept partial.
    resumed: bool,
    /// Every byte came and the hash matched.
    verified: bool,
}

/// Flushes `file`'s bytes to the disk on the blocking pool, through a
/// handle of its own: a large flush never holds a worker, and a cancel
/// leaves it running there and returns at once.
async fn sync_data(
    file: &File,
    cancel: &mut Pin<&mut impl Future<Output = ()>>,
) -> Result<(), ChunkError> {
    #[cfg(test)]
    if crate::seams::SYNC_ERROR.replace(false) {
        return Err(ChunkError::Io(io::ErrorKind::StorageFull));
    }
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

/// The kept partial made ready to append to: its hasher on a chunk within
/// the size, the file at least that long and cut to it.
fn ready(file: &File, hasher: &Hasher, size: u64) -> Result<(), ChunkError> {
    let have = hasher.position();
    if have > size || !have.is_multiple_of(hasher.chunk()) {
        return Err(ChunkError::Offset {
            from: have,
            size,
            hashed: have,
        });
    }
    let len = file.metadata()?.len();
    if len < have {
        return Err(ChunkError::PartialShort { have, len });
    }
    file.set_len(have)?;
    Ok(())
}

async fn fill<R: AsyncRead + Unpin + ?Sized>(
    reader: &mut R,
    open: &Open<'_>,
    run: &mut Run,
    options: &SaveOptions<'_>,
    hooks: &dyn Hooks,
    cancel: &mut Pin<&mut impl Future<Output = ()>>,
) -> Result<Saved, ChunkError> {
    let Open {
        target: Target { dir, name, size },
        file,
        partial,
    } = *open;
    let from = run.hasher.position();
    if run.resumed {
        ready(file, &run.hasher, size)?;
        if from > 0 {
            run.whole = Some(run.hasher.clone());
        }
    } else {
        hooks
            .created(file, partial)
            .map_err(|e| ChunkError::Hook(e.kind()))?;
    }
    let chunk = run.hasher.chunk();
    let (hasher, whole) = (&mut run.hasher, &mut run.whole);
    let mut writer = Placed::new(file, from)?;
    let receiving = async {
        let mut at = from;
        while at < size {
            // Each chunk at its own place. No write is under way here:
            // `copy_exact` flushes before it returns.
            writer.at = at;
            // One chunk at a time, so the caller hears of each.
            let end = (at / chunk + 1).saturating_mul(chunk).min(size);
            #[cfg(test)]
            if let Some(stop) = crate::seams::WRITE_ERROR_AT.get()
                && (at..end).contains(&stop)
            {
                crate::seams::WRITE_ERROR_AT.set(None);
                // The bytes up to `stop` are written, and the next write fails.
                let (progress, pace) = (options.progress, options.pace);
                copy_exact(reader, &mut writer, stop, at, hasher, progress, pace).await?;
                return Err(ChunkError::Io(io::ErrorKind::StorageFull));
            }
            copy_exact(
                reader,
                &mut writer,
                end,
                at,
                hasher,
                options.progress,
                options.pace,
            )
            .await
            .map_err(|e| match e {
                ChunkError::CutShort { got, .. } => ChunkError::CutShort { got, size },
                e => e,
            })?;
            at = end;
            if at.is_multiple_of(chunk) {
                *whole = Some(hasher.clone());
                hooks
                    .whole(file, at, hasher.clone().finish())
                    .map_err(|e| ChunkError::Hook(e.kind()))?;
            }
        }
        let trailer = read_trailer(reader).await?;
        read_end(reader).await?;
        Ok(trailer)
    };
    let trailer = tokio::select! {
        biased;
        () = cancel.as_mut() => Err(ChunkError::Cancelled),
        trailer = receiving => trailer,
    };
    // Every write is done, and the writer's gate shut and its handle
    // closed, before the file is used for anything else: no write of
    // this save runs or starts after a cut-back, a hand-back or an end.
    writer.settle().await;
    drop(writer);
    let trailer = trailer?;
    let digest = hasher.clone().finish();
    if digest != trailer {
        return Err(ChunkError::Damaged);
    }
    run.verified = true;
    if options.sync {
        sync_data(file, cancel).await?;
    }
    hooks
        .before_publish(file, partial)
        .map_err(|e| ChunkError::Hook(e.kind()))?;
    #[cfg(test)]
    if crate::seams::PUBLISH_ERROR.replace(false) {
        return Err(ChunkError::Io(io::ErrorKind::PermissionDenied));
    }
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

/// Writes a file at offsets it keeps itself, never at the handle's
/// position: whatever moves that position, through the handle a hook was
/// given or a clone of it on any thread, moves no byte. Each write runs on
/// the blocking pool; its error comes with the next write or the flush.
///
/// A write handed to the pool outlives the future that asked for it, so
/// the writer and its writes share a gate: dropped, the writer waits for
/// the write that holds it and shuts every later one out.
pub(crate) struct Placed {
    gate: Arc<Gate>,
    /// Where the next byte goes.
    at: u64,
    writing: Option<JoinHandle<io::Result<()>>>,
    #[cfg(test)]
    seam: Option<crate::seams::Writes>,
}

/// The handle a writer's writes go through: held for the length of each
/// write, and empty once the writer is gone.
struct Gate(Mutex<Option<File>>);

impl Gate {
    fn lock(&self) -> MutexGuard<'_, Option<File>> {
        // A write that panicked left the file as whole as it will be.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Placed {
    pub(crate) fn new(file: &File, at: u64) -> io::Result<Self> {
        Ok(Self {
            gate: Arc::new(Gate(Mutex::new(Some(file.try_clone()?)))),
            at,
            writing: None,
            #[cfg(test)]
            seam: crate::seams::WRITES.take(),
        })
    }

    /// The write under way, ended: its error, when it had one.
    fn poll_done(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Some(writing) = &mut self.writing else {
            return Poll::Ready(Ok(()));
        };
        let done = ready!(Pin::new(writing).poll(cx));
        self.writing = None;
        Poll::Ready(done.unwrap_or_else(|_| Err(io::ErrorKind::Other.into())))
    }

    /// Waits for the write under way, whatever became of it: for a save
    /// that has its error already.
    async fn settle(&mut self) {
        let _ = std::future::poll_fn(|cx| self.poll_done(cx)).await;
    }
}

impl Drop for Placed {
    fn drop(&mut self) {
        #[cfg(test)]
        if let Some(seam) = &self.seam {
            seam(crate::seams::Step::Closing);
        }
        // Waits for the write that holds the gate, one slice, as long
        // as the disk takes. A write that has not started finds no file.
        self.gate.lock().take();
    }
}

impl AsyncWrite for Placed {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        ready!(this.poll_done(cx))?;
        let (gate, at, owned) = (this.gate.clone(), this.at, bytes.to_vec());
        #[cfg(test)]
        let seam = this.seam.clone();
        this.writing = Some(tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            let step = |step| {
                if let Some(seam) = &seam {
                    seam(step);
                }
            };
            #[cfg(test)]
            step(crate::seams::Step::Queued);
            let written = match &*gate.lock() {
                Some(file) => {
                    #[cfg(test)]
                    step(crate::seams::Step::Writing);
                    write_at(file, &owned, at)
                }
                // The writer is gone, and its save with it.
                None => Ok(()),
            };
            #[cfg(test)]
            step(crate::seams::Step::Ended);
            written
        }));
        this.at += bytes.len() as u64;
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_done(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_done(cx)
    }
}

/// Writes all of `bytes` at the offset `at` of `file`.
fn write_at(file: &File, bytes: &[u8], at: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.write_all_at(bytes, at)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let (mut bytes, mut at) = (bytes, at);
        while !bytes.is_empty() {
            match file.seek_write(bytes, at) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    bytes = &bytes[n..];
                    at += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}
