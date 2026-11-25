//! Writing a bundle.

use safecopy_chunks::{DIGEST_LEN, Digest, Hasher, Pace, Progress, SLICE, write_version};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::wire::{self, COUNT_LEN};
use crate::{BundleError, BundleLimits, GIVEN_UP, TableEntry, WHOLE};

/// How [`BundleWriter`] writes.
#[derive(Clone, Copy)]
pub struct WriteOptions<'a> {
    /// Hears of every slice of a file written.
    pub progress: &'a dyn Progress,
    /// Awaited before every slice of a file written, padding included.
    pub pace: &'a dyn Pace,
}

impl Default for WriteOptions<'_> {
    fn default() -> Self {
        Self {
            progress: &(),
            pace: &(),
        }
    }
}

/// What [`BundleWriter::write`] sent of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    /// The whole file, with its hash.
    Whole(Digest),
    /// Given up: the source ended early, failed, or changed; its place was
    /// padded.
    GivenUp,
}

/// Writes one bundle: the table on [`Self::new`], then [`Self::write`] or
/// [`Self::give_up`] for each entry in the table's order, then
/// [`Self::finish`].
///
/// A sender opens every file before it makes the table, so it knows each
/// size; a file that then changes or cannot be read is given up with the
/// mark, never resized, so the framing stays whole. A write that fails
/// on the stream, or whose future is dropped, leaves its entry torn: every
/// later call then fails with [`BundleError::Torn`] and nothing more is
/// written, so a receiver sees the stream end short, never misframed.
pub struct BundleWriter<'a, W> {
    writer: W,
    sizes: Vec<u64>,
    next: usize,
    /// An entry was started and not ended.
    torn: bool,
    options: WriteOptions<'a>,
    buffer: Vec<u8>,
}

impl<'a, W: AsyncWrite + Unpin> BundleWriter<'a, W> {
    /// Writes the framing version and the table of the longest run of
    /// `entries`, from the first, that keeps within `limits`: at most
    /// [`BundleLimits::files`] entries, [`BundleLimits::bytes`] bytes and
    /// [`BundleLimits::table_bytes`] of table. [`Self::entries`] says how
    /// many it took; the rest go in another bundle.
    ///
    /// Refused, with nothing written, when an entry it reaches is larger
    /// than [`BundleLimits::small`] ([`BundleError::TooLarge`]), has a
    /// metadata block past [`BundleLimits::meta_bytes`]
    /// ([`BundleError::MetaTooLong`]), or is the first and alone would pass
    /// the table's bound ([`BundleError::TableTooLong`]). No entries make
    /// an empty bundle.
    pub async fn new(
        mut writer: W,
        entries: &[TableEntry],
        limits: &BundleLimits,
        options: WriteOptions<'a>,
    ) -> Result<Self, BundleError> {
        let most = u64::from(limits.table_bytes);
        let mut table_len = COUNT_LEN;
        let mut bytes = 0;
        let mut taken = 0;
        for entry in entries {
            if taken == limits.files as usize {
                break;
            }
            let index = u32::try_from(taken).expect("at most `files`");
            let total = match wire::check_entry(
                index,
                entry.size,
                entry.meta.len() as u64,
                bytes,
                limits,
            ) {
                Ok(total) => total,
                // A small file past the bundle's bytes starts the next.
                Err(BundleError::TooManyBytes { .. }) if taken > 0 => break,
                Err(e) => return Err(e),
            };
            let len = table_len + wire::entry_len(entry);
            if len > most {
                if taken == 0 {
                    return Err(BundleError::TableTooLong { len, most });
                }
                break;
            }
            table_len = len;
            bytes = total;
            taken += 1;
        }
        let entries = &entries[..taken];
        let table = wire::encode(entries);
        debug_assert_eq!(table.len() as u64, table_len);
        write_version(&mut writer).await.map_err(chunk_error)?;
        let len = u32::try_from(table.len()).expect("at most `table_bytes`");
        writer.write_all(&len.to_le_bytes()).await?;
        writer.write_all(&table).await?;
        Ok(Self {
            writer,
            sizes: entries.iter().map(|e| e.size).collect(),
            next: 0,
            torn: false,
            options,
            buffer: vec![0; SLICE],
        })
    }

    /// How many entries the table holds.
    pub fn entries(&self) -> usize {
        self.sizes.len()
    }

    /// How many entries are still to write.
    pub fn left(&self) -> usize {
        self.sizes.len() - self.next
    }

