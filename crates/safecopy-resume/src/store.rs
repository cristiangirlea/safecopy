//! The resume records of one program, in a folder it passes.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
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
    /// Bytes the kept files hold after it.
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

#[derive(Debug, Default)]
struct State {
    /// The record files this store holds, as their only writer.
    held: HashMap<String, File>,
    /// What each record file held when it was last read.
    index: HashMap<String, Indexed>,
    /// The files a save of this store is on ([`Saving`]).
    saving: HashSet<(PeerId, TransferKey)>,
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
/// **Blocking.** Every function here but the three async ones is
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

    /// The journal's settings the store removes partial files by: which
    /// ending a partial's name has.
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
                        Some(file) => read_held(file),
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
    /// file holding its key, then flushed to the disk. A file another
    /// store holds is not written, nor one that does not show it is this
    /// record's: [`ResumeError::Busy`].
    fn write(&self, record: &Record) -> Result<(), ResumeError> {
        let receiving = matches!(record, Record::Receiving(_));
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
        // Where the newest record of the file is: its name, sequence
        // number and slot.
        let mut place = None;
        if let Some(found) = existing {
            match self.hold(&mut state, &found.name)? {
                Hold::Had => place = Some((found.name, found.seq, found.slot)),
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
        let (name, seq, newest, new) = match place {
            Some((name, seq, slot)) => (name, seq, slot, false),
            None => self.make(&mut state, record)?,
        };
        let (seq, at) = (seq + 1, 1 - newest);
        let bytes = slot(seq, &payload).ok_or(ResumeError::TooLarge { most: SLOT })?;
        let written = (|| -> Result<bool, ResumeError> {
            let mut file = state.held.get(&name).expect("held above");
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
        match written {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(e) => {
                // A new file that holds no record is not left: nothing
                // would read it, and this store would hold it for good.
                if new && let Some(file) = state.held.remove(&name) {
                    let _ = files::delete(&file, &self.dir.join(&name));
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
        let exists = match files::create(&self.dir.join(&name)) {
            Ok(file) => {
                state.held.insert(name.clone(), file);
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
                let bytes = state.held.get(&name).and_then(read_held);
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
            None => had || state.held.get(&name).is_some_and(empty),
        };
        if !own {
            if !had {
                self.drop_hold(state, &name);
            }
            return Err(ResumeError::Busy);
        }
        let (seq, slot) = now.map_or((0, 1), |now| (now.seq, now.slot));
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
                state.held.insert(name.to_string(), file);
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
    fn take_received(&self, peer: PeerId, key: TransferKey) -> Result<Found, ResumeError> {
        let found = self.find(peer, key, true).ok_or(ResumeError::NotHeld)?;
        let mut state = lock(&self.state);
        let now = match self.hold(&mut state, &found.name)? {
            Hold::Had => return Err(ResumeError::Busy),
            Hold::Taken(now) => now.map(|now| *now),
            Hold::Gone => None,
        };
        let now = now.filter(
            |now| matches!(&now.record, Record::Receiving(r) if r.peer == peer && r.key == key),
        );
        if now.is_none() {
            self.drop_hold(&mut state, &found.name);
        }
        now.ok_or(ResumeError::NotHeld)
    }

    /// What a reopening that failed leaves: nothing held by this store.
    /// The record goes, with its partial, when the partial is no longer
    /// what it says ([`lost`]); otherwise it is let go as it is.
    fn settle<T>(&self, found: &Found, opened: Result<T, ResumeError>) -> Result<T, ResumeError> {
        match &opened {
            Ok(_) => {}
            Err(e) if lost(e) => {
                self.remove(found);
            }
            Err(_) => self.drop_hold(&mut lock(&self.state), &found.name),
        }
        opened
    }

    /// The record file `name`, for this store alone: the handle it holds,
    /// or taken whole, so one another store holds is left. `None` when it
    /// cannot be had, or when one taken no longer holds the record of
    /// sequence number `seq` (`None`: no record that reads): it was judged
    /// by an older reading, and is left for the next look.
    ///
    /// With `own`, one this store holds is given up to the claim: the
    /// caller's own word (a forget, an expiry past its `live` list).
    /// Without, it is left: a save of this store's may be using it.
    fn claim(&self, name: &str, seq: Option<u64>, own: bool) -> Option<File> {
        let mut state = lock(&self.state);
        if state.held.contains_key(name) {
            if !own {
                return None;
            }
            state.index.remove(name);
            return state.held.remove(name);
        }
        let taken = files::take(&self.dir.join(name), MOST);
        state.index.remove(name);
        let (file, bytes) = taken.ok()?;
        let now = newest(&bytes).map(|(_, seq, _)| seq);
        (now == seq).then_some(file)
    }

    /// Removes the record file `name`, which no slot of reads, claimed
    /// first.
    fn remove_file(&self, name: &str) -> bool {
        match self.claim(name, None, true) {
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
        let Some(file) = self.claim(&found.name, Some(found.seq), own) else {
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

    /// What this side keeps from `peer`, by the records and the folders'
    /// listings alone: each receiving record of `peer` whose partial file
    /// is still the recorded one under its name
    /// ([`PartialAt::present`]) and at least `have` bytes long. Oldest
    /// first, at most [`Limits::per_peer`].
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
            if record.peer != peer || record.at.present().is_none_or(|len| len < record.have) {
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
            match hash_partial(record, &self.chunks).await {
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

    /// The kept partial of `key` from `peer`, opened by identity for
    /// writing and cut to its `have`, to append the rest; with its folder.
    /// Its bytes are not read: [`Self::reopen_hashed`] checks them. The
    /// folder is opened by identity, which on Windows gives it no path: a
    /// file published into it reports its name alone.
    ///
    /// The record is taken first, and stays held while the file is
    /// continued. One in use is [`ResumeError::Busy`], and its partial is
    /// not touched: held by another store, held by this one (a save it
    /// started, or a reopening not yet released, on every system), or
    /// with its partial open elsewhere (Windows). A reopening that fails
    /// leaves nothing held: the record is removed with its partial when
    /// that is gone, replaced or shorter than `have`
    /// ([`ResumeError::Gone`], [`ResumeError::WrongSize`]), and let go as
    /// it is otherwise.
    pub fn reopen(&self, peer: PeerId, key: TransferKey) -> Result<(Directory, File), ResumeError> {
        let found = self.take_received(peer, key)?;
        let opened = match &found.record {
            Record::Receiving(record) => open_partial(record).and_then(|(dir, file)| {
                file.set_len(record.have)?;
                Ok((dir, file))
            }),
            Record::Sending(_) => Err(ResumeError::NotHeld),
        };
        self.settle(&found, opened)
    }

    /// [`Self::reopen`], with the partial's first `have` bytes read once
    /// and checked against the record's `prefix`: its folder, and the
    /// partial as a save continues it (`safecopy_chunks::Start::Resume`),
    /// its hasher standing at `have`. Bytes that do not hash to `prefix`
    /// are [`ResumeError::PrefixChanged`], and the record and the partial
    /// are removed: nothing is appended, and the file comes from zero.
    /// Busy records and failures are as [`Self::reopen`] has them.
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
        let found = self.take_received(peer, key)?;
        let Record::Receiving(record) = &found.record else {
            return self.settle(&found, Err(ResumeError::NotHeld));
        };
        // The reading may be long, and its future dropped before it ends.
        let mut reading = Reading {
            store: self,
            name: &found.name,
            under_way: true,
        };
        let hashed = match hash_partial(record, &self.chunks).await {
            Ok((dir, file, hasher)) => match file.set_len(record.have) {
                Ok(()) => Ok((dir, file, hasher)),
                Err(e) => Err(e.into()),
            },
            Err(e) => Err(e),
        };
        reading.under_way = false;
        let (dir, file, hasher) = self.settle(&found, hashed)?;
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
    /// [`ResumeError::Busy`], with nothing removed, while a [`Saving`] is
    /// on it or the store holds its receiving record. The same key coming
    /// on a second connection so never has its record and its partial
    /// removed under the save of the first, whatever a check answered for
    /// the second. The save itself forgets through its guard
    /// ([`Saving::forget`]); a record held with no save left is let go
    /// first ([`Self::release`]). [`Self::remove_all`] and
    /// [`Self::expire`] (for a key not in its `live`) still remove a
    /// file being received: the person's word, and the caller's list.
    /// The refusal is here, and not a question
    /// asked before the call, so that no caller can leave it out or have
    /// the answer change between the two.
    pub fn forget(&self, peer: PeerId, key: TransferKey) -> Result<(), ResumeError> {
        // Taken as a save takes it, for as long as the removal: a save
        // that starts meanwhile is told it is in use.
        let claim = self.saving(peer, key)?;
        let forgotten = self.forget_as(peer, key, false);
        drop(claim);
        forgotten
    }

    /// Removes the records of `key` from or to `peer`. With `saving`, the
    /// word of the save on that file, its receiving record goes held or
    /// not; without, one this store holds stays: a save took it since.
    fn forget_as(&self, peer: PeerId, key: TransferKey, saving: bool) -> Result<(), ResumeError> {
        for (_, found) in self.all()? {
            if let Some(found) = found
                && found.record.peer() == peer
                && found.record.key() == key
            {
                let own = saving || matches!(found.record, Record::Sending(_));
                self.remove_claimed(&found, own);
            }
        }
        Ok(())
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
    /// touched. It reads the store's folder, as a lookup does.
    pub fn saving(&self, peer: PeerId, key: TransferKey) -> Result<Saving<'_>, ResumeError> {
        let all = self.all()?;
        let mut state = lock(&self.state);
        let held = all.into_iter().filter_map(|(_, found)| found).any(|found| {
            matches!(&found.record, Record::Receiving(r) if r.peer == peer && r.key == key)
                && state.held.contains_key(&found.name)
        });
        // Looked up and taken under one lock: of two at once, one is told.
        if held || !state.saving.insert((peer, key)) {
            return Err(ResumeError::Busy);
        }
        Ok(Saving {
            store: self,
            peer,
            key,
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
/// Dropping it writes nothing, and reads a record file only in a store
/// with more of them than its index remembers ([`Limits::indexed`]). On
/// Windows a write of the dropped save still under way on the blocking
/// pool keeps the partial open for a moment longer: a reopening in that
/// moment is [`ResumeError::Busy`], and is tried again. On Unix nothing
/// shuts that reopening out, and such a write (one slice at most) may
/// land after it, at its own offset past the bytes the record names: the
/// bytes the dropped stream brought there. A file whose sender's bytes
/// changed in between is not continued at all (the sender's check), so
/// they are the bytes the continued stream brings too.
#[derive(Debug)]
pub struct Saving<'a> {
    store: &'a Store,
    peer: PeerId,
    key: TransferKey,
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
        self.store.keep(self.peer, held, now, partial, true)?;
        Ok(())
    }

    /// The save published its file, or ended with nothing kept: the
    /// records of the file go, as [`Store::forget`] removes them, with
    /// what is left of a partial, by identity.
    pub fn forget(self) -> Result<(), ResumeError> {
        self.store.forget_as(self.peer, self.key, true)
    }
}

impl Drop for Saving<'_> {
    fn drop(&mut self) {
        let (peer, key) = (self.peer, self.key);
        let mut state = lock(&self.store.state);
        let State { held, index, .. } = &mut *state;
        // Only the record of the file received: a sending one has no save.
        // From what the store holds, with no folder listed: this must not
        // fail. One whose first write was torn is known by its name.
        let own = file_name(true, peer, key);
        let received =
            |r: &Record| matches!(r, Record::Receiving(r) if r.peer == peer && r.key == key);
        let names: Vec<String> = held
            .iter()
            .filter(|(name, file)| {
                **name == own
                    || match index.get(*name) {
                        Some(was) => was.found.as_ref().is_some_and(|f| received(&f.record)),
                        None => read_held(file)
                            .and_then(|bytes| parse(name, &bytes))
                            .is_some_and(|f| received(&f.record)),
                    }
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in names {
            held.remove(&name);
            // No longer its only writer: read again when next looked up.
            index.remove(&name);
        }
        state.saving.remove(&(peer, key));
    }
}

/// A record taken to reopen its partial, while the prefix is read: let go
/// when that reading's future is dropped, so a reopening that was
/// cancelled does not leave the file in use for good.
struct Reading<'a> {
    store: &'a Store,
    name: &'a str,
    under_way: bool,
}

impl Drop for Reading<'_> {
    fn drop(&mut self) {
        if self.under_way {
            self.store
                .drop_hold(&mut lock(&self.store.state), self.name);
        }
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

/// A kept partial opened by identity, with its folder:
/// [`ResumeError::Gone`] when it is not there or another file has its
/// name, [`ResumeError::Busy`] when it is open elsewhere,
/// [`ResumeError::Unreachable`] when its disk or folder cannot be read
/// now, [`ResumeError::WrongSize`] when it is shorter than `have`.
fn open_partial(record: &Receiving) -> Result<(Directory, File), ResumeError> {
    let (dir, file) = record.at.try_reopen().map_err(|left| match left {
        Left::Gone | Left::Replaced => ResumeError::Gone,
        Left::Busy => ResumeError::Busy,
        _ => ResumeError::Unreachable,
    })?;
    if file.metadata()?.len() < record.have {
        return Err(ResumeError::WrongSize);
    }
    Ok((dir, file))
}

/// [`open_partial`], with the hasher over its first `have` bytes, read
/// once: [`ResumeError::PrefixChanged`] when they do not hash to `prefix`.
async fn hash_partial(
    record: &Receiving,
    settings: &safecopy_chunks::Settings,
) -> Result<(Directory, File, Hasher), ResumeError> {
    let (dir, file) = open_partial(record)?;
    #[cfg(test)]
    crate::tests::seams::stall().await;
    #[cfg(test)]
    crate::tests::seams::PREFIX_BYTES.set(crate::tests::seams::PREFIX_BYTES.get() + record.have);
    let mut hasher = settings.receiving();
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
