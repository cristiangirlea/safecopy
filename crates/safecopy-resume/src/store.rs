//! The resume records of one program, in a folder it passes.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime};

use safecopy_chunks::{CHUNK, Hasher, Kept};
use safecopy_folder::Directory;
use safecopy_journal::{Cleaned, Left, PartialAt, files};

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
    /// Bytes the kept files hold after it, those of `live` left out:
    /// [`Store::kept_bytes`] counts them all. Never more than a `u64`
    /// holds.
    pub kept_bytes: u64,
}

/// A record file's length says how often it was written: its two slots
/// and up to this many bytes less one, the sequence number's remainder. A
/// rewrite so changes the length, which a folder's listing shows at once
/// on every system, where it shows a held file's new time only later.
const TAIL: u64 = 256;

/// The longest a record file is.
const MOST: usize = 2 * SLOT + TAIL as usize - 1;

fn file_len(seq: u64) -> u64 {
    2 * SLOT as u64 + seq % TAIL
}

/// What became of a record file this store asked to hold.
enum Hold {
    /// It held it already: what it knows of it is current.
    Had,
    /// Held now, with what it holds read again; `None` when no slot reads.
    Taken(Option<Box<Found>>),
    /// The file is gone.
    Gone,
}

/// One record file read.
#[derive(Debug, Clone)]
struct Found {
    name: String,
    record: Record,
    seq: u64,
    slot: usize,
}

/// What a folder's listing says of a record file, without opening it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
}

/// A record file as the index remembers it.
#[derive(Debug)]
struct Indexed {
    /// The listing's stamp when it was read; `None` for one this store
    /// wrote, which it holds.
    stamp: Option<Stamp>,
    /// What it held; `None` when no slot read.
    found: Option<Found>,
}

/// The lock identity survives rewrites. Its sequence counts the written slots.
#[derive(Debug)]
struct HeldFile {
    file: File,
    id: Arc<()>,
    seq: u64,
}

impl HeldFile {
    fn new(file: File, seq: u64) -> Self {
        Self {
            file,
            seq,
            id: Arc::new(()),
        }
    }
}

#[derive(Debug, Default)]
struct State {
    /// The record files this store holds, as their only writer.
    held: HashMap<String, HeldFile>,
    /// What each record file held when it was last read.
    index: HashMap<String, Indexed>,
    /// The files being received that a save of this store is on
    /// ([`Saving`]), or a removal ([`Claimed`]): by the peer they come
    /// from. A file sent under the same key is not in it.
    saving: HashMap<(PeerId, TransferKey), Option<RecordLock>>,
}

/// The lock acquired for one save, independent of its later slot writes.
#[derive(Debug, Clone)]
struct RecordLock {
    name: String,
    id: Arc<()>,
}

impl State {
    fn track(&mut self, peer: PeerId, key: TransferKey, name: &str) {
        if let Some(owner @ None) = self.saving.get_mut(&(peer, key)) {
            *owner = Some(RecordLock {
                name: name.to_owned(),
                id: self.held[name].id.clone(),
            });
        }
    }

    fn owned(&self, owner: &RecordLock) -> Result<&HeldFile, ResumeError> {
        self.held
            .get(&owner.name)
            .filter(|held| Arc::ptr_eq(&held.id, &owner.id))
            .ok_or(ResumeError::Busy)
    }
}

