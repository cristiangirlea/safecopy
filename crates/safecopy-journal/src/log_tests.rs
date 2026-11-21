//! The journal's log: segments, groups and recovery after a crash.

use super::*;
use crate::tests::partial;
use safecopy_manifest::{Entry as ListEntry, Kind, groups};

fn temp(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let target = exe.ancestors().nth(3).unwrap();
    let dir = target
        .join("test-tmp")
        .join(format!("jlog-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(name: &str) -> (Scratch, PathBuf, PathBuf) {
    let base = temp(name);
    let (place, journals) = (base.join("Work"), base.join("journals"));
    std::fs::create_dir_all(&place).unwrap();
    std::fs::create_dir_all(&journals).unwrap();
    (Scratch(base), place, journals)
}

fn journals(dir: &Path) -> usize {
    std::fs::read_dir(dir).unwrap().count()
}

fn cleaned(removed: usize, left: usize) -> Cleaned {
    Cleaned {
        removed,
        left,
        kept: 0,
    }
}

/// Groups of four entries, so a few files make several.
fn small_groups() -> Settings {
    Settings {
        groups: GroupLimits {
            entries: 4,
            min_entries: 4,
            ..GroupLimits::default()
        },
        ..Settings::default()
    }
}

fn flat(n: u32) -> Vec<ListEntry> {
    (0..n)
        .map(|i| ListEntry {
            path: format!("F/{i:02}.txt"),
            kind: Kind::File,
            size: 4,
        })
        .collect()
}

/// Publishes the partial of `place` (`NN.txt.part` in `dir`) under its name.
fn publish(dir: &Path, place: u32) {
    std::fs::rename(
        dir.join(format!("{place:02}.txt.part")),
        dir.join(format!("{place:02}.txt")),
    )
    .unwrap();
}

/// A transfer of the list `flat(n)` into `root` with `journal`: opens the
/// groups it needs and starts every entry of `places`.
fn start(root: &Directory, journal: &Journal, cut: &safecopy_manifest::Groups, places: &[u32]) {
    for &place in places {
        let index = cut.of(place).unwrap();
        let open = journal.progress().unwrap();
        if !open.open_groups().iter().any(|g| g.index == index) {
            journal.group(index, cut.get(index).unwrap()).unwrap();
        }
        drop(partial(
            root,
            journal,
            place,
            &format!("{place:02}.txt.part"),
            b"data",
        ));
    }
}

fn segments(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension() == Some(OsStr::new(LOG_EXTENSION)))
        .collect();
    paths.sort();
    paths
}

#[test]
fn a_torn_journal_append_leaves_the_earlier_entries_usable() {
    let (_scratch, base, journal_dir) = scratch("torn-append");
    let settings = Settings::default();
    let mut cut = 0;
    loop {
        // A fresh transfer for each cut: two partials, then a third whose entry is torn.
        let place = base.join(format!("cut-{cut}"));
        std::fs::create_dir_all(&place).unwrap();
        let root = Directory::destination(&place).unwrap();
        let journal = Journal::start(&journal_dir, &root, &settings).unwrap();
        drop(partial(&root, &journal, 0, "a.part", b"half"));
        drop(partial(&root, &journal, 1, "b.part", b"half"));
        let path = journal.segment_path();
        let before = std::fs::metadata(&path).unwrap().len();
        drop(partial(&root, &journal, 2, "c.part", b"half"));
        let after = std::fs::metadata(&path).unwrap().len();
        journal.crash();
        drop(root);
        let at = before + cut;
        if at >= after {
            break;
        }
        // The last entry cut at this byte, and whole with this byte changed.
        let whole = std::fs::read(&path).unwrap();
        let mut changed = whole.clone();
        changed[at as usize] ^= 0x40;
        for bytes in [whole[..at as usize].to_vec(), changed] {
            let segment = read_segment(&bytes, &settings).unwrap();
            assert_eq!(segment.partials().len(), 2, "cut at {cut}");
            assert_eq!(segment.read as u64, before);
        }
        std::fs::write(&path, &whole[..at as usize]).unwrap();
        assert_eq!(
            clean_up(&journal_dir, &settings).unwrap(),
            cleaned(2, 0),
            "cut at {cut}"
        );
        assert!(!place.join("a.part").exists());
        assert!(!place.join("b.part").exists());
        assert!(
            place.join("c.part").exists(),
            "the torn entry's file is not touched"
        );
        cut += 1;
    }
    assert!(cut > 10, "every byte of the last entry was cut");
}

/// A shuffle-like order from a fixed seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self, below: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % below as u64) as usize
    }
}

