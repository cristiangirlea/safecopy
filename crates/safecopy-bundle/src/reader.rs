//! Reading a bundle: the whole table first, then every file in its order.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::LazyLock;

use safecopy_chunks::{DIGEST_LEN, Digest, Hasher, Pace, Progress, SLICE, Saved, read_version};
use safecopy_folder::{Directory, Finish, Meta, MetaRules, PublishError, PublishRules};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::placed::Placed;
use crate::writer::chunk_error;
use crate::{BundleError, BundleLimits, GIVEN_UP, Outcome, TableEntry, WHOLE, wire};

/// Opens bundles.
#[derive(Debug, Clone, Copy)]
pub struct BundleReader;

impl BundleReader {
    /// Reads the framing version and the whole table from `reader`, and
    /// checks it against `limits`, before any byte of any file: a table
    /// longer than [`BundleLimits::table_bytes`] is refused from its
    /// length, before it is read. Pass `&mut stream` to keep the stream
    /// after a refusal.
    pub async fn open<R: AsyncRead + Unpin>(
        mut reader: R,
        limits: &BundleLimits,
    ) -> Result<Table<R>, BundleError> {
        read_version(&mut reader).await.map_err(chunk_error)?;
        let mut len = [0; 4];
        read_table(&mut reader, &mut len).await?;
        let len = u32::from_le_bytes(len);
        if len > limits.table_bytes {
            return Err(BundleError::TableTooLong {
                len: len.into(),
                most: limits.table_bytes.into(),
            });
        }
        let mut table = vec![0; len as usize];
        read_table(&mut reader, &mut table).await?;
        let entries = wire::decode(&table, limits)?;
        Ok(Table { reader, entries })
    }
}

async fn read_table<R: AsyncRead + Unpin>(
    reader: &mut R,
    buffer: &mut [u8],
) -> Result<(), BundleError> {
    match reader.read_exact(buffer).await {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(BundleError::TableCutShort),
        Err(e) => Err(e.into()),
    }
}

/// Where a file taken from a bundle is saved.
#[derive(Debug)]
pub struct Target<'a> {
    /// The held folder it goes in.
    pub dir: &'a Directory,
    /// The name asked for; a numbered one when it is taken.
    pub name: String,
    /// The sender's modified time and execute bit, set as
    /// [`ReadOptions::meta_rules`] allow.
    pub meta: Meta,
}

/// The receiver's decision on one entry of the table.
#[derive(Debug)]
pub enum Take<'a> {
    /// Save it.
    Save(Target<'a>),
    /// Save it as [`Self::Save`] does, but when its hash equals this one (a
    /// candidate's at the destination, already identical) remove its
    /// partial file and report [`Outcome::AlreadyThere`].
    SaveUnlessSame(Target<'a>, Digest),
    /// Create nothing; read its bytes and throw them away
    /// ([`Outcome::Refused`]).
    Skip,
}

/// A partial file created for an entry, as [`BundleHooks::admitted`] sees it.
#[derive(Debug, Clone, Copy)]
pub struct Partial<'a> {
    /// The entry's place in the table.
    pub entry: usize,
    /// The folder it is in.
    pub dir: &'a Directory,
    /// The file.
    pub file: &'a File,
    /// Its name there.
    pub name: &'a OsStr,
}

/// The caller's steps around [`Table::read`], each with a default that
/// does nothing.
pub trait BundleHooks: Send + Sync {
    /// Before any partial file is created, with the entries taken: the last
    /// moment to refuse with nothing on the disk (room, an intake place).
    /// An error ends the bundle with [`BundleError::Hook`].
    fn before_create(&self, taken: &[usize]) -> io::Result<()> {
        let _ = taken;
        Ok(())
    }

