//! The resume records of one program, in a folder it passes.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use safecopy_chunks::{CHUNK, hash_prefix};
use safecopy_folder::Directory;
use safecopy_journal::{Cleaned, PartialAt, files};

use crate::record::{Record, SLOT, newest, slot};
use crate::{Held, Limits, PeerId, Receiving, ResumeError, Sending, TransferKey};

/// The ending of a record file's name.
const EXTENSION: &str = "resume";

/// What [`Store::expire`] removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Expired {
    /// Records removed, with their partial files: too old, past a cap, or
    /// with no slot that reads.
    pub removed: usize,
    /// Bytes the kept files hold after it.
    pub kept_bytes: u64,
}

/// One record file read.
struct Found {
    name: String,
    record: Record,
    seq: u64,
    slot: usize,
}

/// The resume records of one program, one file per transfer in a folder
/// the caller passes (see the [crate] docs). A record this store wrote is
/// held open while the store lasts (without write sharing on Windows,
/// locked on Unix), so another program's store leaves it alone.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    limits: Limits,
    journal: safecopy_journal::Settings,
    held: Mutex<HashMap<String, File>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn on_chunk(n: u64) -> bool {
    n.is_multiple_of(CHUNK)
}

impl Store {
    /// The store in the folder `dir`, made when missing.
    pub fn open(dir: &Path, limits: Limits) -> Result<Self, ResumeError> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            limits,
            journal: safecopy_journal::Settings::default(),
            held: Mutex::new(HashMap::new()),
        })
    }

    /// The journal's settings the store removes partial files by: which
    /// ending a partial's name has.
    pub fn with_journal_settings(mut self, settings: safecopy_journal::Settings) -> Self {
        self.journal = settings;
        self
    }

    /// Its limits.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The bytes of the record file `name`, through the handle this store
    /// holds or opened for reading.
    fn read(&self, name: &str) -> Option<Vec<u8>> {
        let mut bytes = Vec::new();
        let held = lock(&self.held);
        let result = match held.get(name) {
            Some(file) => {
                let mut file = file;
                file.seek(SeekFrom::Start(0))
                    .and_then(|_| std::io::Read::read_to_end(&mut file, &mut bytes))
            }
            None => {
                drop(held);
                files::open_read(&self.dir.join(name)).and_then(|file| {
                    std::io::Read::read_to_end(
                        &mut std::io::Read::take(file, 2 * SLOT as u64 + 1),
                        &mut bytes,
                    )
                })
            }
        };
        result
            .ok()
            .filter(|_| bytes.len() <= 2 * SLOT)
            .map(|_| bytes)
    }

    /// Every record file's name, and what it holds when a slot reads.
    fn all(&self) -> Result<Vec<(String, Option<Found>)>, ResumeError> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension() != Some(OsStr::new(EXTENSION)) {
                continue;
            }
            let Some(name) = path.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            let found = self.read(name).and_then(|bytes| {
                let (record, seq, slot) = newest(&bytes)?;
                Some(Found {
                    name: name.to_string(),
                    record,
                    seq,
                    slot,
                })
            });
            out.push((name.to_string(), found));
        }
        Ok(out)
    }

    fn find(&self, peer: PeerId, key: TransferKey, receiving: bool) -> Option<Found> {
        self.all().ok()?.into_iter().find_map(|(_, found)| {
            let found = found?;
            let kind = matches!(found.record, Record::Receiving(_)) == receiving;
            (kind && found.record.peer() == peer && found.record.key() == key).then_some(found)
        })
    }

    /// The receiving record of `key` from `peer`.
    pub fn received(&self, peer: PeerId, key: TransferKey) -> Option<Receiving> {
        match self.find(peer, key, true)?.record {
            Record::Receiving(r) => Some(r),
            Record::Sending(_) => None,
        }
    }

    /// The sending record of `key` to `peer`.
    pub fn sent(&self, peer: PeerId, key: TransferKey) -> Option<Sending> {
        match self.find(peer, key, false)?.record {
            Record::Sending(s) => Some(s),
            Record::Receiving(_) => None,
        }
    }

    /// Writes `record`: into a new file, or over the older slot of the
    /// file holding its key, then flushed to the disk.
    fn write(&self, record: &Record) -> Result<(), ResumeError> {
        let receiving = matches!(record, Record::Receiving(_));
        let existing = self.find(record.peer(), record.key(), receiving);
        let payload = record.encode();
        let (seq, at) = existing
            .as_ref()
            .map_or((1, 0), |f| (f.seq + 1, 1 - f.slot));
        let bytes = slot(seq, &payload).ok_or(ResumeError::TooLarge { most: SLOT })?;
        let mut held = lock(&self.held);
        let mut new = false;
        let name = match &existing {
            Some(found) => {
                if !held.contains_key(&found.name) {
                    let (file, _) = files::take(&self.dir.join(&found.name), 0)?;
                    held.insert(found.name.clone(), file);
                }
                found.name.clone()
            }
            None => {
                let name = format!("{}.{EXTENSION}", hex(&TransferKey::random()?.0));
                let file = files::create(&self.dir.join(&name))?;
                file.set_len(2 * SLOT as u64)?;
                held.insert(name.clone(), file);
                new = true;
                name
            }
        };
        let mut file = held.get(&name).expect("held above");
        file.seek(SeekFrom::Start((at * SLOT) as u64))?;
        #[cfg(test)]
        if crate::tests::seams::TEAR.replace(false) {
            // Cut within its payload: the crash came halfway through.
            file.write_all(&bytes[..(12 + payload.len()) / 2])?;
            return Ok(());
        }
        file.write_all(&bytes)?;
        file.sync_data()?;
        if new {
            // The new file's name reaches the disk too.
            #[cfg(test)]
            crate::tests::seams::DIR_SYNCS.set(crate::tests::seams::DIR_SYNCS.get() + 1);
            files::sync_dir(&self.dir)?;
        }
        Ok(())
    }

    /// The record file `name`, for this store alone: the handle it holds,
    /// or taken whole, so one another store holds is left. `None` when it
    /// cannot be had.
    fn claim(&self, name: &str) -> Option<File> {
        if let Some(file) = lock(&self.held).remove(name) {
            return Some(file);
        }
        files::take(&self.dir.join(name), 0)
            .ok()
            .map(|(file, _)| file)
    }

    /// Removes the record file `name`, claimed first.
    fn remove_file(&self, name: &str) -> bool {
        match self.claim(name) {
            Some(file) => files::delete(&file, &self.dir.join(name)).is_ok(),
            None => false,
        }
    }

    /// Removes a record and, for a receiving one, its partial file by
    /// identity. The record is claimed first: one another store holds
    /// keeps its record and its partial.
    fn remove(&self, found: &Found) -> bool {
        let Some(file) = self.claim(&found.name) else {
            return false;
        };
        if let Record::Receiving(r) = &found.record {
            r.at.remove(&self.journal);
        }
        files::delete(&file, &self.dir.join(&found.name)).is_ok()
    }

    /// Starts keeping a file being received, from its first whole chunk:
    /// `record.have` at least one chunk and a whole number of them, within
    /// the size. `partial` is flushed to the disk first, so `have` never
    /// claims a byte that is not there.
    pub fn receiving(&self, record: &Receiving, partial: &File) -> Result<(), ResumeError> {
        if record.have > record.size {
            return Err(ResumeError::PastSize);
        }
        if record.have < CHUNK || !on_chunk(record.have) {
            return Err(ResumeError::OffChunk);
        }
        if record.same.len() > self.limits.same_bytes {
            return Err(ResumeError::TooLarge {
                most: self.limits.same_bytes,
            });
        }
        partial.sync_data()?;
        self.write(&Record::Receiving(record.clone()))
    }

    /// The kept file `held.key` from `peer` now holds `held.have` whole
    /// bytes, whose hash is `held.prefix`; its last byte came at `now`. The
    /// record is rewritten, after `partial` is flushed, once at least
    /// [`Limits::rewrite_bytes`] more are whole and
    /// [`Limits::rewrite_every`] has passed, or at once with `at_drop`.
    /// Whether it was written.
    pub fn keep(
        &self,
        peer: PeerId,
        held: &Held,
        now: u64,
        partial: &File,
        at_drop: bool,
    ) -> Result<bool, ResumeError> {
        let (key, have, prefix) = (held.key, held.have, held.prefix);
        let mut record = self.received(peer, key).ok_or(ResumeError::NotHeld)?;
        if have > record.size {
            return Err(ResumeError::PastSize);
        }
        if have < CHUNK || !on_chunk(have) {
            return Err(ResumeError::OffChunk);
        }
        let due = have.saturating_sub(record.have) >= self.limits.rewrite_bytes
            && Duration::from_secs(now.saturating_sub(record.last)) >= self.limits.rewrite_every;
        if !at_drop && !due {
            return Ok(false);
        }
        partial.sync_data()?;
        record.have = have;
        record.prefix = prefix;
        record.last = now;
        self.write(&Record::Receiving(record))?;
        Ok(true)
    }

    /// Starts keeping a file being sent.
    pub fn sending(&self, record: &Sending) -> Result<(), ResumeError> {
        self.write(&Record::Sending(record.clone()))
    }

    /// What this side keeps from `peer`, each checked first: its partial
    /// opened by identity, at least `have` bytes long, its first `have`
    /// bytes hashing to `prefix`. One that fails is removed, with its
    /// record, and not listed: it will come from zero. Oldest first, at
    /// most [`Limits::per_peer`].
    pub async fn held_from(&self, peer: PeerId) -> Result<Vec<Held>, ResumeError> {
        let mut kept = Vec::new();
        for (_, found) in self.all()? {
            let Some(found) = found else {
                continue;
            };
            let Record::Receiving(record) = &found.record else {
                continue;
            };
            if record.peer != peer {
                continue;
            }
            if check_partial(record).await {
                kept.push((
                    record.created,
                    record.last,
                    Held {
                        key: record.key,
                        have: record.have,
                        prefix: record.prefix,
                    },
                ));
            } else {
                self.remove(&found);
            }
        }
        kept.sort_by_key(|(created, last, _)| (*created, *last));
        kept.truncate(self.limits.per_peer);
        Ok(kept.into_iter().map(|(_, _, held)| held).collect())
    }

    /// The kept partial of `key` from `peer`, opened by identity for
    /// writing and cut to its `have`, to append the rest; with its folder.
    pub fn reopen(&self, peer: PeerId, key: TransferKey) -> Result<(Directory, File), ResumeError> {
        let record = self.received(peer, key).ok_or(ResumeError::NotHeld)?;
        let (dir, file) = record.at.reopen().ok_or(ResumeError::Gone)?;
        if file.metadata()?.len() < record.have {
            return Err(ResumeError::WrongSize);
        }
        file.set_len(record.have)?;
        Ok((dir, file))
    }

    /// Forgets the file of `key` from or to `peer`: its records go, and a
    /// received partial is removed by identity.
    pub fn forget(&self, peer: PeerId, key: TransferKey) -> Result<(), ResumeError> {
        for (_, found) in self.all()? {
            if let Some(found) = found
                && found.record.peer() == peer
                && found.record.key() == key
            {
                self.remove(&found);
            }
        }
        Ok(())
    }

    /// Removes every record, of `peer` only when given, with the received
    /// partials: a disconnect the person made. How many.
    pub fn remove_all(&self, peer: Option<PeerId>) -> Result<usize, ResumeError> {
        let mut removed = 0;
        for (_, found) in self.all()? {
            if let Some(found) = found
                && peer.is_none_or(|p| found.record.peer() == p)
                && self.remove(&found)
            {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// The bytes the kept files hold.
    pub fn kept_bytes(&self) -> u64 {
        self.all()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(_, f)| match f?.record {
                Record::Receiving(r) => Some(r.have),
                Record::Sending(_) => None,
            })
            .sum()
    }

    /// Whether a valid receiving record keeps the partial `at`: a journal's
    /// cleanup leaves it.
    pub fn keeps(&self, at: &PartialAt) -> bool {
        self.all().unwrap_or_default().into_iter().any(
            |(_, f)| matches!(f, Some(Found { record: Record::Receiving(r), .. }) if same_partial(&r.at, at)),
        )
    }

    /// The journals' cleanup in `journals`, leaving the partials this
    /// store's records keep.
    pub fn clean_up(
        &self,
        journals: &Path,
        settings: &safecopy_journal::Settings,
    ) -> Result<Cleaned, ResumeError> {
        Ok(safecopy_journal::clean_up_keeping(
            journals,
            settings,
            &|at| self.keeps(at),
        )?)
    }

    /// Removes what is past its time or the caps, never a record of `live`
    /// (sessions still running, moving or paused):
    ///
    /// - a file with no slot that reads (its journal entry removes the
    ///   partial);
    /// - any record whose last byte is older than `keep_for` at `now`;
    /// - past [`Limits::per_peer`] files of one peer, that peer's oldest;
    /// - past [`Limits::total`] files, the oldest of the peer keeping the
    ///   most, so one peer filling the cap loses its own first;
    /// - past [`Limits::disk_share`] of a disk's free space and what the
    ///   kept files hold there, the oldest on that disk.
    pub fn expire(
        &self,
        now: u64,
        keep_for: Duration,
        live: &[TransferKey],
    ) -> Result<Expired, ResumeError> {
        let mut expired = Expired::default();
        let mut kept: Vec<Found> = Vec::new();
        for (name, found) in self.all()? {
            let Some(found) = found else {
                if !lock(&self.held).contains_key(&name) && self.remove_file(&name) {
                    expired.removed += 1;
                }
                continue;
            };
            if live.contains(&found.record.key()) {
                continue;
            }
            let age = Duration::from_secs(now.saturating_sub(found.record.last()));
            if age > keep_for {
                expired.removed += usize::from(self.remove(&found));
                continue;
            }
            if matches!(found.record, Record::Receiving(_)) {
                kept.push(found);
            }
        }
        // Oldest first.
        kept.sort_by_key(|f| (f.record.last(), f.record.key()));
        let mut gone = vec![false; kept.len()];
        let count = |gone: &[bool], peer: Option<PeerId>| {
            kept.iter()
                .zip(gone)
                .filter(|(f, g)| !**g && peer.is_none_or(|p| f.record.peer() == p))
                .count()
        };
        let mut drop_at = |i: usize, gone: &mut [bool]| {
            gone[i] = true;
            expired.removed += usize::from(self.remove(&kept[i]));
        };
        // Per peer.
        let peers: Vec<PeerId> = kept.iter().map(|f| f.record.peer()).collect();
        for peer in &peers {
            while count(&gone, Some(*peer)) > self.limits.per_peer {
                let i = (0..kept.len())
                    .find(|&i| !gone[i] && kept[i].record.peer() == *peer)
                    .expect("counted");
                drop_at(i, &mut gone);
            }
        }
        // In all: the peer keeping the most loses its oldest first.
        while count(&gone, None) > self.limits.total {
            let most = peers
                .iter()
                .max_by_key(|p| (count(&gone, Some(**p)), std::cmp::Reverse(**p)))
                .copied()
                .expect("some kept");
            let i = (0..kept.len())
                .find(|&i| !gone[i] && kept[i].record.peer() == most)
                .expect("counted");
            drop_at(i, &mut gone);
        }
        // Each disk's share.
        let (num, den) = self.limits.disk_share;
        let mut disks: HashMap<u64, Option<u64>> = HashMap::new();
        for i in 0..kept.len() {
            let Record::Receiving(r) = &kept[i].record else {
                continue;
            };
            let disk =
                r.at.location
                    .levels
                    .last()
                    .map_or_else(|| root_device(&r.at.location.root), |level| level.device);
            // Each disk is held to its share once, with its free space read once.
            if disks.contains_key(&disk) {
                continue;
            }
            let free =
                r.at.location
                    .open()
                    .ok()
                    .and_then(|d| crate::space::free(&d));
            disks.insert(disk, free);
            let Some(free) = free else {
                continue;
            };
            let on_disk: Vec<usize> = (0..kept.len())
                .filter(|&j| !gone[j] && disk_of(&kept[j]) == Some(disk))
                .collect();
            let holds: u64 = on_disk.iter().map(|&j| have_of(&kept[j])).sum();
            // Removing a kept file frees its bytes: free space and what is held
            // together stay the same while the oldest go.
            let total = u128::from(free) + u128::from(holds);
            let share =
                |holds: u64| u128::from(holds) * u128::from(den.max(1)) > total * u128::from(num);
            let mut holds = holds;
            for &j in &on_disk {
                if !share(holds) {
                    break;
                }
                holds -= have_of(&kept[j]);
                drop_at(j, &mut gone);
            }
        }
        expired.kept_bytes = kept
            .iter()
            .zip(&gone)
            .filter(|(_, g)| !**g)
            .map(|(f, _)| have_of(f))
            .sum();
        Ok(expired)
    }
}

/// Whether two places name one partial: the same folder by identity (the
/// last level's, or the root's), the same name in it and the same file ID,
/// whatever root each was recorded from.
fn same_partial(a: &PartialAt, b: &PartialAt) -> bool {
    let folder = |at: &PartialAt| {
        at.location.levels.last().map_or_else(
            || match &at.location.root {
                safecopy_journal::Root::Windows { serial, folder, .. } => (*serial, *folder),
                safecopy_journal::Root::Unix { device, inode, .. } => (*device, *inode),
            },
            |level| (level.device, level.id),
        )
    };
    folder(a) == folder(b) && a.partial == b.partial && a.file == b.file
}

fn root_device(root: &safecopy_journal::Root) -> u64 {
    match root {
        safecopy_journal::Root::Windows { serial, .. } => *serial,
        safecopy_journal::Root::Unix { device, .. } => *device,
    }
}

fn disk_of(found: &Found) -> Option<u64> {
    match &found.record {
        Record::Receiving(r) => Some(
            r.at.location
                .levels
                .last()
                .map_or_else(|| root_device(&r.at.location.root), |l| l.device),
        ),
        Record::Sending(_) => None,
    }
}

fn have_of(found: &Found) -> u64 {
    match &found.record {
        Record::Receiving(r) => r.have,
        Record::Sending(_) => 0,
    }
}

/// Whether a kept partial is still whole: there, at least `have` bytes,
/// its first `have` hashing to `prefix`.
async fn check_partial(record: &Receiving) -> bool {
    let Some((_, file)) = record.at.reopen() else {
        return false;
    };
    if file.metadata().map(|m| m.len()).unwrap_or(0) < record.have {
        return false;
    }
    let mut file = tokio::fs::File::from_std(file);
    matches!(hash_prefix(&mut file, record.have).await, Ok(d) if d == record.prefix)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
