use super::*;
use std::path::PathBuf;

/// A new, empty folder for one test, under the build's target folder.
fn temp(name: &str) -> PathBuf {
    // The test binary is in target/<profile>/deps.
    let exe = std::env::current_exe().unwrap();
    let target = exe.ancestors().nth(3).unwrap();
    let dir = target
        .join("test-tmp")
        .join(format!("journal-{name}-{}", std::process::id()));
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

/// A scratch folder with a folder to write into and one for journals.
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

fn settings() -> Settings {
    Settings::default()
}

fn cleaned(removed: usize, left: usize) -> Cleaned {
    Cleaned {
        removed,
        left,
        kept: 0,
    }
}

/// Whether a file symbolic link that cannot be made fails its test: CI sets
/// `SAFECOPY_REQUIRE_SYMLINKS` on its runners, which are administrators.
#[cfg(windows)]
fn symlinks_required() -> bool {
    std::env::var_os("SAFECOPY_REQUIRE_SYMLINKS").is_some_and(|v| !v.is_empty())
}

// The record's bytes, on every system.

fn sample() -> Record {
    Record {
        volume: r"\\?\Volume{0}\".to_string(),
        serial: 300,
        partials: vec![
            Partial {
                folder: 1,
                name: "a.part".to_string(),
                file: u128::MAX,
            },
            Partial {
                folder: 1 << 70,
                name: "Fotografii vechi.jpg.part".to_string(),
                file: 0,
            },
        ],
    }
}

#[test]
fn a_record_has_the_whole_record_format_of_before() {
    let record = Record {
        volume: "V".to_string(),
        serial: 300,
        partials: vec![Partial {
            folder: 1,
            name: "a.part".to_string(),
            file: u128::MAX,
        }],
    };
    let mut expected = vec![1, b'V', 0xAC, 0x02, 1, 1, 6];
    expected.extend_from_slice(b"a.part");
    expected.extend_from_slice(&[0xFF; 18]);
    expected.push(0x03);
    assert_eq!(record.encode(&settings()).unwrap(), expected);
    assert_eq!(Record::decode(&expected, &settings()).unwrap(), record);
}

#[test]
fn a_record_reads_back_as_it_was_written() {
    let record = sample();
    let bytes = record.encode(&settings()).unwrap();
    assert_eq!(Record::decode(&bytes, &settings()).unwrap(), record);
    let empty = Record {
        partials: Vec::new(),
        ..record
    };
    let bytes = empty.encode(&settings()).unwrap();
    assert_eq!(Record::decode(&bytes, &settings()).unwrap(), empty);
}

#[test]
fn a_record_cut_at_any_byte_is_damaged() {
    let bytes = sample().encode(&settings()).unwrap();
    for len in 0..bytes.len() {
        assert_eq!(
            Record::decode(&bytes[..len], &settings()),
            Err(JournalError::Damaged),
            "cut at {len}"
        );
    }
}

#[test]
fn bytes_after_the_last_partial_make_a_record_damaged() {
    let mut bytes = sample().encode(&settings()).unwrap();
    bytes.push(0);
    assert_eq!(
        Record::decode(&bytes, &settings()),
        Err(JournalError::Damaged)
    );
}

#[test]
fn a_number_past_its_width_or_text_that_is_not_utf8_is_damaged() {
    // A serial of eleven bytes, and one of ten whose last byte passes 64 bits.
    let mut long = vec![0, 0x80];
    long.extend_from_slice(&[0x80; 9]);
    long.extend_from_slice(&[0x00, 0x00]);
    assert_eq!(
        Record::decode(&long, &settings()),
        Err(JournalError::Damaged)
    );
    let mut wide = vec![0];
    wide.extend_from_slice(&[0xFF; 9]);
    wide.extend_from_slice(&[0x02, 0x00]);
    assert_eq!(
        Record::decode(&wide, &settings()),
        Err(JournalError::Damaged)
    );
    assert_eq!(
        Record::decode(&[1, 0xFF, 0, 0], &settings()),
        Err(JournalError::Damaged)
    );
    // A longer form of a number than needed is taken.
    assert_eq!(
        Record::decode(&[0, 0x81, 0x00, 0], &settings())
            .unwrap()
            .serial,
        1
    );
}

#[test]
fn a_record_past_its_limit_is_neither_written_nor_read() {
    let record = sample();
    let bytes = record.encode(&settings()).unwrap();
    let tight = Settings {
        record_bytes: bytes.len() - 1,
        ..settings()
    };
    let most = tight.record_bytes;
    assert_eq!(record.encode(&tight), Err(JournalError::TooLarge { most }));
    assert_eq!(
        Record::decode(&bytes, &tight),
        Err(JournalError::TooLarge { most })
    );
    let exact = Settings {
        record_bytes: bytes.len(),
        ..settings()
    };
    assert_eq!(Record::decode(&bytes, &exact).unwrap(), record);
}

#[test]
fn a_count_larger_than_the_bytes_allocates_nothing_for_it() {
    // A count of u64::MAX partials with no bytes after it.
    let mut bytes = vec![0, 0];
    bytes.extend_from_slice(&[0xFF; 9]);
    bytes.push(0x01);
    assert_eq!(
        Record::decode(&bytes, &settings()),
        Err(JournalError::Damaged)
    );
}

#[test]
fn only_a_partial_name_of_one_part_naming_no_stream_is_removable() {
    let s = settings();
    assert!(removable("a.txt.part", &s));
    assert!(!removable("report.docx", &s));
    assert!(!removable("report.docx:x.part", &s));
    assert!(!removable(r"..\a.part", &s));
    assert!(!removable("../a.part", &s));
    let none = Settings {
        partial_ending: String::new(),
        ..settings()
    };
    assert!(
        !removable("report.docx", &none),
        "an empty ending removes nothing"
    );
}

#[test]
fn a_missing_journal_folder_has_nothing_to_clean() {
    let base = temp("missing");
    let _scratch = Scratch(base.clone());
    assert_eq!(
        clean_up(&base.join("none"), &settings()).unwrap(),
        Cleaned::default()
    );
}

#[test]
fn a_corrupt_journal_is_removed_and_touches_nothing_in_the_folder() {
    let (_scratch, place, journal_dir) = scratch("corrupt");
    std::fs::write(place.join("a.txt.part"), b"theirs").unwrap();
    std::fs::write(place.join("b.txt"), b"theirs").unwrap();
    std::fs::write(journal_dir.join("1-1.journal"), b"\xff\xff\xffnot a record").unwrap();
    std::fs::write(journal_dir.join("1-2.journal"), b"").unwrap();
    std::fs::write(journal_dir.join("notes.txt"), b"not a journal").unwrap();
    assert_eq!(
        clean_up(&journal_dir, &settings()).unwrap(),
        Cleaned::default()
    );
    assert_eq!(journals(&journal_dir), 1, "only journals are removed");
    assert_eq!(std::fs::read(place.join("a.txt.part")).unwrap(), b"theirs");
    assert_eq!(std::fs::read(place.join("b.txt")).unwrap(), b"theirs");
}

#[test]
fn a_corrupt_log_segment_is_removed_and_touches_nothing() {
    let (_scratch, place, journal_dir) = scratch("corrupt-log");
    std::fs::write(place.join("a.txt.part"), b"theirs").unwrap();
    std::fs::write(journal_dir.join("1-1.1.jlog"), b"\xff\xff\xffnot a log").unwrap();
    std::fs::write(journal_dir.join("1-2.1.jlog"), b"").unwrap();
    std::fs::write(journal_dir.join("no-number.jlog"), b"").unwrap();
    assert_eq!(
        clean_up(&journal_dir, &settings()).unwrap(),
        Cleaned::default()
    );
    assert_eq!(
        journals(&journal_dir),
        1,
        "a name that is no segment's is left"
    );
    assert_eq!(std::fs::read(place.join("a.txt.part")).unwrap(), b"theirs");
}

/// Creates the partial `name` in `dir`, records it for `place` and writes
/// `bytes` to it.
pub(crate) fn partial(
    dir: &Directory,
    journal: &Journal,
    place: u32,
    name: &str,
    bytes: &[u8],
) -> File {
    use std::io::Write;
    let mut file = dir.create(OsStr::new(name)).unwrap();
    let published = name.strip_suffix(".part").unwrap_or(name);
    journal
        .started(
            place,
            dir,
            &file,
            OsStr::new(name),
            published,
            bytes.len() as u64,
        )
        .unwrap();
    file.write_all(bytes).unwrap();
    file
}

// Partials the caller keeps.

#[test]
fn cleanup_keeps_a_journal_while_a_partial_it_lists_is_kept() {
    let (_scratch, place, journal_dir) = scratch("kept-by-caller");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    drop(partial(&root, &journal, 0, "a.part", b"half"));
    drop(partial(&root, &journal, 1, "b.part", b"half"));
    // The cut: `a.part` is kept, and nothing is called for it.
    journal.crash();
    drop(root);
    let keep_a = |at: &PartialAt| at.partial == "a.part";
    // The second time `b.part` is gone already, and counted as left.
    for (removed, left) in [(1, 1), (0, 2)] {
        let done = clean_up_keeping(&journal_dir, &settings(), &keep_a).unwrap();
        assert_eq!(done.kept, 1, "the journal stays with what it lists");
        assert_eq!((done.removed, done.left), (removed, left));
        assert!(place.join("a.part").exists());
        assert!(!place.join("b.part").exists());
        assert_eq!(journals(&journal_dir), 1);
    }
    // Kept no more (its record torn or gone): the partial goes, then the journal.
    assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(1, 1));
    assert!(!place.join("a.part").exists(), "no orphan");
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn a_journal_kept_for_the_caller_is_held_to_the_most_kept() {
    let (_scratch, place, journal_dir) = scratch("kept-by-caller-most");
    let root = Directory::destination(&place).unwrap();
    let day = std::time::Duration::from_secs(24 * 60 * 60);
    for (n, ago) in [(1, 2), (2, 1)] {
        let before: Vec<PathBuf> = std::fs::read_dir(&journal_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
        drop(partial(&root, &journal, 0, &format!("{n}.part"), b"half"));
        journal.crash();
        let path = std::fs::read_dir(&journal_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| !before.contains(p))
            .unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(std::time::SystemTime::now() - ago * day)
            .unwrap();
    }
    drop(root);
    // The defaults hold a store's 256 kept files, each alone in its journal.
    assert!(settings().keep_most >= 256);
    assert!(settings().keep_for >= day);
    let one = Settings {
        keep_most: 1,
        ..settings()
    };
    let done = clean_up_keeping(&journal_dir, &one, &|_| true).unwrap();
    assert_eq!((done.removed, done.left, done.kept), (0, 2, 1));
    assert_eq!(journals(&journal_dir), 1, "the oldest journal goes");
    // Past the limit a journal goes alone: its partial is not removed.
    assert!(place.join("1.part").exists());
    assert!(place.join("2.part").exists());
}

#[test]
fn a_reopened_partial_with_bytes_is_listed_by_the_new_runs_journal() {
    let (_scratch, place, journal_dir) = scratch("reopened");
    let root = Directory::destination(&place).unwrap();
    std::fs::write(place.join("a.bin.part"), b"kept from before").unwrap();
    let file = root.reopen(OsStr::new("a.bin.part")).unwrap();
    assert_eq!(file.metadata().unwrap().len(), 16);
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    journal
        .started(0, &root, &file, OsStr::new("a.bin.part"), "a.bin", 100)
        .unwrap();
    assert_eq!(journal.partials()[0].partial, "a.bin.part");
    drop(file);
    journal.crash();
    drop(root);
    assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(1, 0));
    assert!(!place.join("a.bin.part").exists());
}

#[test]
fn a_partial_is_found_present_without_being_opened_for_its_bytes() {
    let (_scratch, place, _) = scratch("present");
    let root = Directory::destination(&place).unwrap();
    // As a save holds it: on Windows, with no delete sharing.
    let file = root.create(OsStr::new("a.bin.part")).unwrap();
    file.set_len(4096).unwrap();
    let at = PartialAt::of(&root, &root, &file, OsStr::new("a.bin.part")).unwrap();
    assert_eq!(at.present(), Some(4096), "while a save has it open");
    // The handle shuts nothing out: the save's next opens still work.
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Sharing nothing at all, as no save does.
        drop(file);
        let alone = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(place.join("a.bin.part"))
            .unwrap();
        assert_eq!(at.present(), Some(4096), "attributes need no sharing");
        assert_eq!(at.try_reopen().err(), Some(Left::Busy));
        assert!(at.reopen().is_none());
        drop(alone);
    }
    #[cfg(not(windows))]
    drop(file);
    let (_, again) = at.try_reopen().unwrap();
    again.set_len(10).unwrap();
    drop(again);
    assert_eq!(at.present(), Some(10));
    // Another file under the name is not it, whatever its length.
    std::fs::rename(place.join("a.bin.part"), place.join("moved")).unwrap();
    std::fs::write(place.join("a.bin.part"), [0; 4096]).unwrap();
    assert_eq!(at.present(), None, "replaced");
    assert_eq!(at.try_reopen().err(), Some(Left::Replaced));
    // Nor is a folder, a link or nothing at all.
    std::fs::remove_file(place.join("a.bin.part")).unwrap();
    assert_eq!(at.present(), None, "gone");
    assert_eq!(at.try_reopen().err(), Some(Left::Gone));
    std::fs::create_dir(place.join("a.bin.part")).unwrap();
    assert_eq!(at.present(), None, "a folder");
    std::fs::remove_dir(place.join("a.bin.part")).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(place.join("moved"), place.join("a.bin.part")).unwrap();
        assert_eq!(at.present(), None, "a link to the very file");
    }
}