/// Sends `list` into `root` through `journal` with completions out of
/// order, up to six single files and two bundles of 64 at once, checking
/// the bounds after every step.
fn send_all(
    list: &[ListEntry],
    root: &Directory,
    journal: &Journal,
    settings: &Settings,
    seed: u64,
) {
    let cut = groups(list, &settings.groups);
    let mut rng = Rng(seed);
    // The partial files: one per folder, reused (the journal needs only real IDs).
    let mut pool: std::collections::HashMap<String, (Directory, File)> = Default::default();
    let mut open_dir = |folder: &str| -> (Directory, File) {
        let entry = pool.entry(folder.to_string()).or_insert_with(|| {
            let mut dir = root.try_clone().unwrap();
            for part in folder.split('/').filter(|p| !p.is_empty()) {
                dir = dir.directory(OsStr::new(part)).unwrap();
            }
            let file = dir.create(OsStr::new("p.part")).unwrap();
            (dir, file)
        });
        (entry.0.try_clone().unwrap(), entry.1.try_clone().unwrap())
    };
    let mut singles: Vec<u32> = Vec::new();
    let mut bundles: Vec<Vec<u32>> = Vec::new();
    let path = |p: u32| &list[p as usize];
    let folder_of = |p: u32| {
        let at = &path(p).path;
        at.rsplit_once('/')
            .map_or(String::new(), |(f, _)| f.to_string())
    };
    let (mut starts, mut rolls) = (0u32, 0u32);
    seams::SYNCS.set(0);
    let mut most_segment = 0u64;
    let mut next = 0u32;
    loop {
        let progress = journal.progress().unwrap();
        if progress.below() == cut.entries() {
            break;
        }
        // Folders of the list are settled when created; files start, small ones bundled.
        let mut place = progress.below();
        while place < cut.entries() && (singles.len() < 6 || bundles.len() < 2) {
            let progress = journal.progress().unwrap();
            if !cut.may_start(place, &progress) {
                place += 1;
                continue;
            }
            let index = cut.of(place).unwrap();
            if !progress.open_groups().iter().any(|g| g.index == index) {
                journal.group(index, cut.get(index).unwrap()).unwrap();
            }
            if path(place).kind == Kind::Folder {
                let before = journal.progress().unwrap().mark();
                journal.done(place).unwrap();
                rolls += u32::from(journal.progress().unwrap().mark() != before);
                place += 1;
                continue;
            }
            if bundles.len() < 2 && rng.next(3) == 0 {
                // A bundle: the next files of this group and folder, up to 64.
                let group = cut.get(index).unwrap();
                let folder = folder_of(place);
                let (dir, file) = open_dir(&folder);
                let mut members = Vec::new();
                let mut p = place;
                while p < group.end() && members.len() < 64 {
                    let progress = journal.progress().unwrap();
                    if path(p).kind == Kind::File
                        && folder_of(p) == folder
                        && cut.may_start(p, &progress)
                        && !members.contains(&p)
                    {
                        members.push(p);
                    } else if path(p).kind == Kind::Folder || folder_of(p) != folder {
                        break;
                    }
                    p += 1;
                }
                let names: Vec<String> = members.iter().map(|m| format!("{m}")).collect();
                let starts_of: Vec<Start<'_>> = members
                    .iter()
                    .zip(&names)
                    .map(|(&m, name)| Start {
                        place: m,
                        file: &file,
                        partial: OsStr::new("p.part"),
                        name,
                        size: 1,
                    })
                    .collect();
                journal.started_many(&dir, &starts_of).unwrap();
                starts += 1;
                place = p;
                bundles.push(members);
            } else if singles.len() < 6 {
                let (dir, file) = open_dir(&folder_of(place));
                journal
                    .started(place, &dir, &file, OsStr::new("p.part"), "x", 1)
                    .unwrap();
                starts += 1;
                singles.push(place);
                place += 1;
            } else {
                break;
            }
            next += 1;
        }
        // Some finish, in any order.
        if !singles.is_empty() {
            for _ in 0..=rng.next(singles.len()) {
                if singles.is_empty() {
                    break;
                }
                let done = singles.swap_remove(rng.next(singles.len()));
                let before = journal.progress().unwrap().mark();
                journal.done(done).unwrap();
                rolls += u32::from(journal.progress().unwrap().mark() != before);
            }
        }
        if !bundles.is_empty() && rng.next(2) == 0 {
            let done = bundles.swap_remove(rng.next(bundles.len()));
            let before = journal.progress().unwrap().mark();
            journal.done_many(&done).unwrap();
            rolls += u32::from(journal.progress().unwrap().mark() != before);
        }
        // The bounds hold at every step.
        let progress = journal.progress().unwrap();
        assert!(progress.open_groups().len() <= 2);
        assert!(progress.open_groups().iter().all(|g| g.len <= 256));
        assert!(journal.partials().len() <= 134);
        let segment = journal.segment_path();
        let len = std::fs::metadata(&segment).unwrap().len();
        most_segment = most_segment.max(len);
        assert!(len <= 2 << 20, "a segment of {len} bytes");
        if next.is_multiple_of(61) {
            // One `Folder` entry per level in the segment, never two for one folder.
            let bytes = std::fs::read(&segment).unwrap();
            let read = read_segment(&bytes, settings).unwrap();
            let mut seen = std::collections::HashSet::new();
            for folder in read.folders() {
                assert!(seen.insert((folder.parent, folder.name.clone())));
            }
            for started in read.partials() {
                let chain = read.chain(started.folder).unwrap();
                let depth = folder_of(started.place)
                    .split('/')
                    .filter(|p| !p.is_empty())
                    .count();
                assert_eq!(chain.len(), depth);
            }
        }
    }
    // One sync per start or bundle, one per mark move; none per Done.
    assert_eq!(seams::SYNCS.get(), starts + rolls);
    assert!(rolls >= cut.groups().len() as u32 / 2);
    assert!(journal.partials().is_empty());
    drop(pool);
}