/// The resume records of one program, one file per transfer in a folder
/// the caller passes (see the [crate] docs).
///
/// **A record in use.** A record a store writes or reopens is held by
/// it, as the record's only writer, until the store is dropped, forgets
/// it or lets it go ([`Self::release`], [`Self::release_peer`]): with
/// read sharing only on Windows, under an exclusive lock on Unix. While
/// it is held, the store takes a save of its own to be using the partial
/// file: it does not reopen it a second time ([`ResumeError::Busy`]), so
/// one key coming on two connections never has its partial cut under the
/// save appending to it, and [`Self::held_from`] and
/// [`Self::held_hashed`] pass it by, neither hashing nor removing it.
/// Let a record go when its save has ended without publishing, and
/// forget it when the file is published: a save does both through its
/// guard ([`Self::saving`], [`Saving`]), which also lets the record go
/// when the save's future is dropped or it panics.
///
/// **A file under a record's name that is not that record.** A record
/// file is named by its kind, peer and key, which anyone can work out,
/// so another program may put a file there first. The store writes a
/// file it did not make only when its bytes read as that record and it
/// has that one name; an empty one, not yet written, is taken. Any other
/// is [`ResumeError::Busy`] and never written, cut or reopened: bytes
/// that read as nothing or as another record, a symbolic link, a file
/// with a second name. [`Self::expire`] removes the name of a regular
/// file among them; a link stays, and so does a folder there (the
/// system's error, [`ResumeError::Io`]), until the person removes it. A
/// record whose first
/// write was torn by a crash is such a file too, and its key has no
/// record until then: run [`Self::expire`] at each start.
///
/// **Blocking.** Every function here but the async ones is
/// synchronous and does file work, most of it under the store's lock: on
/// an async runtime, call them where blocking is allowed (see the
/// [crate] docs). No async function holds the lock across a wait.
///
/// **Several stores on one folder** (several processes). Another store
/// still reads a held record, so its lookups, [`Self::keeps`],
/// [`Self::kept_by`] and [`Self::held_from`] see it; it cannot write it
/// or reopen its partial ([`ResumeError::Busy`]), nor remove either. A
/// record file is named by its kind, peer and key, so two stores that
/// first write the same one at the same moment make one file, and the
/// second is told [`ResumeError::Busy`].
///
/// **Lookups** go through an index in memory, built when the store opens:
/// what each record file held when it was last read. Each lookup lists the
/// folder once and opens only the files the listing shows as new or
/// changed (another length or modified time; every rewrite changes a
/// record file's length), and forgets the ones that are gone. A record
/// this store holds is never read again: nothing else writes it. Before a
/// store writes a record it does not hold, it takes it and reads it again
/// through that handle, so a write never follows a stale reading. The
/// index remembers at most [`Limits::indexed`] files; any past that are
/// read at each lookup.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    limits: Limits,
    journal: safecopy_journal::Settings,
    chunks: safecopy_chunks::Settings,
    state: Mutex<State>,
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
        let store = Self {
            dir: dir.to_path_buf(),
            limits,
            journal: safecopy_journal::Settings::default(),
            chunks: safecopy_chunks::Settings::default(),
            state: Mutex::new(State::default()),
        };
        // The index, from what is there.
        store.all()?;
        Ok(store)
    }

    /// The journal's settings the store knows a partial file by: which
    /// ending its name has. A record naming a file without it is never
    /// written, and one found is never listed, reopened, cut or removed
    /// through ([`PartialAt::is_partial`]).
    pub fn with_journal_settings(mut self, settings: safecopy_journal::Settings) -> Self {
        self.journal = settings;
        self
    }

    /// The settings a receiver's hasher is made with
    /// ([`Self::held_hashed`], [`Self::reopen_hashed`]): how many chunk
    /// boundaries it keeps.
    pub fn with_chunk_settings(mut self, settings: safecopy_chunks::Settings) -> Self {
        self.chunks = settings;
        self
    }

    /// Its limits.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Every record file's name, and what it holds when a slot reads:
    /// from the index, reading only the files the listing shows as new or
    /// changed.
    fn all(&self) -> Result<Vec<(String, Option<Found>)>, ResumeError> {
        let mut state = lock(&self.state);
        let State { held, index, .. } = &mut *state;
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension() != Some(OsStr::new(EXTENSION)) {
                continue;
            }
            let Some(name) = path.file_name().and_then(OsStr::to_str) else {
                continue;
            };
            let stamp = entry.metadata().ok().map(|meta| Stamp {
                len: meta.len(),
                modified: meta.modified().ok(),
            });
            // A file it holds has no other writer; another is as it was
            // read while the listing says the same of it.
            let current = index
                .get(name)
                .filter(|was| held.contains_key(name) || (stamp.is_some() && was.stamp == stamp));
            let found = match current {
                Some(was) => was.found.clone(),
                None => {
                    let bytes = match held.get(name) {
                        Some(held) => read_held(&held.file),
                        None => read_path(&path),
                    };
                    let found = bytes.and_then(|bytes| parse(name, &bytes));
                    if index.len() < self.limits.indexed || index.contains_key(name) {
                        let found = found.clone();
                        index.insert(name.to_string(), Indexed { stamp, found });
                    }
                    found
                }
            };
            seen.insert(name.to_string());
            out.push((name.to_string(), found));
        }
        index.retain(|name, _| seen.contains(name));
        held.retain(|name, _| seen.contains(name));
        Ok(out)
    }

    fn find(&self, peer: PeerId, key: TransferKey, receiving: bool) -> Option<Found> {
        self.all().ok()?.into_iter().find_map(|(_, found)| {
            let found = found?;
            let kind = matches!(found.record, Record::Receiving(_)) == receiving;
            (kind && found.record.peer() == peer && found.record.key() == key).then_some(found)
        })
    }

    /// The receiving record of `key` from `peer`. It lists the store's
    /// folder, as every lookup does: file work.
    pub fn received(&self, peer: PeerId, key: TransferKey) -> Option<Receiving> {
        match self.find(peer, key, true)?.record {
            Record::Receiving(r) => Some(r),
            Record::Sending(_) => None,
        }
    }

    /// The sending record of `key` to `peer`. It lists the store's
    /// folder, as every lookup does: file work.
    pub fn sent(&self, peer: PeerId, key: TransferKey) -> Option<Sending> {
        match self.find(peer, key, false)?.record {
            Record::Sending(s) => Some(s),
            Record::Receiving(_) => None,
        }
    }

    /// Writes `record`: into a new file, or over the older slot of the
    /// file holding its key, then flushed to the disk. A file another
    /// store holds is not written, nor one that does not show it is this
    /// record's: [`ResumeError::Busy`]. A receiving record whose file is
    /// not named as a partial is not written:
    /// [`ResumeError::NotPartial`].
    fn write(&self, record: &Record) -> Result<(), ResumeError> {
        self.write_owned(record, None)
    }

    fn write_owned(&self, record: &Record, owner: Option<&RecordLock>) -> Result<(), ResumeError> {
        let receiving = matches!(record, Record::Receiving(_));
        // Here, where every record passes: none is kept that would name
        // a file the store never opens, cuts or removes.
        if let Record::Receiving(r) = record
            && !r.at.is_partial(&self.journal)
        {
            return Err(ResumeError::NotPartial);
        }
        let existing = self.find(record.peer(), record.key(), receiving);
        let payload = record.encode();
        // Before a file is made for it.
        if slot(0, &payload).is_none() {
            return Err(ResumeError::TooLarge { most: SLOT });
        }
        #[cfg(test)]
        if let Some(hook) = crate::tests::seams::LOOKED.take() {
            hook();
        }
        let mut state = lock(&self.state);
        if let Some(owner) = owner {
            state.owned(owner)?;
            if existing
                .as_ref()
                .is_none_or(|found| found.name != owner.name)
            {
                return Err(ResumeError::Busy);
            }
        }
        // Where the newest record of the file is: its name, sequence
        // number and slot.
        let mut place = None;
        if let Some(found) = existing {
            match self.hold(&mut state, &found.name)? {
                Hold::Had => {
                    let held = &state.held[&found.name];
                    let current = if held.seq == found.seq {
                        found
                    } else {
                        read_held(&held.file)
                            .and_then(|bytes| parse(&found.name, &bytes))
                            .filter(|now| same_record(&now.record, record))
                            .ok_or(ResumeError::Busy)?
                    };
                    place = Some((current.name, current.seq, current.slot));
                }
                // Another store may have written it since it was read, or
                // another file may be under its name: only one that still
                // reads as this record is written.
                Hold::Taken(now) => match now.filter(|now| same_record(&now.record, record)) {
                    Some(now) => place = Some((found.name, now.seq, now.slot)),
                    None => {
                        self.drop_hold(&mut state, &found.name);
                        return Err(ResumeError::Busy);
                    }
                },
                // Removed since: written as a new one.
                Hold::Gone => {}
            }
        }
        let (name, seq, latest_slot, new) = match place {
            Some((name, seq, slot)) => (name, seq, slot, false),
            None => self.make(&mut state, record)?,
        };
        let seq = seq.checked_add(1).ok_or(ResumeError::Damaged)?;
        let at = 1 - latest_slot;
        let bytes = slot(seq, &payload).ok_or(ResumeError::TooLarge { most: SLOT })?;
        let written = (|| -> Result<bool, ResumeError> {
            let held = &state.held[&name];
            let mut file = &held.file;
            file.seek(SeekFrom::Start((at * SLOT) as u64))?;
            #[cfg(test)]
            if crate::tests::seams::FULL.replace(false) {
                return Err(ResumeError::Io(std::io::ErrorKind::StorageFull));
            }
            #[cfg(test)]
            if crate::tests::seams::TEAR.replace(false) {
                // Cut within its payload: the crash came halfway through.
                file.write_all(&bytes[..(12 + payload.len()) / 2])?;
                return Ok(false);
            }
            file.write_all(&bytes)?;
            // After the bytes, so a store that sees the new length reads them.
            file.set_len(file_len(seq))?;
            file.sync_data()?;
            if new {
                // The new file's name reaches the disk too.
                #[cfg(test)]
                crate::tests::seams::DIR_SYNCS.set(crate::tests::seams::DIR_SYNCS.get() + 1);
                files::sync_dir(&self.dir)?;
            }
            Ok(true)
        })();
        if receiving && (written.is_ok() || !new) {
            state.track(record.peer(), record.key(), &name);
        }
        match written {
            Ok(true) => state.held.get_mut(&name).expect("held above").seq = seq,
            Ok(false) => return Ok(()),
            Err(e) => {
                // A new file that holds no record is not left: nothing
                // would read it, and this store would hold it for good.
                if new && let Some(file) = state.held.remove(&name) {
                    let _ = files::delete(&file.file, &self.dir.join(&name));
                } else if let Some(held) = state.held.get_mut(&name) {
                    // An unreadable slot cannot vouch for the earlier generation.
                    held.seq = read_held(&held.file)
                        .and_then(|b| newest(&b))
                        .map_or(seq, |(_, current, _)| current);
                    state.index.remove(&name);
                }
                return Err(e);
            }
        }
        let found = Some(Found {
            name: name.clone(),
            record: record.clone(),
            seq,
            slot: at,
        });
        if state.index.len() < self.limits.indexed || state.index.contains_key(&name) {
            state.index.insert(name, Indexed { stamp: None, found });
        }
        Ok(())
    }

    /// The file for a record that `find` showed none of, held by this
    /// store: its name, the sequence number and slot of the newest record
    /// in it (0 and 1 when it has none), and whether it was made now.
    ///
    /// It is made under the one name its transfer has. When another
    /// store made it since this one looked, whoever holds it has the
    /// transfer ([`ResumeError::Busy`]); one nobody holds is taken when
    /// it holds this record, or nothing at all.
    ///
    /// The name can be worked out by anyone, so a file found under it is
    /// written only when it shows it is this record's: any other, with
    /// bytes that do not read or another record's, is
    /// [`ResumeError::Busy`] and not touched, and [`Store::expire`]
    /// removes its name. A file with a second name is refused before
    /// that, where it is held.
    fn make(
        &self,
        state: &mut State,
        record: &Record,
    ) -> Result<(String, u64, usize, bool), ResumeError> {
        let receiving = matches!(record, Record::Receiving(_));
        let name = file_name(receiving, record.peer(), record.key());
        #[cfg(test)]
        if let Some(why) = crate::tests::seams::REFUSED_RECORD.take() {
            return Err(std::io::Error::other(why).into());
        }
        let exists = match files::create(&self.dir.join(&name)) {
            Ok(file) => {
                state.held.insert(name.clone(), HeldFile::new(file, 0));
                return Ok((name, 0, 1, true));
            }
            Err(e) => e,
        };
        // On Unix a file is made, then locked: another store may lock it between.
        if exists.kind() != std::io::ErrorKind::AlreadyExists && !files::busy(&exists) {
            return Err(exists.into());
        }
        let (now, had) = match self.hold(state, &name)? {
            Hold::Taken(now) => (now.map(|now| *now), false),
            // A write of this store's own since this one looked, or one
            // that was torn: what the file holds now is read, so this
            // record is the newer of the two.
            Hold::Had => {
                let bytes = state.held.get(&name).and_then(|held| read_held(&held.file));
                (bytes.and_then(|bytes| parse(&name, &bytes)), true)
            }
            // Removed in this very moment: the caller's next write makes it.
            Hold::Gone => return Err(ResumeError::Busy),
        };
        let own = match &now {
            Some(now) => same_record(&now.record, record),
            // One this store made, with its first write torn, is its own.
            // One it took just now is only when it is empty: a store made
            // it and has not written yet, and no byte is lost by a write.
            None => had || state.held.get(&name).is_some_and(|held| empty(&held.file)),
        };
        if !own {
            if !had {
                self.drop_hold(state, &name);
            }
            return Err(ResumeError::Busy);
        }
        let (seq, slot) = now.map_or((state.held[&name].seq, 1), |now| (now.seq, now.slot));
        Ok((name, seq, slot, false))
    }

    /// Makes this store the only writer of the record file `name`, read
    /// again through the handle when it was not held before:
    /// [`ResumeError::Busy`] when another store holds it, or when it is
    /// not a file with this one name.
    fn hold(&self, state: &mut State, name: &str) -> Result<Hold, ResumeError> {
        if state.held.contains_key(name) {
            return Ok(Hold::Had);
        }
        match files::hold(&self.dir.join(name)) {
            Ok(file) => {
                let now = read_held(&file).and_then(|bytes| parse(name, &bytes));
                let seq = now.as_ref().map_or(0, |found| found.seq);
                state
                    .held
                    .insert(name.to_string(), HeldFile::new(file, seq));
                if let Some(was) = state.index.get_mut(name) {
                    *was = Indexed {
                        stamp: None,
                        found: now.clone(),
                    };
                }
                Ok(Hold::Taken(now.map(Box::new)))
            }
            // Held elsewhere, or not a file a record is written to: a
            // link, or a file with a second name.
            Err(e) if files::busy(&e) || files::refused(&e) => Err(ResumeError::Busy),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                state.index.remove(name);
                Ok(Hold::Gone)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Stops holding the record file `name`: read again when next looked up.
    fn drop_hold(&self, state: &mut State, name: &str) {
        state.held.remove(name);
        state.index.remove(name);
    }

    /// The receiving record of `key` from `peer`, taken by this store to
    /// reopen its partial: the only one that may then touch that file.
    /// One it holds already is in use, by a save it started or by an
    /// earlier reopening: [`ResumeError::Busy`], on every system.
    fn take_received(
        &self,
        peer: PeerId,
        key: TransferKey,
    ) -> Result<(Receiving, Generation<'_>), ResumeError> {
        let found = self.find(peer, key, true).ok_or(ResumeError::NotHeld)?;
        self.take_found(found, peer, key)
    }

    fn take_found(
        &self,
        found: Found,
        peer: PeerId,
        key: TransferKey,
    ) -> Result<(Receiving, Generation<'_>), ResumeError> {
        let mut state = lock(&self.state);
        let now = match self.hold(&mut state, &found.name)? {
            Hold::Had => return Err(ResumeError::Busy),
            Hold::Taken(now) => now.map(|now| *now),
            Hold::Gone => None,
        };
        if let Some(Found {
            record: Record::Receiving(record),
            seq,
            ..
        }) = now
            && record.peer == peer
            && record.key == key
        {
            let id = state.held[&found.name].id.clone();
            let held = Generation {
                store: self,
                owner: RecordLock {
                    name: found.name,
                    id,
                },
                seq,
                release: true,
            };
            return Ok((record, held));
        }
        self.drop_hold(&mut state, &found.name);
        Err(ResumeError::NotHeld)
    }

    /// The record file `name`, for this store alone: the handle it holds,
    /// or taken whole, so one another store holds is left. `None` when it
    /// cannot be had, or when one taken no longer holds the record of
    /// the previous reading (`None`: no record that reads): it was judged
    /// by an older reading, and is left for the next look.
    ///
    /// With `own`, one this store holds is given up to the claim: the
    /// caller's own word (a forget, an expiry past its `live` list).
    /// Without, it is left: a save of this store's may be using it.
    fn claim(&self, name: &str, expected: Option<&Found>, own: bool) -> Option<File> {
        let mut state = lock(&self.state);
        let matches = |bytes: &[u8]| match (newest(bytes), expected) {
            (Some((record, seq, _)), Some(found)) => seq == found.seq && record == found.record,
            (None, None) => true,
            _ => false,
        };
        if let Some(held) = state.held.get(name) {
            if !own || !read_held(&held.file).is_some_and(|bytes| matches(&bytes)) {
                return None;
            }
            state.index.remove(name);
            return state.held.remove(name).map(|held| held.file);
        }
        let taken = files::take(&self.dir.join(name), MOST);
        state.index.remove(name);
        let (file, bytes) = taken.ok()?;
        matches(&bytes).then_some(file)
    }

    /// Removes the record file `name`, which no slot of reads, claimed
    /// first.
    fn remove_file(&self, name: &str) -> bool {
        match self.claim(name, None, false) {
            Some(file) => files::delete(&file, &self.dir.join(name)).is_ok(),
            None => false,
        }
    }

    /// Removes a record and, for a receiving one, its partial file by
    /// identity. The record is claimed first: one another store holds
    /// keeps its record and its partial.
    fn remove(&self, found: &Found) -> bool {
        self.remove_claimed(found, true)
    }

    /// [`Self::remove`] for a listing, which leaves a record this store
    /// holds: nothing said it may go.
    fn remove_unheld(&self, found: &Found) -> bool {
        self.remove_claimed(found, false)
    }

    fn remove_claimed(&self, found: &Found, own: bool) -> bool {
        #[cfg(test)]
        if let Some(hook) = crate::tests::seams::REMOVING.take() {
            hook();
        }
        let Some(file) = self.claim(&found.name, Some(found), own) else {
            return false;
        };
        if let Record::Receiving(r) = &found.record {
            r.at.remove(&self.journal);
        }
        files::delete(&file, &self.dir.join(&found.name)).is_ok()
    }

    /// Lets go of the record files named by `which`, removing none.
    fn let_go(&self, which: impl Fn(&Record) -> bool) -> usize {
        let Ok(all) = self.all() else {
            return 0;
        };
        let mut state = lock(&self.state);
        let mut released = 0;
        for found in all.into_iter().filter_map(|(_, found)| found) {
            if which(&found.record) && state.held.remove(&found.name).is_some() {
                // No longer its only writer: read again when next looked up.
                state.index.remove(&found.name);
                released += 1;
            }
        }
        released
    }

    /// Stops holding the records of `key` from or to `peer`, without
    /// removing them: its save has ended. This store may then list and
    /// reopen the partial again, and another store may write the record,
    /// or remove it with its partial file. Whether any was held. This
    /// store takes one again when it next writes or reopens it: write
    /// first ([`Self::keep`] at a drop), then release.
    pub fn release(&self, peer: PeerId, key: TransferKey) -> bool {
        self.let_go(|record| record.peer() == peer && record.key() == key) > 0
    }

    /// Stops holding every record of `peer`, as [`Self::release`] does for
    /// one. How many were held. It lets go of all of them, whichever
    /// connection of the peer was saving each: call it only when no save
    /// of that peer's still runs, and [`Self::release`] for one file
    /// otherwise. A record let go under a running save is no longer kept
    /// from a second reopening.
    pub fn release_peer(&self, peer: PeerId) -> usize {
        self.let_go(|record| record.peer() == peer)
    }

    /// Starts keeping a file being received, from its first whole chunk:
    /// `record.have` at least one chunk and a whole number of them, within
    /// the size. `partial` is flushed to the disk first, so `have` never
    /// claims a byte that is not there.
    ///
    /// `record.at` must name a partial as the store's journal settings
    /// have it ([`PartialAt::is_partial`],
    /// [`Self::with_journal_settings`]): any other name is
    /// [`ResumeError::NotPartial`], before anything is flushed or
    /// written.
    ///
    /// It flushes `partial` and the record to the disk, each time, and
    /// holds its thread for that long: in a save's `whole` hook on a
    /// multi-threaded runtime, wrap it in `tokio::task::block_in_place`.
    pub fn receiving(&self, record: &Receiving, partial: &File) -> Result<(), ResumeError> {
        if record.have > record.size {
            return Err(ResumeError::PastSize);
        }
        if record.have < CHUNK || !on_chunk(record.have) {
            return Err(ResumeError::OffChunk);
        }
        if !record.at.is_partial(&self.journal) {
            return Err(ResumeError::NotPartial);
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
    /// Whether it was written. A record found there whose file is not
    /// named as a partial ([`PartialAt::is_partial`]) is not this store's
    /// to rewrite: [`ResumeError::NotPartial`], with nothing flushed or
    /// written.
    ///
    /// When it writes, it flushes `partial` and the record to the disk,
    /// and holds its thread for that long: in a save's `whole` hook on a
    /// multi-threaded runtime, wrap it in `tokio::task::block_in_place`.
    pub fn keep(
        &self,
        peer: PeerId,
        held: &Held,
        now: u64,
        partial: &File,
        at_drop: bool,
    ) -> Result<bool, ResumeError> {
        self.keep_owned(peer, held, now, partial, at_drop, None)
    }

    fn keep_owned(
        &self,
        peer: PeerId,
        held: &Held,
        now: u64,
        partial: &File,
        at_drop: bool,
        owner: Option<&RecordLock>,
    ) -> Result<bool, ResumeError> {
        let (key, have, prefix) = (held.key, held.have, held.prefix);
        let mut record = self.received(peer, key).ok_or(ResumeError::NotHeld)?;
        if !record.at.is_partial(&self.journal) {
            return Err(ResumeError::NotPartial);
        }
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
        self.write_owned(&Record::Receiving(record), owner)?;
        Ok(true)
    }

    /// Starts keeping a file being sent: its record is written and
    /// flushed to the disk.
    pub fn sending(&self, record: &Sending) -> Result<(), ResumeError> {
        self.write(&Record::Sending(record.clone()))
    }

    /// What this side keeps from `peer`, by the records and the folders'
    /// listings alone: each receiving record of `peer` whose partial file
    /// is still the recorded one under its name
    /// ([`PartialAt::present`]) and at least `have` bytes long. Oldest
    /// first, at most [`Limits::per_peer`]. A record whose file is not
    /// named as a partial ([`PartialAt::is_partial`]) is not listed, and
    /// nothing is opened for it.
    ///
    /// No byte of a partial is read and no record file whose listing is
    /// unchanged is opened, so the other side cannot make this side read
    /// its kept files by connecting. A partial open in a running save is
    /// listed too. The others are passed by, and nothing is removed:
    /// [`Self::expire`] and [`Self::clean_up`] do that. What is listed is
    /// what the records say; the bytes are checked when a file is
    /// continued ([`Self::reopen_hashed`]).
    ///
    /// It is synchronous: it lists the store's folder under the store's
    /// lock, then inspects each partial's name. On an async runtime, run
    /// it on the blocking pool.
    pub fn kept_by(&self, peer: PeerId) -> Result<Vec<Held>, ResumeError> {
        let mut kept = Vec::new();
        for found in self.all()?.into_iter().filter_map(|(_, found)| found) {
            let Record::Receiving(record) = found.record else {
                continue;
            };
            if record.peer != peer
                || record
                    .at
                    .present(&self.journal)
                    .is_none_or(|len| len < record.have)
            {
                continue;
            }
            let held = Held {
                key: record.key,
                have: record.have,
                prefix: record.prefix,
            };
            kept.push((record.created, record.last, held));
        }
        kept.sort_by_key(|(created, last, _)| (*created, *last));
        kept.truncate(self.limits.per_peer);
        Ok(kept.into_iter().map(|(_, _, held)| held).collect())
    }

    /// What this side keeps from `peer`, each checked first: its partial
    /// opened by identity, at least `have` bytes long, its first `have`
    /// bytes hashing to `prefix`. Oldest first, at most
    /// [`Limits::per_peer`]. [`Self::held_hashed`] without the hashers.
    ///
    /// It reads `have` bytes of every kept file: for a check the program
    /// itself decides on, never for each connection of the other side,
    /// which [`Self::kept_by`] answers without reading.
    ///
    /// A record is removed, with its partial, and not listed, only when
    /// the check says the file is no longer what was recorded: gone or
    /// replaced under its name, shorter than `have`, or with other bytes
    /// in its prefix. One whose partial is open elsewhere (Windows: a
    /// save is writing it, or a [`Kept`] is still held) or cannot be
    /// reached now is passed by and stays. So is every record this store
    /// holds: its own save may be using it.
    pub async fn held_from(&self, peer: PeerId) -> Result<Vec<Held>, ResumeError> {
        let hashed = self.held_hashed(peer).await?;
        Ok(hashed.into_iter().map(|(held, _)| held).collect())
    }

    /// [`Self::held_from`], each with the hasher its check built: standing
    /// at `have`, over the bytes of the partial as they were read.
    ///
    /// No handle is kept on the partial after the check, so the hasher
    /// says nothing of bytes changed since. Continue a file from it
    /// ([`Self::reopen`], [`Kept`]) only where nothing else can have
    /// written the partial in between; otherwise a changed prefix is
    /// published under a hash that matches the sender's.
    /// [`Self::reopen_hashed`] reads the prefix through the handle it
    /// hands on, and is the one to use when in doubt.
    pub async fn held_hashed(&self, peer: PeerId) -> Result<Vec<(Held, Hasher)>, ResumeError> {
        let mut kept = Vec::new();
        for (_, found) in self.all()? {
            let Some(found) = found else {
                continue;
            };
            let Record::Receiving(record) = &found.record else {
                continue;
            };
            // One this store holds is in use by it, as far as it knows.
            if record.peer != peer || lock(&self.state).held.contains_key(&found.name) {
                continue;
            }
            match hash_partial(record, &self.journal, &self.chunks).await {
                Ok((_, _, hasher)) => {
                    let held = Held {
                        key: record.key,
                        have: record.have,
                        prefix: record.prefix,
                    };
                    kept.push((record.created, record.last, held, hasher));
                }
                Err(e) if lost(&e) => {
                    self.remove_unheld(&found);
                }
                // Open elsewhere, or not reachable now: it may be whole.
                Err(_) => {}
            }
        }
        kept.sort_by_key(|(created, last, ..)| (*created, *last));
        kept.truncate(self.limits.per_peer);
        Ok(kept
            .into_iter()
            .map(|(_, _, held, hasher)| (held, hasher))
            .collect())
    }

    /// Claims the current receiving record and checks its prefix without cutting
    /// or removing anything. Inspect the claimed record before choosing an act.
    /// A record in use gives `Busy`; a partial's error is on the returned claim.
    /// Dropping the future or the claim releases this generation.
    pub async fn inspect(
        &self,
        peer: PeerId,
        key: TransferKey,
    ) -> Result<Reopening<'_>, ResumeError> {
        let claim = self.claim_received(peer, key)?;
        let mut records = self.all()?.into_iter().filter_map(|(_, f)| f).filter(
            |f| matches!(&f.record, Record::Receiving(r) if r.peer == peer && r.key == key),
        );
        let found = records.next().ok_or(ResumeError::NotHeld)?;
        if records.next().is_some() {
            return Err(ResumeError::Busy);
        }
        let (record, held) = self.take_found(found, peer, key)?;
        let checked = self.checked(&record).await;
        Ok(Reopening {
            claim,
            held,
            record,
            checked,
        })
    }

    /// The kept partial of `key` from `peer`, opened by identity for
    /// writing and cut to its `have`, to append the rest; with its folder.
    /// Its bytes are not read: [`Self::reopen_hashed`] checks them. The
    /// folder is opened by identity, which on Windows gives it no path: a
    /// file published into it reports its name alone.
    ///
    /// Use [`Self::inspect`] when the caller decides whether to cut or discard.
    /// The record is taken first, and stays held while the file is
    /// continued. One in use is [`ResumeError::Busy`], and its partial is
    /// not touched: held by another store, held by this one (a save it
    /// started, or a reopening not yet released, on every system), or
    /// with its partial open elsewhere (Windows). A reopening that fails
    /// leaves nothing held: the record is removed with its partial when
    /// that is gone, replaced or shorter than `have`
    /// ([`ResumeError::Gone`], [`ResumeError::WrongSize`]), and let go as
    /// it is otherwise.
    ///
    /// Only a file named as a partial is opened and cut, as the store's
    /// journal settings have it ([`PartialAt::is_partial`]): a record
    /// naming any other file is [`ResumeError::Gone`] with nothing
    /// opened, and goes; the file it names is not touched. A file that
    /// is named as one is cut and handed out on the record's word
    /// alone: the store's folder must be its program's to write and
    /// nobody else's (see the [crate] docs).
    ///
    /// It is synchronous and does file work: the folder listed, the
    /// record file taken, the partial found by identity, opened and cut.
    pub fn reopen(&self, peer: PeerId, key: TransferKey) -> Result<(Directory, File), ResumeError> {
        let (record, held) = self.take_received(peer, key)?;
        let opened = open_partial(&record, &self.journal);
        held.finish(&record, opened, |(_, file)| file, true)
    }

    /// [`Self::reopen`], with the partial's first `have` bytes read once
    /// and checked against the record's `prefix`: its folder, and the
    /// partial as a save continues it (`safecopy_chunks::Start::Resume`),
    /// its hasher standing at `have`. Bytes that do not hash to `prefix`
    /// are [`ResumeError::PrefixChanged`], and the record and the partial
    /// are removed: nothing is appended, and the file comes from zero.
    /// Busy records and failures are as [`Self::reopen`] has them.
    /// Use [`Self::inspect`] to keep a mismatching partial pending a decision.
    ///
    /// The bytes are read through the handle handed back, the one the
    /// save appends through, so no moment passes between the check and
    /// the save in which the partial is not held. Dropped while it
    /// reads, it leaves the record as it was and not held.
    pub async fn reopen_hashed(
        &self,
        peer: PeerId,
        key: TransferKey,
    ) -> Result<(Directory, Kept), ResumeError> {
        let (record, held) = self.take_received(peer, key)?;
        let checked = self.checked(&record).await;
        held.finish(&record, checked, |(_, kept)| &kept.file, true)
    }

    async fn checked(&self, record: &Receiving) -> Result<(Directory, Kept), ResumeError> {
        let (dir, file, hasher) = hash_partial(record, &self.journal, &self.chunks).await?;
        let partial = record.at.partial.clone().into();
        Ok((
            dir,
            Kept {
                file,
                partial,
                hasher,
            },
        ))
    }

    /// Forgets the file of `key` from or to `peer`: its records go, and a
    /// received partial is removed by identity.
    ///
    /// A file this store is receiving is not forgotten here:
    /// [`ResumeError::Busy`], with nothing of it removed, while a
    /// [`Saving`] is on it or the store holds its receiving record. The
    /// same key coming on a second connection so never has its record
    /// and its partial removed under the save of the first, whatever a
    /// check answered for the second. The save itself forgets through
    /// its guard ([`Saving::forget`]); a record held with no save left is
    /// let go first ([`Self::release`]). [`Self::remove_all`] and
    /// [`Self::expire`] (for a key not in its `live`) still remove a
    /// file being received: the person's word, and the caller's list.
    /// The refusal is here, and not a question
    /// asked before the call, so that no caller can leave it out or have
    /// the answer change between the two.
    ///
    /// A sending record goes first, held or not, whatever is being
    /// received: the other side knows the key of a file sent to it, and a
    /// stream it holds open under that key does not keep the sender's
    /// record. When one was removed, the key is this side's own and the
    /// file to forget was that one: a file being received under the same
    /// key is left to its save, and the answer is `Ok`.
    ///
    /// `Ok` says no record of the key is left that this call found: one
    /// it could not remove (taken by a reopening while it ran, or held
    /// by another store) is [`ResumeError::Busy`] too.
    pub fn forget(&self, peer: PeerId, key: TransferKey) -> Result<(), ResumeError> {
        let sent = self.forget_kind(peer, key, false, true)?;
        // Taken as a save takes the file, for as long as the removal: a
        // save that starts meanwhile is told it is in use.
        let claim = match self.claim_received(peer, key) {
            Ok(claim) => claim,
            Err(ResumeError::Busy) if sent.removed && !sent.left => return Ok(()),
            Err(e) => return Err(e),
        };
        #[cfg(test)]
        if let Some(hook) = crate::tests::seams::FORGETTING.take() {
            hook();
        }
        let received = self.forget_kind(peer, key, true, false);
        drop(claim);
        if sent.left || received?.left {
            return Err(ResumeError::Busy);
        }
        Ok(())
    }

    /// Removes the records of one kind of `key` from or to `peer`, and
    /// says what became of them. With `own`, one this store holds goes
    /// too: a sending record, or the word of the save on that file.
    /// Without, a held one stays: a save took it since.
    fn forget_kind(
        &self,
        peer: PeerId,
        key: TransferKey,
        receiving: bool,
        own: bool,
    ) -> Result<Forgotten, ResumeError> {
        let mut forgotten = Forgotten::default();
        for (_, found) in self.all()? {
            if let Some(found) = found
                && matches!(found.record, Record::Receiving(_)) == receiving
                && found.record.peer() == peer
                && found.record.key() == key
            {
                if self.remove_claimed(&found, own) {
                    forgotten.removed = true;
                } else {
                    forgotten.left = true;
                }
            }
        }
        Ok(forgotten)
    }

    /// Marks the file of `key` from `peer` as in use by a save of this
    /// store, when none is on it: no other guard, and its receiving
    /// record not held. [`ResumeError::Busy`] otherwise.
    fn enter(&self, peer: PeerId, key: TransferKey) -> Result<(), ResumeError> {
        let all = self.all()?;
        let mut state = lock(&self.state);
        let held = all.into_iter().filter_map(|(_, found)| found).any(|found| {
            matches!(&found.record, Record::Receiving(r) if r.peer == peer && r.key == key)
                && state.held.contains_key(&found.name)
        });
        // Looked up and taken under one lock: of two at once, one is told.
        if held || state.saving.contains_key(&(peer, key)) {
            return Err(ResumeError::Busy);
        }
        state.saving.insert((peer, key), None);
        Ok(())
    }

    /// The file of `key` from `peer` taken as [`Self::saving`] takes it,
    /// for a removal: the claim keeps a save from starting and, unlike a
    /// save's guard, lets go of no record when it ends.
    fn claim_received(&self, peer: PeerId, key: TransferKey) -> Result<Claimed<'_>, ResumeError> {
        self.enter(peer, key)?;
        Ok(Claimed {
            store: self,
            peer,
            key,
        })
    }

    /// Takes the file of `key` from `peer` for one save, before anything
    /// is done for a stream that brings it: before a check's answer is
    /// acted on, before [`Self::reopen_hashed`], before the save. The
    /// guard is held around the save, and says how it ended
    /// ([`Saving::kept`], [`Saving::forget`]); dropped without either, by
    /// a save's future dropped or a panic, it lets the record go, so the
    /// file can be continued later and is not in use for good.
    ///
    /// [`ResumeError::Busy`] when a save of this store is on that file
    /// already: another guard is, or the store holds its receiving
    /// record. The stream is then refused, and nothing of the file is
    /// touched. It lists the store's folder, as a lookup does: file
    /// work, to be called where blocking is allowed (see the [crate]
    /// docs).
    pub fn saving(&self, peer: PeerId, key: TransferKey) -> Result<Saving<'_>, ResumeError> {
        Ok(Saving {
            claim: self.claim_received(peer, key)?,
            reserved: false,
        })
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

    /// The bytes the kept files hold, as their records say: of each
    /// receiving record whose file is named as a partial
    /// ([`PartialAt::is_partial`]), the only ones the store acts on.
    /// Never more than a `u64` holds.
    pub fn kept_bytes(&self) -> u64 {
        self.all()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(_, f)| match f?.record {
                Record::Receiving(r) if r.at.is_partial(&self.journal) => Some(r.have),
                _ => None,
            })
            .fold(0, u64::saturating_add)
    }

    /// Whether a valid receiving record keeps the partial `at`: a journal's
    /// cleanup leaves it.
    pub fn keeps(&self, at: &PartialAt) -> bool {
        self.all().unwrap_or_default().into_iter().any(
            |(_, f)| matches!(f, Some(Found { record: Record::Receiving(r), .. }) if same_partial(&r.at, at)),
        )
    }

    /// The journals' cleanup in `journals`, leaving the partials this
    /// store's records keep, and the journals that list them: when a
    /// record is gone or no longer reads, the next cleanup removes its
    /// partial, and then the journal. The journals kept are held to
    /// `settings`, raised to what this store may keep: at least
    /// [`Limits::total`] journals, for at least [`Limits::keep_for`].
    pub fn clean_up(
        &self,
        journals: &Path,
        settings: &safecopy_journal::Settings,
    ) -> Result<Cleaned, ResumeError> {
        let settings = safecopy_journal::Settings {
            keep_most: settings.keep_most.max(self.limits.total),
            keep_for: settings.keep_for.max(self.limits.keep_for),
            ..settings.clone()
        };
        Ok(safecopy_journal::clean_up_keeping(
            journals,
            &settings,
            &|at| self.keeps(at),
        )?)
    }

    /// Removes what is past its time or the caps, never a record of `live`
    /// (the files of sessions still running, moving or paused, each by its
    /// peer and its key: a key names a file to one peer only). Run it at
    /// each start: until it has, a file under a record's name that is not
    /// that record keeps its key without one.
    ///
    /// - a file with no slot that reads, by its name alone (its journal
    ///   entry removes the partial): a record torn at its first write, or
    ///   a file another program put under a record's name;
    /// - a receiving record whose file is not named as a partial
    ///   ([`PartialAt::is_partial`]), by its own name alone, at once and
    ///   whatever its age or `live`: no store wrote it, and it counts
    ///   toward no cap. The file it names is not touched;
    /// - any record whose last byte is older than `keep_for` at `now`;
    /// - past [`Limits::sending`] sending records, the oldest;
    /// - past [`Limits::per_peer`] files of one peer, that peer's oldest;
    /// - past [`Limits::total`] files, the oldest of the peer keeping the
    ///   most, so one peer filling the cap loses its own first;
    /// - past [`Limits::disk_share`] of a disk's free space and what the
    ///   kept files hold there, the oldest on that disk.
    pub fn expire(
        &self,
        now: u64,
        keep_for: Duration,
        live: &[(PeerId, TransferKey)],
    ) -> Result<Expired, ResumeError> {
        let mut expired = Expired::default();
        let mut kept: Vec<Found> = Vec::new();
        let mut sending: Vec<Found> = Vec::new();
        for (name, found) in self.all()? {
            let Some(found) = found else {
                if !lock(&self.state).held.contains_key(&name) && self.remove_file(&name) {
                    expired.removed += 1;
                }
                continue;
            };
            // Not a record the store acts on: it holds no place among
            // the kept, and its folder is not opened for its free space.
            if let Record::Receiving(r) = &found.record
                && !r.at.is_partial(&self.journal)
            {
                expired.removed += usize::from(self.remove(&found));
                continue;
            }
            if live.contains(&(found.record.peer(), found.record.key())) {
                continue;
            }
            let age = Duration::from_secs(now.saturating_sub(found.record.last()));
            if age > keep_for {
                expired.removed += usize::from(self.remove(&found));
                continue;
            }
            if matches!(found.record, Record::Receiving(_)) {
                kept.push(found);
            } else {
                sending.push(found);
            }
        }
        // Sending records hold no file, only their count: the oldest go.
        sending.sort_by_key(|f| (f.record.last(), f.record.peer(), f.record.key()));
        let over = sending.len().saturating_sub(self.limits.sending);
        for found in &sending[..over] {
            expired.removed += usize::from(self.remove(found));
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
            // A record's `have` is whatever its file says: summed wide,
            // so no count of them wraps.
            let holds: u128 = on_disk.iter().map(|&j| u128::from(have_of(&kept[j]))).sum();
            // Removing a kept file frees its bytes: free space and what is held
            // together stay the same while the oldest go.
            let total = u128::from(free).saturating_add(holds);
            let share = |holds: u128| {
                holds.saturating_mul(u128::from(den.max(1))) > total.saturating_mul(u128::from(num))
            };
            let mut holds = holds;
            for &j in &on_disk {
                if !share(holds) {
                    break;
                }
                holds -= u128::from(have_of(&kept[j]));
                drop_at(j, &mut gone);
            }
        }
        expired.kept_bytes = kept
            .iter()
            .zip(&gone)
            .filter(|(_, g)| !**g)
            .map(|(f, _)| have_of(f))
            .fold(0, u64::saturating_add);
        Ok(expired)
    }
}

