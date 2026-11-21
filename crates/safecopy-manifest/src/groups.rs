//! A list cut into groups, and the progress of a transfer counted in them.
//!
//! The list goes group by group, in order. A group is settled when every
//! entry in it is; the **mark** is how many groups are settled from the
//! first, so every entry below the mark's first entry is settled. At most
//! [`GroupLimits::open`] groups are open past the mark, so the progress
//! holds at most that many groups' bits, whatever the list's length.

use std::fmt;

use crate::Entry;

/// How a list is cut into groups. `Default` gives the values below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupLimits {
    /// The most entries in one group: 256.
    pub entries: u32,
    /// The most bytes of files in one group, unless it is one file: 256 MiB.
    pub bytes: u64,
    /// A group ends where the parent folder changes only once it holds at
    /// least this many entries, or [`Self::min_bytes`]: 64.
    pub min_entries: u32,
    /// Or this many bytes: 64 MiB.
    pub min_bytes: u64,
    /// The most groups open at once past the mark: 2.
    pub open: usize,
}

impl Default for GroupLimits {
    fn default() -> Self {
        Self {
            entries: 256,
            bytes: 256 << 20,
            min_entries: 64,
            min_bytes: 64 << 20,
            open: 2,
        }
    }
}

/// One group: the entries `first..first + len` of the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Group {
    /// Its first entry's place.
    pub first: u32,
    /// How many entries.
    pub len: u32,
    /// The bytes of its files.
    pub bytes: u64,
}

impl Group {
    /// Whether `place` is in this group.
    pub fn holds(&self, place: u32) -> bool {
        place >= self.first && place - self.first < self.len
    }

    /// The place after its last entry.
    pub fn end(&self) -> u32 {
        self.first + self.len
    }
}

/// A list cut into groups, by [`groups`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Groups {
    groups: Vec<Group>,
    entries: u32,
    open: usize,
}

/// The folder `path` is in: everything before its last `/` (a folder's
/// own trailing `/` dropped first).
pub(crate) fn parent(path: &str) -> &str {
    let path = path.strip_suffix('/').unwrap_or(path);
    path.rfind('/').map_or("", |at| &path[..at])
}

/// Cuts `entries` into groups, in order: a pure function of the list, so
/// both sides compute the same groups.
///
/// A group ends where the parent folder changes once it holds at least
/// [`GroupLimits::min_entries`] entries or [`GroupLimits::min_bytes`]; it
/// never holds more than [`GroupLimits::entries`] entries, or more than
/// [`GroupLimits::bytes`] unless it is one file. A list longer than
/// `u32::MAX` entries is cut only up to there.
pub fn groups(entries: &[Entry], limits: &GroupLimits) -> Groups {
    let most_entries = limits.entries.max(1);
    let mut groups = Vec::new();
    let mut current: Option<Group> = None;
    // The parent of the entry before, where a run of one folder ends.
    let mut last = "";
    let count = u32::try_from(entries.len()).unwrap_or(u32::MAX);
    for (place, entry) in (0..count).zip(entries) {
        let at = parent(&entry.path);
        if let Some(group) = current {
            let full =
                group.len >= most_entries || group.bytes.saturating_add(entry.size) > limits.bytes;
            let enough = group.len >= limits.min_entries || group.bytes >= limits.min_bytes;
            if full || (at != last && enough) {
                groups.push(group);
                current = None;
            }
        }
        let group = current.get_or_insert(Group {
            first: place,
            len: 0,
            bytes: 0,
        });
        group.len += 1;
        group.bytes = group.bytes.saturating_add(entry.size);
        last = at;
    }
    groups.extend(current);
    Groups {
        groups,
        entries: count,
        open: limits.open.max(1),
    }
}

impl Groups {
    /// The groups, in order.
    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    /// How many entries the list has.
    pub fn entries(&self) -> u32 {
        self.entries
    }

    /// Group `index`.
    pub fn get(&self, index: u32) -> Option<&Group> {
        self.groups.get(index as usize)
    }

    /// The index of the group `place` is in.
    pub fn of(&self, place: u32) -> Option<u32> {
        if place >= self.entries {
            return None;
        }
        let at = self.groups.partition_point(|g| g.end() <= place);
        u32::try_from(at).ok()
    }