#[test]
fn the_progress_stays_bounded_for_10000_entries_flat_and_nested() {
    let (_scratch, place, journal_dir) = scratch("bounded");
    let settings = Settings::default();

    // Flat: one folder of 10,000 files.
    std::fs::create_dir_all(place.join("Flat")).unwrap();
    let list: Vec<ListEntry> = (0..10_000)
        .map(|n| ListEntry {
            path: format!("Flat/{n:05}.txt"),
            kind: Kind::File,
            size: 1000,
        })
        .collect();
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings).unwrap();
    send_all(&list, &root, &journal, &settings, 0x5eed);
    drop(journal);
    assert_eq!(journals(&journal_dir), 0);

    // Nested, in a paste's order: 2,500 folders each with a folder of two files.
    let mut list = Vec::new();
    for n in 0..2500 {
        let top = format!("T/d{n:04}");
        std::fs::create_dir_all(place.join(&top).join("s")).unwrap();
        list.push(ListEntry {
            path: top.clone(),
            kind: Kind::Folder,
            size: 0,
        });
        list.push(ListEntry {
            path: format!("{top}/s"),
            kind: Kind::Folder,
            size: 0,
        });
        for f in ["a", "b"] {
            list.push(ListEntry {
                path: format!("{top}/s/{f}"),
                kind: Kind::File,
                size: 10,
            });
        }
    }
    assert_eq!(list.len(), 10_000);
    let journal = Journal::start(&journal_dir, &root, &settings).unwrap();
    send_all(&list, &root, &journal, &settings, 0xfeed);
    drop(journal);
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn a_crash_before_the_mark_moves_resumes_from_the_last_snapshot() {
    let settings = small_groups();
    let list = flat(12);
    let cut = groups(&list, &settings.groups);
    assert_eq!(cut.groups().len(), 3);
    for lose_the_done in [false, true] {
        let (_scratch, place, journal_dir) = scratch(&format!("before-mark-{lose_the_done}"));
        let root = Directory::destination(&place).unwrap();
        let journal = Journal::start(&journal_dir, &root, &settings).unwrap();
        // Group 0 settles whole: the mark moves and the next segment begins.
        start(&root, &journal, &cut, &[0, 1, 2, 3]);
        for p in 0..4 {
            publish(&place, p);
            journal.done(p).unwrap();
        }
        assert_eq!(journal.progress().unwrap().mark(), 1);
        // Group 1: three settled, then the last one's Done, and a crash before the snapshot.
        start(&root, &journal, &cut, &[4, 5, 6, 7]);
        for p in [5, 4, 6] {
            publish(&place, p);
            journal.done(p).unwrap();
        }
        publish(&place, 7);
        let path = journal.segment_path();
        let before_done = std::fs::metadata(&path).unwrap().len();
        seams::CRASH.set(Some(seams::Crash::BeforeSnapshot));
        assert_eq!(journal.done(7), Err(JournalError::Broken));
        seams::CRASH.set(None);
        journal.crash();
        drop(root);
        if lose_the_done {
            // Not yet synced: the Done is lost with the crash.
            let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.set_len(before_done).unwrap();
        }
        assert_eq!(
            segments(&journal_dir).len(),
            1,
            "the old segment, no new one"
        );
        let recovered = recover(&journal_dir, &settings).unwrap();
        assert_eq!(recovered.len(), 1);
        let got = &recovered[0];
        assert_eq!(got.segment.seq(), 2);
        let found_by_id = !lose_the_done || cfg!(windows);
        for p in 0..12 {
            let settled = p < 7 || (p == 7 && found_by_id);
            assert_eq!(got.progress().skips(p), settled, "place {p}");
        }
        if lose_the_done {
            if cfg!(windows) {
                assert_eq!(got.settled, [7]);
                assert!(got.partials().is_empty());
            } else {
                // Only a candidate here: fetched again, never twice saved.
                assert_eq!(got.candidates, [(7, "07.txt".to_string())]);
                assert_eq!(got.partials().len(), 1);
            }
        } else {
            assert!(got.partials().is_empty());
            assert_eq!(got.progress().mark(), 2);
        }
        // Cleanup then removes nothing published.
        clean_up(&journal_dir, &settings).unwrap();
        for p in 0..8 {
            assert!(place.join(format!("{p:02}.txt")).exists());
        }
    }
}