/// A hold can be released explicitly and its name taken again, even at slot one.
#[derive(Debug)]
struct Generation<'a> {
    store: &'a Store,
    owner: RecordLock,
    seq: u64,
    release: bool,
}

impl Generation<'_> {
    fn current<'s>(&self, state: &'s State) -> Result<&'s File, ResumeError> {
        let held = state.owned(&self.owner)?;
        if held.seq != self.seq {
            return Err(ResumeError::Busy);
        }
        Ok(&held.file)
    }

    fn finish<T>(
        mut self,
        record: &Receiving,
        opened: Result<T, ResumeError>,
        file: impl FnOnce(&T) -> &File,
        remove_lost: bool,
    ) -> Result<T, ResumeError> {
        let mut state = lock(&self.store.state);
        let slot = self.current(&state)?;
        match opened {
            Ok(opened) => {
                file(&opened).set_len(record.have)?;
                state.track(record.peer, record.key, &self.owner.name);
                self.release = false;
                Ok(opened)
            }
            Err(e) => {
                if remove_lost && lost(&e) {
                    record.at.remove(&self.store.journal);
                    let _ = files::delete(slot, &self.store.dir.join(&self.owner.name));
                    self.store.drop_hold(&mut state, &self.owner.name);
                    self.release = false;
                }
                Err(e)
            }
        }
    }
}