    /// Whether the entry at `place` may start now, by the open-groups rule:
    /// it is not settled nor being written, and its group is open, or is
    /// the next one and may open. The next group opens once every entry of
    /// the newest open group has started (settled or being written) and
    /// fewer than [`GroupLimits::open`] groups are open; the one after
    /// waits until the oldest finishes and the mark moves.
    pub fn may_start(&self, place: u32, progress: &Progress) -> bool {
        let Some(index) = self.of(place) else {
            return false;
        };
        if progress.skips(place) || progress.writing(place) {
            return false;
        }
        if progress.open.iter().any(|g| g.index == index) {
            return true;
        }
        let next = progress.mark as usize + progress.open.len();
        if index as usize != next || progress.open.len() >= self.open {
            return false;
        }
        progress.open.last().is_none_or(|newest| {
            (newest.first..newest.first + newest.len)
                .all(|p| progress.skips(p) || progress.writing(p))
        })
    }

    /// Opens group `index` in `progress`, as [`Progress::open`] does with
    /// this list's group.
    pub fn open(&self, index: u32, progress: &mut Progress) -> Result<(), ProgressError> {
        let group = self.get(index).ok_or(ProgressError::NoGroup)?;
        progress.open(index, group.first, group.len)
    }
}

/// Why a change to a [`Progress`] is refused: it would not describe a
/// transfer that goes group by group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProgressError {
    /// The list has no such group.
    NoGroup,
    /// A group opened out of order: not the one right after the newest
    /// open one (or the mark), or not starting where the one before ended.
    OutOfOrder,
    /// A group longer than [`Progress::most_entries`], or empty.
    BadLength,
    /// More groups open than [`GroupLimits::open`].
    TooManyOpen,
    /// The place is in no open group.
    NotOpen,
    /// The place is already settled.
    Settled,
    /// The place is already being written.
    Writing,
    /// More partials than [`Progress::most_partials`].
    TooManyPartials,
}

impl fmt::Display for ProgressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The caller words it; this is the kind for logs and tests.
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for ProgressError {}

/// One group open past the mark, and which of its entries are settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenGroup {
    /// The group's index.
    pub index: u32,
    /// Its first entry's place.
    pub first: u32,
    /// How many entries.
    pub len: u32,
    settled: Vec<u64>,
}

impl OpenGroup {
    fn new(index: u32, first: u32, len: u32) -> Self {
        Self {
            index,
            first,
            len,
            settled: vec![0; len.div_ceil(64) as usize],
        }
    }

    /// Whether the entry at `place` (in this group) is settled.
    pub fn is_settled(&self, place: u32) -> bool {
        let Some(bit) = place.checked_sub(self.first).filter(|b| *b < self.len) else {
            return false;
        };
        self.settled[(bit / 64) as usize] & (1 << (bit % 64)) != 0
    }

    fn settle(&mut self, place: u32) {
        let bit = place - self.first;
        self.settled[(bit / 64) as usize] |= 1 << (bit % 64);
    }

    /// One bit per entry, the first entry's the lowest bit of the first
    /// byte: `len.div_ceil(8)` bytes.
    pub fn bits(&self) -> Vec<u8> {
        let mut bytes: Vec<u8> = self.settled.iter().flat_map(|w| w.to_le_bytes()).collect();
        bytes.truncate(self.len.div_ceil(8) as usize);
        bytes
    }

    fn whole(&self) -> bool {
        (self.first..self.first + self.len).all(|p| self.is_settled(p))
    }
}

/// What is done of a list that goes group by group: the mark, the open
/// groups' settled entries, and the places being written. Its size stays
/// bounded by the open groups and the partials, whatever the list's
/// length.
///
/// A journal (`safecopy-journal`) keeps one on the disk; a program keeps
/// one in memory for a rejoin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    mark: u32,
    below: u32,
    open: Vec<OpenGroup>,
    partials: Vec<u32>,
    most_entries: u32,
    most_open: usize,
    most_partials: usize,
}

impl Default for Progress {
    fn default() -> Self {
        Self::new(&GroupLimits::default(), 134)
    }
}

impl Progress {
    /// Nothing done yet. `limits` bounds each group's length and how many
    /// are open; `partials` how many places may be written at once (134
    /// for six single files and two bundles of 64).
    pub fn new(limits: &GroupLimits, partials: usize) -> Self {
        Self {
            mark: 0,
            below: 0,
            open: Vec::new(),
            partials: Vec::new(),
            most_entries: limits.entries.max(1),
            most_open: limits.open.max(1),
            most_partials: partials,
        }
    }

    /// How many groups are settled from the first.
    pub fn mark(&self) -> u32 {
        self.mark
    }

    /// The first place of the group at the mark: every place below it is
    /// settled.
    pub fn below(&self) -> u32 {
        self.below
    }

    /// The groups open past the mark, oldest first.
    pub fn open_groups(&self) -> &[OpenGroup] {
        &self.open
    }

    /// The places being written.
    pub fn partials(&self) -> &[u32] {
        &self.partials
    }