#[test]
fn a_crash_after_a_publish_whose_done_was_lost_counts_the_file_settled() {
    let settings = Settings::default();
    for (case, published) in [
        ("expected", "a.txt"),
        ("numbered", "a (2).txt"),
        ("deleted", "a.txt"),
    ] {
        let (_scratch, place, journal_dir) = scratch(&format!("lost-done-{case}"));
        let root = Directory::destination(&place).unwrap();
        let journal = Journal::start(&journal_dir, &root, &settings).unwrap();
        // Another file has the name already, for the numbered one.
        std::fs::write(place.join("a.txt"), b"other, older").unwrap();
        if case != "numbered" {
            std::fs::remove_file(place.join("a.txt")).unwrap();
        }
        let mut file = root.create(OsStr::new("a.txt.part")).unwrap();
        journal
            .started(0, &root, &file, OsStr::new("a.txt.part"), "a.txt", 4)
            .unwrap();
        std::io::Write::write_all(&mut file, b"data").unwrap();
        drop(file);
        std::fs::rename(place.join("a.txt.part"), place.join(published)).unwrap();
        // The crash comes before the Done is written.
        journal.crash();
        drop(root);
        if case == "deleted" {
            std::fs::remove_file(place.join(published)).unwrap();
        }
        let got = recover(&journal_dir, &settings).unwrap().remove(0);
        let found = case != "deleted";
        if cfg!(windows) && found {
            assert_eq!(got.settled, [0], "{case}");
            assert!(got.partials().is_empty());
        } else {
            assert!(got.settled.is_empty(), "{case}");
            assert_eq!(got.partials().len(), 1, "{case}: fetched again");
        }
        if !cfg!(windows) && found {
            assert_eq!(got.candidates, [(0, published.to_string())]);
        }
        // Cleanup removes no published file.
        clean_up(&journal_dir, &settings).unwrap();
        if found {
            assert_eq!(std::fs::read(place.join(published)).unwrap(), b"data");
        }
    }
}

#[cfg(unix)]
#[test]
fn a_reused_inode_does_not_settle_a_lost_done() {
    let settings = Settings::default();
    for case in ["another name", "another size"] {
        let (_scratch, place, journal_dir) = scratch(&format!("reused-{}", case.len()));
        let root = Directory::destination(&place).unwrap();
        let journal = Journal::start(&journal_dir, &root, &settings).unwrap();
        let mut file = root.create(OsStr::new("a.txt.part")).unwrap();
        journal
            .started(0, &root, &file, OsStr::new("a.txt.part"), "a.txt", 4)
            .unwrap();
        std::io::Write::write_all(&mut file, b"data").unwrap();
        drop(file);
        std::fs::rename(place.join("a.txt.part"), place.join("a.txt")).unwrap();
        journal.crash();
        drop(root);
        // The person's own file now holds that inode: the same inode is the
        // surest reuse, under another name or with another size.
        let theirs = if case == "another name" {
            std::fs::rename(place.join("a.txt"), place.join("copy.txt")).unwrap();
            place.join("copy.txt")
        } else {
            std::fs::write(place.join("a.txt"), b"their longer file").unwrap();
            place.join("a.txt")
        };
        let got = recover(&journal_dir, &settings).unwrap().remove(0);
        assert!(got.settled.is_empty(), "{case}");
        assert!(got.candidates.is_empty(), "{case}");
        assert!(!got.progress().skips(0), "{case}: fetched again");
        assert_eq!(got.partials().len(), 1);
        clean_up(&journal_dir, &settings).unwrap();
        assert!(theirs.exists(), "{case}: never removed");
    }
}

#[test]
fn a_crash_after_started_many_before_the_first_byte_removes_every_empty_partial() {
    let (_scratch, place, journal_dir) = scratch("started-many");
    let settings = Settings::default();
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings).unwrap();
    let names: Vec<String> = (0..64).map(|n| format!("{n:02}.bin.part")).collect();
    let files: Vec<File> = names
        .iter()
        .map(|n| root.create(OsStr::new(n)).unwrap())
        .collect();
    let starts: Vec<Start<'_>> = files
        .iter()
        .zip(&names)
        .enumerate()
        .map(|(n, (file, name))| Start {
            place: n as u32,
            file,
            partial: OsStr::new(name),
            name: name.strip_suffix(".part").unwrap(),
            size: 100,
        })
        .collect();
    journal.started_many(&root, &starts).unwrap();
    // The crash comes before any byte.
    drop(starts);
    drop(files);
    journal.crash();
    drop(root);
    assert_eq!(clean_up(&journal_dir, &settings).unwrap(), cleaned(64, 0));
    assert_eq!(std::fs::read_dir(&place).unwrap().count(), 0);
    assert_eq!(journals(&journal_dir), 0);
}

/// A transfer whose group 0 settles while group 1's partials are live,
/// crashed at `crash` as the mark moves.
fn crash_at_the_mark(name: &str, crash: seams::Crash) -> (Scratch, PathBuf, PathBuf) {
    let (scratch, place, journal_dir) = scratch(name);
    let settings = small_groups();
    let list = flat(12);
    let cut = groups(&list, &settings.groups);
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings).unwrap();
    start(&root, &journal, &cut, &[0, 1, 2, 3, 4, 5]);
    for p in [0, 1, 2] {
        publish(&place, p);
        journal.done(p).unwrap();
    }
    publish(&place, 3);
    seams::CRASH.set(Some(crash));
    assert_eq!(journal.done(3), Err(JournalError::Broken));
    seams::CRASH.set(None);
    journal.crash();
    (scratch, place, journal_dir)
}