#[test]
fn a_held_record_file_is_read_by_others_and_written_by_one() {
    use std::io::{Read, Write};
    let (_scratch, place, _) = scratch("hold");
    let path = place.join("a.resume");
    let mut made = files::create(&path).unwrap();
    made.write_all(b"one").unwrap();
    // A new file is read while its maker holds it, and held by no other.
    let read = |path: &Path| {
        let mut text = String::new();
        files::open_read(path)
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        text
    };
    assert_eq!(read(&path), "one");
    assert!(files::busy(&files::hold(&path).unwrap_err()));
    drop(made);
    // So is one that was there and is held again.
    let mut held = files::hold(&path).unwrap();
    held.write_all(b"two").unwrap();
    assert_eq!(read(&path), "two");
    assert!(files::busy(&files::hold(&path).unwrap_err()));
    assert!(files::busy(&files::take(&path, 16).unwrap_err()));
    drop(held);
    // Let go, it is another's to hold or to take.
    drop(files::hold(&path).unwrap());
    let (taken, bytes) = files::take(&path, 16).unwrap();
    assert_eq!(bytes, b"two");
    assert!(files::busy(&files::hold(&path).unwrap_err()));
    files::delete(&taken, &path).unwrap();
    assert!(
        !files::busy(&files::hold(&path).unwrap_err()),
        "gone, not held"
    );
}

/// Where a file is removed by name while another has it open, a record
/// removed between a hold's open and its lock is not held: the lock would
/// be on a file no name reaches.
#[cfg(unix)]
#[test]
fn a_record_file_removed_while_it_is_being_held_is_not_held() {
    let (_scratch, place, _) = scratch("hold-removed");
    let path = place.join("a.resume");
    std::fs::write(&path, b"one").unwrap();
    let gone = path.clone();
    seams::HOLD_OPENED.set(Some(Box::new(move || {
        std::fs::remove_file(&gone).unwrap();
    })));
    let held = files::hold(&path).unwrap_err();
    assert_eq!(held.kind(), io::ErrorKind::NotFound);
    // So is one whose name another file has taken since.
    std::fs::write(&path, b"one").unwrap();
    let (gone, other) = (path.clone(), place.join("b.resume"));
    std::fs::write(&other, b"two").unwrap();
    seams::HOLD_OPENED.set(Some(Box::new(move || {
        std::fs::rename(&other, &gone).unwrap();
    })));
    let held = files::hold(&path).unwrap_err();
    assert_eq!(held.kind(), io::ErrorKind::NotFound);
    assert!(files::hold(&path).is_ok());
}

use std::fs::File;
use std::process::Command;

/// The test binary run again as another process, in `role`.
fn child(role: &str, place: &Path, journal_dir: &Path) -> String {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["tests::as_child", "--exact", "--ignored", "--nocapture"])
        .args(["--test-threads", "1"])
        .env("SAFECOPY_JOURNAL_CHILD", role)
        .env("SAFECOPY_JOURNAL_PLACE", place)
        .env("SAFECOPY_JOURNAL_DIR", journal_dir)
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "the child failed: {out}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    out
}

