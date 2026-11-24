//! A partial's second identity: a new file given a removed partial's inode
//! is not taken for it, and both formats of the log are read as written.

use super::*;
use crate::log::{FORMAT, FORMAT_SECOND};
use crate::record::{text, varint};
use crate::tests::{Scratch, partial};

fn scratch(name: &str) -> (Scratch, PathBuf, PathBuf) {
    Scratch::folders("second", name)
}

fn journals(dir: &Path) -> usize {
    std::fs::read_dir(dir).unwrap().count()
}

fn cleaned(removed: usize, left: usize, kept: usize) -> Cleaned {
    Cleaned {
        removed,
        left,
        kept,
    }
}

const NAME: &str = "a.bin.part";

fn born(secs: i64) -> Second {
    Second::Born { secs, nanos: 7 }
}

fn both(secs: i64, generation: u32) -> Second {
    Second::Both {
        secs,
        nanos: 7,
        generation,
    }
}

/// What every file of this thread reports as its second identity, until
/// this is dropped: as a system that gives it would.
struct Acted;

impl Acted {
    fn all(second: Second) -> Self {
        seams::SECOND.set(Some(second));
        Self
    }

    /// From here on, in place of what was acted out before.
    fn now(&self, second: Second) {
        seams::SECOND.set(Some(second));
    }
}

impl Drop for Acted {
    fn drop(&mut self) {
        seams::SECOND.set(None);
    }
}

/// A crashed journal that lists one partial with bytes, and its place.
fn crashed(place: &Path, journal_dir: &Path) -> PartialAt {
    let root = Directory::destination(place).unwrap();
    let journal = Journal::start(journal_dir, &root, &Settings::default()).unwrap();
    let file = partial(&root, &journal, 0, NAME, b"half");
    let at = PartialAt::of(&root, &root, &file, OsStr::new(NAME)).unwrap();
    assert_eq!(journal.partials()[0].second, at.second);
    journal.crash();
    at
}

/// The same file stands for the two: the one recorded, and a new one that
/// was given its inode. Only what it reports as its second identity
/// changes, which is all that tells such two apart.
#[test]
fn a_file_with_the_recorded_inode_and_another_second_identity_is_left() {
    let settings = Settings::default();
    for (kind, recorded, other) in [
        ("born", born(1), born(2)),
        ("nanos", born(1), Second::Born { secs: 1, nanos: 8 }),
        ("generation", Second::Generation(40), Second::Generation(41)),
        // Made within one step of the clock: only the generation differs.
        ("one-step", both(1, 40), both(1, 41)),
        ("both-born", both(1, 40), both(2, 40)),
    ] {
        let (_scratch, place, journal_dir) = scratch(&format!("same-inode-{kind}"));
        let acted = Acted::all(recorded);
        let at = crashed(&place, &journal_dir);
        assert_eq!(at.second, recorded, "{kind}");
        assert_eq!(at.present(&settings), Some(4), "{kind}");
        assert!(at.reopen(&settings).is_some(), "{kind}");

        acted.now(other);
        assert_eq!(at.present(&settings), None, "{kind}");
        assert_eq!(
            at.try_reopen(&settings).map(|_| ()),
            Err(Left::Replaced),
            "{kind}"
        );
        assert!(!at.remove(&settings), "{kind}");
        // Another file is under the name for good: the journal ends.
        assert_eq!(
            clean_up(&journal_dir, &settings).unwrap(),
            cleaned(0, 1, 0),
            "{kind}"
        );
        assert_eq!(std::fs::read(place.join(NAME)).unwrap(), b"half", "{kind}");
        assert_eq!(journals(&journal_dir), 0, "{kind}");

        // The file recorded, with what it reported then, still goes.
        acted.now(recorded);
        std::fs::remove_file(place.join(NAME)).unwrap();
        crashed(&place, &journal_dir);
        assert_eq!(
            clean_up(&journal_dir, &settings).unwrap(),
            cleaned(1, 0, 0),
            "{kind}"
        );
        assert!(!place.join(NAME).exists(), "{kind}");
    }
}