#[test]
fn a_crash_between_two_segments_leaves_both_and_cleanup_removes_each_partial_once() {
    let settings = small_groups();
    let (_scratch, place, journal_dir) =
        crash_at_the_mark("two-segments", seams::Crash::BeforeOldDeleted);
    let both = segments(&journal_dir);
    assert_eq!(both.len(), 2, "the new segment and the old one");
    // Each lists the two live partials of group 1.
    for path in &both {
        let segment = read_segment(&std::fs::read(path).unwrap(), &settings).unwrap();
        let places: Vec<u32> = segment.partials().iter().map(|s| s.place).collect();
        assert_eq!(places, [4, 5]);
    }
    assert_eq!(clean_up(&journal_dir, &settings).unwrap(), cleaned(2, 2));
    assert!(!place.join("04.txt.part").exists());
    assert!(!place.join("05.txt.part").exists());
    for p in 0..4 {
        assert!(place.join(format!("{p:02}.txt")).exists());
    }
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn the_highest_numbered_whole_segment_is_read() {
    let settings = small_groups();
    let (_scratch, _place, journal_dir) =
        crash_at_the_mark("highest", seams::Crash::BeforeOldDeleted);
    let name = log_name(&segments(&journal_dir)[0]).unwrap().0;
    // A third segment, damaged, is passed over.
    std::fs::write(
        journal_dir.join(format!("{name}.3.{LOG_EXTENSION}")),
        b"\x05\0\0\0\0\0\0\0torn",
    )
    .unwrap();
    let got = recover(&journal_dir, &settings).unwrap().remove(0);
    assert_eq!(got.segment.seq(), 2);
    assert_eq!(got.progress().mark(), 1);
    assert!(got.progress().skips(3) && !got.progress().skips(4));
    let places: Vec<u32> = got.partials().iter().map(|s| s.place).collect();
    assert_eq!(places, [4, 5]);
}

#[test]
fn a_torn_snapshot_leaves_the_old_segment_in_charge() {
    let settings = small_groups();
    let (_scratch, place, journal_dir) =
        crash_at_the_mark("torn-snapshot", seams::Crash::TornSnapshot);
    assert_eq!(segments(&journal_dir).len(), 2);
    let got = recover(&journal_dir, &settings).unwrap().remove(0);
    assert_eq!(got.segment.seq(), 1, "the old, whole segment");
    assert_eq!(got.progress().mark(), 1, "with its entries applied");
    let places: Vec<u32> = got.partials().iter().map(|s| s.place).collect();
    assert_eq!(places, [4, 5]);
    // Cleanup removes each partial once and the torn segment touches nothing.
    assert_eq!(clean_up(&journal_dir, &settings).unwrap(), cleaned(2, 0));
    assert!(!place.join("04.txt.part").exists());
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn a_finished_file_deleted_meanwhile_is_not_fetched_again() {
    let (_scratch, place, journal_dir) = scratch("deleted-meanwhile");
    let settings = small_groups();
    let list = flat(12);
    let cut = groups(&list, &settings.groups);
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings).unwrap();
    start(&root, &journal, &cut, &[0, 1, 2, 3, 4, 5]);
    for p in [0, 1, 2, 3, 5] {
        publish(&place, p);
        journal.done(p).unwrap();
    }
    journal.crash();
    drop(root);
    // One below the mark and one settled past it, both deleted by the person.
    std::fs::remove_file(place.join("01.txt")).unwrap();
    std::fs::remove_file(place.join("05.txt")).unwrap();
    let got = recover(&journal_dir, &settings).unwrap().remove(0);
    assert!(got.progress().skips(1), "below the mark");
    assert!(got.progress().skips(5), "settled past it");
    assert!(!got.progress().skips(4), "the partial continues");
    assert!(!got.progress().skips(6));
}

#[test]
fn a_bundle_is_journalled_in_one_write() {
    let (_scratch, place, journal_dir) = scratch("bundle");
    let settings = Settings::default();
    let list: Vec<ListEntry> = (0..64)
        .map(|n| ListEntry {
            path: format!("B/{n:02}"),
            kind: Kind::File,
            size: 1,
        })
        .collect();
    let cut = groups(&list, &settings.groups);
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings).unwrap();
    journal.group(0, cut.get(0).unwrap()).unwrap();
    let names: Vec<String> = (0..64).map(|n| format!("{n:02}.part")).collect();
    let files: Vec<File> = names
        .iter()
        .map(|n| root.create(OsStr::new(n)).unwrap())
        .collect();
    let starts: Vec<Start<'_>> = files
        .iter()
        .zip(&names)
        .enumerate()
        .map(|(n, (file, name))| Start {
            place: n as u32,
            file,
            partial: OsStr::new(name),
            name: &name[..2],
            size: 1,
        })
        .collect();
    let read = || read_segment(&std::fs::read(journal.segment_path()).unwrap(), &settings).unwrap();
    let entries = read().entries;
    seams::SYNCS.set(0);
    journal.started_many(&root, &starts).unwrap();
    assert_eq!(seams::SYNCS.get(), 1, "one sync for 64 partials");
    // The group's entry and the bundle's, in one append.
    assert_eq!(read().entries, entries + 2);
    assert_eq!(read().partials().len(), 64);
    let places: Vec<u32> = (0..64).collect();
    let before = read().entries;
    seams::SYNCS.set(0);
    journal.done_many(&places[..63]).unwrap();
    assert_eq!(seams::SYNCS.get(), 0, "settling syncs nothing of its own");
    assert_eq!(read().entries, before + 1, "one entry for the bundle");
    assert_eq!(read().partials().len(), 1);
    drop(starts);
    for (name, file) in names.iter().zip(files) {
        root.discard(&file, OsStr::new(name)).unwrap();
    }
}