    /// Every taken entry's partial file was created, before any byte of
    /// any file is read: journal them in one write. An error removes them
    /// all and ends the bundle with [`BundleError::Hook`].
    fn admitted(&self, partials: &[Partial<'_>]) -> io::Result<()> {
        let _ = partials;
        Ok(())
    }

    /// The entry's hash matched; it is not yet marked and has no name. An
    /// error removes it ([`Outcome::Failed`]) and the bundle reads on.
    fn before_publish(&self, entry: usize, file: &File, partial: &OsStr) -> io::Result<()> {
        let _ = (entry, file, partial);
        Ok(())
    }

    /// The bundle ended: every entry's outcome, settled in one entry.
    /// Called once, whatever happened, after every partial file not
    /// published was removed (left only when the system refused, for the
    /// caller's crash cleanup to find).
    fn ended(&self, report: &Report) {
        let _ = report;
    }
}

/// No hooks.
impl BundleHooks for () {}

/// How [`Table::read`] saves.
#[derive(Clone, Copy)]
pub struct ReadOptions<'a> {
    /// The downloaded-file mark and the publish's retries.
    pub publish: &'a PublishRules,
    /// Which of each file's time and execute bit are set.
    pub meta_rules: MetaRules,
    /// Hears of every slice written to a partial file.
    pub progress: &'a dyn Progress,
    /// Awaited before every slice written to a partial file.
    pub pace: &'a dyn Pace,
    /// Flush each file's bytes to the disk before it gets its name. On by
    /// default.
    pub sync: bool,
}

static DEFAULT_PUBLISH: LazyLock<PublishRules> = LazyLock::new(PublishRules::default);

impl Default for ReadOptions<'_> {
    fn default() -> Self {
        Self {
            publish: &DEFAULT_PUBLISH,
            meta_rules: MetaRules::default(),
            progress: &(),
            pace: &(),
            sync: true,
        }
    }
}

/// What [`Table::read`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// One per entry, in the table's order.
    pub outcomes: Vec<Outcome>,
    /// Why the bundle ended early, when it did: every taken entry not
    /// settled before it is [`Outcome::NotReceived`]; a skipped one stays
    /// [`Outcome::Refused`], and one whose partial file could not be
    /// created [`Outcome::Failed`].
    pub error: Option<BundleError>,
}

/// A bundle's table, read and checked; its files not read yet.
#[derive(Debug)]
pub struct Table<R> {
    reader: R,
    entries: Vec<TableEntry>,
}

/// An entry as the read goes.
enum Slot<'a> {
    Skip,
    Failed(io::ErrorKind),
    Open(Open<'a>),
}

/// A taken entry's partial file. Removed when dropped unless published,
/// on every path: an error, a cancel, or the caller dropping the read.
struct Open<'a> {
    target: Target<'a>,
    unless: Option<Digest>,
    file: File,
    partial: OsString,
    published: bool,
}

impl Drop for Open<'_> {
    fn drop(&mut self) {
        if !self.published {
            // One the system refuses to remove is left for the caller's
            // crash cleanup.
            let _ = self.target.dir.discard(&self.file, &self.partial);
        }
    }
}

impl<R: AsyncRead + Unpin> Table<R> {
    /// The table's entries, in order.
    pub fn entries(&self) -> &[TableEntry] {
        &self.entries
    }

    /// The stream, for its transport: to stop it with a code after a
    /// refusal, say.
    pub fn into_inner(self) -> R {
        self.reader
    }

    /// Reads the bundle's files with one decision per entry, in order.
    ///
    /// First, admission in one batch: [`BundleHooks::before_create`], every
    /// taken entry's partial file created new (`name.part`), and
    /// [`BundleHooks::admitted`], before any byte is read. Then each entry:
    /// exactly its size in bytes, written to its partial file or thrown
    /// away, its mark and its hash. A whole file whose hash matches is
    /// flushed ([`ReadOptions::sync`]), passed to
    /// [`BundleHooks::before_publish`] and published under its name, or a
    /// numbered one, never replacing a file; with
    /// [`Take::SaveUnlessSame`] and an equal hash it is removed instead.
    /// Any other entry's partial file is removed, and the bundle reads on.
    ///
    /// The stream ending early, a bad mark, a byte after the last hash (read
    /// before the last file is published), a stream error or `cancel`
    /// end the bundle: the files published stay, every partial file not
    /// published is removed, and the rest taken are
    /// [`Outcome::NotReceived`] (see [`Report::error`]).
    /// [`BundleHooks::ended`] hears the report.
    ///
    /// `cancel` is watched while bytes come, while [`ReadOptions::pace`]
    /// holds a write, and while a file is flushed and published. Dropping
    /// this future instead also removes every partial file not published,
    /// but [`BundleHooks::ended`] is not called.
    ///
    /// No write is left behind. Each slice is written on the blocking
    /// pool, at its own offset in the file, behind a gate its entry's
    /// writer shares with it. A bundle that ends early waits for the
    /// write under way before it removes the partial file, and a dropped
    /// read shuts the gate, which waits for that write too: one slice
    /// ([`safecopy_chunks::SLICE`]) at most, and on a disk that stalls,
    /// as long as it stalls. A write that had not started writes nothing.
    /// So when this returns or is dropped, nothing of it is still being
    /// written and no handle of a partial file is still open: on Windows,
    /// where a file's name goes only with its last handle, a removed
    /// partial file is gone from its folder by then. A flush
    /// ([`ReadOptions::sync`], and the flush of a copy a publish made) is
    /// not behind the gate: one still on the pool when `cancel` came or
    /// the read was dropped ends there, and holds its file open until it
    /// has.
    pub async fn read(
        mut self,
        decisions: Vec<Take<'_>>,
        options: &ReadOptions<'_>,
        hooks: &dyn BundleHooks,
        cancel: impl Future<Output = ()>,
    ) -> Report {
        let report = self.read_all(decisions, options, hooks, cancel).await;
        hooks.ended(&report);
        report
    }

