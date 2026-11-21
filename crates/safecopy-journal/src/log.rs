//! The journal's log: segments of checksummed entries.
//!
//! A segment starts with a header frame (the format, its sequence number,
//! the destination's root and a snapshot of the progress), then one frame
//! per entry. A frame is its payload's length (4 bytes, little-endian), a
//! CRC-32C of the payload (4 bytes, little-endian) and the payload, whose
//! numbers are unsigned LEB128 varints and whose text is length-prefixed
//! UTF-8. A reader stops at the first frame that runs past the end, fails
//! its checksum, does not decode, or could not follow from the ones before:
//! a torn append loses only that entry.

use std::collections::BTreeMap;

use safecopy_manifest::Progress;

use crate::record::{Reader, text, varint};
use crate::{JournalError, Settings};

/// The first bytes of a header frame's payload.
const MAGIC: &[u8; 4] = b"SCJL";
/// The log format this crate writes and reads.
const FORMAT: u8 = 1;
/// A frame's length and checksum.
pub(crate) const FRAME_HEAD: usize = 8;

/// The folder a journal's partial files are written below, as it is found
/// again after a crash.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Root {
    /// Windows: the volume by its `\\?\Volume{...}\` path and serial
    /// number, and the folder's file ID on it.
    Windows {
        /// The volume's GUID path.
        volume: String,
        /// The volume's serial number.
        serial: u64,
        /// The root folder's file ID.
        folder: u128,
    },
    /// Linux, macOS and Android: the folder's path, opened again and its
    /// device and inode compared.
    Unix {
        /// The path's bytes.
        path: Vec<u8>,
        /// Its device.
        device: u64,
        /// Its inode.
        inode: u128,
    },
}

/// A folder below the root, one level: its parent's number (0 is the
/// root), its name there and its identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Folder {
    /// Its number in the journal, never 0.
    pub n: u32,
    /// The number of the folder it is in.
    pub parent: u32,
    /// Its name there.
    pub name: String,
    /// Its volume or device.
    pub device: u64,
    /// Its file ID or inode.
    pub id: u128,
}

/// A partial file the journal recorded before it got a byte.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Started {
    /// The entry's place in its list.
    pub place: u32,
    /// The number of the folder it is in (0 is the root).
    pub folder: u32,
    /// The name the entry is published under.
    pub name: String,
    /// The partial file's own name.
    pub partial: String,
    /// The entry's declared size.
    pub size: u64,
    /// The partial file's file ID or inode.
    pub file: u128,
}

/// One entry after a segment's header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Entry {
    Folder(Folder),
    Group { index: u32, first: u32, len: u32 },
    Started(Started),
    StartedMany(Vec<Started>),
    Done { place: u32 },
    DoneMany(Vec<u32>),
    Ended { place: u32 },
}

const FOLDER: u8 = 0;
const GROUP: u8 = 1;
const STARTED: u8 = 2;
const STARTED_MANY: u8 = 3;
const DONE: u8 = 4;
const DONE_MANY: u8 = 5;
const ENDED: u8 = 6;

/// What a journal says of its transfer: the replay of a segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct State {
    pub(crate) seq: u64,
    pub(crate) root: Root,
    /// Whether the transfer goes by groups: a partial must be in an open
    /// group, and the mark moves.
    pub(crate) grouped: bool,
    pub(crate) progress: Progress,
    pub(crate) folders: BTreeMap<u32, Folder>,
    pub(crate) live: Vec<Started>,
}