    /// Writes the next entry's bytes from `source`: exactly its size, then
    /// the mark and the hash. A source that ends early or fails gives the
    /// file up ([`Written::GivenUp`]); one with more bytes than the size is
    /// not read past it (check it with [`Self::write_checked`]).
    pub async fn write<R: AsyncRead + Unpin + ?Sized>(
        &mut self,
        source: &mut R,
    ) -> Result<Written, BundleError> {
        self.write_checked(source, || true).await
    }

    /// [`Self::write`], asking `unchanged` after the last byte was read and
    /// before the mark: `false` (the source changed while it was read)
    /// gives the file up.
    pub async fn write_checked<R: AsyncRead + Unpin + ?Sized>(
        &mut self,
        source: &mut R,
        unchanged: impl FnOnce() -> bool,
    ) -> Result<Written, BundleError> {
        let size = self.take_next()?;
        let mut hasher = Hasher::new(0);
        let mut at = 0;
        let mut whole = true;
        while at < size {
            let want = usize::try_from(size - at).map_or(SLICE, |left| left.min(SLICE));
            let n = match source.read(&mut self.buffer[..want]).await {
                Ok(n) if n > 0 => n,
                // The source ended or failed: the rest is padding.
                _ => {
                    whole = false;
                    break;
                }
            };
            self.send(n).await?;
            hasher.update(&self.buffer[..n]);
            at += n as u64;
        }
        if whole && unchanged() {
            let digest = hasher.finish();
            self.end(WHOLE, &digest.0).await?;
            Ok(Written::Whole(digest))
        } else {
            self.pad(size - at).await?;
            self.end(GIVEN_UP, &[0; DIGEST_LEN]).await?;
            Ok(Written::GivenUp)
        }
    }

    /// Gives up the next entry: its size in zeros, the mark, zeros for the
    /// hash. The receiver throws them away.
    pub async fn give_up(&mut self) -> Result<(), BundleError> {
        let size = self.take_next()?;
        self.pad(size).await?;
        self.end(GIVEN_UP, &[0; DIGEST_LEN]).await
    }

    /// Gives up every entry not yet written, flushes and ends the stream
    /// (its shutdown); gives the stream back.
    pub async fn finish(mut self) -> Result<W, BundleError> {
        if self.torn {
            return Err(BundleError::Torn);
        }
        while self.left() > 0 {
            self.give_up().await?;
        }
        self.writer.flush().await?;
        self.writer.shutdown().await?;
        Ok(self.writer)
    }

    fn take_next(&mut self) -> Result<u64, BundleError> {
        if self.torn {
            return Err(BundleError::Torn);
        }
        let size = *self.sizes.get(self.next).ok_or(BundleError::NoEntryLeft)?;
        self.next += 1;
        self.torn = true;
        Ok(size)
    }

    /// Writes the first `n` bytes of the buffer, after the pace.
    async fn send(&mut self, n: usize) -> Result<(), BundleError> {
        self.options.pace.before_write(n).await;
        self.writer.write_all(&self.buffer[..n]).await?;
        self.options.progress.advanced(n as u64);
        Ok(())
    }

    async fn pad(&mut self, mut len: u64) -> Result<(), BundleError> {
        self.buffer.fill(0);
        while len > 0 {
            let n = usize::try_from(len).map_or(SLICE, |left| left.min(SLICE));
            self.send(n).await?;
            len -= n as u64;
        }
        Ok(())
    }

    async fn end(&mut self, mark: u8, hash: &[u8; DIGEST_LEN]) -> Result<(), BundleError> {
        self.writer.write_all(&[mark]).await?;
        self.writer.write_all(hash).await?;
        self.torn = false;
        Ok(())
    }
}

/// The errors [`write_version`] gives are the stream's.
pub(crate) fn chunk_error(e: safecopy_chunks::ChunkError) -> BundleError {
    match e {
        safecopy_chunks::ChunkError::Io(kind) => BundleError::Io(kind),
        safecopy_chunks::ChunkError::Folder(why) => BundleError::Folder(why),
        safecopy_chunks::ChunkError::Version { got } => BundleError::Version { got },
        safecopy_chunks::ChunkError::CutShort { .. } => BundleError::TableCutShort,
        safecopy_chunks::ChunkError::Cancelled => BundleError::Cancelled,
        _ => BundleError::Io(std::io::ErrorKind::Other),
    }
}