    /// The most entries a group may have.
    pub fn most_entries(&self) -> u32 {
        self.most_entries
    }

    /// The most places written at once.
    pub fn most_partials(&self) -> usize {
        self.most_partials
    }

    /// Whether the entry at `place` is settled, so a resume skips it: it is
    /// below the mark, or settled in an open group. Read without the disk:
    /// a file settled and deleted since is not fetched again.
    pub fn skips(&self, place: u32) -> bool {
        place < self.below || self.open.iter().any(|g| g.is_settled(place))
    }

    /// Whether the entry at `place` is being written.
    pub fn writing(&self, place: u32) -> bool {
        self.partials.contains(&place)
    }

    fn group_of(&mut self, place: u32) -> Option<&mut OpenGroup> {
        self.open
            .iter_mut()
            .find(|g| place >= g.first && place - g.first < g.len)
    }

    /// Opens group `index`, the entries `first..first + len`: the next one
    /// after the newest open group, or after the mark when none is open,
    /// starting where the one before it ended.
    pub fn open(&mut self, index: u32, first: u32, len: u32) -> Result<(), ProgressError> {
        if len == 0 || len > self.most_entries || first.checked_add(len).is_none() {
            return Err(ProgressError::BadLength);
        }
        let (next, start) = match self.open.last() {
            Some(newest) => (newest.index.checked_add(1), newest.first + newest.len),
            None => (Some(self.mark), self.below),
        };
        if next != Some(index) || first != start {
            return Err(ProgressError::OutOfOrder);
        }
        if self.open.len() >= self.most_open {
            return Err(ProgressError::TooManyOpen);
        }
        self.open.push(OpenGroup::new(index, first, len));
        Ok(())
    }

    /// The entry at `place` is being written.
    pub fn start(&mut self, place: u32) -> Result<(), ProgressError> {
        if self.skips(place) {
            return Err(ProgressError::Settled);
        }
        if self.writing(place) {
            return Err(ProgressError::Writing);
        }
        if self.group_of(place).is_none() {
            return Err(ProgressError::NotOpen);
        }
        if self.partials.len() >= self.most_partials {
            return Err(ProgressError::TooManyPartials);
        }
        self.partials.push(place);
        Ok(())
    }

    /// The entry at `place` stopped being written without being settled
    /// (cut, to come again).
    pub fn end(&mut self, place: u32) {
        self.partials.retain(|p| *p != place);
    }

    /// The entry at `place` is settled. When that finishes the oldest open
    /// group the mark moves, past every whole group in a row; returns how
    /// many groups it moved.
    pub fn settle(&mut self, place: u32) -> Result<u32, ProgressError> {
        if self.skips(place) {
            return Err(ProgressError::Settled);
        }
        let group = self.group_of(place).ok_or(ProgressError::NotOpen)?;
        group.settle(place);
        self.partials.retain(|p| *p != place);
        let mut moved = 0;
        while self.open.first().is_some_and(OpenGroup::whole) {
            let done = self.open.remove(0);
            self.mark = done.index + 1;
            self.below = done.first + done.len;
            moved += 1;
        }
        Ok(moved)
    }

    /// A progress read back: the mark and its first place, and the open
    /// groups with their bits as [`OpenGroup::bits`] gives them. Refused
    /// when it could not have been made by the calls above.
    pub fn restore(
        mark: u32,
        below: u32,
        open: &[(u32, u32, u32, Vec<u8>)],
        partials: &[u32],
        limits: &GroupLimits,
        most_partials: usize,
    ) -> Result<Self, ProgressError> {
        let mut progress = Self::new(limits, most_partials);
        progress.mark = mark;
        progress.below = below;
        for (index, first, len, bits) in open {
            progress.open(*index, *first, *len)?;
            if bits.len() != len.div_ceil(8) as usize {
                return Err(ProgressError::BadLength);
            }
            for place in *first..*first + *len {
                let bit = place - first;
                if bits[(bit / 8) as usize] & (1 << (bit % 8)) != 0 {
                    progress
                        .group_of(place)
                        .expect("opened above")
                        .settle(place);
                }
            }
            // A bit past the group's length is not one it could hold.
            let spare = len % 8;
            if spare != 0 && bits.last().is_some_and(|b| b >> spare != 0) {
                return Err(ProgressError::BadLength);
            }
        }
        // A whole oldest group would have moved the mark.
        if progress.open.first().is_some_and(OpenGroup::whole) {
            return Err(ProgressError::OutOfOrder);
        }
        for place in partials {
            progress.start(*place)?;
        }
        Ok(progress)
    }
}
