//! The bytes of the wire types: the `postcard` encoding of their values,
//! written out by hand so a dependency's update never changes them.
//! Arrays go as their bytes, numbers as unsigned LEB128 varints, an enum
//! as its variant's number then its fields.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use safecopy_chunks::{DIGEST_LEN, Digest};

use crate::{Held, Resumable, ResumeError, TransferKey, Verdict};

pub(crate) fn varint(out: &mut Vec<u8>, mut n: u64) {
    loop {
        let low = (n & 0x7F) as u8;
        n >>= 7;
        if n == 0 {
            out.push(low);
            return;
        }
        out.push(low | 0x80);
    }
}

/// Bytes being read: each read fails with [`ResumeError::Damaged`] past
/// the end.
pub(crate) struct Slice<'a>(pub(crate) &'a [u8]);

impl Slice<'_> {
    pub(crate) fn byte(&mut self) -> Result<u8, ResumeError> {
        let (&b, rest) = self.0.split_first().ok_or(ResumeError::Damaged)?;
        self.0 = rest;
        Ok(b)
    }

    pub(crate) fn take(&mut self, len: usize) -> Result<&[u8], ResumeError> {
        if len > self.0.len() {
            return Err(ResumeError::Damaged);
        }
        let (taken, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(taken)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], ResumeError> {
        Ok(self.take(N)?.try_into().expect("N bytes"))
    }

    /// A varint of a `bits`-wide number, read as `postcard` reads one.
    pub(crate) fn varint(&mut self, bits: u32) -> Result<u64, ResumeError> {
        let mut decoder = Varint::new(bits);
        loop {
            if let Some(n) = decoder.push(self.byte()?)? {
                return Ok(n);
            }
        }
    }

    pub(crate) fn u64(&mut self) -> Result<u64, ResumeError> {
        self.varint(u64::BITS)
    }

    pub(crate) fn bytes(&mut self) -> Result<Vec<u8>, ResumeError> {
        let len = usize::try_from(self.varint(u32::BITS)?).map_err(|_| ResumeError::Damaged)?;
        Ok(self.take(len)?.to_vec())
    }

    pub(crate) fn end(&self) -> Result<(), ResumeError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(ResumeError::Damaged)
        }
    }
}

/// A varint read a byte at a time: at most as many bytes as the width
/// needs, the last of them within the bits left.
struct Varint {
    bits: u32,
    n: u64,
    i: u32,
}

impl Varint {
    fn new(bits: u32) -> Self {
        Self { bits, n: 0, i: 0 }
    }

    fn push(&mut self, byte: u8) -> Result<Option<u64>, ResumeError> {
        let longest = self.bits.div_ceil(7);
        if self.i >= longest {
            return Err(ResumeError::Damaged);
        }
        self.n |= u64::from(byte & 0x7F) << (7 * self.i);
        self.i += 1;
        if byte & 0x80 != 0 {
            return if self.i == longest {
                Err(ResumeError::Damaged)
            } else {
                Ok(None)
            };
        }
        let last_most = (1u16 << (self.bits - 7 * (longest - 1))) - 1;
        if self.i == longest && u16::from(byte) > last_most {
            return Err(ResumeError::Damaged);
        }
        Ok(Some(self.n))
    }
}

/// Reads exactly `buffer.len()` bytes; the stream ending first is
/// [`ResumeError::Damaged`].
pub(crate) async fn read_exact<R: AsyncRead + Unpin + ?Sized>(
    reader: &mut R,
    buffer: &mut [u8],
) -> Result<(), ResumeError> {
    let mut got = 0;
    while got < buffer.len() {
        match reader.read(&mut buffer[got..]).await? {
            0 => return Err(ResumeError::Damaged),
            n => got += n,
        }
    }
    Ok(())
}

pub(crate) async fn read_varint<R: AsyncRead + Unpin + ?Sized>(
    reader: &mut R,
    bits: u32,
) -> Result<u64, ResumeError> {
    let mut decoder = Varint::new(bits);
    loop {
        let mut byte = [0];
        read_exact(reader, &mut byte).await?;
        if let Some(n) = decoder.push(byte[0])? {
            return Ok(n);
        }
    }
}

pub(crate) async fn read_array<R: AsyncRead + Unpin + ?Sized, const N: usize>(
    reader: &mut R,
) -> Result<[u8; N], ResumeError> {
    let mut out = [0; N];
    read_exact(reader, &mut out).await?;
    Ok(out)
}

pub(crate) async fn write<W: AsyncWrite + Unpin + ?Sized>(
    writer: &mut W,
    bytes: &[u8],
) -> Result<(), ResumeError> {
    writer.write_all(bytes).await?;
    Ok(())
}

impl TransferKey {
    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0);
    }
}

impl Resumable {
    /// Its bytes: the key, `from` and `size`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + 20);
        self.key.encode(&mut out);
        varint(&mut out, self.from);
        varint(&mut out, self.size);
        out
    }

    /// Writes the prefix a resumable stream starts with, after the framing
    /// version.
    pub async fn write_prefix<W: AsyncWrite + Unpin + ?Sized>(
        &self,
        writer: &mut W,
    ) -> Result<(), ResumeError> {
        write(writer, &self.encode()).await
    }

    /// Reads a stream's prefix. It is not checked here: see
    /// [`crate::receiver_takes`].
    pub async fn read_prefix<R: AsyncRead + Unpin + ?Sized>(
        reader: &mut R,
    ) -> Result<Self, ResumeError> {
        Ok(Self {
            key: TransferKey(read_array(reader).await?),
            from: read_varint(reader, u64::BITS).await?,
            size: read_varint(reader, u64::BITS).await?,
        })
    }
}

impl Held {
    /// Its bytes: the key, `have` and `prefix`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(16 + 10 + DIGEST_LEN);
        self.key.encode(&mut out);
        varint(&mut out, self.have);
        out.extend_from_slice(&self.prefix.0);
        out
    }

    /// One read back from its bytes, all of them.
    pub fn decode(bytes: &[u8]) -> Result<Self, ResumeError> {
        let mut slice = Slice(bytes);
        let held = Self::read(&mut slice)?;
        slice.end()?;
        Ok(held)
    }

    pub(crate) fn read(slice: &mut Slice<'_>) -> Result<Self, ResumeError> {
        Ok(Self {
            key: TransferKey(slice.array()?),
            have: slice.u64()?,
            prefix: Digest(slice.array()?),
        })
    }

    pub(crate) async fn read_from<R: AsyncRead + Unpin + ?Sized>(
        reader: &mut R,
    ) -> Result<Self, ResumeError> {
        Ok(Self {
            key: TransferKey(read_array(reader).await?),
            have: read_varint(reader, u64::BITS).await?,
            prefix: Digest(read_array(reader).await?),
        })
    }
}

impl Verdict {
    /// Its number on the wire.
    pub fn code(self) -> u8 {
        match self {
            Verdict::Damaged => 0,
            Verdict::Restart => 1,
            Verdict::Refused => 2,
            Verdict::Dropped => 3,
            Verdict::AlreadyThere => 4,
            Verdict::Paused => 5,
        }
    }

    /// The verdict of a number: `None` for one no version has.
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Verdict::Damaged,
            1 => Verdict::Restart,
            2 => Verdict::Refused,
            3 => Verdict::Dropped,
            4 => Verdict::AlreadyThere,
            5 => Verdict::Paused,
            _ => return None,
        })
    }
}