impl Drop for Generation<'_> {
    fn drop(&mut self) {
        if self.release {
            let mut state = lock(&self.store.state);
            if self.current(&state).is_ok() {
                self.store.drop_hold(&mut state, &self.owner.name);
            }
        }
    }
}

/// A checked receiving record. Dropping it keeps the record and partial as found.
#[derive(Debug)]
pub struct Reopening<'a> {
    claim: Claimed<'a>,
    held: Generation<'a>,
    record: Receiving,
    checked: Result<(Directory, Kept), ResumeError>,
}

impl<'a> Reopening<'a> {
    /// The record read under the lock. Compare it with what the caller offered.
    pub fn record(&self) -> &Receiving {
        &self.record
    }

    /// Whether the prefix fits. A mismatch, short file, missing file or
    /// temporary failure retains its [`ResumeError`] kind.
    pub fn status(&self) -> Result<(), ResumeError> {
        self.checked.as_ref().map(|_| ()).map_err(|e| *e)
    }

    /// Cuts the checked handle to `have` and hands it and its hasher to the save.
    /// A released or rewritten generation gives [`ResumeError::Busy`].
    pub fn resume(self) -> Result<(Saving<'a>, Directory, Kept), ResumeError> {
        let (dir, kept) =
            self.held
                .finish(&self.record, self.checked, |(_, kept)| &kept.file, false)?;
        Ok((
            Saving {
                claim: self.claim,
                reserved: false,
            },
            dir,
            kept,
        ))
    }