/// What another process does: started by [`child`], skipped otherwise.
#[test]
#[ignore = "run by other tests as a second process"]
fn as_child() {
    let Some(role) = std::env::var_os("SAFECOPY_JOURNAL_CHILD") else {
        return;
    };
    let place = PathBuf::from(std::env::var_os("SAFECOPY_JOURNAL_PLACE").unwrap());
    let journal_dir = PathBuf::from(std::env::var_os("SAFECOPY_JOURNAL_DIR").unwrap());
    match role.to_str().unwrap() {
        "crash" => {
            let root = Directory::destination(&place).unwrap();
            let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
            let sub = root.claim("Photos").unwrap();
            let _a = partial(&root, &journal, 0, "a.txt.part", b"half");
            let _b = partial(&sub, &journal, 1, "b.jpg.part", b"half");
            let _c = partial(&root, &journal, 2, "c.txt.part", b"half");
            // A crash: no destructor runs; the system closes the handles.
            std::process::exit(0);
        }
        "clean" => {
            let done = clean_up(&journal_dir, &settings()).unwrap();
            println!("cleaned {} {}", done.removed, done.left);
        }
        other => panic!("no role {other}"),
    }
}

#[test]
fn a_crash_leaves_a_record_that_cleanup_removes_by_identity() {
    let (_scratch, place, journal_dir) = scratch("crash-process");
    child("crash", &place, &journal_dir);
    assert_eq!(journals(&journal_dir), 1, "the crash leaves its journal");
    assert!(place.join("a.txt.part").exists());
    let done = clean_up(&journal_dir, &settings()).unwrap();
    assert_eq!(done, cleaned(3, 0));
    assert!(!place.join("a.txt.part").exists());
    assert!(!place.join("Photos/b.jpg.part").exists());
    assert!(!place.join("c.txt.part").exists());
    assert!(place.join("Photos").is_dir(), "only the partial files go");
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn a_renamed_or_replaced_partial_is_left_alone() {
    let (_scratch, place, journal_dir) = scratch("renamed-replaced");
    child("crash", &place, &journal_dir);
    // Someone renamed one and put another file under the other's name.
    std::fs::rename(place.join("a.txt.part"), place.join("kept.txt")).unwrap();
    // Made before the old one goes, so it never takes the old inode.
    std::fs::write(place.join("theirs.tmp"), b"theirs").unwrap();
    std::fs::remove_file(place.join("Photos/b.jpg.part")).unwrap();
    std::fs::rename(place.join("theirs.tmp"), place.join("Photos/b.jpg.part")).unwrap();
    let done = clean_up(&journal_dir, &settings()).unwrap();
    assert_eq!(done, cleaned(1, 2));
    assert_eq!(std::fs::read(place.join("kept.txt")).unwrap(), b"half");
    assert_eq!(
        std::fs::read(place.join("Photos/b.jpg.part")).unwrap(),
        b"theirs"
    );
    assert!(!place.join("c.txt.part").exists());
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn a_record_held_open_by_one_process_is_left_alone_by_another() {
    let (_scratch, place, journal_dir) = scratch("held");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    drop(partial(&root, &journal, 0, "a.txt.part", b"half"));
    let out = child("clean", &place, &journal_dir);
    assert!(out.contains("cleaned 0 0"), "the child said: {out}");
    assert!(place.join("a.txt.part").exists());
    assert_eq!(journals(&journal_dir), 1);
    // Once this process is done with it, another takes it.
    journal.crash();
    let out = child("clean", &place, &journal_dir);
    assert!(out.contains("cleaned 1 0"), "the child said: {out}");
    assert!(!place.join("a.txt.part").exists());
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn a_crash_leaves_a_partial_file_the_next_start_removes_through_its_handle() {
    let (_scratch, place, journal_dir) = scratch("crash");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    let sub = root.claim("Photos").unwrap();
    drop(partial(&root, &journal, 0, "a.txt.part", b"half"));
    drop(partial(&sub, &journal, 1, "b.jpg.part", b"half"));
    // While its transfer runs, a cleanup leaves it alone.
    assert_eq!(
        clean_up(&journal_dir, &settings()).unwrap(),
        Cleaned::default()
    );
    assert!(place.join("a.txt.part").exists());
    journal.crash();
    drop(root);
    drop(sub);
    assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(2, 0));
    assert!(!place.join("a.txt.part").exists());
    assert!(!place.join("Photos/b.jpg.part").exists());
    assert!(place.join("Photos").is_dir(), "only the partial files go");
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn a_published_or_removed_partial_file_leaves_the_journal() {
    let (_scratch, place, journal_dir) = scratch("ended");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    drop(partial(&root, &journal, 0, "a.txt.part", b"whole"));
    drop(partial(&root, &journal, 1, "b.txt.part", b"cut"));
    assert_eq!(journals(&journal_dir), 1);
    journal.done(0).unwrap();
    std::fs::rename(place.join("a.txt.part"), place.join("a.txt")).unwrap();
    journal.ended(1).unwrap();
    root.remove(OsStr::new("b.txt.part")).unwrap();
    assert!(journal.partials().is_empty());
    drop(journal);
    assert_eq!(
        journals(&journal_dir),
        0,
        "an ended transfer keeps no journal"
    );
    assert!(place.join("a.txt").exists());
}

#[test]
fn a_dropped_journal_with_a_partial_still_recorded_is_kept() {
    let (_scratch, place, journal_dir) = scratch("dropped");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    drop(partial(&root, &journal, 0, "a.txt.part", b"half"));
    drop(journal);
    assert_eq!(journals(&journal_dir), 1);
    assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(1, 0));
    assert!(!place.join("a.txt.part").exists());
}

#[test]
fn a_name_without_the_partial_ending_is_never_removed() {
    let (_scratch, place, journal_dir) = scratch("pattern");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    drop(partial(&root, &journal, 0, "report.docx", b"theirs"));
    journal.crash();
    drop(root);
    assert_eq!(clean_up(&journal_dir, &settings()).unwrap().left, 1);
    assert!(place.join("report.docx").exists());
}

#[test]
fn a_partial_whose_entry_would_pass_the_segment_limit_is_not_recorded() {
    let (_scratch, place, journal_dir) = scratch("limit");
    let root = Directory::destination(&place).unwrap();
    // Room for the header and exactly one partial's entry.
    let probe = Journal::start(&journal_dir, &root, &settings()).unwrap();
    let a = root.create(OsStr::new("a.part")).unwrap();
    probe
        .started(0, &root, &a, OsStr::new("a.part"), "a", 4)
        .unwrap();
    let one = std::fs::metadata(probe.segment_path()).unwrap().len() as usize;
    probe.ended(0).unwrap();
    drop(probe);
    let tight = Settings {
        segment_bytes: one,
        ..settings()
    };
    let journal = Journal::start(&journal_dir, &root, &tight).unwrap();
    journal
        .started(0, &root, &a, OsStr::new("a.part"), "a", 4)
        .unwrap();
    drop(a);
    let b = root.create(OsStr::new("b.part")).unwrap();
    assert_eq!(
        journal.started(1, &root, &b, OsStr::new("b.part"), "b", 4),
        Err(JournalError::TooLarge { most: one })
    );
    assert_eq!(journal.partials().len(), 1, "the one it took is kept");
    root.discard(&b, OsStr::new("b.part")).unwrap();
    drop(b);
    journal.crash();
    drop(root);
    // The journal on the disk still holds the one partial it took.
    assert_eq!(clean_up(&journal_dir, &tight).unwrap(), cleaned(1, 0));
    assert!(!place.join("a.part").exists());
}

#[test]
fn a_segment_longer_than_the_limit_is_read_only_that_far() {
    let (_scratch, place, journal_dir) = scratch("too-long-log");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    drop(partial(&root, &journal, 0, "a.part", b"half"));
    journal.crash();
    drop(root);
    // Not even its header fits: it is removed and touches nothing.
    let tight = Settings {
        segment_bytes: 8,
        ..settings()
    };
    assert_eq!(clean_up(&journal_dir, &tight).unwrap(), Cleaned::default());
    assert!(place.join("a.part").exists());
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn a_journal_name_left_by_an_earlier_process_is_stepped_past() {
    use std::sync::atomic::AtomicU64;
    let (_scratch, _, journal_dir) = scratch("taken-name");
    let pid = std::process::id();
    let name = |n: u64| journal_dir.join(format!("{pid}-{n}.1.{LOG_EXTENSION}"));
    for n in 1..=4 {
        std::fs::write(name(n), b"old").unwrap();
    }
    let next = AtomicU64::new(1);
    let tail = format!(".1.{LOG_EXTENSION}");
    drop(files::create_named(&journal_dir, &next, "", &tail).unwrap());
    assert_eq!(
        std::fs::read(name(5)).unwrap(),
        b"",
        "the fifth name is taken"
    );
    for n in 1..=4 {
        assert_eq!(
            std::fs::read(name(n)).unwrap(),
            b"old",
            "never written over"
        );
    }
    // Past 64 taken names it gives up.
    for n in 6..=69 {
        std::fs::write(name(n), b"old").unwrap();
    }
    let next = AtomicU64::new(6);
    assert_eq!(
        files::create_named(&journal_dir, &next, "", &tail)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert!(!name(70).exists());
}

#[test]
fn a_link_named_as_a_journal_is_never_followed() {
    let (_scratch, place, journal_dir) = scratch("linked-journal");
    let target = place.join("thesis.docx");
    std::fs::write(&target, b"theirs").unwrap();
    for link in ["1-1.journal", "1-1.1.jlog"] {
        let link = journal_dir.join(link);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        // A file symbolic link needs Developer Mode or an administrator, as
        // on CI's runners; a junction cannot name a file.
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(&target, &link).is_err() {
            assert!(
                !symlinks_required(),
                "SAFECOPY_REQUIRE_SYMLINKS is set, but no file symbolic link can be made \
                 (it needs Developer Mode or an administrator)"
            );
            eprintln!("skipped: no symbolic link can be made here");
            return;
        }
    }
    assert_eq!(
        clean_up(&journal_dir, &settings()).unwrap(),
        Cleaned::default()
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"theirs");
    assert_eq!(journals(&journal_dir), 2, "the links are left");
}

#[test]
fn a_partial_in_a_folder_that_is_gone_ends_its_journal() {
    let (_scratch, place, journal_dir) = scratch("folder-gone");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    let sub = root.claim("Photos").unwrap();
    drop(partial(&sub, &journal, 0, "a.part", b"half"));
    journal.crash();
    drop((root, sub));
    std::fs::remove_dir_all(place.join("Photos")).unwrap();
    assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(0, 1));
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn a_record_write_that_fails_keeps_the_partials_on_the_disk() {
    let (_scratch, place, journal_dir) = scratch("failed-write");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    drop(partial(&root, &journal, 0, "a.part", b"half"));
    let b = root.create(OsStr::new("b.part")).unwrap();
    seams::FAIL_WRITES.set(1);
    assert!(matches!(
        journal.started(1, &root, &b, OsStr::new("b.part"), "b", 1),
        Err(JournalError::Io { .. })
    ));
    root.discard(&b, OsStr::new("b.part")).unwrap();
    drop(b);
    // The disk holds the entries before, whole, and nothing of the failed one.
    let bytes = std::fs::read(journal.segment_path()).unwrap();
    let segment = read_segment(&bytes, &settings()).unwrap();
    assert_eq!(segment.read, bytes.len(), "no torn bytes are left");
    let names: Vec<_> = segment
        .partials()
        .iter()
        .map(|p| p.partial.as_str())
        .collect();
    assert_eq!(names, ["a.part"]);
    // An end that cannot be written leaves the partial recorded.
    seams::FAIL_WRITES.set(1);
    assert!(matches!(journal.ended(0), Err(JournalError::Io { .. })));
    assert_eq!(journal.partials().len(), 1);
    journal.crash();
    drop(root);
    assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(1, 0));
    assert!(!place.join("a.part").exists());
}

#[test]
fn a_partial_name_that_is_not_text_is_refused() {
    let (_scratch, place, journal_dir) = scratch("not-text");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    let file = root.create(OsStr::new("a.part")).unwrap();
    // A lone surrogate on Windows, a byte that is no UTF-8 elsewhere.
    #[cfg(windows)]
    let name = {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[0xD800, u16::from(b'.')])
    };
    #[cfg(unix)]
    let name = {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(vec![0xFF, b'.'])
    };
    assert_eq!(
        journal.started(0, &root, &file, &name, "a", 0),
        Err(JournalError::NameNotText)
    );
    assert!(journal.partials().is_empty());
    root.discard(&file, OsStr::new("a.part")).unwrap();
}

#[test]
fn a_panic_while_the_journal_is_held_does_not_stop_it() {
    let (_scratch, place, journal_dir) = scratch("poisoned");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    let open = journal.inner.as_ref().unwrap();
    let panicked = std::thread::scope(|s| {
        s.spawn(|| {
            let _held = lock(&open.log);
            panic!("a panic while the journal is held");
        })
        .join()
    });
    assert!(panicked.is_err());
    drop(partial(&root, &journal, 0, "a.part", b"half"));
    journal.ended(0).unwrap();
    root.remove(OsStr::new("a.part")).unwrap();
    drop(journal);
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn a_journal_that_records_nothing_writes_nothing() {
    let (_scratch, place, journal_dir) = scratch("none");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::none();
    let file = root.create(OsStr::new("a.part")).unwrap();
    journal
        .started(0, &root, &file, OsStr::new("a.part"), "a", 0)
        .unwrap();
    journal.done(0).unwrap();
    journal.ended(0).unwrap();
    assert!(journal.progress().is_none());
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn a_folder_not_below_the_root_is_refused() {
    let (_scratch, place, journal_dir) = scratch("not-below");
    let root = Directory::destination(&place.join("..")).unwrap();
    let inner = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &inner, &settings()).unwrap();
    let file = root.create(OsStr::new("outside.part")).unwrap();
    assert_eq!(
        journal.started(0, &root, &file, OsStr::new("outside.part"), "o", 0),
        Err(JournalError::NotBelow)
    );
    // Nor is it taken for a renamed folder where no place can be read.
    seams::PLACE.set(seams::Place::Unknown);
    let unread = journal.started(0, &root, &file, OsStr::new("outside.part"), "o", 0);
    seams::PLACE.set(seams::Place::Read);
    assert_eq!(unread, Err(JournalError::NotBelow));
    // Nor where the place is read and the reading fails.
    seams::PLACE.set(seams::Place::Fails);
    let failed = journal.started(0, &root, &file, OsStr::new("outside.part"), "o", 0);
    seams::PLACE.set(seams::Place::Read);
    assert_eq!(failed, Err(JournalError::NotBelow));
    assert!(journal.partials().is_empty());
    root.discard(&file, OsStr::new("outside.part")).unwrap();
}

/// Whether this system reads where a held folder is now (see the crate
/// docs): elsewhere a renamed folder is known by its identity alone.
fn place_known() -> bool {
    cfg!(any(windows, target_os = "linux", target_os = "android"))
}

/// A link to the folder `target` at `link`: a junction on Windows, which
/// needs no privilege.
fn link_folder(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    {
        let made = Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
    }
}

/// The names of the folders the journal records for its one live partial.
fn recorded_levels(journal: &Journal) -> Vec<String> {
    let bytes = std::fs::read(journal.segment_path()).unwrap();
    let segment = read_segment(&bytes, &settings()).unwrap();
    let [started] = segment.partials() else {
        panic!("{:?}", segment.partials());
    };
    let chain = segment.chain(started.folder).unwrap();
    chain.iter().map(|f| f.name.clone()).collect()
}

#[test]
fn a_folder_renamed_during_a_transfer_does_not_stop_it() {
    let (_scratch, place, journal_dir) = scratch("renamed");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    let sub = root.claim("Photos").unwrap();
    // Closed before the rename: Windows renames no folder with a file open in it.
    drop(partial(&sub, &journal, 0, "a.jpg.part", b"cut"));
    std::fs::rename(place.join("Photos"), place.join("Album")).unwrap();
    let file = partial(&sub, &journal, 1, "b.jpg.part", b"whole");
    journal.ended(0).unwrap();
    sub.remove(OsStr::new("a.jpg.part")).unwrap();
    let expected = if place_known() { "Album" } else { "" };
    assert_eq!(recorded_levels(&journal), [expected]);
    let (from, to) = (OsStr::new("b.jpg.part"), OsStr::new("b.jpg"));
    sub.finish(&file, from, to)
        .or_else(|_| sub.rename(&file, from, to))
        .unwrap();
    journal.done(1).unwrap();
    assert!(journal.partials().is_empty());
    assert_eq!(std::fs::read(place.join("Album/b.jpg")).unwrap(), b"whole");
    drop(file);
    drop(journal);
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn a_crash_after_a_folder_was_renamed_removes_only_its_own_partials() {
    let (_scratch, place, journal_dir) = scratch("renamed-crash");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    let sub = root.claim("Photos").unwrap();
    drop(partial(&sub, &journal, 0, "a.jpg.part", b"half"));
    std::fs::rename(place.join("Photos"), place.join("Album")).unwrap();
    drop(partial(&sub, &journal, 1, "b.jpg.part", b"half"));
    journal.crash();
    drop((root, sub));
    // Another folder where the renamed one used to be, with files of the same names.
    std::fs::create_dir(place.join("Photos")).unwrap();
    for name in ["a.jpg.part", "b.jpg.part"] {
        std::fs::write(place.join("Photos").join(name), b"theirs").unwrap();
    }
    let done = clean_up(&journal_dir, &settings()).unwrap();
    for name in ["a.jpg.part", "b.jpg.part"] {
        assert_eq!(
            std::fs::read(place.join("Photos").join(name)).unwrap(),
            b"theirs",
            "{name}"
        );
    }
    let before = place.join("Album/a.jpg.part");
    let after = place.join("Album/b.jpg.part");
    if cfg!(windows) {
        // The folder's ID opens it under any name.
        assert_eq!(done, cleaned(2, 0));
        assert!(!before.exists() && !after.exists());
    } else if place_known() {
        // Recorded under its present names after the rename; the names
        // recorded before no longer reach it.
        assert_eq!(done, cleaned(1, 1));
        assert!(before.exists() && !after.exists());
    } else {
        assert_eq!(done, cleaned(0, 2));
        assert!(before.exists() && after.exists());
    }
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn a_renamed_folder_whose_place_cannot_be_read_is_recorded_by_identity_alone() {
    let (_scratch, place, journal_dir) = scratch("renamed-no-place");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    let sub = root.claim("Photos").unwrap();
    std::fs::rename(place.join("Photos"), place.join("Album")).unwrap();
    seams::PLACE.set(seams::Place::Unknown);
    drop(partial(&sub, &journal, 0, "a.jpg.part", b"half"));
    seams::PLACE.set(seams::Place::Read);
    assert_eq!(recorded_levels(&journal), [""]);
    journal.crash();
    drop((root, sub));
    std::fs::create_dir(place.join("Photos")).unwrap();
    std::fs::write(place.join("Photos/a.jpg.part"), b"theirs").unwrap();
    let done = clean_up(&journal_dir, &settings()).unwrap();
    assert_eq!(
        std::fs::read(place.join("Photos/a.jpg.part")).unwrap(),
        b"theirs"
    );
    // Windows opens the folder by its ID; Unix has no name to find it by.
    let removed = usize::from(cfg!(windows));
    assert_eq!(done, cleaned(removed, 1 - removed));
    assert_eq!(place.join("Album/a.jpg.part").exists(), !cfg!(windows));
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn a_folder_moved_out_of_the_root_is_refused() {
    let (_scratch, place, journal_dir) = scratch("moved-out");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    let sub = root.claim("Photos").unwrap();
    let elsewhere = place.parent().unwrap().join("Elsewhere");
    std::fs::rename(place.join("Photos"), &elsewhere).unwrap();
    let file = sub.create(OsStr::new("a.jpg.part")).unwrap();
    let started = journal.started(0, &sub, &file, OsStr::new("a.jpg.part"), "a.jpg", 0);
    if place_known() {
        assert_eq!(started, Err(JournalError::NotBelow));
        assert!(journal.partials().is_empty());
    } else {
        // Nothing says where it went: recorded by its identity, never found by cleanup.
        assert_eq!(started, Ok(()));
        assert_eq!(recorded_levels(&journal), [""]);
        journal.ended(0).unwrap();
    }
    sub.discard(&file, OsStr::new("a.jpg.part")).unwrap();
}

#[test]
fn a_folder_reached_through_a_link_that_leads_outside_the_root_is_refused() {
    let (_scratch, place, journal_dir) = scratch("through-link");
    let outside = place.parent().unwrap().join("Outside");
    std::fs::create_dir_all(outside.join("Inner")).unwrap();
    link_folder(&outside, &place.join("Link"));
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    // Below the root by its path, outside it in fact.
    let dir = Directory::open(&place.join("Link").join("Inner")).unwrap();
    let file = dir.create(OsStr::new("a.part")).unwrap();
    let started = || journal.started(0, &dir, &file, OsStr::new("a.part"), "a", 0);
    let link = Err(JournalError::Folder(Refusal::Link));
    assert_eq!(
        started(),
        if place_known() {
            Err(JournalError::NotBelow)
        } else {
            link
        }
    );
    // With no place to read, a link on the way is not taken for a rename.
    seams::PLACE.set(seams::Place::Unknown);
    let unread = started();
    seams::PLACE.set(seams::Place::Read);
    assert_eq!(unread, link);
    // Nor is a reading that fails: the walk's own refusal stands.
    seams::PLACE.set(seams::Place::Fails);
    let failed = started();
    seams::PLACE.set(seams::Place::Read);
    assert_eq!(failed, link);
    assert!(journal.partials().is_empty());
    dir.discard(&file, OsStr::new("a.part")).unwrap();
}

#[test]
fn a_link_put_under_a_renamed_folders_name_is_not_followed() {
    let (_scratch, place, journal_dir) = scratch("renamed-link");
    let outside = place.parent().unwrap().join("Outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("a.jpg.part"), b"theirs").unwrap();
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    let sub = root.claim("Photos").unwrap();
    std::fs::rename(place.join("Photos"), place.join("Album")).unwrap();
    link_folder(&outside, &place.join("Photos"));
    let file = sub.create(OsStr::new("a.jpg.part")).unwrap();
    let started = journal.started(0, &sub, &file, OsStr::new("a.jpg.part"), "a.jpg", 0);
    drop(file);
    if !place_known() {
        // The link could be how the folder was opened: refused.
        assert_eq!(started, Err(JournalError::Folder(Refusal::Link)));
        return;
    }
    assert_eq!(started, Ok(()));
    assert_eq!(recorded_levels(&journal), ["Album"]);
    journal.crash();
    drop((root, sub));
    assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(1, 0));
    assert!(!place.join("Album/a.jpg.part").exists());
    assert_eq!(
        std::fs::read(outside.join("a.jpg.part")).unwrap(),
        b"theirs"
    );
}

/// A link that leads to a folder really below the root: recorded under
/// its real names where they can be read, refused where they cannot.
#[test]
fn a_folder_reached_through_a_link_inside_the_root_is_recorded_where_it_is() {
    let (_scratch, place, journal_dir) = scratch("link-inside");
    std::fs::create_dir_all(place.join("Real/Inner")).unwrap();
    link_folder(&place.join("Real"), &place.join("Link"));
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    let dir = Directory::open(&place.join("Link").join("Inner")).unwrap();
    let file = dir.create(OsStr::new("a.part")).unwrap();
    let started = journal.started(0, &dir, &file, OsStr::new("a.part"), "a", 0);
    drop(file);
    if !place_known() {
        assert_eq!(started, Err(JournalError::Folder(Refusal::Link)));
        return;
    }
    assert_eq!(started, Ok(()));
    assert_eq!(recorded_levels(&journal), ["Real", "Inner"]);
    journal.crash();
    drop((root, dir));
    assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(1, 0));
    assert!(!place.join("Real/Inner/a.part").exists());
}

#[test]
fn a_file_put_under_a_renamed_folders_name_does_not_stop_it() {
    for no_place in [false, true] {
        let (_scratch, place, journal_dir) = scratch("renamed-file");
        let root = Directory::destination(&place).unwrap();
        let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
        let sub = root.claim("Photos").unwrap();
        std::fs::rename(place.join("Photos"), place.join("Album")).unwrap();
        std::fs::write(place.join("Photos"), b"a file now").unwrap();
        seams::PLACE.set(if no_place {
            seams::Place::Unknown
        } else {
            seams::Place::Read
        });
        drop(partial(&sub, &journal, 0, "a.jpg.part", b"half"));
        seams::PLACE.set(seams::Place::Read);
        let expected = if place_known() && !no_place {
            "Album"
        } else {
            ""
        };
        assert_eq!(recorded_levels(&journal), [expected], "{no_place}");
        journal.ended(0).unwrap();
        sub.remove(OsStr::new("a.jpg.part")).unwrap();
    }
}

/// Where the system reads a folder's place and the reading fails, nothing
/// shows the folder is below the root: renamed inside it, a file put under
/// its name, or moved out, it is refused.
#[test]
fn a_folder_whose_place_cannot_be_read_is_not_taken_on_trust() {
    let (_scratch, place, journal_dir) = scratch("place-fails");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    let sub = root.claim("Photos").unwrap();
    let file = sub.create(OsStr::new("a.jpg.part")).unwrap();
    drop(file);
    let started = || {
        let file = sub.reopen(OsStr::new("a.jpg.part")).unwrap();
        seams::PLACE.set(seams::Place::Fails);
        let started = journal.started(0, &sub, &file, OsStr::new("a.jpg.part"), "a.jpg", 0);
        seams::PLACE.set(seams::Place::Read);
        started
    };
    // Where it was opened, no place is read at all.
    assert_eq!(started(), Ok(()));
    journal.ended(0).unwrap();
    std::fs::rename(place.join("Photos"), place.join("Album")).unwrap();
    assert_eq!(started(), Err(JournalError::NotBelow));
    std::fs::write(place.join("Photos"), b"a file now").unwrap();
    assert_eq!(started(), Err(JournalError::NotBelow));
    let elsewhere = place.parent().unwrap().join("Elsewhere");
    std::fs::rename(place.join("Album"), &elsewhere).unwrap();
    assert_eq!(started(), Err(JournalError::NotBelow));
    assert!(journal.partials().is_empty());
    assert!(elsewhere.join("a.jpg.part").exists());
}

/// Names are compared whole: a folder moved beside the root, into one whose
/// name starts with the root's, is not below it.
#[test]
fn a_folder_moved_beside_the_root_under_a_longer_name_is_not_below_it() {
    let (_scratch, place, journal_dir) = scratch("beside-root");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    let sub = root.claim("Photos").unwrap();
    drop(partial(&sub, &journal, 0, "a.jpg.part", b"half"));
    let beside = place.with_file_name("Work2");
    std::fs::create_dir(&beside).unwrap();
    std::fs::rename(place.join("Photos"), beside.join("Photos")).unwrap();
    if place_known() {
        let file = sub.create(OsStr::new("b.jpg.part")).unwrap();
        let started = journal.started(1, &sub, &file, OsStr::new("b.jpg.part"), "b.jpg", 0);
        assert_eq!(started, Err(JournalError::NotBelow));
    }
    journal.crash();
    drop((root, sub));
    clean_up(&journal_dir, &settings()).unwrap();
    assert!(beside.join("Photos/a.jpg.part").exists());
    assert_eq!(journals(&journal_dir), 0);
}

/// Linux shows a deleted folder's place as its name and " (deleted)", so a
/// folder really named so reads as no place: a failed reading, not a rename.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn a_folder_named_as_deleted_and_moved_out_of_the_root_is_refused() {
    let (_scratch, place, journal_dir) = scratch("named-deleted");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    let sub = root.claim("Photos (deleted)").unwrap();
    let file = sub.create(OsStr::new("a.jpg.part")).unwrap();
    let started = || journal.started(0, &sub, &file, OsStr::new("a.jpg.part"), "a.jpg", 0);
    assert_eq!(started(), Ok(()));
    journal.ended(0).unwrap();
    let elsewhere = place.parent().unwrap().join("Gone (deleted)");
    std::fs::rename(place.join("Photos (deleted)"), &elsewhere).unwrap();
    assert_eq!(started(), Err(JournalError::NotBelow));
    assert!(journal.partials().is_empty());
}

/// A folder's ID opens it anywhere on its volume: cleanup removes nothing
/// in one that left the root after the crash, however it was recorded.
#[test]
fn cleanup_leaves_a_partial_whose_folder_was_moved_out_of_the_root() {
    let (_scratch, place, journal_dir) = scratch("moved-out-crash");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    let moved = root.claim("Photos").unwrap();
    let renamed = root.claim("Music").unwrap();
    let alone = root.claim("Notes").unwrap();
    drop(partial(&moved, &journal, 0, "a.jpg.part", b"half"));
    drop(partial(&renamed, &journal, 1, "b.mp3.part", b"half"));
    // Recorded by its identity alone, as where no place can be read.
    std::fs::rename(place.join("Notes"), place.join("Letters")).unwrap();
    seams::PLACE.set(seams::Place::Unknown);
    drop(partial(&alone, &journal, 2, "c.txt.part", b"half"));
    seams::PLACE.set(seams::Place::Read);
    journal.crash();
    drop((root, moved, renamed, alone));
    let outside = place.parent().unwrap().join("Outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::rename(place.join("Photos"), outside.join("Photos")).unwrap();
    std::fs::rename(place.join("Letters"), outside.join("Letters")).unwrap();
    std::fs::rename(place.join("Music"), place.join("Songs")).unwrap();
    let done = clean_up(&journal_dir, &settings()).unwrap();
    assert!(outside.join("Photos/a.jpg.part").exists());
    assert!(outside.join("Letters/c.txt.part").exists());
    // Renamed and still below the root: Windows finds it by its ID, Unix
    // has only the names recorded.
    let removed = usize::from(cfg!(windows));
    assert_eq!(done, cleaned(removed, 3 - removed));
    assert_eq!(place.join("Songs/b.mp3.part").exists(), !cfg!(windows));
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn cleanup_leaves_a_partial_whose_folders_place_cannot_be_read() {
    let (_scratch, place, journal_dir) = scratch("place-fails-crash");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    let stays = root.claim("Photos").unwrap();
    let renamed = root.claim("Music").unwrap();
    drop(partial(&stays, &journal, 0, "a.jpg.part", b"half"));
    drop(partial(&renamed, &journal, 1, "b.mp3.part", b"half"));
    journal.crash();
    drop((root, stays, renamed));
    std::fs::rename(place.join("Music"), place.join("Songs")).unwrap();
    seams::PLACE.set(seams::Place::Fails);
    let done = clean_up(&journal_dir, &settings());
    seams::PLACE.set(seams::Place::Read);
    // Its recorded names still reach the first: no place is read for it.
    let done = done.unwrap();
    assert_eq!((done.removed, done.left), (1, 1));
    assert!(!place.join("Photos/a.jpg.part").exists());
    assert!(place.join("Songs/b.mp3.part").exists());
    // Only Windows reads a place at cleanup: the reading may succeed later.
    if cfg!(windows) {
        assert_eq!(done.kept, 1);
        assert_eq!(journals(&journal_dir), 1);
        let later = clean_up(&journal_dir, &settings()).unwrap();
        assert_eq!((later.removed, later.kept), (1, 0));
        assert!(!place.join("Songs/b.mp3.part").exists());
    }
    assert_eq!(journals(&journal_dir), 0);
}

/// A segment as a damaged or crafted journal could hold it: the folders
/// `levels` name, each below the one before, the first below the root, all
/// with the identity of `folder`, and one partial in the last.
fn crafted_segment(
    root: &Directory,
    folder: &Directory,
    levels: [&str; 2],
    file: &File,
) -> Vec<u8> {
    use crate::log::{Entry, State, frame};
    let id = folder.identity().unwrap();
    let state = State::new(location::root_of(root).unwrap(), &settings());
    let mut bytes = frame(&state.header());
    for (n, name) in (1u32..).zip(levels) {
        let entry = Entry::Folder(Folder {
            n,
            parent: n - 1,
            name: name.to_string(),
            device: id.volume,
            id: id.file,
        });
        bytes.extend(frame(&entry.encode()));
    }
    let started = Entry::Started(Started {
        place: 0,
        folder: 2,
        name: "a.jpg".to_string(),
        partial: "a.jpg.part".to_string(),
        size: 4,
        file: identity(file).unwrap().file,
    });
    bytes.extend(frame(&started.encode()));
    bytes
}

#[test]
fn an_empty_folder_name_that_is_not_the_only_level_is_damaged() {
    let (_scratch, place, _) = scratch("empty-name");
    let root = Directory::destination(&place).unwrap();
    let sub = root.claim("Photos").unwrap();
    let file = sub.create(OsStr::new("a.jpg.part")).unwrap();
    for levels in [["Photos", ""], ["", "Photos"]] {
        let bytes = crafted_segment(&root, &sub, levels, &file);
        let segment = read_segment(&bytes, &settings()).unwrap();
        // Read up to the level out of place: its partial is never reached.
        assert_eq!(segment.entries, 1, "{levels:?}");
        assert_eq!(segment.folders().count(), 1, "{levels:?}");
        assert!(segment.partials().is_empty(), "{levels:?}");
        // In a header, the whole segment is damaged.
        let mut state = segment.state.clone();
        let second = Folder {
            n: 2,
            parent: 1,
            name: levels[1].to_string(),
            device: 0,
            id: 0,
        };
        state.folders.insert(2, second);
        state.live.push(Started {
            place: 0,
            folder: 2,
            name: "a.jpg".to_string(),
            partial: "a.jpg.part".to_string(),
            size: 4,
            file: 1,
        });
        let header = crate::log::frame(&state.header());
        assert_eq!(
            read_segment(&header, &settings()),
            Err(JournalError::Damaged),
            "{levels:?}"
        );
    }
    // Alone and directly below the root, it reads.
    let mut alone = crate::log::State::new(location::root_of(&root).unwrap(), &settings());
    let level = Folder {
        n: 1,
        parent: 0,
        name: String::new(),
        device: 0,
        id: 0,
    };
    alone.folders.insert(1, level);
    alone.live.push(Started {
        place: 0,
        folder: 1,
        name: "a.jpg".to_string(),
        partial: "a.jpg.part".to_string(),
        size: 4,
        file: 1,
    });
    let read = read_segment(&crate::log::frame(&alone.header()), &settings()).unwrap();
    assert_eq!(read.partials().len(), 1);
}

#[test]
fn cleanup_removes_nothing_for_an_empty_folder_name_out_of_place() {
    for levels in [["Photos", ""], ["", "Photos"]] {
        let (_scratch, place, journal_dir) = scratch("empty-name-cleanup");
        let root = Directory::destination(&place).unwrap();
        let sub = root.claim("Photos").unwrap();
        let file = sub.create(OsStr::new("a.jpg.part")).unwrap();
        let bytes = crafted_segment(&root, &sub, levels, &file);
        drop((root, sub, file));
        std::fs::write(journal_dir.join("1-1.1.jlog"), bytes).unwrap();
        assert_eq!(
            clean_up(&journal_dir, &settings()).unwrap(),
            Cleaned::default(),
            "{levels:?}"
        );
        assert!(place.join("Photos/a.jpg.part").exists(), "{levels:?}");
        assert_eq!(journals(&journal_dir), 0);
    }
}

#[test]
fn a_system_error_keeps_its_raw_code() {
    let (_scratch, place, journal_dir) = scratch("raw-code");
    let root = Directory::destination(&place).unwrap();
    let missing = journal_dir.join("missing");
    let started = Journal::start(&missing, &root, &settings());
    let Err(JournalError::Io { kind, raw }) = started else {
        panic!("{started:?}");
    };
    assert_eq!(kind, io::ErrorKind::NotFound);
    // ERROR_PATH_NOT_FOUND, ENOENT.
    assert_eq!(raw, Some(if cfg!(windows) { 3 } else { 2 }));
    // An error that is no system's has no code.
    assert_eq!(
        JournalError::from(io::Error::other("none")),
        JournalError::Io {
            kind: io::ErrorKind::Other,
            raw: None
        }
    );
}

#[cfg(unix)]
#[test]
fn a_root_that_cannot_be_found_keeps_its_journal_until_it_can() {
    let (_scratch, place, journal_dir) = scratch("unmounted");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    drop(partial(&root, &journal, 0, "a.part", b"half"));
    journal.crash();
    drop(root);
    // The disk is not mounted at this start: the path is not there.
    let away = place.with_file_name("Away");
    std::fs::rename(&place, &away).unwrap();
    let done = clean_up(&journal_dir, &settings()).unwrap();
    assert_eq!(
        done,
        Cleaned {
            removed: 0,
            left: 1,
            kept: 1
        }
    );
    std::fs::rename(&away, &place).unwrap();
    assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(1, 0));
    assert_eq!(journals(&journal_dir), 0);
}

#[cfg(not(windows))]
#[test]
fn elsewhere_a_journal_of_the_whole_record_format_is_removed_and_touches_nothing() {
    let (_scratch, place, journal_dir) = scratch("elsewhere");
    std::fs::write(place.join("a.part"), b"theirs").unwrap();
    std::fs::write(
        journal_dir.join("1-1.journal"),
        sample().encode(&settings()).unwrap(),
    )
    .unwrap();
    assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(0, 2));
    assert_eq!(journals(&journal_dir), 0);
    assert!(place.join("a.part").exists());
    assert_eq!(remove_by_identity(&sample(), &settings()), cleaned(0, 2));
}

/// A journal of the whole-record format of before, recording `names` in `dir`.
#[cfg(windows)]
pub(crate) fn windows_old_journal(journal_dir: &Path, dir: &Directory, names: &[&str]) -> PathBuf {
    windows::old_journal(journal_dir, 1, dir, names)
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::os::windows::fs::MetadataExt;

    /// A journal of the whole-record format of before, as it was written,
    /// recording `names` in `dir`.
    pub(crate) fn old_journal(
        journal_dir: &Path,
        n: u32,
        dir: &Directory,
        names: &[&str],
    ) -> PathBuf {
        let record = Record {
            volume: dir.volume_path().unwrap(),
            serial: dir.identity().unwrap().volume,
            partials: names
                .iter()
                .map(|name| Partial {
                    folder: dir.identity().unwrap().file,
                    name: (*name).to_string(),
                    file: safecopy_folder::identity(&dir.entry(OsStr::new(name)).unwrap())
                        .unwrap()
                        .file,
                })
                .collect(),
        };
        let path = journal_dir.join(format!("1-{n}.{EXTENSION}"));
        std::fs::write(&path, record.encode(&settings()).unwrap()).unwrap();
        path
    }

    #[test]
    fn cleanup_leaves_a_file_on_another_volume_than_recorded() {
        let (_scratch, place, journal_dir) = scratch("other-serial");
        let root = Directory::destination(&place).unwrap();
        let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
        drop(partial(&root, &journal, 0, "c.txt.part", b"mine"));
        journal.crash();
        drop(root);
        // A volume whose serial is not the recorded one is another disk.
        let path = std::fs::read_dir(&journal_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let bytes = std::fs::read(&path).unwrap();
        let mut segment = read_segment(&bytes, &settings()).unwrap();
        if let Root::Windows { serial, .. } = &mut segment.state.root {
            *serial ^= 1;
        }
        std::fs::write(&path, log::frame(&segment.state.header())).unwrap();
        assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(0, 1));
        assert!(place.join("c.txt.part").exists());
        assert_eq!(journals(&journal_dir), 0);
    }

    #[test]
    fn a_recorded_name_naming_a_stream_is_never_removed() {
        let (_scratch, place, _) = scratch("stream");
        let root = Directory::destination(&place).unwrap();
        std::fs::write(place.join("report.docx"), b"theirs").unwrap();
        std::fs::write(place.join("report.docx:x.part"), b"stream").unwrap();
        let file = File::open(place.join("report.docx")).unwrap();
        let record = Record {
            volume: root.volume_path().unwrap(),
            serial: root.identity().unwrap().volume,
            partials: vec![Partial {
                folder: root.identity().unwrap().file,
                name: "report.docx:x.part".into(),
                file: safecopy_folder::identity(&file).unwrap().file,
            }],
        };
        drop(file);
        assert_eq!(
            win::remove(&record, &record.partials[0], &settings()),
            Err(location::Left::Name)
        );
        assert_eq!(remove_by_identity(&record, &settings()), cleaned(0, 1));
        assert_eq!(
            std::fs::read(place.join("report.docx:x.part")).unwrap(),
            b"stream"
        );
        assert_eq!(std::fs::read(place.join("report.docx")).unwrap(), b"theirs");
    }

    #[test]
    fn a_junction_named_as_a_journal_is_never_followed() {
        let (_scratch, place, journal_dir) = scratch("junction-journal");
        let target = place.join("Thesis");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("thesis.docx"), b"theirs").unwrap();
        for link in ["1-1.journal", "1-1.1.jlog"] {
            let link = journal_dir.join(link);
            // A junction needs no privilege: made everywhere.
            let made = Command::new("cmd")
                .args(["/D", "/C", "mklink", "/J"])
                .arg(&link)
                .arg(&target)
                .output()
                .unwrap();
            assert!(made.status.success(), "{made:?}");
        }
        assert_eq!(
            clean_up(&journal_dir, &settings()).unwrap(),
            Cleaned::default()
        );
        assert_eq!(
            std::fs::read(target.join("thesis.docx")).unwrap(),
            b"theirs"
        );
        for link in ["1-1.journal", "1-1.1.jlog"] {
            assert!(
                std::fs::symlink_metadata(journal_dir.join(link))
                    .unwrap()
                    .file_attributes()
                    & ::windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT.0
                    != 0,
                "the junction is still there"
            );
        }
    }

    #[test]
    fn a_journal_for_a_folder_on_a_network_share_is_refused() {
        use std::path::{Component, Prefix};
        let (_scratch, place, journal_dir) = scratch("share");
        // The same folder through the computer's own administrative share.
        let mut parts = place.components();
        let drive = match parts.next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                Prefix::Disk(d) | Prefix::VerbatimDisk(d) => char::from(d),
                other => panic!("the scratch folder is on no drive: {other:?}"),
            },
            other => panic!("the scratch folder is on no drive: {other:?}"),
        };
        let mut share = PathBuf::from(format!(r"\\localhost\{drive}$\"));
        share.extend(parts.filter(|p| matches!(p, Component::Normal(_))));
        if std::fs::metadata(&share).is_err() {
            // Not reachable on this computer (no sharing, no rights): skipped
            // even where links are required, as it is no link.
            eprintln!("skipped: the share {} cannot be opened", share.display());
            return;
        }
        let root = Directory::destination(&share).unwrap();
        assert_eq!(
            Journal::start(&journal_dir, &root, &settings()).unwrap_err(),
            JournalError::Folder(Refusal::NoVolume)
        );
        assert_eq!(journals(&journal_dir), 0, "no journal file is left");
    }

    /// The one journal in `dir`.
    fn journal_path(dir: &Path) -> PathBuf {
        let mut paths = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path());
        let path = paths.next().unwrap();
        assert!(paths.next().is_none());
        path
    }

    /// Sets when a journal last changed.
    fn changed(path: &Path, ago: std::time::Duration) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(std::time::SystemTime::now() - ago)
            .unwrap();
    }

    const DAY: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

    #[test]
    fn a_journal_whose_volume_cannot_be_opened_is_kept_until_it_can() {
        let (_scratch, place, journal_dir) = scratch("unplugged");
        let root = Directory::destination(&place).unwrap();
        let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
        drop(partial(&root, &journal, 0, "a.part", b"half"));
        drop(partial(&root, &journal, 1, "b.part", b"half"));
        journal.crash();
        std::fs::write(place.join("c.part"), b"half").unwrap();
        old_journal(&journal_dir, 1, &root, &["c.part"]);
        drop(root);
        // The disk is not plugged in at this start.
        seams::NO_VOLUME.set(true);
        let done = clean_up(&journal_dir, &settings());
        seams::NO_VOLUME.set(false);
        assert_eq!(
            done.unwrap(),
            Cleaned {
                removed: 0,
                left: 3,
                kept: 2
            }
        );
        assert!(place.join("a.part").exists());
        assert!(place.join("b.part").exists());
        assert_eq!(journals(&journal_dir), 2, "both journals are kept");
        // Back at a later start, with one partial gone meanwhile.
        std::fs::remove_file(place.join("a.part")).unwrap();
        assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(2, 1));
        assert!(!place.join("b.part").exists());
        assert!(!place.join("c.part").exists());
        assert_eq!(journals(&journal_dir), 0, "then they are removed");
    }

    #[test]
    fn a_journal_longer_than_the_limit_is_removed_and_touches_nothing() {
        let (_scratch, place, journal_dir) = scratch("too-long");
        let root = Directory::destination(&place).unwrap();
        std::fs::write(place.join("a.part"), b"half").unwrap();
        old_journal(&journal_dir, 1, &root, &["a.part"]);
        drop(root);
        let tight = Settings {
            record_bytes: 8,
            ..settings()
        };
        assert_eq!(clean_up(&journal_dir, &tight).unwrap(), Cleaned::default());
        assert!(place.join("a.part").exists());
        assert_eq!(journals(&journal_dir), 0);
    }

    #[test]
    fn a_recorded_volume_guid_that_is_not_there_cannot_be_opened() {
        let (_scratch, place, _) = scratch("no-such-volume");
        let root = Directory::destination(&place).unwrap();
        let file = root.create(OsStr::new("a.part")).unwrap();
        let record = Record {
            volume: r"\\?\Volume{00000000-0000-0000-0000-000000000000}\".to_string(),
            serial: root.identity().unwrap().volume,
            partials: vec![Partial {
                folder: root.identity().unwrap().file,
                name: "a.part".into(),
                file: safecopy_folder::identity(&file).unwrap().file,
            }],
        };
        drop(file);
        assert_eq!(
            win::remove(&record, &record.partials[0], &settings()),
            Err(location::Left::Unreachable)
        );
        assert!(place.join("a.part").exists());
    }

    #[test]
    fn a_partial_in_use_keeps_its_journal_for_a_later_cleanup() {
        use std::os::windows::fs::OpenOptionsExt;
        let (_scratch, place, journal_dir) = scratch("in-use");
        let root = Directory::destination(&place).unwrap();
        let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
        drop(partial(&root, &journal, 0, "a.part", b"half"));
        journal.crash();
        drop(root);
        // Another program holds it, sharing nothing.
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(place.join("a.part"))
            .unwrap();
        assert_eq!(
            clean_up(&journal_dir, &settings()).unwrap(),
            Cleaned {
                removed: 0,
                left: 1,
                kept: 1
            }
        );
        drop(held);
        assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(1, 0));
        assert_eq!(journals(&journal_dir), 0);
    }

    #[test]
    fn a_kept_journal_past_its_age_is_removed_and_leaves_its_partials() {
        let (_scratch, place, journal_dir) = scratch("kept-age");
        let root = Directory::destination(&place).unwrap();
        let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
        drop(partial(&root, &journal, 0, "a.part", b"half"));
        journal.crash();
        drop(root);
        let week = Settings {
            keep_for: 7 * DAY,
            ..settings()
        };
        let path = journal_path(&journal_dir);
        changed(&path, 6 * DAY);
        seams::NO_VOLUME.set(true);
        let young = clean_up(&journal_dir, &week);
        changed(&path, 8 * DAY);
        let old = clean_up(&journal_dir, &week);
        seams::NO_VOLUME.set(false);
        assert_eq!(young.unwrap().kept, 1);
        assert_eq!(old.unwrap(), cleaned(0, 1));
        assert_eq!(journals(&journal_dir), 0);
        assert!(place.join("a.part").exists());
    }

    #[test]
    fn past_the_most_kept_journals_the_oldest_are_removed() {
        let (_scratch, place, journal_dir) = scratch("kept-most");
        let root = Directory::destination(&place).unwrap();
        for (n, ago) in [(1, 3), (2, 1), (3, 2)] {
            let before: Vec<PathBuf> = std::fs::read_dir(&journal_dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .collect();
            let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
            drop(partial(&root, &journal, 0, &format!("{n}.part"), b"half"));
            journal.crash();
            let path = std::fs::read_dir(&journal_dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| !before.contains(p))
                .unwrap();
            changed(&path, ago * DAY);
        }
        drop(root);
        let two = Settings {
            keep_most: 2,
            ..settings()
        };
        seams::NO_VOLUME.set(true);
        let done = clean_up(&journal_dir, &two);
        let mut names: Vec<String> = std::fs::read_dir(&journal_dir)
            .unwrap()
            .map(|e| {
                let bytes = std::fs::read(e.unwrap().path()).unwrap();
                read_segment(&bytes, &settings()).unwrap().partials()[0]
                    .partial
                    .clone()
            })
            .collect();
        seams::NO_VOLUME.set(false);
        names.sort();
        assert_eq!(
            done.unwrap(),
            Cleaned {
                removed: 0,
                left: 3,
                kept: 2
            }
        );
        assert_eq!(names, ["2.part", "3.part"], "the oldest is removed");
        for n in 1..=3 {
            assert!(place.join(format!("{n}.part")).exists());
        }
    }

    #[test]
    fn a_recorded_volume_that_is_not_a_volume_guid_path_is_never_opened() {
        let (_scratch, place, _) = scratch("volume-path");
        let root = Directory::destination(&place).unwrap();
        let file = root.create(OsStr::new("a.part")).unwrap();
        let mut record = Record {
            volume: root.volume_path().unwrap(),
            serial: root.identity().unwrap().volume,
            partials: vec![Partial {
                folder: root.identity().unwrap().file,
                name: "a.part".into(),
                file: safecopy_folder::identity(&file).unwrap().file,
            }],
        };
        drop(file);
        assert!(win::volume_guid_path(&record.volume));
        let good = record.volume.clone();
        for volume in [
            r"\\localhost\share\".to_string(),
            r"\\.\pipe\journal".to_string(),
            r"\\?\C:\".to_string(),
            format!("{good}x"),
            good.replace('-', "x"),
        ] {
            assert!(!win::volume_guid_path(&volume), "{volume}");
            record.volume = volume;
            assert_eq!(
                win::remove(&record, &record.partials[0], &settings()),
                Err(location::Left::Volume)
            );
        }
        assert!(place.join("a.part").exists());
        record.volume = good;
        assert_eq!(remove_by_identity(&record, &settings()), cleaned(1, 0));
    }

    #[test]
    fn a_partial_on_another_volume_than_the_journal_s_is_refused() {
        let (_scratch, place, journal_dir) = scratch("other-volume");
        let root = Directory::destination(&place).unwrap();
        let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
        let file = root.create(OsStr::new("a.part")).unwrap();
        // The journal's volume acted out as another one.
        seams::OTHER_VOLUME.set(true);
        let started = journal.started(0, &root, &file, OsStr::new("a.part"), "a", 0);
        seams::OTHER_VOLUME.set(false);
        assert_eq!(started, Err(JournalError::OtherVolume));
        assert!(journal.partials().is_empty());
        root.discard(&file, OsStr::new("a.part")).unwrap();
    }
}