    async fn read_all(
        &mut self,
        decisions: Vec<Take<'_>>,
        options: &ReadOptions<'_>,
        hooks: &dyn BundleHooks,
        cancel: impl Future<Output = ()>,
    ) -> Report {
        tokio::pin!(cancel);
        let count = self.entries.len();
        let mut outcomes = vec![Outcome::NotReceived; count];
        if decisions.len() != count {
            return Report {
                outcomes,
                error: Some(BundleError::Decisions {
                    got: decisions.len(),
                    entries: count,
                }),
            };
        }
        let taken: Vec<usize> = decisions
            .iter()
            .enumerate()
            .filter(|(_, take)| !matches!(take, Take::Skip))
            .map(|(entry, _)| entry)
            .collect();
        for (entry, take) in decisions.iter().enumerate() {
            if matches!(take, Take::Skip) {
                outcomes[entry] = Outcome::Refused;
            }
        }
        if let Err(e) = hooks.before_create(&taken) {
            return Report {
                outcomes,
                error: Some(BundleError::Hook(e.kind())),
            };
        }
        let mut slots: Vec<Slot<'_>> = decisions.into_iter().map(admit).collect();
        for (entry, slot) in slots.iter().enumerate() {
            if let Slot::Failed(kind) = slot {
                outcomes[entry] = Outcome::Failed(*kind);
            }
        }
        let partials: Vec<Partial<'_>> = slots
            .iter()
            .enumerate()
            .filter_map(|(entry, slot)| match slot {
                Slot::Open(open) => Some(Partial {
                    entry,
                    dir: open.target.dir,
                    file: &open.file,
                    name: &open.partial,
                }),
                _ => None,
            })
            .collect();
        let admitted = hooks.admitted(&partials);
        drop(partials);
        if let Err(e) = admitted {
            discard_from(&mut slots, 0);
            return Report {
                outcomes,
                error: Some(BundleError::Hook(e.kind())),
            };
        }
        let mut buffer = vec![0; SLICE];
        for entry in 0..count {
            let size = self.entries[entry].size;
            let last = entry + 1 == count;
            let slot = std::mem::replace(&mut slots[entry], Slot::Skip);
            let read = self
                .read_entry(entry, size, last, &slot, &mut buffer, options, &mut cancel)
                .await;
            let (mark, trailer, failed) = match read {
                Ok(read) => read,
                Err(e) => {
                    slots[entry] = slot;
                    discard_from(&mut slots, entry);
                    return Report {
                        outcomes,
                        error: Some(e),
                    };
                }
            };
            let outcome = match slot {
                Slot::Skip => Outcome::Refused,
                Slot::Failed(kind) => Outcome::Failed(kind),
                Slot::Open(open) => {
                    let settled = settle(
                        entry,
                        open,
                        mark,
                        trailer,
                        failed,
                        options,
                        hooks,
                        &mut cancel,
                    )
                    .await;
                    match settled {
                        Ok(outcome) => outcome,
                        Err(e) => {
                            discard_from(&mut slots, entry + 1);
                            return Report {
                                outcomes,
                                error: Some(e),
                            };
                        }
                    }
                }
            };
            outcomes[entry] = outcome;
        }
        if count == 0 {
            // No file to read before the end.
            let mut more = [0];
            let end = read_some(&mut self.reader, &mut more, &mut cancel).await;
            let error = match end {
                Ok(0) => None,
                Ok(_) => Some(BundleError::TooLong),
                Err(e) => Some(e),
            };
            return Report { outcomes, error };
        }
        Report {
            outcomes,
            error: None,
        }
    }