    /// Discards this receiving record and its partial by identity. The returned
    /// save holds the replacement key across its first [`Store::receiving`].
    /// Drop it to leave the key free. A changed generation gives `Busy`.
    pub fn discard(mut self) -> Result<Saving<'a>, ResumeError> {
        let store = self.claim.store;
        let mut state = lock(&store.state);
        let file = self.held.current(&state)?;
        drop(self.checked);
        let name = file_name(true, self.record.peer, self.record.key);
        // Older records have random names. Reserve the writer's name first.
        let reserved = if self.held.owner.name != name {
            Some(files::create(&store.dir.join(&name)).map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists || files::busy(&e) {
                    ResumeError::Busy
                } else {
                    e.into()
                }
            })?)
        } else {
            None
        };
        let discard = || -> Result<(), ResumeError> {
            discard_partial(&self.record, &store.journal)?;
            if reserved.is_some() {
                files::delete(file, &store.dir.join(&self.held.owner.name))?;
                files::sync_dir(&store.dir)?;
            } else {
                // Keep this same lock until the first replacement slot is written.
                file.set_len(0)?;
                file.sync_data()?;
            }
            Ok(())
        };
        if let Err(e) = discard() {
            if let Some(file) = reserved {
                let _ = files::delete(&file, &store.dir.join(&name));
            }
            return Err(e);
        }
        state.index.remove(&self.held.owner.name);
        if let Some(file) = reserved {
            state.held.remove(&self.held.owner.name);
            state
                .held
                .insert(name.clone(), HeldFile::new(file, self.held.seq));
        }
        state.track(self.record.peer, self.record.key, &name);
        self.held.release = false;
        Ok(Saving {
            claim: self.claim,
            reserved: true,
        })
    }
}

