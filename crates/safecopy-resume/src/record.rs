//! A resume record's bytes, and the two slots of its file.
//!
//! A record file has two slots of [`SLOT`] bytes. A slot is the payload's
//! length (4 bytes, little-endian), a sequence number (8 bytes,
//! little-endian), the payload, and the SHA-256 of the sequence number and
//! the payload. A rewrite goes to the slot not holding the newest record,
//! so a torn write leaves the other whole; a reader takes the newest slot
//! whose checksum holds.
//!
//! A receiving record whose partial has a second identity
//! (`safecopy_journal::Second`) is a kind of its own, with the second
//! identity after the partial's place; one without is the kind of before.

use std::path::PathBuf;

use ring::digest::{SHA256, digest};
use safecopy_chunks::{DIGEST_LEN, Digest};
use safecopy_folder::{FileId, Time};
use safecopy_journal::{Level, Location, PartialAt, Root, Second};

use crate::wire::{Slice, varint};
use crate::{PeerId, ResumeError, TransferKey};

/// One slot's bytes: a constant of the record file's format.
pub(crate) const SLOT: usize = 16 << 10;
/// A slot's bytes around its payload.
const AROUND: usize = 4 + 8 + DIGEST_LEN;
/// The record format this crate writes and reads.
const VERSION: u8 = 1;
const RECEIVING: u8 = 0;
const SENDING: u8 = 1;
/// A receiving record whose partial has a second identity, which follows
/// its place. One without is written as [`RECEIVING`], as before; a
/// version of before reads this kind as damaged.
const RECEIVING_SECOND: u8 = 2;
const BORN: u8 = 1;
const GENERATION: u8 = 2;
const BOTH: u8 = 3;

/// What a receiver keeps of one file it was receiving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receiving {
    /// The other side.
    pub peer: PeerId,
    /// The file's transfer key.
    pub key: TransferKey,
    /// What the caller says must not change between the first stream of
    /// the file and any later one (its name, its folder, its metadata):
    /// a stream whose `same` differs is refused.
    pub same: Vec<u8>,
    /// The file's size, as its first stream declared it.
    pub size: u64,
    /// Where the partial file is, found again by identity.
    pub at: PartialAt,
    /// The bytes kept: a whole number of chunks, flushed to the disk before
    /// they were recorded.
    pub have: u64,
    /// The SHA-256 of those bytes.
    pub prefix: Digest,
    /// When the record was made, in seconds from the Unix epoch.
    pub created: u64,
    /// When its last byte came (or its session ended, for a file paused
    /// then), in seconds from the Unix epoch: its age counts from here.
    pub last: u64,
}

/// What a sender keeps of one file it was sending: enough to open it again
/// and check it is the same file. It never travels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sending {
    /// The other side.
    pub peer: PeerId,
    /// The file's transfer key.
    pub key: TransferKey,
    /// The path the person chose. With the rest it must fit one slot of
    /// 16 KiB: a longer path (Windows keeps it as UTF-16) is refused with
    /// [`ResumeError::TooLarge`].
    pub path: PathBuf,
    /// The file's identity when it was opened.
    pub file: FileId,
    /// Its size then.
    pub size: u64,
    /// Its modified time then.
    pub modified: Option<Time>,
    /// When the record was made, in seconds from the Unix epoch.
    pub created: u64,
    /// When it last changed, in seconds from the Unix epoch.
    pub last: u64,
}

/// A record of either kind, as a file holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Record {
    Receiving(Receiving),
    Sending(Sending),
}

impl Record {
    pub(crate) fn peer(&self) -> PeerId {
        match self {
            Record::Receiving(r) => r.peer,
            Record::Sending(s) => s.peer,
        }
    }

    pub(crate) fn key(&self) -> TransferKey {
        match self {
            Record::Receiving(r) => r.key,
            Record::Sending(s) => s.key,
        }
    }