#[test]
fn cleanup_still_reads_a_journal_in_todays_format() {
    let (_scratch, place, journal_dir) = scratch("todays-format");
    let settings = Settings::default();
    std::fs::write(place.join("a.part"), b"half").unwrap();
    std::fs::write(place.join("b.part"), b"half").unwrap();
    let root = Directory::destination(&place).unwrap();
    #[cfg(windows)]
    let path = crate::tests::windows_old_journal(&journal_dir, &root, &["a.part", "b.part"]);
    #[cfg(not(windows))]
    let path = {
        let path = journal_dir.join(format!("1-1.{EXTENSION}"));
        let record = Record {
            volume: r"\\?\Volume{00000000-0000-0000-0000-000000000000}\".into(),
            serial: 1,
            partials: vec![Partial {
                folder: 1,
                name: "a.part".into(),
                file: 2,
            }],
        };
        std::fs::write(&path, record.encode(&settings).unwrap()).unwrap();
        path
    };
    drop(root);
    // A log journal beside it is read in the same cleanup.
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings).unwrap();
    drop(partial(&root, &journal, 0, "c.part", b"half"));
    journal.crash();
    drop(root);
    let done = clean_up(&journal_dir, &settings).unwrap();
    if cfg!(windows) {
        assert_eq!(done, cleaned(3, 0));
        assert!(!place.join("a.part").exists());
        assert!(!place.join("b.part").exists());
    } else {
        // Only Windows wrote that format: elsewhere it is removed, touching nothing.
        assert_eq!(done, cleaned(1, 1));
        assert!(place.join("a.part").exists());
    }
    assert!(!place.join("c.part").exists());
    assert!(!path.exists());
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn cleanup_leaves_a_renamed_or_replaced_file_alone() {
    let (_scratch, place, journal_dir) = scratch("renamed-replaced-log");
    let settings = Settings::default();
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings).unwrap();
    let photos = root.claim("Photos").unwrap();
    let music = root.claim("Music").unwrap();
    drop(partial(&root, &journal, 0, "renamed.part", b"mine"));
    drop(partial(&root, &journal, 1, "replaced.part", b"mine"));
    drop(partial(
        &photos,
        &journal,
        2,
        "in-replaced-folder.part",
        b"mine",
    ));
    drop(partial(&music, &journal, 3, "kept.part", b"mine"));
    journal.crash();
    drop((root, photos, music));
    std::fs::rename(place.join("renamed.part"), place.join("renamed.txt")).unwrap();
    // Made before the old one goes, so it never takes the old inode.
    std::fs::write(place.join("theirs.tmp"), b"theirs").unwrap();
    std::fs::remove_file(place.join("replaced.part")).unwrap();
    std::fs::rename(place.join("theirs.tmp"), place.join("replaced.part")).unwrap();
    // A folder on the way replaced by another of the same name and content.
    std::fs::rename(place.join("Photos"), place.join("Photos old")).unwrap();
    std::fs::create_dir(place.join("Photos")).unwrap();
    std::fs::write(place.join("Photos/in-replaced-folder.part"), b"theirs").unwrap();
    let done = clean_up(&journal_dir, &settings).unwrap();
    if cfg!(windows) {
        // A folder is found by its ID wherever it moved, and its partial removed.
        assert_eq!(done, cleaned(2, 2));
        assert!(!place.join("Photos old/in-replaced-folder.part").exists());
    } else {
        // A walk by name stops at the folder put in its place.
        assert_eq!(done, cleaned(1, 3));
        assert!(place.join("Photos old/in-replaced-folder.part").exists());
    }
    assert_eq!(std::fs::read(place.join("renamed.txt")).unwrap(), b"mine");
    assert_eq!(
        std::fs::read(place.join("replaced.part")).unwrap(),
        b"theirs"
    );
    assert_eq!(
        std::fs::read(place.join("Photos/in-replaced-folder.part")).unwrap(),
        b"theirs"
    );
    assert!(!place.join("Music/kept.part").exists());
    assert_eq!(journals(&journal_dir), 0);
}