/// One save's claim on a file being received, from [`Store::saving`]:
/// while it lives, no other save of its store takes that file, and
/// [`Store::forget`] leaves it.
///
/// Hold it around the save, outside the save's future, and end it by how
/// the save ended:
///
/// - published, or stopped with nothing kept: [`Self::forget`];
/// - stopped with its partial kept: [`Self::kept`];
/// - neither (the save's future dropped, a panic, an early return):
///   dropping it lets the receiving record go, removing nothing, so a
///   later [`Store::reopen_hashed`] continues the file from what the
///   record last said.
///
/// Dropping a replacement before its first write removes its empty reservation.
/// It checks the held slots to distinguish a torn first write from a kept record.
///
/// **A save dropped.** The guard lives outside the save's future, so that
/// future is dropped first, and when its drop returns no write of the
/// save is running and none can start
/// (`safecopy_chunks::save_resumable`): one already handed to the
/// blocking pool has ended, or writes nothing. No byte of the dropped
/// stream so lands in the partial after a later reopening has hashed
/// it, on any system and whatever the other side then sends. The save's
/// drop waits for a write already under way, for as long as that one
/// write takes (64 KiB at most; on a disk that stalls, as long as it
/// stalls). On Windows a flush of the
/// dropped save still under way on the blocking pool keeps the partial
/// open a moment longer: a reopening in that moment is
/// [`ResumeError::Busy`], and is tried again.
#[derive(Debug)]
pub struct Saving<'a> {
    claim: Claimed<'a>,
    reserved: bool,
}