/// The CRC-32C (Castagnoli) of `bytes`.
pub(crate) fn crc32c(bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut n = 0;
        while n < 256 {
            let mut crc = n as u32;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0x82F6_3B78
                } else {
                    crc >> 1
                };
                bit += 1;
            }
            table[n] = crc;
            n += 1;
        }
        table
    };
    let mut crc = !0u32;
    for &byte in bytes {
        crc = TABLE[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

/// `payload` framed: its length, its checksum, itself.
pub(crate) fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEAD + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc32c(payload).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// The next whole frame's payload, and the bytes after it; `None` at the
/// end, or at a frame cut short or whose checksum fails.
fn next_frame(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let (head, rest) = bytes.split_first_chunk::<FRAME_HEAD>()?;
    let len = u32::from_le_bytes(head[..4].try_into().expect("4 bytes")) as usize;
    let crc = u32::from_le_bytes(head[4..].try_into().expect("4 bytes"));
    if len > rest.len() {
        return None;
    }
    let (payload, rest) = rest.split_at(len);
    (crc32c(payload) == crc).then_some((payload, rest))
}

fn bytes_field(out: &mut Vec<u8>, bytes: &[u8]) {
    varint(out, bytes.len() as u128);
    out.extend_from_slice(bytes);
}

impl Reader<'_> {
    fn u32(&mut self) -> Result<u32, JournalError> {
        Ok(self.varint(u32::BITS)? as u32)
    }

    fn u64(&mut self) -> Result<u64, JournalError> {
        Ok(self.varint(u64::BITS)? as u64)
    }

    fn u128(&mut self) -> Result<u128, JournalError> {
        self.varint(u128::BITS)
    }

    fn byte(&mut self) -> Result<u8, JournalError> {
        let (&byte, rest) = self.0.split_first().ok_or(JournalError::Damaged)?;
        self.0 = rest;
        Ok(byte)
    }

    fn take(&mut self, len: usize) -> Result<&[u8], JournalError> {
        if len > self.0.len() {
            return Err(JournalError::Damaged);
        }
        let (taken, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(taken)
    }

    fn bytes_field(&mut self) -> Result<Vec<u8>, JournalError> {
        let len = usize::try_from(self.varint(usize::BITS)?).map_err(|_| JournalError::Damaged)?;
        Ok(self.take(len)?.to_vec())
    }

    /// A count of items each at least `least` bytes long: never more than
    /// the bytes left could hold, nor `most`.
    fn count(&mut self, least: usize, most: usize) -> Result<usize, JournalError> {
        let count =
            usize::try_from(self.varint(usize::BITS)?).map_err(|_| JournalError::Damaged)?;
        if count > most || count > self.0.len() / least {
            return Err(JournalError::Damaged);
        }
        Ok(count)
    }

    fn end(&self) -> Result<(), JournalError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(JournalError::Damaged)
        }
    }
}

fn encode_root(out: &mut Vec<u8>, root: &Root) {
    match root {
        Root::Windows {
            volume,
            serial,
            folder,
        } => {
            out.push(0);
            text(out, volume);
            varint(out, u128::from(*serial));
            varint(out, *folder);
        }
        Root::Unix {
            path,
            device,
            inode,
        } => {
            out.push(1);
            bytes_field(out, path);
            varint(out, u128::from(*device));
            varint(out, *inode);
        }
    }
}

fn decode_root(reader: &mut Reader<'_>) -> Result<Root, JournalError> {
    match reader.byte()? {
        0 => Ok(Root::Windows {
            volume: reader.text()?,
            serial: reader.u64()?,
            folder: reader.u128()?,
        }),
        1 => Ok(Root::Unix {
            path: reader.bytes_field()?,
            device: reader.u64()?,
            inode: reader.u128()?,
        }),
        _ => Err(JournalError::Damaged),
    }
}

fn encode_folder(out: &mut Vec<u8>, folder: &Folder) {
    varint(out, folder.n.into());
    varint(out, folder.parent.into());
    text(out, &folder.name);
    varint(out, folder.device.into());
    varint(out, folder.id);
}

fn decode_folder(reader: &mut Reader<'_>) -> Result<Folder, JournalError> {
    Ok(Folder {
        n: reader.u32()?,
        parent: reader.u32()?,
        name: reader.text()?,
        device: reader.u64()?,
        id: reader.u128()?,
    })
}

fn encode_started(out: &mut Vec<u8>, started: &Started) {
    varint(out, started.place.into());
    varint(out, started.folder.into());
    text(out, &started.name);
    text(out, &started.partial);
    varint(out, started.size.into());
    varint(out, started.file);
}

fn decode_started(reader: &mut Reader<'_>) -> Result<Started, JournalError> {
    Ok(Started {
        place: reader.u32()?,
        folder: reader.u32()?,
        name: reader.text()?,
        partial: reader.text()?,
        size: reader.u64()?,
        file: reader.u128()?,
    })
}