#[cfg(unix)]
#[test]
fn cleanup_finds_a_partial_in_a_nested_folder_on_linux() {
    let (_scratch, place, journal_dir) = scratch("nested");
    let settings = Settings::default();
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings).unwrap();
    let year = root.claim("Photos").unwrap().claim("2024").unwrap();
    drop(partial(&year, &journal, 0, "beach.jpg.part", b"half"));
    // The same folder used again costs no second entry per level.
    drop(partial(&year, &journal, 1, "sea.jpg.part", b"half"));
    let bytes = std::fs::read(journal.segment_path()).unwrap();
    assert_eq!(
        read_segment(&bytes, &settings).unwrap().folders().count(),
        2
    );
    journal.crash();
    drop((root, year));
    assert_eq!(clean_up(&journal_dir, &settings).unwrap(), cleaned(2, 0));
    assert!(!place.join("Photos/2024/beach.jpg.part").exists());
    assert!(place.join("Photos/2024").is_dir());
}

#[cfg(unix)]
#[test]
fn cleanup_stops_at_a_level_that_became_a_link_on_linux() {
    let (_scratch, place, journal_dir) = scratch("level-link");
    let settings = Settings::default();
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings).unwrap();
    let year = root.claim("Photos").unwrap().claim("2024").unwrap();
    drop(partial(&year, &journal, 0, "beach.jpg.part", b"half"));
    journal.crash();
    drop((root, year));
    // The level moved away and a link to it put in its place.
    std::fs::rename(place.join("Photos"), place.join("Elsewhere")).unwrap();
    std::os::unix::fs::symlink(place.join("Elsewhere"), place.join("Photos")).unwrap();
    assert_eq!(clean_up(&journal_dir, &settings).unwrap(), cleaned(0, 1));
    assert!(place.join("Elsewhere/2024/beach.jpg.part").exists());
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn a_segment_reads_back_what_was_written() {
    let settings = small_groups();
    let mut state = State::new(
        Root::Unix {
            path: b"/srv/in".to_vec(),
            device: 7,
            inode: 9,
        },
        &settings,
    );
    let entries = vec![
        Entry::Group {
            index: 0,
            first: 0,
            len: 4,
        },
        Entry::Folder(Folder {
            n: 1,
            parent: 0,
            name: "Photos".into(),
            device: 7,
            id: 10,
        }),
        Entry::StartedMany(
            (0..3)
                .map(|p| Started {
                    place: p,
                    folder: 1,
                    name: format!("{p}.jpg"),
                    partial: format!("{p}.jpg.part"),
                    size: 5,
                    file: 100 + u128::from(p),
                })
                .collect(),
        ),
        Entry::DoneMany(vec![0, 2]),
        Entry::Ended { place: 1 },
        Entry::Done { place: 3 },
    ];
    let mut bytes = frame(&state.header());
    for entry in &entries {
        bytes.extend_from_slice(&frame(&entry.encode()));
        assert_eq!(Entry::decode(&entry.encode(), &settings).unwrap(), *entry);
    }
    for entry in entries.clone() {
        state.apply(entry).unwrap();
    }
    let read = read_segment(&bytes, &settings).unwrap();
    assert_eq!(read.entries, entries.len());
    assert_eq!(read.state, state);
    assert!(read.progress().skips(0) && read.progress().skips(3));
    // An entry that cannot follow stops the reading there: a Done of a
    // place in no open group.
    let mut more = bytes.clone();
    more.extend_from_slice(&frame(&Entry::Done { place: 40 }.encode()));
    more.extend_from_slice(&frame(&Entry::Ended { place: 1 }.encode()));
    assert_eq!(
        read_segment(&more, &settings).unwrap().entries,
        entries.len()
    );
}

#[test]
fn partials_started_at_once_in_new_folders_are_all_recorded() {
    let (_scratch, place, journal_dir) = scratch("at-once");
    let settings = Settings::default();
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings).unwrap();
    // Eight streams, two to each new folder, all starting together.
    let folders: Vec<Directory> = (0..4)
        .map(|n| root.claim(&format!("New {n}")).unwrap())
        .collect();
    let barrier = std::sync::Barrier::new(8);
    std::thread::scope(|s| {
        for place in 0..8u32 {
            let (dir, journal, barrier) = (&folders[place as usize / 2], &journal, &barrier);
            s.spawn(move || {
                let name = format!("{place}.part");
                let file = dir.create(OsStr::new(&name)).unwrap();
                barrier.wait();
                journal
                    .started(place, dir, &file, OsStr::new(&name), "x", 1)
                    .unwrap();
            });
        }
    });
    let bytes = std::fs::read(journal.segment_path()).unwrap();
    let segment = read_segment(&bytes, &settings).unwrap();
    assert_eq!(segment.partials().len(), 8);
    assert_eq!(segment.folders().count(), 4, "one entry per folder");
    journal.crash();
    drop((root, folders));
    assert_eq!(clean_up(&journal_dir, &settings).unwrap(), cleaned(8, 0));
}