impl Saving<'_> {
    /// The save stopped and kept its partial, now `held.have` whole bytes
    /// hashing to `held.prefix`, its last byte come at `now`: the record
    /// is written at once, after `partial` is flushed
    /// ([`Store::keep`] at a drop), and let go. An error leaves the
    /// record as it was last written, and lets it go all the same.
    ///
    /// It flushes a file and writes a record: on an async runtime, call it
    /// where blocking is allowed (`tokio::task::block_in_place`; the guard
    /// borrows its store, so it does not move to the blocking pool).
    pub fn kept(self, held: &Held, now: u64, partial: &File) -> Result<(), ResumeError> {
        let store = self.claim.store;
        if held.key != self.claim.key {
            return Err(ResumeError::NotHeld);
        }
        let owner = {
            let state = lock(&store.state);
            let owner = self.claim.owner(&state);
            if let Some(owner) = &owner {
                state.owned(owner)?;
            }
            owner
        };
        store.keep_owned(self.claim.peer, held, now, partial, true, owner.as_ref())?;
        Ok(())
    }

    /// The save published its file, or ended with nothing kept: the
    /// records of the file go, as [`Store::forget`] removes them, with
    /// what is left of a partial, by identity.
    /// Use [`Self::forget_received`] to leave the sending record in place.
    pub fn forget(self) -> Result<(), ResumeError> {
        self.claim
            .store
            .forget_kind(self.claim.peer, self.claim.key, false, true)?;
        self.claim
            .store
            .forget_kind(self.claim.peer, self.claim.key, true, true)?;
        Ok(())
    }

    /// Ends this save and forgets only what it received under the key.
    pub fn forget_received(self) -> Result<(), ResumeError> {
        let store = self.claim.store;
        let mut state = lock(&store.state);
        let Some(owner) = self.claim.owner(&state) else {
            return Ok(());
        };
        let held = state.owned(&owner)?;
        if self.reserved && empty(&held.file) {
            return Ok(());
        }
        let bytes = read_held(&held.file).ok_or(ResumeError::Unreachable)?;
        let Some((Record::Receiving(r), _, _)) = newest(&bytes) else {
            return Err(ResumeError::Damaged);
        };
        if r.peer != self.claim.peer || r.key != self.claim.key {
            return Err(ResumeError::Busy);
        }
        discard_partial(&r, &store.journal)?;
        files::delete(&held.file, &store.dir.join(&owner.name))?;
        store.drop_hold(&mut state, &owner.name);
        Ok(())
    }
}