    /// Reads one entry: its bytes, to its partial file when it has one,
    /// then its mark and hash, and for the last, the end of the stream.
    /// Gives the mark, the hash of the bytes and of the trailer, and the
    /// kind of a failed write.
    #[allow(clippy::too_many_arguments)]
    async fn read_entry(
        &mut self,
        entry: usize,
        size: u64,
        last: bool,
        slot: &Slot<'_>,
        buffer: &mut [u8],
        options: &ReadOptions<'_>,
        cancel: &mut Pin<&mut impl Future<Output = ()>>,
    ) -> Result<(u8, Option<(Digest, Digest)>, Option<io::ErrorKind>), BundleError> {
        let index = u32::try_from(entry).expect("at most `files`");
        let cut = || BundleError::CutShort { entry: index };
        let mut failed = None;
        let mut writer = match slot {
            Slot::Open(open) => match Placed::new(&open.file) {
                Ok(writer) => Some(writer),
                Err(e) => {
                    failed = Some(e.kind());
                    None
                }
            },
            _ => None,
        };
        let mut hasher = Hasher::new(0);
        let received = async {
            let mut at = 0;
            while at < size {
                let want = usize::try_from(size - at).map_or(SLICE, |left| left.min(SLICE));
                let n = read_some(&mut self.reader, &mut buffer[..want], cancel).await?;
                if n == 0 {
                    return Err(cut());
                }
                if let Some(file) = &mut writer {
                    tokio::select! {
                        biased;
                        () = cancel.as_mut() => return Err(BundleError::Cancelled),
                        () = options.pace.before_write(n) => {}
                    }
                    match file.write(&buffer[..n]).await {
                        Ok(()) => options.progress.advanced(n as u64),
                        Err(e) => {
                            failed = Some(e.kind());
                            writer = None;
                        }
                    }
                    hasher.update(&buffer[..n]);
                }
                at += n as u64;
            }
            Ok(())
        }
        .await;
        // Every write is done and the writer's handle closed before the
        // partial file is used again or removed, however the bytes ended:
        // nothing of this read is written after it returns.
        if let Some(mut file) = writer
            && let Err(e) = file.settle().await
        {
            failed = Some(e.kind());
        }
        received?;
        let mut end = [0; 1 + DIGEST_LEN];
        let mut got = 0;
        while got < end.len() {
            match read_some(&mut self.reader, &mut end[got..], cancel).await? {
                0 => return Err(cut()),
                n => got += n,
            }
        }
        let mark = end[0];
        if mark != WHOLE && mark != GIVEN_UP {
            return Err(BundleError::BadMark { entry: index, mark });
        }
        if last {
            let mut more = [0];
            if read_some(&mut self.reader, &mut more, cancel).await? > 0 {
                return Err(BundleError::TooLong);
            }
        }
        let mut trailer = [0; DIGEST_LEN];
        trailer.copy_from_slice(&end[1..]);
        let hashes = matches!(slot, Slot::Open(_)).then(|| (hasher.finish(), Digest(trailer)));
        Ok((mark, hashes, failed))
    }
}

/// One read from the stream, or [`BundleError::Cancelled`].
async fn read_some<R: AsyncRead + Unpin>(
    reader: &mut R,
    buffer: &mut [u8],
    cancel: &mut Pin<&mut impl Future<Output = ()>>,
) -> Result<usize, BundleError> {
    tokio::select! {
        biased;
        () = cancel.as_mut() => Err(BundleError::Cancelled),
        n = reader.read(buffer) => Ok(n?),
    }
}