/// The fewest bytes a folder and a started partial take encoded.
const LEAST_FOLDER: usize = 5;
const LEAST_STARTED: usize = 6;

impl Entry {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Entry::Folder(folder) => {
                out.push(FOLDER);
                encode_folder(&mut out, folder);
            }
            Entry::Group { index, first, len } => {
                out.push(GROUP);
                for n in [index, first, len] {
                    varint(&mut out, (*n).into());
                }
            }
            Entry::Started(started) => {
                out.push(STARTED);
                encode_started(&mut out, started);
            }
            Entry::StartedMany(many) => {
                out.push(STARTED_MANY);
                varint(&mut out, many.len() as u128);
                for started in many {
                    encode_started(&mut out, started);
                }
            }
            Entry::Done { place } => {
                out.push(DONE);
                varint(&mut out, (*place).into());
            }
            Entry::DoneMany(places) => {
                out.push(DONE_MANY);
                varint(&mut out, places.len() as u128);
                for place in places {
                    varint(&mut out, (*place).into());
                }
            }
            Entry::Ended { place } => {
                out.push(ENDED);
                varint(&mut out, (*place).into());
            }
        }
        out
    }

    pub(crate) fn decode(payload: &[u8], settings: &Settings) -> Result<Self, JournalError> {
        let mut reader = Reader(payload);
        let most = settings.partials;
        let entry = match reader.byte()? {
            FOLDER => Entry::Folder(decode_folder(&mut reader)?),
            GROUP => Entry::Group {
                index: reader.u32()?,
                first: reader.u32()?,
                len: reader.u32()?,
            },
            STARTED => Entry::Started(decode_started(&mut reader)?),
            STARTED_MANY => {
                let count = reader.count(LEAST_STARTED, most)?;
                let mut many = Vec::with_capacity(count);
                for _ in 0..count {
                    many.push(decode_started(&mut reader)?);
                }
                Entry::StartedMany(many)
            }
            DONE => Entry::Done {
                place: reader.u32()?,
            },
            DONE_MANY => {
                let count = reader.count(1, most)?;
                let mut places = Vec::with_capacity(count);
                for _ in 0..count {
                    places.push(reader.u32()?);
                }
                Entry::DoneMany(places)
            }
            ENDED => Entry::Ended {
                place: reader.u32()?,
            },
            _ => return Err(JournalError::Damaged),
        };
        reader.end()?;
        Ok(entry)
    }
}

impl State {
    /// A new transfer's state: nothing recorded.
    pub(crate) fn new(root: Root, settings: &Settings) -> Self {
        Self {
            seq: 1,
            root,
            grouped: false,
            progress: Progress::new(&settings.groups, settings.partials),
            folders: BTreeMap::new(),
            live: Vec::new(),
        }
    }

    /// The header frame's payload: the format, the sequence number, the
    /// root and the snapshot of the progress, with the folders the live
    /// partials are in and every folder above them.
    pub(crate) fn header(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.push(FORMAT);
        varint(&mut out, self.seq.into());
        encode_root(&mut out, &self.root);
        out.push(u8::from(self.grouped));
        varint(&mut out, self.progress.mark().into());
        varint(&mut out, self.progress.below().into());
        let open = self.progress.open_groups();
        varint(&mut out, open.len() as u128);
        for group in open {
            varint(&mut out, group.index.into());
            varint(&mut out, group.first.into());
            varint(&mut out, group.len.into());
            out.extend_from_slice(&group.bits());
        }
        let folders = self.needed_folders();
        varint(&mut out, folders.len() as u128);
        for folder in &folders {
            encode_folder(&mut out, folder);
        }
        varint(&mut out, self.live.len() as u128);
        for started in &self.live {
            encode_started(&mut out, started);
        }
        out
    }

    /// The folders the live partials are in, and every folder above them,
    /// parents first.
    pub(crate) fn needed_folders(&self) -> Vec<Folder> {
        let mut needed = std::collections::BTreeSet::new();
        for started in &self.live {
            let mut n = started.folder;
            while n != 0 && needed.insert(n) {
                n = self.folders.get(&n).map_or(0, |f| f.parent);
            }
        }
        // Numbers grow as folders are first used, so a parent's is lower.
        needed
            .into_iter()
            .filter_map(|n| self.folders.get(&n).cloned())
            .collect()
    }