    pub(crate) fn last(&self) -> u64 {
        match self {
            Record::Receiving(r) => r.last,
            Record::Sending(s) => s.last,
        }
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = vec![VERSION];
        match self {
            Record::Receiving(r) => {
                out.push(match r.at.second {
                    Second::None => RECEIVING,
                    _ => RECEIVING_SECOND,
                });
                out.extend_from_slice(&r.peer.0);
                out.extend_from_slice(&r.key.0);
                bytes(&mut out, &r.same);
                varint(&mut out, r.size);
                partial_at(&mut out, &r.at);
                match r.at.second {
                    Second::None => {}
                    Second::Born { secs, nanos } => {
                        out.push(BORN);
                        varint(&mut out, zigzag(secs));
                        varint(&mut out, u64::from(nanos));
                    }
                    Second::Generation(n) => {
                        out.push(GENERATION);
                        varint(&mut out, u64::from(n));
                    }
                    Second::Both {
                        secs,
                        nanos,
                        generation,
                    } => {
                        out.push(BOTH);
                        varint(&mut out, zigzag(secs));
                        varint(&mut out, u64::from(nanos));
                        varint(&mut out, u64::from(generation));
                    }
                }
                varint(&mut out, r.have);
                out.extend_from_slice(&r.prefix.0);
                varint(&mut out, r.created);
                varint(&mut out, r.last);
            }
            Record::Sending(s) => {
                out.push(SENDING);
                out.extend_from_slice(&s.peer.0);
                out.extend_from_slice(&s.key.0);
                path(&mut out, &s.path);
                varint(&mut out, s.file.volume);
                u128_field(&mut out, s.file.file);
                varint(&mut out, s.size);
                match s.modified {
                    None => out.push(0),
                    Some(time) => {
                        out.push(1);
                        varint(&mut out, zigzag(time.secs));
                        varint(&mut out, u64::from(time.nanos));
                    }
                }
                varint(&mut out, s.created);
                varint(&mut out, s.last);
            }
        }
        out
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, ResumeError> {
        let mut s = Slice(bytes);
        if s.byte()? != VERSION {
            return Err(ResumeError::Damaged);
        }
        let record = match s.byte()? {
            kind @ (RECEIVING | RECEIVING_SECOND) => Record::Receiving(Receiving {
                peer: PeerId(s.array()?),
                key: TransferKey(s.array()?),
                same: s.bytes()?,
                size: s.u64()?,
                at: {
                    let mut at = read_partial_at(&mut s)?;
                    if kind == RECEIVING_SECOND {
                        at.second = read_second(&mut s)?;
                    }
                    at
                },
                have: s.u64()?,
                prefix: Digest(s.array()?),
                created: s.u64()?,
                last: s.u64()?,
            }),
            SENDING => Record::Sending(Sending {
                peer: PeerId(s.array()?),
                key: TransferKey(s.array()?),
                path: read_path(&mut s)?,
                file: FileId {
                    volume: s.u64()?,
                    file: read_u128(&mut s)?,
                },
                size: s.u64()?,
                modified: match s.byte()? {
                    0 => None,
                    1 => {
                        let (secs, nanos) = read_time(&mut s)?;
                        Some(Time { secs, nanos })
                    }
                    _ => return Err(ResumeError::Damaged),
                },
                created: s.u64()?,
                last: s.u64()?,
            }),
            _ => return Err(ResumeError::Damaged),
        };
        s.end()?;
        // No store writes more kept bytes than the file has: such a record
        // is not one, and its number is not counted anywhere.
        if let Record::Receiving(r) = &record
            && r.have > r.size
        {
            return Err(ResumeError::Damaged);
        }
        Ok(record)
    }
}

fn bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// A signed number as postcard zigzags it.
fn zigzag(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

/// A time's seconds, zigzagged, and its nanoseconds, below a second.
fn read_time(s: &mut Slice<'_>) -> Result<(i64, u32), ResumeError> {
    let zigzag = s.u64()?;
    let secs = ((zigzag >> 1) as i64) ^ -((zigzag & 1) as i64);
    let nanos = u32::try_from(s.u64()?).map_err(|_| ResumeError::Damaged)?;
    if nanos >= 1_000_000_000 {
        return Err(ResumeError::Damaged);
    }
    Ok((secs, nanos))
}

/// The second identity a record of [`RECEIVING_SECOND`] holds: never none,
/// which is written as the kind of before.
fn read_second(s: &mut Slice<'_>) -> Result<Second, ResumeError> {
    match s.byte()? {
        BORN => {
            let (secs, nanos) = read_time(s)?;
            Ok(Second::Born { secs, nanos })
        }
        GENERATION => Ok(Second::Generation(read_generation(s)?)),
        BOTH => {
            let (secs, nanos) = read_time(s)?;
            Ok(Second::Both {
                secs,
                nanos,
                generation: read_generation(s)?,
            })
        }
        _ => Err(ResumeError::Damaged),
    }
}

fn read_generation(s: &mut Slice<'_>) -> Result<u32, ResumeError> {
    u32::try_from(s.u64()?).map_err(|_| ResumeError::Damaged)
}

fn u128_field(out: &mut Vec<u8>, n: u128) {
    out.extend_from_slice(&n.to_le_bytes());
}

fn read_u128(s: &mut Slice<'_>) -> Result<u128, ResumeError> {
    Ok(u128::from_le_bytes(s.array()?))
}

fn text(s: &mut Slice<'_>) -> Result<String, ResumeError> {
    String::from_utf8(s.bytes()?).map_err(|_| ResumeError::Damaged)
}

/// A path as the system holds it: Unix's bytes, Windows' UTF-16 units.
fn path(out: &mut Vec<u8>, path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        out.push(0);
        bytes(out, path.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        out.push(1);
        let wide: Vec<u8> = path
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect();
        bytes(out, &wide);
    }
}

fn read_path(s: &mut Slice<'_>) -> Result<PathBuf, ResumeError> {
    let kind = s.byte()?;
    let raw = s.bytes()?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        if kind != 0 {
            return Err(ResumeError::Damaged);
        }
        Ok(PathBuf::from(std::ffi::OsString::from_vec(raw)))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        if kind != 1 || raw.len() % 2 != 0 {
            return Err(ResumeError::Damaged);
        }
        let wide: Vec<u16> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        Ok(PathBuf::from(std::ffi::OsString::from_wide(&wide)))
    }
}

fn partial_at(out: &mut Vec<u8>, at: &PartialAt) {
    match &at.location.root {
        Root::Windows {
            volume,
            serial,
            folder,
        } => {
            out.push(0);
            bytes(out, volume.as_bytes());
            varint(out, *serial);
            u128_field(out, *folder);
        }
        Root::Unix {
            path,
            device,
            inode,
        } => {
            out.push(1);
            bytes(out, path);
            varint(out, *device);
            u128_field(out, *inode);
        }
    }
    varint(out, at.location.levels.len() as u64);
    for level in &at.location.levels {
        bytes(out, level.name.as_bytes());
        varint(out, level.device);
        u128_field(out, level.id);
    }
    bytes(out, at.partial.as_bytes());
    u128_field(out, at.file);
}

fn read_partial_at(s: &mut Slice<'_>) -> Result<PartialAt, ResumeError> {
    let root = match s.byte()? {
        0 => Root::Windows {
            volume: text(s)?,
            serial: s.u64()?,
            folder: read_u128(s)?,
        },
        1 => Root::Unix {
            path: s.bytes()?,
            device: s.u64()?,
            inode: read_u128(s)?,
        },
        _ => return Err(ResumeError::Damaged),
    };
    let count = usize::try_from(s.u64()?).map_err(|_| ResumeError::Damaged)?;
    // A level takes at least 18 bytes: no more room than the bytes can fill.
    if count > s.0.len() / 18 {
        return Err(ResumeError::Damaged);
    }
    let mut levels = Vec::with_capacity(count);
    for _ in 0..count {
        levels.push(Level {
            name: text(s)?,
            device: s.u64()?,
            id: read_u128(s)?,
        });
    }
    Ok(PartialAt {
        location: Location { root, levels },
        partial: text(s)?,
        file: read_u128(s)?,
        // A record of before has none; a newer kind's follows.
        second: Second::None,
    })
}

/// A slot's bytes for `payload` at `seq`, padded to [`SLOT`]; `None` when
/// it does not fit.
pub(crate) fn slot(seq: u64, payload: &[u8]) -> Option<Vec<u8>> {
    if payload.len() > SLOT - AROUND {
        return None;
    }
    let mut out = Vec::with_capacity(SLOT);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(payload);
    let sum = digest(&SHA256, &out[4..]);
    out.extend_from_slice(sum.as_ref());
    out.resize(SLOT, 0);
    Some(out)
}

/// The sequence number and payload of a slot's bytes, when its checksum
/// holds.
pub(crate) fn read_slot(bytes: &[u8]) -> Option<(u64, &[u8])> {
    let len = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
    if len > SLOT - AROUND {
        return None;
    }
    let end = 12 + len;
    let body = bytes.get(4..end)?;
    let sum = bytes.get(end..end + DIGEST_LEN)?;
    if digest(&SHA256, body).as_ref() != sum {
        return None;
    }
    let seq = u64::from_le_bytes(body[..8].try_into().ok()?);
    Some((seq, &body[8..]))
}

/// The newest record a file's two slots hold, its sequence number, and
/// which slot it is in.
pub(crate) fn newest(file: &[u8]) -> Option<(Record, u64, usize)> {
    (0..2)
        .filter_map(|n| {
            let bytes = file.get(n * SLOT..(n + 1) * SLOT)?;
            let (seq, payload) = read_slot(bytes)?;
            Some((Record::decode(payload).ok()?, seq, n))
        })
        .max_by_key(|(_, seq, _)| *seq)
}