#[test]
fn a_file_that_gives_no_second_identity_is_known_as_before() {
    let settings = Settings::default();
    let (_scratch, place, journal_dir) = scratch("none");
    let acted = Acted::all(Second::None);
    let at = crashed(&place, &journal_dir);
    assert_eq!(at.second, Second::None);
    // Nothing to say: on Windows the journal is in the format of before.
    let segment = std::fs::read_dir(&journal_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let read = read_segment(&std::fs::read(segment).unwrap(), &settings).unwrap();
    assert_eq!(
        read.format(),
        if cfg!(unix) { FORMAT_SECOND } else { FORMAT }
    );
    assert_eq!(read.partials()[0].second, Second::None);
    // Whatever the file reports later, nothing recorded is compared with it.
    acted.now(born(9));
    assert_eq!(at.present(&settings), Some(4));
    assert!(at.reopen(&settings).is_some());
    assert_eq!(clean_up(&journal_dir, &settings).unwrap(), cleaned(1, 0, 0));
    assert!(!place.join(NAME).exists());
}

#[test]
fn a_recorded_second_identity_that_cannot_be_read_now_leaves_the_file_for_later() {
    let settings = Settings::default();
    for (kind, recorded, now) in [
        ("none", born(1), Second::None),
        ("another kind", born(1), Second::Generation(3)),
        ("no generation", Second::Generation(3), born(1)),
        ("half of both", both(1, 3), born(1)),
    ] {
        let (_scratch, place, journal_dir) = scratch(&format!("unread-{}", kind.len()));
        let acted = Acted::all(recorded);
        let at = crashed(&place, &journal_dir);
        acted.now(now);
        assert_eq!(at.present(&settings), None, "{kind}");
        assert_eq!(
            at.try_reopen(&settings).map(|_| ()),
            Err(Left::Io),
            "{kind}"
        );
        // Nothing shows it is another file: the journal waits with it.
        assert_eq!(
            clean_up(&journal_dir, &settings).unwrap(),
            cleaned(0, 1, 1),
            "{kind}"
        );
        assert!(place.join(NAME).exists(), "{kind}");
        acted.now(recorded);
        assert_eq!(
            clean_up(&journal_dir, &settings).unwrap(),
            cleaned(1, 0, 0),
            "{kind}"
        );
        assert!(!place.join(NAME).exists(), "{kind}");
    }
}

#[test]
fn a_published_file_with_the_inode_and_another_second_identity_is_no_candidate() {
    let settings = Settings::default();
    let (_scratch, place, journal_dir) = scratch("published");
    let acted = Acted::all(born(1));
    crashed(&place, &journal_dir);
    std::fs::rename(place.join(NAME), place.join("a.bin")).unwrap();
    let found = |journal_dir: &Path| {
        let got = recover(journal_dir, &settings).unwrap().remove(0);
        got.settled.len() + got.candidates.len()
    };
    assert_eq!(found(&journal_dir), 1, "the partial, published");
    acted.now(born(2));
    assert_eq!(found(&journal_dir), 0, "a file made since with its inode");
}

#[test]
fn two_second_identities_agree_unless_one_kind_says_otherwise() {
    let (a, b) = (born(1), born(2));
    let (g, h) = (Second::Generation(1), Second::Generation(2));
    assert!(a.agrees(a) && g.agrees(g));
    assert!(!a.agrees(b) && !g.agrees(h));
    for one in [a, g, Second::None] {
        assert!(one.agrees(Second::None) && Second::None.agrees(one));
    }
    // Read on two systems, or before and after one learned a kind.
    assert!(a.agrees(g) && g.agrees(a));
    // Both are compared where both hold them, each alone where one does.
    let (one, other) = (both(1, 1), both(1, 2));
    assert!(one.agrees(one) && !one.agrees(other) && !one.agrees(both(2, 1)));
    let late = Second::Born { secs: 2, nanos: 7 };
    let here = Second::Born { secs: 1, nanos: 7 };
    assert!(one.agrees(here) && one.agrees(g) && !one.agrees(late) && !one.agrees(h));
}

/// macOS and the BSDs move a birth time back when a modified time is set
/// to an earlier one, as a publish sets it: acted out, the file born
/// earlier than recorded is still the one recorded, and one born later is
/// not. Elsewhere a birth time never moves, and either is another file.
#[test]
fn a_birth_time_moved_back_is_the_file_recorded_where_the_system_moves_it() {
    let settings = Settings::default();
    let (_scratch, place, journal_dir) = scratch("moved-back");
    let acted = Acted::all(both(5, 9));
    let at = crashed(&place, &journal_dir);
    acted.now(both(4, 9));
    if cfg!(any(target_os = "linux", target_os = "android", windows)) {
        assert_eq!(at.present(&settings), None);
        assert_eq!(at.try_reopen(&settings).map(|_| ()), Err(Left::Replaced));
    } else {
        assert_eq!(at.present(&settings), Some(4), "this system moves it");
    }
    seams::BIRTH_MOVES_BACK.set(true);
    let moved = (at.present(&settings), at.try_reopen(&settings).is_ok());
    // Published with its time set: recovery still finds it.
    std::fs::rename(place.join(NAME), place.join("a.bin")).unwrap();
    let got = recover(&journal_dir, &settings).unwrap().remove(0);
    std::fs::rename(place.join("a.bin"), place.join(NAME)).unwrap();
    // Born later, or with another generation, it is a file made since.
    acted.now(both(6, 9));
    let later = (at.present(&settings), at.try_reopen(&settings).map(|_| ()));
    acted.now(both(4, 8));
    let other = at.try_reopen(&settings).map(|_| ());
    acted.now(both(4, 9));
    let done = clean_up(&journal_dir, &settings).unwrap();
    seams::BIRTH_MOVES_BACK.set(false);
    assert_eq!(moved, (Some(4), true));
    assert_eq!(got.settled.len() + got.candidates.len(), 1);
    assert_eq!(later, (None, Err(Left::Replaced)));
    assert_eq!(other, Err(Left::Replaced));
    assert_eq!(done, cleaned(1, 0, 0));
    assert!(!place.join(NAME).exists());
}

/// A partial recorded with a generation is opened to read it only once a
/// look has shown a regular file with its ID: a folder under its name is
/// not it, with the recorded ID or its own.
#[test]
fn a_generation_is_asked_only_of_a_regular_file_with_the_recorded_id() {
    let settings = Settings::default();
    let (_scratch, place, journal_dir) = scratch("looked-first");
    let _acted = Acted::all(both(1, 40));
    let at = crashed(&place, &journal_dir);
    assert_eq!(at.present(&settings), Some(4));
    std::fs::remove_file(place.join(NAME)).unwrap();
    std::fs::create_dir(place.join(NAME)).unwrap();
    assert_eq!(at.present(&settings), None);
    let folder = Directory::open(&place.join(NAME)).unwrap();
    let as_folder = PartialAt {
        file: folder.identity().unwrap().file,
        ..at.clone()
    };
    assert_eq!(as_folder.present(&settings), None, "its ID, and no file");
}

/// A started partial's bytes, laid out field by field: `second` is what
/// follows the file's ID in the second format, and nothing in the first.
fn started_by_hand(out: &mut Vec<u8>, place: u32, partial: &str, file: u128, second: &[u8]) {
    varint(out, place.into());
    // In the root.
    varint(out, 0);
    text(out, partial.trim_end_matches(".part"));
    text(out, partial);
    // Its declared size.
    varint(out, 4);
    varint(out, file);
    out.extend_from_slice(second);
}

/// A segment of `format` for `root`, laid out byte by byte: a snapshot
/// that lists `a.bin.part`, and an entry that starts `b.bin.part`.
fn segment_by_hand(format: u8, root: &Directory, files: [(u128, &[u8]); 2]) -> Vec<u8> {
    let mut header = b"SCJL".to_vec();
    header.push(format);
    // Its sequence number.
    varint(&mut header, 1);
    match location::root_of(root).unwrap() {
        Root::Windows {
            volume,
            serial,
            folder,
        } => {
            header.push(0);
            text(&mut header, &volume);
            varint(&mut header, serial.into());
            varint(&mut header, folder);
        }
        Root::Unix {
            path,
            device,
            inode,
        } => {
            header.push(1);
            varint(&mut header, path.len() as u128);
            header.extend_from_slice(&path);
            varint(&mut header, device.into());
            varint(&mut header, inode);
        }
    }
    // Not by groups; the mark, the entries below it, no open group, no folder.
    header.extend_from_slice(&[0, 0, 0, 0, 0]);
    // One live partial.
    header.push(1);
    started_by_hand(&mut header, 0, "a.bin.part", files[0].0, files[0].1);
    let mut bytes = log::frame(&header);
    // The kind of an entry that starts one partial.
    let mut entry = vec![2];
    started_by_hand(&mut entry, 1, "b.bin.part", files[1].0, files[1].1);
    bytes.extend_from_slice(&log::frame(&entry));
    bytes
}

/// Two partials with bytes in `place`, and their IDs.
fn two_partials(place: &Path) -> (Directory, [u128; 2]) {
    let root = Directory::destination(place).unwrap();
    let ids = ["a.bin.part", "b.bin.part"].map(|name| {
        std::fs::write(place.join(name), b"half").unwrap();
        identity(&root.entry(OsStr::new(name)).unwrap())
            .unwrap()
            .file
    });
    (root, ids)
}

#[test]
fn a_segment_of_the_format_of_before_is_read_and_its_partials_known_as_before() {
    let settings = Settings::default();
    let (_scratch, place, journal_dir) = scratch("format-1");
    let (root, [a, b]) = two_partials(&place);
    let bytes = segment_by_hand(1, &root, [(a, &[]), (b, &[])]);
    let segment = read_segment(&bytes, &settings).unwrap();
    assert_eq!((segment.format(), segment.entries), (FORMAT, 1));
    assert_eq!(segment.read, bytes.len());
    let partials = segment.partials();
    assert_eq!(partials.len(), 2);
    assert!(partials.iter().all(|s| s.second == Second::None));
    assert!(
        segment
            .places()
            .iter()
            .flatten()
            .all(|at| at.second == Second::None)
    );
    // It writes back as it was: a format is kept, never raised.
    assert_eq!(
        read_segment(&segment.snapshot(), &settings)
            .unwrap()
            .format(),
        FORMAT
    );
    // Whatever the files report now, they are the ones recorded.
    let _acted = Acted::all(born(5));
    std::fs::write(journal_dir.join("1-1.1.jlog"), &bytes).unwrap();
    assert_eq!(clean_up(&journal_dir, &settings).unwrap(), cleaned(2, 0, 0));
    assert!(!place.join("a.bin.part").exists() && !place.join("b.bin.part").exists());
}

#[test]
fn a_segment_of_the_second_format_holds_each_partials_second_identity() {
    let settings = Settings::default();
    let (_scratch, place, journal_dir) = scratch("format-2");
    let (root, [a, b]) = two_partials(&place);
    // A birth time 3 seconds and 5 nanoseconds before the epoch, zigzagged
    // (-4 is 7), and a generation of 300.
    let bytes = segment_by_hand(
        2,
        &root,
        [
            (a, &[1, 7, 0xFB, 0x93, 0xEB, 0xDC, 0x03]),
            (b, &[2, 0xAC, 0x02]),
        ],
    );
    // Both of them for one file: the kind byte 3, the time, the generation.
    let with_both = segment_by_hand(
        2,
        &root,
        [
            (a, &[3, 7, 0xFB, 0x93, 0xEB, 0xDC, 0x03, 0xAC, 0x02]),
            (b, &[0]),
        ],
    );
    let with_both = read_segment(&with_both, &settings).unwrap();
    assert_eq!(with_both.entries, 1);
    let whole = Second::Both {
        secs: -4,
        nanos: 999_999_995,
        generation: 300,
    };
    assert_eq!(with_both.partials()[0].second, whole);
    let again = read_segment(&with_both.snapshot(), &settings).unwrap();
    assert_eq!(again.partials(), with_both.partials());
    let segment = read_segment(&bytes, &settings).unwrap();
    assert_eq!((segment.format(), segment.entries), (FORMAT_SECOND, 1));
    assert_eq!(segment.read, bytes.len());
    let before_epoch = Second::Born {
        secs: -4,
        nanos: 999_999_995,
    };
    let seconds: Vec<Second> = segment.partials().iter().map(|s| s.second).collect();
    assert_eq!(seconds, [before_epoch, Second::Generation(300)]);
    let places: Vec<Second> = segment
        .places()
        .iter()
        .flatten()
        .map(|at| at.second)
        .collect();
    assert_eq!(places, seconds);
    // The snapshot the next segment would begin with says the same.
    let again = read_segment(&segment.snapshot(), &settings).unwrap();
    assert_eq!(again.format(), FORMAT_SECOND);
    assert_eq!(again.partials(), segment.partials());
    // One with none, the kind byte 0, is known as before.
    let none = segment_by_hand(2, &root, [(a, &[0]), (b, &[0])]);
    let none = read_segment(&none, &settings).unwrap();
    assert!(none.partials().iter().all(|s| s.second == Second::None));
    assert_eq!(none.entries, 1);

    // Each is removed only while it reports what was recorded for it.
    let _acted = Acted::all(Second::Generation(300));
    std::fs::write(journal_dir.join("1-1.1.jlog"), &bytes).unwrap();
    assert_eq!(clean_up(&journal_dir, &settings).unwrap(), cleaned(1, 1, 1));
    assert!(place.join("a.bin.part").exists() && !place.join("b.bin.part").exists());
}

#[test]
fn a_second_identity_that_does_not_read_ends_the_reading_there() {
    let settings = Settings::default();
    let (_scratch, place, _) = scratch("damaged");
    let (root, [a, b]) = two_partials(&place);
    // A kind no version has, both with no generation or one past 32 bits,
    // a second of a billion nanoseconds, a generation past 32 bits, and
    // one cut short.
    let bad: [&[u8]; 6] = [
        &[4, 1],
        &[3, 2, 1],
        &[3, 2, 1, 0x80, 0x80, 0x80, 0x80, 0x10],
        &[1, 2, 0x80, 0x94, 0xEB, 0xDC, 0x03],
        &[2, 0x80, 0x80, 0x80, 0x80, 0x10],
        &[1, 2],
    ];
    for second in bad {
        // In the snapshot nothing is read; in an entry, the snapshot is.
        let bytes = segment_by_hand(2, &root, [(a, second), (b, &[0])]);
        assert_eq!(
            read_segment(&bytes, &settings).unwrap_err(),
            JournalError::Damaged,
            "{second:?}"
        );
        let bytes = segment_by_hand(2, &root, [(a, &[0]), (b, second)]);
        let segment = read_segment(&bytes, &settings).unwrap();
        assert_eq!((segment.entries, segment.partials().len()), (0, 1));
    }
    // Bytes of the second format under the first's number: the kind byte
    // would be read as the next field, so nothing of it is taken.
    let bytes = segment_by_hand(1, &root, [(a, &[2, 0xAC, 0x02]), (b, &[])]);
    assert_eq!(
        read_segment(&bytes, &settings).unwrap_err(),
        JournalError::Damaged
    );
}

/// Every version before this one takes a header for whole only when its
/// format byte is 1, and removes a journal with no whole header, touching
/// no file. This version does the same with a format after its own, by the
/// same rule: that is what is acted out here, with the bytes it writes.
#[test]
fn a_format_a_version_does_not_know_is_a_damaged_journal_that_touches_nothing() {
    let settings = Settings::default();
    let (_scratch, place, journal_dir) = scratch("later-format");
    let _acted = Acted::all(born(1));
    crashed(&place, &journal_dir);
    let path = std::fs::read_dir(&journal_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let bytes = std::fs::read(&path).unwrap();
    // The format is the fifth byte of the first frame's payload, after the
    // frame's length and checksum: the byte a version of before compares.
    let at = log::FRAME_HEAD + 4;
    assert_eq!(&bytes[log::FRAME_HEAD..at], b"SCJL");
    assert_eq!(bytes[at], FORMAT_SECOND, "what a version of before refuses");
    assert_ne!(bytes[at], FORMAT);

    // The same journal one format on, its checksum right.
    let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
    let mut payload = bytes[log::FRAME_HEAD..log::FRAME_HEAD + len].to_vec();
    payload[4] = FORMAT_SECOND + 1;
    let mut later = log::frame(&payload);
    later.extend_from_slice(&bytes[log::FRAME_HEAD + len..]);
    assert_eq!(
        read_segment(&later, &settings).unwrap_err(),
        JournalError::Damaged
    );
    std::fs::write(&path, &later).unwrap();
    assert!(recover(&journal_dir, &settings).unwrap().is_empty());
    assert_eq!(clean_up(&journal_dir, &settings).unwrap(), cleaned(0, 0, 0));
    assert_eq!(std::fs::read(place.join(NAME)).unwrap(), b"half");
    assert_eq!(journals(&journal_dir), 0);
}

/// What the system gives, with no seam: a journal on Unix is in the second
/// format, and a file reports the same second identity each time it is
/// asked, whatever is done to it meanwhile.
#[test]
fn a_file_keeps_its_second_identity_through_what_a_transfer_does_to_it() {
    let settings = Settings::default();
    let (_scratch, place, journal_dir) = scratch("real");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings).unwrap();
    let file = partial(&root, &journal, 0, NAME, b"half");
    let second = Second::of(&file);
    assert_eq!(second, Second::of(&file), "written to and asked again");
    let at = PartialAt::of(&root, &root, &file, OsStr::new(NAME)).unwrap();
    assert_eq!(at.second, second);
    let read = read_segment(&std::fs::read(journal.segment_path()).unwrap(), &settings).unwrap();
    let format = if cfg!(unix) { FORMAT_SECOND } else { FORMAT };
    assert_eq!(read.format(), format);
    assert_eq!(read.partials()[0].second, second);
    // Windows keeps a file's ID for its life: nothing more is read there.
    if cfg!(windows) {
        assert_eq!(second, Second::None);
    }
    eprintln!("this file system gives: {second:?}");
    // Where CI says its disk gives both, a reading that fails is a failure
    // here, not a partial quietly known by its inode alone.
    if std::env::var_os("SAFECOPY_REQUIRE_SECOND").is_some_and(|v| !v.is_empty()) {
        assert!(matches!(second, Second::Both { .. }), "{second:?}");
    }
    // Written to, synced, cut and renamed: it is still the file recorded.
    file.set_len(2).unwrap();
    file.sync_all().unwrap();
    assert_eq!(at.present(&settings), Some(2));
    drop(file);
    std::fs::rename(place.join(NAME), place.join("moved.part")).unwrap();
    std::fs::rename(place.join("moved.part"), place.join(NAME)).unwrap();
    assert_eq!(at.present(&settings), Some(2));
    assert!(at.reopen(&settings).is_some());
    journal.crash();
    drop(root);
    assert_eq!(clean_up(&journal_dir, &settings).unwrap(), cleaned(1, 0, 0));
}

/// The state itself, where the file system gives a removed file's inode to
/// the next one made: an entry that outlived its partial, and a new partial
/// of the same name with the same inode.
#[cfg(unix)]
#[test]
fn a_new_partial_given_the_inode_of_one_removed_is_not_taken_for_it() {
    /// How many files are made, looking for one with the inode and another
    /// second identity: a birth time moves on only with the clock.
    const TRIES: u32 = 200_000;
    /// How many of them are kept, each holding an inode that is not the one.
    const SPARES: u32 = 4096;
    let settings = Settings::default();
    let (_scratch, place, journal_dir) = scratch("reused");
    let at = crashed(&place, &journal_dir);
    if at.second == Second::None {
        eprintln!("skipped: this file system gives a file no second identity");
        return;
    }
    let root = Directory::destination(&place).unwrap();
    // The person removes the partial by hand: its entry lives on.
    std::fs::remove_file(place.join(NAME)).unwrap();
    let (mut new, mut spares) = (None, 0);
    for _ in 0..TRIES {
        let file = root.create(OsStr::new(NAME)).unwrap();
        if identity(&file).unwrap().file != at.file {
            // Another free inode came first, one a test beside this one
            // let go of: kept in use, so the next file gets the next.
            drop(file);
            if spares == SPARES {
                break;
            }
            spares += 1;
            std::fs::rename(place.join(NAME), place.join(format!("spare-{spares}"))).unwrap();
        } else if at.second.agrees(Second::of(&file)) {
            // Made in the clock's step the first was made in: once more.
            drop(file);
            std::fs::remove_file(place.join(NAME)).unwrap();
        } else {
            new = Some(file);
            break;
        }
    }
    let Some(mut new) = new else {
        eprintln!("skipped: no new file was given the removed one's inode ({spares} others)");
        return;
    };
    eprintln!("ran: a new file has the removed partial's inode");
    io::Write::write_all(&mut new, b"a new transfer").unwrap();
    // By the inode alone it is the file recorded: what a record without a
    // second identity still says.
    let as_before = PartialAt {
        second: Second::None,
        ..at.clone()
    };
    assert_eq!(as_before.present(&settings), Some(14));

    assert_eq!(at.present(&settings), None);
    assert_eq!(at.try_reopen(&settings).map(|_| ()), Err(Left::Replaced));
    assert!(at.reopen(&settings).is_none());
    assert_eq!(clean_up(&journal_dir, &settings).unwrap(), cleaned(0, 1, 0));
    assert_eq!(std::fs::read(place.join(NAME)).unwrap(), b"a new transfer");
    assert_eq!(journals(&journal_dir), 0);
}