    /// A state read from a header frame's payload.
    pub(crate) fn from_header(payload: &[u8], settings: &Settings) -> Result<Self, JournalError> {
        let mut reader = Reader(payload);
        if reader.take(MAGIC.len())? != MAGIC || reader.byte()? != FORMAT {
            return Err(JournalError::Damaged);
        }
        let seq = reader.u64()?;
        let root = decode_root(&mut reader)?;
        let grouped = match reader.byte()? {
            0 => false,
            1 => true,
            _ => return Err(JournalError::Damaged),
        };
        let mark = reader.u32()?;
        let below = reader.u32()?;
        let open_count = reader.count(4, settings.groups.open.max(1))?;
        let mut open = Vec::with_capacity(open_count);
        for _ in 0..open_count {
            let (index, first, len) = (reader.u32()?, reader.u32()?, reader.u32()?);
            if len > settings.groups.entries.max(1) {
                return Err(JournalError::Damaged);
            }
            let bits = reader.take(len.div_ceil(8) as usize)?.to_vec();
            open.push((index, first, len, bits));
        }
        let folder_count = reader.count(LEAST_FOLDER, usize::MAX)?;
        let mut folders = Vec::with_capacity(folder_count);
        for _ in 0..folder_count {
            folders.push(decode_folder(&mut reader)?);
        }
        let live_count = reader.count(LEAST_STARTED, settings.partials)?;
        let mut live = Vec::with_capacity(live_count);
        for _ in 0..live_count {
            live.push(decode_started(&mut reader)?);
        }
        reader.end()?;
        let places: Vec<u32> = if grouped {
            live.iter().map(|s| s.place).collect()
        } else {
            Vec::new()
        };
        let progress = Progress::restore(
            mark,
            below,
            &open,
            &places,
            &settings.groups,
            settings.partials,
        )
        .map_err(|_| JournalError::Damaged)?;
        if !grouped && (mark != 0 || below != 0 || !open.is_empty()) {
            return Err(JournalError::Damaged);
        }
        let mut state = Self {
            seq,
            root,
            grouped,
            progress,
            folders: BTreeMap::new(),
            live: Vec::new(),
        };
        for folder in folders {
            state.add_folder(folder)?;
        }
        for started in live {
            state.add_live(started)?;
        }
        Ok(state)
    }

    fn add_folder(&mut self, folder: Folder) -> Result<(), JournalError> {
        // A parent is numbered before its child: snapshots list them in that order.
        let known_parent = folder.parent == 0
            || (folder.parent < folder.n && self.folders.contains_key(&folder.parent));
        if folder.n == 0 || !known_parent || self.folders.contains_key(&folder.n) {
            return Err(JournalError::Damaged);
        }
        self.folders.insert(folder.n, folder);
        Ok(())
    }

    fn check_live(&self, started: &Started) -> Result<(), JournalError> {
        let known = started.folder == 0 || self.folders.contains_key(&started.folder);
        if !known || self.live.iter().any(|s| s.place == started.place) {
            return Err(JournalError::Damaged);
        }
        if self.live.len() >= self.progress.most_partials() {
            return Err(JournalError::TooLarge {
                most: self.progress.most_partials(),
            });
        }
        Ok(())
    }

    fn add_live(&mut self, started: Started) -> Result<(), JournalError> {
        self.check_live(&started)?;
        self.live.push(started);
        Ok(())
    }

    fn start(&mut self, started: Started) -> Result<(), JournalError> {
        self.check_live(&started)?;
        if self.grouped {
            self.progress
                .start(started.place)
                .map_err(JournalError::Progress)?;
        }
        self.live.push(started);
        Ok(())
    }

    fn settle(&mut self, place: u32) -> Result<u32, JournalError> {
        let moved = if self.grouped {
            self.progress
                .settle(place)
                .map_err(JournalError::Progress)?
        } else {
            0
        };
        self.live.retain(|s| s.place != place);
        Ok(moved)
    }