#[test]
fn a_next_segment_that_fails_to_begin_is_tried_again_and_loses_nothing() {
    let (_scratch, place, journal_dir) = scratch("roll-again");
    let settings = small_groups();
    let list = flat(12);
    let cut = groups(&list, &settings.groups);
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings).unwrap();
    start(&root, &journal, &cut, &[0, 1, 2, 3, 4]);
    for p in 0..3 {
        journal.done(p).unwrap();
    }
    // The mark moves but the next segment cannot begin: the Done is still recorded.
    seams::FAIL_ROLL.set(true);
    journal.done(3).unwrap();
    let first = journal.segment_path();
    let read = read_segment(&std::fs::read(&first).unwrap(), &settings).unwrap();
    assert_eq!((read.seq(), read.progress().mark()), (1, 1));
    // The next append begins it.
    journal.ended(4).unwrap();
    let read = read_segment(&std::fs::read(journal.segment_path()).unwrap(), &settings).unwrap();
    assert_eq!((read.seq(), read.progress().mark()), (2, 1));
    assert!(!first.exists());
    drop(journal);
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn a_folder_numbered_after_its_child_is_refused() {
    let settings = Settings::default();
    let root = Root::Unix {
        path: b"/in".to_vec(),
        device: 1,
        inode: 1,
    };
    let state = State::new(root, &settings);
    let folder = |n, parent| {
        Entry::Folder(Folder {
            n,
            parent,
            name: format!("f{n}"),
            device: 1,
            id: u128::from(n),
        })
    };
    let mut bytes = frame(&state.header());
    for entry in [folder(5, 0), folder(1, 5), folder(6, 5)] {
        bytes.extend_from_slice(&frame(&entry.encode()));
    }
    // The child numbered below its parent stops the reading there.
    let read = read_segment(&bytes, &settings).unwrap();
    assert_eq!(read.entries, 1);
    // What reads writes back to a snapshot that reads the same.
    assert_eq!(
        read_segment(&read.snapshot(), &settings)
            .unwrap()
            .progress(),
        read.progress()
    );
}

/// Starts and ends the partial of `place` as `NN.txt.part` in `dir`, its
/// file removed after: a stream cut and come again.
fn churn(dir: &Path, root: &Directory, journal: &Journal, place: u32) {
    let name = format!("{place:02}.txt.part");
    drop(partial(root, journal, place, &name, b"data"));
    journal.ended(place).unwrap();
    std::fs::remove_file(dir.join(&name)).unwrap();
}

#[test]
fn a_journal_without_groups_starts_its_next_segment_after_roll_every_ends() {
    let (_scratch, place, journal_dir) = scratch("flat-roll");
    let settings = Settings {
        roll_every: 4,
        ..Settings::default()
    };
    seams::DIR_SYNCS.set(0);
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings).unwrap();
    assert_eq!(
        seams::DIR_SYNCS.get(),
        1,
        "the first segment's name is synced"
    );
    let seq = |journal: &Journal| {
        read_segment(&std::fs::read(journal.segment_path()).unwrap(), &settings)
            .unwrap()
            .seq()
    };
    let mut first_cycle = 0;
    let mut longest = 0;
    for p in 0..40 {
        churn(&place, &root, &journal, p);
        let len = std::fs::metadata(journal.segment_path()).unwrap().len();
        if p < 4 {
            first_cycle = first_cycle.max(len);
        }
        longest = longest.max(len);
        // A new segment after every fourth end, and only then.
        assert_eq!(seq(&journal), 1 + u64::from(p + 1) / 4, "after {p}");
        assert_eq!(segments(&journal_dir).len(), 1, "the old one deleted");
    }
    assert!(longest <= first_cycle, "{longest} past {first_cycle}");
    // Each new segment's name was synced before the old one went.
    assert_eq!(seams::DIR_SYNCS.get(), 1 + 10);
    drop(journal);
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn a_grouped_journal_full_of_churn_starts_its_next_segment_and_takes_more() {
    let (_scratch, place, journal_dir) = scratch("grouped-churn");
    let settings = Settings {
        segment_bytes: 2048,
        ..small_groups()
    };
    let list = flat(12);
    let cut = groups(&list, &settings.groups);
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings).unwrap();
    start(&root, &journal, &cut, &[0, 1]);
    // The mark never moves: only the segment's own limit makes the next one.
    for _ in 0..100 {
        churn(&place, &root, &journal, 2);
        let len = std::fs::metadata(journal.segment_path()).unwrap().len();
        assert!(len <= 2048, "{len}");
    }
    let path = journal.segment_path();
    let read = read_segment(&std::fs::read(&path).unwrap(), &settings).unwrap();
    assert!(read.seq() > 1, "rolled at its limit");
    assert_eq!(read.progress().mark(), 0);
    let places: Vec<u32> = read.partials().iter().map(|s| s.place).collect();
    assert_eq!(places, [0, 1], "the live partials carried over");
    // Settling still goes in.
    publish(&place, 0);
    journal.done(0).unwrap();
    journal.ended(1).unwrap();
    assert!(std::fs::metadata(journal.segment_path()).unwrap().len() <= 2048);
    assert_eq!(segments(&journal_dir).len(), 1);
    drop(journal);
    assert_eq!(journals(&journal_dir), 0);
}
