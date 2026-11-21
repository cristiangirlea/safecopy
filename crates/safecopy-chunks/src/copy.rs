//! The bytes on the stream: the framing version, the caller's metadata
//! block, the file's bytes and the trailer.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{ChunkError, DIGEST_LEN, Digest, Hasher, Pace, Progress, SLICE, Settings};

/// The framing version this crate writes and reads: the first byte of the
/// framing.
pub const FRAMING: u8 = 1;

/// Writes the framing version.
pub async fn write_version<W: AsyncWrite + Unpin + ?Sized>(
    writer: &mut W,
) -> Result<(), ChunkError> {
    writer.write_all(&[FRAMING]).await?;
    Ok(())
}

/// Reads the framing version: [`ChunkError::Version`] for any other.
pub async fn read_version<R: AsyncRead + Unpin + ?Sized>(reader: &mut R) -> Result<(), ChunkError> {
    let mut version = [0];
    read_full(reader, &mut version).await?;
    match version[0] {
        FRAMING => Ok(()),
        got => Err(ChunkError::Version { got }),
    }
}

/// Writes the caller's metadata block: its length (4 bytes, little-endian)
/// and its bytes, which this crate does not read. Refused past
/// [`Settings::meta_bytes`], as the reader would refuse it.
pub async fn write_meta<W: AsyncWrite + Unpin + ?Sized>(
    writer: &mut W,
    meta: &[u8],
    settings: &Settings,
) -> Result<(), ChunkError> {
    let most = u64::from(settings.meta_bytes);
    let len = u32::try_from(meta.len())
        .ok()
        .filter(|len| *len <= settings.meta_bytes)
        .ok_or(ChunkError::MetaTooLong {
            len: meta.len() as u64,
            most,
        })?;
    writer.write_all(&len.to_le_bytes()).await?;
    writer.write_all(meta).await?;
    Ok(())
}

/// Reads the caller's metadata block, refusing one longer than
/// [`Settings::meta_bytes`] before reading any of it.
pub async fn read_meta<R: AsyncRead + Unpin + ?Sized>(
    reader: &mut R,
    settings: &Settings,
) -> Result<Vec<u8>, ChunkError> {
    let mut len = [0; 4];
    read_full(reader, &mut len).await?;
    let len = u32::from_le_bytes(len);
    if len > settings.meta_bytes {
        return Err(ChunkError::MetaTooLong {
            len: len.into(),
            most: settings.meta_bytes.into(),
        });
    }
    let mut meta = vec![0; len as usize];
    read_full(reader, &mut meta).await?;
    Ok(meta)
}

/// Fills `buffer`, or says how much of it came.
async fn read_full<R: AsyncRead + Unpin + ?Sized>(
    reader: &mut R,
    buffer: &mut [u8],
) -> Result<(), ChunkError> {
    let mut got = 0;
    while got < buffer.len() {
        match reader.read(&mut buffer[got..]).await? {
            0 => {
                return Err(ChunkError::CutShort {
                    got: got as u64,
                    size: buffer.len() as u64,
                });
            }
            n => got += n,
        }
    }
    Ok(())
}

/// Copies the bytes of a file from `from` to `size`: reads them from
/// `reader`, writes them to `writer`, and hashes them into `hasher`, which
/// must stand at `from` (it has hashed the bytes before it).
///
/// Bytes go in slices of at most [`SLICE`]; before each, `pace` is awaited
/// and nothing is written until it returns. `progress` hears of each slice
/// written. A stream that ends early is [`ChunkError::CutShort`], with the
/// bytes of the file that came; what came is written and hashed. Nothing is
/// read past `size`. The writer is flushed at the end.
pub async fn copy_exact<R, W>(
    reader: &mut R,
    writer: &mut W,
    size: u64,
    from: u64,
    hasher: &mut Hasher,
    progress: &dyn Progress,
    pace: &dyn Pace,
) -> Result<(), ChunkError>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    if from > size || hasher.position() != from {
        return Err(ChunkError::Offset {
            from,
            size,
            hashed: hasher.position(),
        });
    }
    let mut buffer = vec![0; SLICE];
    let mut at = from;
    while at < size {
        let want = usize::try_from(size - at).map_or(SLICE, |left| left.min(SLICE));
        let n = reader.read(&mut buffer[..want]).await?;
        if n == 0 {
            writer.flush().await?;
            return Err(ChunkError::CutShort { got: at, size });
        }
        pace.before_write(n).await;
        writer.write_all(&buffer[..n]).await?;
        hasher.update(&buffer[..n]);
        at += n as u64;
        progress.advanced(n as u64);
    }
    writer.flush().await?;
    Ok(())
}

/// The hash of the first `have` bytes `reader` gives: a prefix read again
/// from the disk when its [`Hasher`] no longer keeps it.
pub async fn hash_prefix<R: AsyncRead + Unpin + ?Sized>(
    reader: &mut R,
    have: u64,
) -> Result<Digest, ChunkError> {
    let mut hasher = Hasher::new(0);
    hasher.read(reader, have).await?;
    Ok(hasher.finish())
}

impl Hasher {
    /// Hashes the next `len` bytes `reader` gives, writing them nowhere: a
    /// prefix read again from the disk, so a copy can go on from there.
    /// [`ChunkError::CutShort`] says how many of the `len` came.
    pub async fn read<R: AsyncRead + Unpin + ?Sized>(
        &mut self,
        reader: &mut R,
        len: u64,
    ) -> Result<(), ChunkError> {
        let mut buffer = vec![0; SLICE];
        let mut at = 0;
        while at < len {
            let want = usize::try_from(len - at).map_or(SLICE, |left| left.min(SLICE));
            let n = reader.read(&mut buffer[..want]).await?;
            if n == 0 {
                return Err(ChunkError::CutShort { got: at, size: len });
            }
            self.update(&buffer[..n]);
            at += n as u64;
        }
        Ok(())
    }
}

/// Writes the trailer: the 32-byte hash of the whole file, after its last
/// byte.
pub async fn write_trailer<W: AsyncWrite + Unpin + ?Sized>(
    writer: &mut W,
    digest: &Digest,
) -> Result<(), ChunkError> {
    writer.write_all(&digest.0).await?;
    writer.flush().await?;
    Ok(())
}

/// Reads the trailer: [`ChunkError::TrailerCutShort`] with the bytes of
/// it that came when the stream ends first.
pub async fn read_trailer<R: AsyncRead + Unpin + ?Sized>(
    reader: &mut R,
) -> Result<Digest, ChunkError> {
    let mut digest = [0; DIGEST_LEN];
    read_full(reader, &mut digest).await.map_err(|e| match e {
        ChunkError::CutShort { got, .. } => ChunkError::TrailerCutShort { got },
        e => e,
    })?;
    Ok(Digest(digest))
}

/// Checks the stream ends here: [`ChunkError::TooLong`] when any byte
/// follows. Reads at most one byte.
pub async fn read_end<R: AsyncRead + Unpin + ?Sized>(reader: &mut R) -> Result<(), ChunkError> {
    let mut more = [0];
    match reader.read(&mut more).await? {
        0 => Ok(()),
        _ => Err(ChunkError::TooLong),
    }
}