impl Drop for Saving<'_> {
    fn drop(&mut self) {
        let store = self.claim.store;
        let mut state = lock(&store.state);
        if let Some(owner) = self.claim.owner(&state)
            && let Ok(held) = state.owned(&owner)
        {
            if self.reserved && read_held(&held.file).is_some_and(|b| newest(&b).is_none()) {
                let _ = files::delete(&held.file, &store.dir.join(&owner.name));
            }
            store.drop_hold(&mut state, &owner.name);
        }
    }
}

/// What a forget did with the records it found.
#[derive(Debug, Default, Clone, Copy)]
struct Forgotten {
    /// One was removed.
    removed: bool,
    /// One stayed: held by this store or another, or changed since it
    /// was read.
    left: bool,
}

/// A removal's claim on a file being received, as a save's guard claims
/// it. It only stands in the way of a save: ended, it lets go of no
/// record, since it took none, and one a reopening took meanwhile is
/// that reopening's to keep.
#[derive(Debug)]
struct Claimed<'a> {
    store: &'a Store,
    peer: PeerId,
    key: TransferKey,
}

impl Claimed<'_> {
    fn owner(&self, state: &State) -> Option<RecordLock> {
        state.saving.get(&(self.peer, self.key))?.clone()
    }
}

impl Drop for Claimed<'_> {
    fn drop(&mut self) {
        lock(&self.store.state)
            .saving
            .remove(&(self.peer, self.key));
    }
}

/// Whether two places name one partial: the same folder by identity (the
/// last level's, or the root's), the same name in it, the same file ID
/// and no two second identities of one kind that differ (a file made
/// since with the inode of one removed), whatever root each was recorded
/// from.
pub(crate) fn same_partial(a: &PartialAt, b: &PartialAt) -> bool {
    let folder = |at: &PartialAt| {
        at.location.levels.last().map_or_else(
            || match &at.location.root {
                safecopy_journal::Root::Windows { serial, folder, .. } => (*serial, *folder),
                safecopy_journal::Root::Unix { device, inode, .. } => (*device, *inode),
            },
            |level| (level.device, level.id),
        )
    };
    folder(a) == folder(b)
        && a.partial == b.partial
        && a.file == b.file
        && a.second.agrees(b.second)
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

/// Whether `error`, of a kept partial checked, says the file is no longer
/// what its record says, so both go: not there or replaced, too short, or
/// with other bytes. Any other error says nothing of the file (open
/// elsewhere, a disk away), and removes nothing.
fn lost(error: &ResumeError) -> bool {
    matches!(
        error,
        ResumeError::Gone | ResumeError::WrongSize | ResumeError::PrefixChanged
    )
}

/// What a kept partial that was not found again says of its record: the
/// one place a [`Left`] becomes this crate's error. [`ResumeError::Gone`]
/// removes the record, and the partial by its identity, so only what says
/// the file is not there or is another gives it. The rest say nothing of
/// the file, a refusal of `safecopy-folder` that looked at nothing among
/// them: the record stays.
fn not_reopened(left: Left) -> ResumeError {
    match left {
        Left::Gone | Left::Replaced | Left::Name => ResumeError::Gone,
        Left::Busy => ResumeError::Busy,
        Left::Volume | Left::Unreachable | Left::Io => ResumeError::Unreachable,
        // A reason added since: nothing is removed until it has a line here.
        _ => ResumeError::Unreachable,
    }
}

fn discard_partial(
    record: &Receiving,
    settings: &safecopy_journal::Settings,
) -> Result<(), ResumeError> {
    match record.at.remove_checked(settings) {
        Ok(()) | Err(Left::Gone | Left::Replaced | Left::Name) => Ok(()),
        Err(left) => Err(not_reopened(left)),
    }
}

/// A kept partial opened by identity, with its folder:
/// [`ResumeError::Gone`] when it is not there, another file has its
/// name, or the record names a file that is no partial under `journal`
/// (nothing is opened for that one: the caller cuts what this opens),
/// [`ResumeError::Busy`] when it is open elsewhere,
/// [`ResumeError::Unreachable`] when its disk or folder cannot be read
/// now or `safecopy-folder` refused without looking at the file,
/// [`ResumeError::WrongSize`] when it is shorter than `have`.
fn open_partial(
    record: &Receiving,
    journal: &safecopy_journal::Settings,
) -> Result<(Directory, File), ResumeError> {
    // As the folder crate would refuse the partial's name, read as the
    // journal crate reads any error of that open.
    #[cfg(test)]
    if let Some(why) = crate::tests::seams::REFUSED_REOPEN.get() {
        return Err(not_reopened(Left::of(&std::io::Error::other(why))));
    }
    let (dir, file) = record.at.try_reopen(journal).map_err(not_reopened)?;
    if file.metadata()?.len() < record.have {
        return Err(ResumeError::WrongSize);
    }
    Ok((dir, file))
}

/// [`open_partial`], with the hasher over its first `have` bytes, read
/// once: [`ResumeError::PrefixChanged`] when they do not hash to `prefix`.
async fn hash_partial(
    record: &Receiving,
    journal: &safecopy_journal::Settings,
    settings: &safecopy_chunks::Settings,
) -> Result<(Directory, File, Hasher), ResumeError> {
    let (dir, file) = open_partial(record, journal)?;
    #[cfg(test)]
    crate::tests::seams::stall().await;
    #[cfg(test)]
    crate::tests::seams::PREFIX_BYTES.set(crate::tests::seams::PREFIX_BYTES.get() + record.have);
    let mut hasher = settings.receiving();
    // Read from the start of a file just opened, whose position no one
    // else has: this handle is handed on only after the reading.
    let mut reader = tokio::fs::File::from_std(file.try_clone()?);
    hasher.read(&mut reader, record.have).await?;
    if hasher.clone().finish() != record.prefix {
        return Err(ResumeError::PrefixChanged);
    }
    Ok((dir, file, hasher))
}

/// Whether `a` and `b` are records of one transfer: the same kind, peer
/// and key.
fn same_record(a: &Record, b: &Record) -> bool {
    matches!(a, Record::Receiving(_)) == matches!(b, Record::Receiving(_))
        && a.peer() == b.peer()
        && a.key() == b.key()
}

/// Whether the held file has no byte.
fn empty(file: &File) -> bool {
    file.metadata().is_ok_and(|meta| meta.len() == 0)
}

/// What a record file's bytes hold, when a slot reads.
fn parse(name: &str, bytes: &[u8]) -> Option<Found> {
    let (record, seq, slot) = newest(bytes)?;
    Some(Found {
        name: name.to_string(),
        record,
        seq,
        slot,
    })
}

/// The bytes of a record file this store holds, through its handle.
fn read_held(mut file: &File) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.take(MOST as u64 + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= MOST).then_some(bytes)
}

/// The bytes of a record file opened for reading: one another store may
/// hold.
fn read_path(path: &Path) -> Option<Vec<u8>> {
    #[cfg(test)]
    crate::tests::seams::OPENS.set(crate::tests::seams::OPENS.get() + 1);
    let mut bytes = Vec::new();
    let file = files::open_read(path).ok()?;
    file.take(MOST as u64 + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() <= MOST).then_some(bytes)
}

/// A record file's name: the SHA-256 of its kind, peer and key. One
/// transfer has one name, so the second of two stores that first write
/// it at the same moment finds the file made.
pub(crate) fn file_name(receiving: bool, peer: PeerId, key: TransferKey) -> String {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    context.update(&[u8::from(receiving)]);
    context.update(&peer.0);
    context.update(&key.0);
    format!("{}.{EXTENSION}", hex(context.finish().as_ref()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