    /// Applies `entry`; refused when it could not follow what came before.
    /// Returns how many groups the mark moved.
    pub(crate) fn apply(&mut self, entry: Entry) -> Result<u32, JournalError> {
        match entry {
            Entry::Folder(folder) => self.add_folder(folder).map(|()| 0),
            Entry::Group { index, first, len } => {
                if !self.grouped && !self.live.is_empty() {
                    // Places recorded without groups cannot join one.
                    return Err(JournalError::Damaged);
                }
                self.progress
                    .open(index, first, len)
                    .map_err(JournalError::Progress)?;
                self.grouped = true;
                Ok(0)
            }
            Entry::Started(started) => self.start(started).map(|()| 0),
            Entry::StartedMany(many) => {
                for started in many {
                    self.start(started)?;
                }
                Ok(0)
            }
            Entry::Done { place } => self.settle(place),
            Entry::DoneMany(places) => {
                let mut moved = 0;
                for place in places {
                    moved += self.settle(place)?;
                }
                Ok(moved)
            }
            Entry::Ended { place } => {
                if !self.live.iter().any(|s| s.place == place) {
                    return Err(JournalError::Damaged);
                }
                self.live.retain(|s| s.place != place);
                if self.grouped {
                    self.progress.end(place);
                }
                Ok(0)
            }
        }
    }
}

/// What one segment says, read as far as it is whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub(crate) state: State,
    /// How many entries after the header were read.
    pub entries: usize,
    /// How many bytes were read: the header and those entries. Anything
    /// after them was torn, damaged or could not follow.
    pub read: usize,
}

impl Segment {
    /// Its sequence number.
    pub fn seq(&self) -> u64 {
        self.state.seq
    }

    /// The destination's root.
    pub fn root(&self) -> &Root {
        &self.state.root
    }

    /// The progress, with every entry read applied.
    pub fn progress(&self) -> &Progress {
        &self.state.progress
    }

    /// Whether the transfer goes by groups.
    pub fn grouped(&self) -> bool {
        self.state.grouped
    }

    /// The partial files recorded and not yet settled nor ended.
    pub fn partials(&self) -> &[Started] {
        &self.state.live
    }

    /// The bytes of a segment that starts from this one's state: its
    /// header and snapshot, as the next segment would begin.
    pub fn snapshot(&self) -> Vec<u8> {
        frame(&self.state.header())
    }

    /// The folders below the root this segment names.
    pub fn folders(&self) -> impl Iterator<Item = &Folder> {
        self.state.folders.values()
    }

    /// The folders from the root down to folder `n`, the root's child
    /// first; `None` when one is not named.
    pub fn chain(&self, n: u32) -> Option<Vec<&Folder>> {
        let mut chain = Vec::new();
        let mut at = n;
        while at != 0 {
            let folder = self.state.folders.get(&at)?;
            chain.push(folder);
            at = folder.parent;
            // A chain longer than the folders it could use is a loop.
            if chain.len() > self.state.folders.len() {
                return None;
            }
        }
        chain.reverse();
        Some(chain)
    }
}

/// Reads a segment's bytes: [`JournalError::Damaged`] when its header
/// frame is not whole, and otherwise every entry up to the first that is
/// torn, damaged or could not follow the ones before it. Bytes past
/// [`Settings::segment_bytes`] are not read.
pub fn read_segment(bytes: &[u8], settings: &Settings) -> Result<Segment, JournalError> {
    let bytes = &bytes[..bytes.len().min(settings.segment_bytes)];
    let (header, mut rest) = next_frame(bytes).ok_or(JournalError::Damaged)?;
    let mut state = State::from_header(header, settings)?;
    let mut entries = 0;
    while let Some((payload, after)) = next_frame(rest) {
        let Ok(entry) = Entry::decode(payload, settings) else {
            break;
        };
        // A single entry is refused before it changes anything; a bundle's
        // may fail midway, so it is tried on a copy.
        if matches!(entry, Entry::StartedMany(_) | Entry::DoneMany(_)) {
            let mut next = state.clone();
            if next.apply(entry).is_err() {
                break;
            }
            state = next;
        } else if state.apply(entry).is_err() {
            break;
        }
        entries += 1;
        rest = after;
    }
    Ok(Segment {
        state,
        entries,
        read: bytes.len() - rest.len(),
    })
}