/// Creates a taken entry's partial file.
fn admit(take: Take<'_>) -> Slot<'_> {
    let (target, unless) = match take {
        Take::Skip => return Slot::Skip,
        Take::Save(target) => (target, None),
        Take::SaveUnlessSame(target, digest) => (target, Some(digest)),
    };
    match create(target.dir, &target.name) {
        Ok((file, partial)) => Slot::Open(Open {
            target,
            unless,
            file,
            partial,
            published: false,
        }),
        Err(e) => Slot::Failed(e.kind()),
    }
}

/// The partial file's name: `name.part`, or `name.N.part` when taken.
fn create(dir: &Directory, name: &str) -> io::Result<(File, OsString)> {
    for n in 1u64.. {
        let partial = OsString::from(if n == 1 {
            format!("{name}.part")
        } else {
            format!("{name}.{n}.part")
        });
        match dir.create(&partial) {
            Ok(file) => return Ok((file, partial)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!("the numbers never run out")
}

/// Removes the partial files of the entries from `first` on.
fn discard_from(slots: &mut [Slot<'_>], first: usize) {
    for slot in &mut slots[first..] {
        // Dropping an `Open` removes its partial file.
        *slot = Slot::Skip;
    }
}

/// Settles a taken entry read whole: published, already there, or removed.
/// A cancel ends the bundle, the entry's partial file removed.
#[allow(clippy::too_many_arguments)]
async fn settle(
    entry: usize,
    mut open: Open<'_>,
    mark: u8,
    hashes: Option<(Digest, Digest)>,
    failed: Option<io::ErrorKind>,
    options: &ReadOptions<'_>,
    hooks: &dyn BundleHooks,
    cancel: &mut Pin<&mut impl Future<Output = ()>>,
) -> Result<Outcome, BundleError> {
    let (digest, trailer) = hashes.expect("an open entry is hashed");
    let settled = if let Some(kind) = failed {
        Ok(Outcome::Failed(kind))
    } else if mark == GIVEN_UP {
        Ok(Outcome::GivenUp)
    } else if digest != trailer {
        Ok(Outcome::Damaged)
    } else if open.unless == Some(digest) {
        Ok(Outcome::AlreadyThere)
    } else {
        match publish(
            entry,
            &open.target,
            &open.file,
            &open.partial,
            digest,
            options,
            hooks,
            cancel,
        )
        .await
        {
            Ok(saved) => {
                open.published = true;
                Ok(Outcome::Saved(saved))
            }
            Err(e) => e,
        }
    };
    // Not published: dropping `open` removes the partial file, by its handle.
    drop(open);
    settled
}

/// Flushes, asks the hook and publishes. `Ok(Err(..))` fails this entry;
/// `Err` ends the bundle.
#[allow(clippy::too_many_arguments)]
async fn publish(
    entry: usize,
    target: &Target<'_>,
    file: &File,
    partial: &OsStr,
    digest: Digest,
    options: &ReadOptions<'_>,
    hooks: &dyn BundleHooks,
    cancel: &mut Pin<&mut impl Future<Output = ()>>,
) -> Result<Saved, Result<Outcome, BundleError>> {
    let failed = |kind| Err(Ok(Outcome::Failed(kind)));
    if options.sync {
        let synced = tokio::select! {
            biased;
            () = cancel.as_mut() => return Err(Err(BundleError::Cancelled)),
            synced = sync_data(file) => synced,
        };
        if let Err(kind) = synced {
            return failed(kind);
        }
    }
    if let Err(e) = hooks.before_publish(entry, file, partial) {
        return failed(e.kind());
    }
    let finish = Finish {
        meta: target.meta,
        meta_rules: options.meta_rules,
        sync: options.sync,
    };
    let published = target
        .dir
        .publish_with(
            file,
            partial,
            &target.name,
            options.publish,
            &finish,
            cancel.as_mut(),
        )
        .await;
    match published {
        Ok((published, applied)) => Ok(Saved {
            path: published.path,
            marked: published.marked,
            applied,
            digest,
        }),
        Err(PublishError::Cancelled) => Err(Err(BundleError::Cancelled)),
        Err(PublishError::Io(e) | PublishError::Name { error: e, .. }) => failed(e.kind()),
        Err(_) => failed(io::ErrorKind::Other),
    }
}

/// Flushes `file`'s bytes to the disk on the blocking pool.
async fn sync_data(file: &File) -> Result<(), io::ErrorKind> {
    let handle = file.try_clone().map_err(|e| e.kind())?;
    match tokio::task::spawn_blocking(move || handle.sync_data()).await {
        Ok(synced) => synced.map_err(|e| e.kind()),
        Err(_) => Err(io::ErrorKind::Other),
    }
}
