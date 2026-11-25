use super::*;
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Held for writing while a child process starts, and for reading by
/// every test that has folders: on Unix a child holds every open file of
/// this process until it becomes its own program, locks included, so a
/// file a test let go of would read as held.
static CHILD_START: RwLock<()> = RwLock::new(());

thread_local! {
    /// This thread's hold on `CHILD_START` for reading and how many
    /// scratch folders share it: a second hold would wait for ever behind
    /// a child's start that waits for the first.
    static NO_CHILD: RefCell<(usize, Option<RwLockReadGuard<'static, ()>>)> =
        const { RefCell::new((0, None)) };
}

fn no_child() -> RwLockReadGuard<'static, ()> {
    CHILD_START.read().unwrap_or_else(PoisonError::into_inner)
}

/// A new, empty folder for one test, under the build's target folder and
/// removed with it. While a test has one, no child process starts: every
/// lock the test lets go of is then let go of in fact.
pub(crate) struct Scratch(PathBuf);

impl Scratch {
    /// `kind` tells one test module's folders from another's.
    pub(crate) fn new(kind: &str, name: &str) -> Self {
        NO_CHILD.with_borrow_mut(|(count, held)| {
            if *count == 0 {
                *held = Some(no_child());
            }
            *count += 1;
        });
        // The test binary is in target/<profile>/deps.
        let exe = std::env::current_exe().unwrap();
        let target = exe.ancestors().nth(3).unwrap();
        let dir = target
            .join("test-tmp")
            .join(format!("{kind}-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // Made before the folder is: a panic here still gives the hold back.
        let scratch = Self(dir);
        std::fs::create_dir_all(&scratch.0).unwrap();
        scratch
    }

    /// With a folder to write into and one for journals.
    pub(crate) fn folders(kind: &str, name: &str) -> (Self, PathBuf, PathBuf) {
        let scratch = Self::new(kind, name);
        let (place, journals) = (scratch.0.join("Work"), scratch.0.join("journals"));
        std::fs::create_dir_all(&place).unwrap();
        std::fs::create_dir_all(&journals).unwrap();
        (scratch, place, journals)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        NO_CHILD.with_borrow_mut(|(count, held)| {
            *count -= 1;
            if *count == 0 {
                *held = None;
            }
        });
    }
}

/// The only test running, for as long as this is held: this thread's own
/// hold is given up for it, and taken again after.
struct Alone(Option<RwLockWriteGuard<'static, ()>>);

impl Alone {
    fn take() -> Self {
        NO_CHILD.with_borrow_mut(|(_, held)| *held = None);
        Self(Some(
            CHILD_START.write().unwrap_or_else(PoisonError::into_inner),
        ))
    }
}

impl Drop for Alone {
    fn drop(&mut self) {
        self.0 = None;
        NO_CHILD.with_borrow_mut(|(count, held)| {
            if *count > 0 {
                *held = Some(no_child());
            }
        });
    }
}

/// A scratch folder with a folder to write into and one for journals.
fn scratch(name: &str) -> (Scratch, PathBuf, PathBuf) {
    Scratch::folders("journal", name)
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
    let scratch = Scratch::new("journal", "missing");
    assert_eq!(
        clean_up(&scratch.path().join("none"), &settings()).unwrap(),
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
    assert_eq!(
        at.present(&settings()),
        Some(4096),
        "while a save has it open"
    );
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
        assert_eq!(
            at.present(&settings()),
            Some(4096),
            "attributes need no sharing"
        );
        assert_eq!(at.try_reopen(&settings()).err(), Some(Left::Busy));
        assert!(at.reopen(&settings()).is_none());
        drop(alone);
    }
    #[cfg(not(windows))]
    drop(file);
    let (_, again) = at.try_reopen(&settings()).unwrap();
    again.set_len(10).unwrap();
    drop(again);
    assert_eq!(at.present(&settings()), Some(10));
    // Another file under the name is not it, whatever its length.
    std::fs::rename(place.join("a.bin.part"), place.join("moved")).unwrap();
    std::fs::write(place.join("a.bin.part"), [0; 4096]).unwrap();
    assert_eq!(at.present(&settings()), None, "replaced");
    assert_eq!(at.try_reopen(&settings()).err(), Some(Left::Replaced));
    // Nor is a folder, a link or nothing at all.
    std::fs::remove_file(place.join("a.bin.part")).unwrap();
    assert_eq!(at.present(&settings()), None, "gone");
    assert_eq!(at.try_reopen(&settings()).err(), Some(Left::Gone));
    std::fs::create_dir(place.join("a.bin.part")).unwrap();
    assert_eq!(at.present(&settings()), None, "a folder");
    std::fs::remove_dir(place.join("a.bin.part")).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(place.join("moved"), place.join("a.bin.part")).unwrap();
        assert_eq!(at.present(&settings()), None, "a link to the very file");
    }
    #[cfg(windows)]
    {
        // A junction to the folder the very file is in.
        junction(&place, &place.join("a.bin.part"));
        assert_eq!(at.present(&settings()), None, "a junction");
        assert_eq!(
            std::fs::read(place.join("a.bin.part").join("moved"))
                .unwrap()
                .len(),
            10
        );
        std::fs::remove_dir(place.join("a.bin.part")).unwrap();
        // A file symbolic link needs Developer Mode or an administrator.
        let (moved, link) = (place.join("moved"), place.join("a.bin.part"));
        if std::os::windows::fs::symlink_file(moved, link).is_ok() {
            assert_eq!(at.present(&settings()), None, "a link to the very file");
        } else {
            assert!(
                !symlinks_required(),
                "SAFECOPY_REQUIRE_SYMLINKS is set, but no file symbolic link can be made"
            );
            eprintln!("skipped: no symbolic link can be made here");
        }
    }
}

/// Makes `link` a junction to the folder `target`, with no other program
/// run: a junction needs no privilege.
#[cfg(windows)]
fn junction(target: &std::path::Path, link: &std::path::Path) {
    use ::windows::Win32::Foundation::HANDLE;
    use ::windows::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    use ::windows::Win32::System::IO::DeviceIoControl;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    std::fs::create_dir(link).unwrap();
    let folder = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(link)
        .unwrap();
    // The target as the system names it, then an empty name to show.
    let name: Vec<u16> = std::ffi::OsStr::new(r"\??\")
        .encode_wide()
        .chain(target.as_os_str().encode_wide())
        .collect();
    let len = (name.len() * 2) as u16;
    let mut buffer = Vec::new();
    buffer.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    for field in [len + 12, 0, 0, len, len + 2, 0] {
        buffer.extend(field.to_le_bytes());
    }
    buffer.extend(name.iter().flat_map(|unit| unit.to_le_bytes()));
    buffer.extend([0; 4]);
    // SAFETY: the handle is open, and the buffer is whole for the length given.
    unsafe {
        DeviceIoControl(
            HANDLE(folder.as_raw_handle()),
            FSCTL_SET_REPARSE_POINT,
            Some(buffer.as_ptr().cast()),
            buffer.len() as u32,
            None,
            0,
            None,
            None,
        )
    }
    .unwrap();
}

/// A record file with a second name is some other file as well: what a
/// hold would write through its handle would land there.
#[test]
fn a_file_with_a_second_name_is_not_held_and_is_still_taken() {
    let (_scratch, place, _) = scratch("hold-linked");
    let (theirs, path) = (place.join("thesis.docx"), place.join("a.resume"));
    std::fs::write(&theirs, b"theirs").unwrap();
    std::fs::hard_link(&theirs, &path).unwrap();
    let held = files::hold(&path).unwrap_err();
    assert_eq!(held.kind(), io::ErrorKind::InvalidInput);
    assert!(files::refused(&held) && !files::busy(&held));
    // A cleanup still takes it, to remove the name alone.
    let (file, bytes) = files::take(&path, 100).unwrap();
    assert_eq!(bytes, b"theirs");
    files::delete(&file, &path).unwrap();
    drop(file);
    assert!(!path.exists());
    assert_eq!(std::fs::read(&theirs).unwrap(), b"theirs");
    // With one name, it is held; a system's own error is no refusal.
    std::fs::write(&path, b"one").unwrap();
    assert!(files::hold(&path).is_ok());
    let gone = files::hold(&place.join("none.resume")).unwrap_err();
    assert!(!files::refused(&gone));
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

/// The names of a file are counted through its handle, after the open: a
/// second name it is given between the two is seen, and it is not held.
#[test]
fn a_file_given_a_second_name_while_it_is_being_held_is_not_held() {
    let (_scratch, place, _) = scratch("hold-linked-late");
    let (path, other) = (place.join("a.resume"), place.join("notes.txt"));
    std::fs::write(&path, b"one").unwrap();
    let (from, to) = (path.clone(), other.clone());
    seams::HOLD_OPENED.set(Some(Box::new(move || {
        std::fs::hard_link(&from, &to).unwrap();
    })));
    let held = files::hold(&path).unwrap_err();
    assert_eq!(held.kind(), io::ErrorKind::InvalidInput);
    assert!(files::refused(&held) && !files::busy(&held));
    assert_eq!(std::fs::read(&other).unwrap(), b"one");
    // With the one name again, it is held.
    std::fs::remove_file(&other).unwrap();
    assert!(files::hold(&path).is_ok());
}

use std::fs::File;
use std::process::Command;

/// What a child does with its copies of this process's files before it
/// becomes its own program. A start is back before the system has closed
/// them: a file let go of here just after would still read as held.
#[cfg(unix)]
mod let_go {
    use std::io;
    use std::mem::MaybeUninit;
    use std::os::fd::{AsRawFd, RawFd};

    /// The step for `pre_exec`: closes this process's files and folders
    /// in the child, then writes one byte to `tell`. System calls and
    /// memory of its own stack only: nothing else is allowed there.
    pub(super) fn step(
        tell: io::PipeWriter,
    ) -> io::Result<impl FnMut() -> io::Result<()> + Send + Sync + 'static> {
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let end = end()?;
        Ok(move || {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            close_listed()?;
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            for fd in 3..end {
                close_if_file(fd)?;
            }
            told(tell.as_raw_fd())
        })
    }

    fn last<T>() -> io::Result<T> {
        Err(io::Error::last_os_error())
    }

    /// Closes `fd` if it is a file or a folder. A pipe or a socket stays:
    /// a start that fails is heard through one.
    fn close_if_file(fd: RawFd) -> io::Result<bool> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `fstat` writes one `stat` into the memory it is given.
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                // Not open: nothing to close.
                Some(libc::EBADF) => return Ok(false),
                // Only a file or a folder has a number too wide to tell.
                Some(libc::EOVERFLOW) => {}
                _ => return Err(error),
            }
        } else {
            // SAFETY: `fstat` returned 0, so the `stat` is filled.
            let kind = unsafe { stat.assume_init() }.st_mode & libc::S_IFMT;
            if kind != libc::S_IFREG && kind != libc::S_IFDIR {
                return Ok(false);
            }
        }
        // SAFETY: the descriptor is this child's own copy, used by nothing after.
        if unsafe { libc::close(fd) } != 0 {
            let error = io::Error::last_os_error();
            // Interrupted, the descriptor is closed all the same.
            if error.raw_os_error() != Some(libc::EINTR) {
                return Err(error);
            }
        }
        Ok(true)
    }

    /// One byte to the pipe: the next system call after the closes, so
    /// they are done in full when it is read.
    fn told(tell: RawFd) -> io::Result<()> {
        loop {
            // SAFETY: one byte is read from memory that holds one.
            match unsafe { libc::write(tell, [1u8].as_ptr().cast(), 1) } {
                1 => return Ok(()),
                0 => return Err(io::ErrorKind::WriteZero.into()),
                _ => {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::EINTR) {
                        return Err(error);
                    }
                }
            }
        }
    }

    /// Closes every file and folder the system lists as open here, above
    /// the three standard ones.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn close_listed() -> io::Result<()> {
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
        // SAFETY: the path is text that ends in a zero byte.
        let list = unsafe { libc::open(c"/proc/self/fd".as_ptr(), flags) };
        if list < 0 {
            return last();
        }
        let done = close_in(list);
        // The list is a folder too, and is closed last.
        // SAFETY: the descriptor was opened above and is used by nothing after.
        if unsafe { libc::close(list) } != 0 && done.is_ok() {
            return last();
        }
        done
    }

    /// Reads the list until one reading of it closes nothing: whatever
    /// order the system lists in, none is passed over.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn close_in(list: RawFd) -> io::Result<()> {
        // An entry: eight bytes of number, eight of position, two of its
        // length, one of kind, then the name and a zero byte.
        const NAME: usize = 19;
        let mut entries = [0u8; 2048];
        loop {
            let mut closed = false;
            loop {
                // SAFETY: at most the array's length is written into it.
                let read = unsafe {
                    libc::syscall(
                        libc::SYS_getdents64,
                        list,
                        entries.as_mut_ptr(),
                        entries.len(),
                    )
                };
                let Ok(read) = usize::try_from(read) else {
                    return last();
                };
                let Some(mut rest) = entries.get(..read).filter(|read| !read.is_empty()) else {
                    break;
                };
                while !rest.is_empty() {
                    let length = match rest.get(16..18) {
                        Some(&[low, high]) => usize::from(u16::from_ne_bytes([low, high])),
                        _ => return Err(io::ErrorKind::InvalidData.into()),
                    };
                    let (Some(entry), Some(after)) = (rest.get(NAME..length), rest.get(length..))
                    else {
                        return Err(io::ErrorKind::InvalidData.into());
                    };
                    rest = after;
                    if let Some(fd) = number(entry)
                        && fd > 2
                        && fd != list
                        && close_if_file(fd)?
                    {
                        closed = true;
                    }
                }
            }
            if !closed {
                return Ok(());
            }
            // SAFETY: the descriptor is the open list.
            if unsafe { libc::lseek(list, 0, libc::SEEK_SET) } != 0 {
                return last();
            }
        }
    }

    /// The descriptor a name of digits stands for; `.` and `..` are none.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn number(name: &[u8]) -> Option<RawFd> {
        let digits = name.split(|byte| *byte == 0).next()?;
        if digits.is_empty() {
            return None;
        }
        digits.iter().try_fold(0, |fd: RawFd, digit| {
            let digit = digit.checked_sub(b'0').filter(|digit| *digit < 10)?;
            fd.checked_mul(10)?.checked_add(digit.into())
        })
    }

    /// Where no list of open descriptors is read without memory of the
    /// heap, every number below the limit on open files is tried, read
    /// before the fork, and none from here on.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const END: RawFd = 1 << 20;

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn end() -> io::Result<RawFd> {
        let mut limit = MaybeUninit::<libc::rlimit>::uninit();
        // SAFETY: `getrlimit` writes one `rlimit` into the memory it is given.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
            return last();
        }
        // SAFETY: `getrlimit` returned 0, so the `rlimit` is filled.
        let open = unsafe { limit.assume_init() }.rlim_cur;
        Ok(RawFd::try_from(open).map_or(END, |open| open.min(END)))
    }
}

/// Starts `command` as a child, the only test running meanwhile. Back
/// from here the child holds no file or folder of this process: on Unix
/// it closed its copies and said so before becoming its own program.
#[cfg(unix)]
fn start(mut command: Command, _alone: &Alone) -> io::Result<std::process::Child> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    let (mut told, tell) = io::pipe()?;
    let step = let_go::step(tell)?;
    // SAFETY: the step makes system calls only and takes no memory, which
    // is what a child may do between the fork and the exec.
    unsafe { command.pre_exec(step) };
    let mut child = command.spawn()?;
    // This process's end of the pipe goes with the command, so the read
    // ends: with the child's byte, or with the pipe.
    drop(command);
    if let Err(untold) = told.read_exact(&mut [0]) {
        // A child that ended before its byte is not left behind.
        let _ = child.kill();
        let _ = child.wait();
        return Err(untold);
    }
    Ok(child)
}

/// Windows gives a child no handle of this process's files.
#[cfg(not(unix))]
fn start(mut command: Command, _alone: &Alone) -> io::Result<std::process::Child> {
    command.spawn()
}

/// The test binary to run again as another process, in `role`.
fn as_role(role: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["tests::as_child", "--exact", "--ignored", "--nocapture"])
        .args(["--test-threads", "1"])
        .env("SAFECOPY_JOURNAL_CHILD", role);
    command
}

/// The test binary run again as another process, in `role`.
fn child(role: &str, place: &Path, journal_dir: &Path) -> String {
    use std::process::Stdio;
    let mut command = as_role(role);
    command
        .env("SAFECOPY_JOURNAL_PLACE", place)
        .env("SAFECOPY_JOURNAL_DIR", journal_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let started = start(command, &Alone::take()).unwrap();
    let output = started.wait_with_output().unwrap();
    let out = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "the child failed: {out}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    out
}

/// A file locked here and not marked to be closed in a child stays the
/// child's for as long as it runs, unless the child closes it before it
/// becomes its own program: then the file is free once the start is back.
#[cfg(unix)]
#[test]
fn a_started_child_holds_no_file_of_this_process() {
    use std::os::fd::AsRawFd;
    use std::process::Stdio;
    let (_scratch, place, _) = scratch("child-holds-none");
    let path = place.join("held");
    let mut command = as_role("wait");
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // No other test starts a child that would be given the file for good.
    let alone = Alone::take();
    // More open ones than one reading of the list gives, below the two held.
    let many: Vec<File> = (0..100).map(|_| File::open(&place).unwrap()).collect();
    let file = File::create(&path).unwrap();
    file.lock().unwrap();
    let folder = File::open(&place).unwrap();
    folder.lock().unwrap();
    for kept in [&file, &folder] {
        // SAFETY: the flags of an open descriptor are read and set.
        let cleared = unsafe {
            let flags = libc::fcntl(kept.as_raw_fd(), libc::F_GETFD);
            assert!(flags >= 0);
            libc::fcntl(kept.as_raw_fd(), libc::F_SETFD, flags & !libc::FD_CLOEXEC)
        };
        assert_eq!(cleared, 0);
    }
    let mut running = start(command, &alone).unwrap();
    drop((file, folder, many));
    let free = [&path, &place].map(|held| File::open(held).unwrap().try_lock().is_ok());
    // The child runs until its input ends: it was there to hold them.
    assert!(running.try_wait().unwrap().is_none());
    drop(running.stdin.take());
    assert!(running.wait().unwrap().success());
    drop(alone);
    assert_eq!(free, [true, true], "the file, the folder");
}

/// The child keeps the pipe a failed start is told through: a program
/// that is not there is not taken for started.
#[test]
fn a_program_that_is_not_there_is_not_started() {
    let (scratch, _, _) = scratch("child-none");
    let command = Command::new(scratch.path().join("no-such-program"));
    let failed = start(command, &Alone::take()).unwrap_err();
    assert_eq!(failed.kind(), io::ErrorKind::NotFound);
}

/// What another process does: started by [`child`], skipped otherwise.
#[test]
#[ignore = "run by other tests as a second process"]
fn as_child() {
    let Some(role) = std::env::var_os("SAFECOPY_JOURNAL_CHILD") else {
        return;
    };
    if role == "wait" {
        // Runs until its starter closes its input.
        std::io::copy(&mut std::io::stdin(), &mut std::io::sink()).unwrap();
        return;
    }
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

/// A program that starts a child process just as it lets a journal go:
/// until the child is its own program it holds a copy of every open file
/// of this one, the journal's lock with them.
#[cfg(unix)]
#[test]
fn a_journal_let_go_while_a_child_starts_is_left_for_the_next_cleanup() {
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    let (_scratch, place, journal_dir) = scratch("child-start");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    drop(partial(&root, &journal, 0, "a.part", b"half"));
    let (mut forked, tell) = io::pipe().unwrap();
    let (wait, go) = io::pipe().unwrap();
    let go_in_child = go.as_raw_fd();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["tests::as_child", "--exact", "--ignored"])
        .env_remove("SAFECOPY_JOURNAL_CHILD")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: between the fork and the exec only descriptors are closed,
    // written and read, which is allowed there.
    unsafe {
        command.pre_exec(move || {
            // The child's own copy of the end it waits on, or the pipe
            // would never end for it.
            drop(OwnedFd::from_raw_fd(go_in_child));
            (&tell).write_all(&[1])?;
            (&wait).read(&mut [0]).map(drop)
        });
    }
    // No other test's files are copied into the child held here.
    let alone = Alone::take();
    let starting = std::thread::spawn(move || command.status());
    // The child exists and is not yet its own program.
    forked.read_exact(&mut [0]).unwrap();
    journal.crash();
    drop(root);
    // The journal is let go of here, and still reads as held: left whole.
    assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(0, 0));
    assert!(recover(&journal_dir, &settings()).unwrap().is_empty());
    assert_eq!(journals(&journal_dir), 1);
    assert!(place.join("a.part").exists());
    // The child becomes its own program: the copies are closed.
    drop(go);
    assert!(starting.join().unwrap().unwrap().success());
    drop(alone);
    assert_eq!(recover(&journal_dir, &settings()).unwrap().len(), 1);
    assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(1, 0));
    assert!(!place.join("a.part").exists());
    assert_eq!(journals(&journal_dir), 0);
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
    let state = State::new(
        location::root_of(root).unwrap(),
        &settings(),
        crate::log::FORMAT,
    );
    let mut bytes = frame(&state.header());
    for (n, name) in (1u32..).zip(levels) {
        let entry = Entry::Folder(Folder {
            n,
            parent: n - 1,
            name: name.to_string(),
            device: id.volume,
            id: id.file,
        });
        bytes.extend(frame(&entry.encode(crate::log::FORMAT)));
    }
    let started = Entry::Started(Started {
        place: 0,
        folder: 2,
        name: "a.jpg".to_string(),
        partial: "a.jpg.part".to_string(),
        size: 4,
        file: identity(file).unwrap().file,
        second: crate::Second::None,
    });
    bytes.extend(frame(&started.encode(crate::log::FORMAT)));
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
            second: crate::Second::None,
        });
        let header = crate::log::frame(&state.header());
        assert_eq!(
            read_segment(&header, &settings()),
            Err(JournalError::Damaged),
            "{levels:?}"
        );
    }
    // Alone and directly below the root, it reads.
    let mut alone = crate::log::State::new(
        location::root_of(&root).unwrap(),
        &settings(),
        crate::log::FORMAT,
    );
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
        second: crate::Second::None,
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

    /// A journal of the format of before whose partial's name the folder
    /// crate does not open: nothing says the file is another, so the
    /// journal stays for another try.
    #[test]
    fn a_recorded_name_the_folder_crate_refuses_keeps_its_journal() {
        let (_scratch, place, journal_dir) = scratch("refused-record");
        let root = Directory::destination(&place).unwrap();
        std::fs::write(place.join("a.part"), b"half").unwrap();
        let file = File::open(place.join("a.part")).unwrap();
        let record = Record {
            volume: root.volume_path().unwrap(),
            serial: root.identity().unwrap().volume,
            partials: vec![Partial {
                folder: root.identity().unwrap().file,
                name: "a\0.part".into(),
                file: safecopy_folder::identity(&file).unwrap().file,
            }],
        };
        drop(file);
        assert_eq!(
            win::remove(&record, &record.partials[0], &settings()),
            Err(location::Left::Io)
        );
        let path = journal_dir.join(format!("1-1.{EXTENSION}"));
        std::fs::write(&path, record.encode(&settings()).unwrap()).unwrap();
        let kept = Cleaned {
            removed: 0,
            left: 1,
            kept: 1,
        };
        assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), kept);
        assert!(path.exists(), "kept for another try");
        assert_eq!(std::fs::read(place.join("a.part")).unwrap(), b"half");
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

/// A record read from a disk may name any file of the person's: only a
/// name that is a partial's is opened, measured or removed.
#[test]
fn a_recorded_name_that_is_not_a_partials_is_never_opened() {
    let (_scratch, place, _) = scratch("not-partial");
    let root = Directory::destination(&place).unwrap();
    let bytes = [7u8; 64];
    // The place of the file at `path`, recorded under `name`.
    let recorded = |name: &str, path: &Path| {
        let file = std::fs::File::open(path).unwrap();
        PartialAt::of(&root, &root, &file, OsStr::new(name)).unwrap()
    };
    let plain = place.join("report.docx");
    std::fs::write(&plain, bytes).unwrap();
    let alone = place.join(".part");
    std::fs::write(&alone, bytes).unwrap();
    std::fs::create_dir(place.join("sub")).unwrap();
    let below = place.join("sub").join("a.part");
    std::fs::write(&below, bytes).unwrap();
    // Windows reads it as a stream of the file; elsewhere it is a name.
    let stream = if cfg!(windows) {
        plain.clone()
    } else {
        let stream = place.join("report.docx:x.part");
        std::fs::write(&stream, bytes).unwrap();
        stream
    };
    let refused = [
        recorded("report.docx", &plain),
        recorded(".part", &alone),
        recorded("report.docx:x.part", &stream),
        recorded("sub/a.part", &below),
        recorded(r"sub\a.part", &below),
    ];
    let s = settings();
    for at in &refused {
        assert!(!at.is_partial(&s), "{}", at.partial);
        assert_eq!(at.try_reopen(&s).err(), Some(Left::Name), "{}", at.partial);
        assert!(at.reopen(&s).is_none(), "{}", at.partial);
        assert_eq!(at.present(&s), None, "{}", at.partial);
        assert!(!at.remove(&s), "{}", at.partial);
    }
    for path in [&plain, &alone, &below, &stream] {
        assert_eq!(std::fs::read(path).unwrap(), bytes, "as it was");
    }

    // The ending is a setting: each one's partials are the other's files.
    let other = Settings {
        partial_ending: ".tmp".to_string(),
        ..settings()
    };
    let (part, tmp) = (place.join("a.bin.part"), place.join("a.bin.tmp"));
    std::fs::write(&part, bytes).unwrap();
    std::fs::write(&tmp, bytes).unwrap();
    let (part, tmp) = (recorded("a.bin.part", &part), recorded("a.bin.tmp", &tmp));
    assert!(part.is_partial(&s) && !part.is_partial(&other));
    assert!(tmp.is_partial(&other) && !tmp.is_partial(&s));
    assert_eq!(part.try_reopen(&other).err(), Some(Left::Name));
    assert_eq!(part.present(&other), None);
    assert_eq!(tmp.try_reopen(&s).err(), Some(Left::Name));
    assert_eq!(tmp.present(&s), None);
    assert_eq!(part.present(&s), Some(64));
    assert_eq!(tmp.present(&other), Some(64));
    assert!(part.try_reopen(&s).is_ok());
    assert!(tmp.try_reopen(&other).is_ok());
}

// A file system mounted below the root.

/// A folder acted out as a mount point until this is dropped: the walks
/// read it as on another device than its own.
struct Mounted;

impl Mounted {
    /// With the device it is read as on.
    fn at(dir: &Directory) -> (Self, u64) {
        let id = dir.identity().unwrap();
        let device = id.volume ^ 1;
        seams::OTHER_DEVICE.set(Some((id.file, device)));
        (Self, device)
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        seams::OTHER_DEVICE.set(None);
    }
}

/// Each way of recording the partial `file`, made as `a.part` in `dir`.
fn recorded(
    journal: &Journal,
    root: &Directory,
    dir: &Directory,
    file: &File,
) -> [Result<(), JournalError>; 3] {
    let partial = OsStr::new("a.part");
    let start = Start {
        place: 0,
        file,
        partial,
        name: "a",
        size: 0,
    };
    [
        journal.started(0, dir, file, partial, "a", 0),
        journal.started_many(dir, &[start, Start { place: 1, ..start }]),
        PartialAt::of(root, dir, file, partial).map(|_| ()),
    ]
}

#[test]
fn a_folder_on_or_below_another_device_is_refused() {
    let (_scratch, place, journal_dir) = scratch("other-device");
    let root = Directory::destination(&place).unwrap();
    let disk = root.claim("Disk").unwrap();
    let below = disk.claim("Photos").unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    let mounted = Mounted::at(&disk);
    // The folder that is the mount point, and one reached through it that
    // reports the root's own device again.
    for dir in [&disk, &below] {
        let file = dir.create(OsStr::new("a.part")).unwrap();
        assert_eq!(
            recorded(&journal, &root, dir, &file),
            [Err(JournalError::OtherVolume); 3]
        );
        assert!(journal.partials().is_empty());
        dir.discard(&file, OsStr::new("a.part")).unwrap();
    }
    // The root's own files are recorded as ever.
    let file = root.create(OsStr::new("a.part")).unwrap();
    journal
        .started(0, &root, &file, OsStr::new("a.part"), "a", 0)
        .unwrap();
    journal.ended(0).unwrap();
    // One device again, and the same folder is taken.
    drop(mounted);
    let file = below.create(OsStr::new("a.part")).unwrap();
    journal
        .started(0, &below, &file, OsStr::new("a.part"), "a", 0)
        .expect("only the device refused it");
    journal.ended(0).unwrap();
    // Renamed, where nothing reads its place: not by its identity alone.
    below.discard(&file, OsStr::new("a.part")).unwrap();
    drop((file, below));
    let _mounted = Mounted::at(&disk);
    std::fs::rename(place.join("Disk"), place.join("Drive")).unwrap();
    seams::PLACE.set(seams::Place::Unknown);
    let file = disk.create(OsStr::new("a.part")).unwrap();
    let alone = recorded(&journal, &root, &disk, &file);
    seams::PLACE.set(seams::Place::Read);
    assert_eq!(alone, [Err(JournalError::OtherVolume); 3]);
    disk.discard(&file, OsStr::new("a.part")).unwrap();
}

/// A folder as a segment records it: its name, device and inode.
type Recorded<'a> = (&'a str, u64, u128);

/// A segment as a version that walked across a mount wrote it: `levels`
/// one below the other from the root, each with the device and inode it
/// had, and the partial `file` as `partial` in the last.
fn older_segment(root: &Directory, levels: &[Recorded<'_>], partial: &str, file: &File) -> Vec<u8> {
    use crate::log::{Entry, State, frame};
    let state = State::new(
        location::root_of(root).unwrap(),
        &settings(),
        crate::log::FORMAT,
    );
    let mut bytes = frame(&state.header());
    for (n, (name, device, id)) in (1u32..).zip(levels) {
        let entry = Entry::Folder(Folder {
            n,
            parent: n - 1,
            name: name.to_string(),
            device: *device,
            id: *id,
        });
        bytes.extend(frame(&entry.encode(crate::log::FORMAT)));
    }
    let started = Entry::Started(Started {
        place: 0,
        folder: levels.len() as u32,
        name: partial.strip_suffix(".part").unwrap().to_string(),
        partial: partial.to_string(),
        size: 4,
        file: identity(file).unwrap().file,
        second: crate::Second::None,
    });
    bytes.extend(frame(&started.encode(crate::log::FORMAT)));
    bytes
}

/// What every way of finding a partial again says of `at`, which is below
/// a level on another device: nothing is opened, cut or removed.
fn never_found(at: &PartialAt) {
    let s = settings();
    assert!(at.is_partial(&s));
    assert_eq!(at.location.open().err(), Some(Left::Volume));
    assert_eq!(at.try_reopen(&s).err(), Some(Left::Volume));
    assert!(at.reopen(&s).is_none());
    assert_eq!(at.present(&s), None);
    assert!(!at.remove(&s));
}

/// Unix walked a mount below the root and recorded what was there until
/// this rule: such a record is still read, and never followed, on any
/// system and at any level.
#[test]
fn an_older_record_below_another_device_is_left_and_never_found() {
    let (_scratch, place, journal_dir) = scratch("older-device");
    let root = Directory::destination(&place).unwrap();
    let disk = root.claim("Disk").unwrap();
    let below = disk.claim("Photos").unwrap();
    let (mounted, device) = Mounted::at(&disk);
    let (disk_id, below_id) = (disk.identity().unwrap(), below.identity().unwrap());
    // Directly in the mounted folder, and in a folder further down.
    let on = [("Disk", device, disk_id.file)];
    let under = [on[0], ("Photos", below_id.volume, below_id.file)];
    let cases: [(&Directory, &[Recorded<'_>], &str); 2] =
        [(&disk, &on, "a.jpg.part"), (&below, &under, "b.jpg.part")];
    let mut places = Vec::new();
    for (n, (dir, levels, partial)) in cases.into_iter().enumerate() {
        let mut file = dir.create(OsStr::new(partial)).unwrap();
        file.write_all(b"half").unwrap();
        let bytes = older_segment(&root, levels, partial, &file);
        drop(file);
        let segment = read_segment(&bytes, &settings()).unwrap();
        let at = segment.places().pop().unwrap().unwrap();
        assert_eq!(at.location.levels.len(), levels.len());
        places.push(at);
        std::fs::write(journal_dir.join(format!("7-{n}.1.jlog")), bytes).unwrap();
    }
    // The partial under its published name too: a recovery that looked in
    // the folder would find it there.
    for (partial, name) in [
        ("Disk/a.jpg.part", "Disk/a.jpg"),
        ("Disk/Photos/b.jpg.part", "Disk/Photos/b.jpg"),
    ] {
        std::fs::hard_link(place.join(partial), place.join(name)).unwrap();
    }
    let read = recover(&journal_dir, &settings()).unwrap();
    assert_eq!(read.len(), 2);
    for one in &read {
        assert_eq!(one.partials().len(), 1);
        assert!(one.settled.is_empty() && one.candidates.is_empty());
    }
    places.iter().for_each(never_found);
    // Left, and the journals go: their entries are ones never found.
    assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(0, 2));
    assert_eq!(journals(&journal_dir), 0);
    assert_eq!(
        std::fs::read(place.join("Disk/a.jpg.part")).unwrap(),
        b"half"
    );
    assert_eq!(
        std::fs::read(place.join("Disk/Photos/b.jpg.part")).unwrap(),
        b"half"
    );
    drop(mounted);
}

/// A real second file system below the root, where the process may mount
/// one: Linux, as root or with the capability for it.
#[cfg(target_os = "linux")]
mod mounted {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    /// Whether a mount that cannot be made fails its test: set where the
    /// tests run with the right to mount.
    fn mount_required() -> bool {
        std::env::var_os("SAFECOPY_REQUIRE_MOUNT").is_some_and(|v| !v.is_empty())
    }

    /// A memory file system mounted at a folder until this is dropped.
    struct Tmpfs(CString);

    impl Tmpfs {
        fn mount(at: &Path) -> io::Result<Self> {
            let at = CString::new(at.as_os_str().as_bytes()).unwrap();
            let kind = c"tmpfs";
            // SAFETY: three strings that end in a zero byte, and no data.
            let made = unsafe {
                libc::mount(
                    kind.as_ptr(),
                    at.as_ptr(),
                    kind.as_ptr(),
                    0,
                    std::ptr::null(),
                )
            };
            match made {
                0 => Ok(Self(at)),
                _ => Err(io::Error::last_os_error()),
            }
        }
    }

    impl Drop for Tmpfs {
        fn drop(&mut self) {
            // Detached even with a folder of it still open.
            // SAFETY: a string that ends in a zero byte.
            unsafe { libc::umount2(self.0.as_ptr(), libc::MNT_DETACH) };
        }
    }

    /// `at` with a file system mounted on it, or `None` where this process
    /// may not mount one and need not.
    fn mount(at: &Path) -> Option<Tmpfs> {
        match Tmpfs::mount(at) {
            Ok(mounted) => Some(mounted),
            Err(e) => {
                assert!(
                    !mount_required(),
                    "SAFECOPY_REQUIRE_MOUNT is set, but no file system can be mounted: {e}"
                );
                eprintln!("skipped: this process may not mount a file system ({e})");
                None
            }
        }
    }

    #[test]
    fn a_folder_on_a_file_system_mounted_below_the_root_is_refused() {
        let (_scratch, place, journal_dir) = scratch("mount");
        let root = Directory::destination(&place).unwrap();
        // Recorded while it is one file system: what a mount put over the
        // folder later does to it.
        let songs = root.claim("Songs").unwrap();
        let mut early = songs.create(OsStr::new("c.mp3.part")).unwrap();
        early.write_all(b"half").unwrap();
        let before = PartialAt::of(&root, &songs, &early, OsStr::new("c.mp3.part")).unwrap();
        drop((early, songs));
        std::fs::create_dir(place.join("Disk")).unwrap();
        let Some(_disk) = mount(&place.join("Disk")) else {
            return;
        };
        let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
        let disk = root.directory(OsStr::new("Disk")).unwrap();
        let below = disk.claim("Photos").unwrap();
        let (top, disk_id, below_id) = (
            root.identity().unwrap(),
            disk.identity().unwrap(),
            below.identity().unwrap(),
        );
        assert_ne!(top.volume, disk_id.volume, "another device");
        assert_eq!(disk_id.volume, below_id.volume);
        let on = [("Disk", disk_id.volume, disk_id.file)];
        let under = [on[0], ("Photos", below_id.volume, below_id.file)];
        let cases: [(&Directory, &[Recorded<'_>], &str); 2] =
            [(&disk, &on, "a.jpg.part"), (&below, &under, "b.jpg.part")];
        for (n, (dir, levels, partial)) in cases.into_iter().enumerate() {
            let file = dir.create(OsStr::new("a.part")).unwrap();
            assert_eq!(
                recorded(&journal, &root, dir, &file),
                [Err(JournalError::OtherVolume); 3]
            );
            assert!(journal.partials().is_empty());
            dir.discard(&file, OsStr::new("a.part")).unwrap();
            // What a version that walked the mount left there.
            let mut file = dir.create(OsStr::new(partial)).unwrap();
            file.write_all(b"half").unwrap();
            let bytes = older_segment(&root, levels, partial, &file);
            drop(file);
            let segment = read_segment(&bytes, &settings()).unwrap();
            never_found(&segment.places().pop().unwrap().unwrap());
            std::fs::write(journal_dir.join(format!("7-{n}.1.jlog")), bytes).unwrap();
        }
        drop(journal);
        assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(0, 2));
        assert_eq!(journals(&journal_dir), 0);
        assert_eq!(
            std::fs::read(place.join("Disk/a.jpg.part")).unwrap(),
            b"half"
        );
        assert_eq!(
            std::fs::read(place.join("Disk/Photos/b.jpg.part")).unwrap(),
            b"half"
        );

        // A file system mounted over a folder recorded before: another
        // folder is under the name, and the partial below is not reached.
        let s = settings();
        assert_eq!(before.present(&s), Some(4));
        let over = mount(&place.join("Songs")).expect("mounted once already");
        std::fs::write(place.join("Songs/c.mp3.part"), b"other").unwrap();
        assert_eq!(before.try_reopen(&s).err(), Some(Left::Replaced));
        assert_eq!(before.present(&s), None);
        assert!(!before.remove(&s));
        drop(over);
        assert_eq!(before.present(&s), Some(4));
    }
}

/// Every refusal the folder crate has, as an error of an open.
fn refusals() -> ([Refusal; 4], [Refusal; 9]) {
    (
        [
            Refusal::Link,
            Refusal::NotFileOrFolder,
            Refusal::NotFolder,
            Refusal::Replaced,
        ],
        [
            Refusal::LeavesFolder,
            Refusal::Stream,
            Refusal::Nul,
            Refusal::TooLong,
            Refusal::BadListing,
            Refusal::NoVolume,
            Refusal::NoNameLeft { tried: 3 },
            Refusal::Unproven,
            Refusal::OtherFolder,
        ],
    )
}

#[test]
fn every_refusal_reads_as_the_left_its_table_says() {
    let (another, unknown) = refusals();
    for why in another {
        let left = Left::of(&io::Error::other(why));
        assert_eq!(left, Left::Replaced, "{why:?}");
        assert!(!left.may_change(), "{why:?} lets the journal go");
    }
    for why in unknown {
        let left = Left::of(&io::Error::other(why));
        assert_eq!(left, Left::Io, "{why:?}");
        assert!(left.may_change(), "{why:?} keeps the journal");
    }
    // The system's own errors read as before.
    for (kind, left) in [
        (io::ErrorKind::NotFound, Left::Gone),
        (io::ErrorKind::NotADirectory, Left::Gone),
        (io::ErrorKind::InvalidInput, Left::Gone),
        (io::ErrorKind::IsADirectory, Left::Replaced),
        (io::ErrorKind::PermissionDenied, Left::Io),
        (io::ErrorKind::Other, Left::Io),
    ] {
        assert_eq!(Left::of(&kind.into()), left, "{kind:?}");
    }
}

/// A journal left by a crash with one partial of four bytes, and where
/// that partial is.
fn crashed_with_a_partial(place: &Path, journal_dir: &Path) -> PartialAt {
    let root = Directory::destination(place).unwrap();
    let journal = Journal::start(journal_dir, &root, &settings()).unwrap();
    let file = partial(&root, &journal, 0, "a.txt.part", b"half");
    let at = PartialAt::of(&root, &root, &file, OsStr::new("a.txt.part")).unwrap();
    drop(file);
    journal.crash();
    at
}

#[test]
fn a_refusal_that_says_nothing_of_the_file_removes_nothing_and_keeps_the_journal() {
    let (_scratch, place, journal_dir) = scratch("refused-unknown");
    let at = crashed_with_a_partial(&place, &journal_dir);
    let s = settings();
    for why in refusals().1 {
        seams::REFUSED.set(Some(why));
        let kept = Cleaned {
            removed: 0,
            left: 1,
            kept: 1,
        };
        assert_eq!(clean_up(&journal_dir, &s).unwrap(), kept, "{why:?}");
        assert_eq!(journals(&journal_dir), 1, "{why:?}: kept for another try");
        assert_eq!(at.present(&s), None, "{why:?}");
        assert_eq!(at.try_reopen(&s).err(), Some(Left::Io), "{why:?}");
        assert!(at.reopen(&s).is_none(), "{why:?}");
        assert!(!at.remove(&s), "{why:?}");
        assert_eq!(std::fs::read(place.join("a.txt.part")).unwrap(), b"half");
    }
    seams::REFUSED.set(None);
    // With no switch: a recorded name with a NUL in it, which the folder
    // crate refuses before it opens anything.
    let mut nul = at.clone();
    nul.partial = "a\0.txt.part".to_string();
    assert!(nul.is_partial(&s));
    assert_eq!(nul.try_reopen(&s).err(), Some(Left::Io));
    assert_eq!(nul.present(&s), None);
    assert!(!nul.remove(&s));
    // And a recorded folder whose name is not one part of a path. Unix
    // opens each level by its name; Windows opens the folder by its ID.
    #[cfg(unix)]
    {
        let Root::Unix { device, .. } = at.location.root else {
            panic!("a root recorded on Unix");
        };
        let mut below = at.clone();
        below.location.levels = vec![Level {
            name: "no/such".to_string(),
            device,
            id: 1,
        }];
        assert_eq!(below.try_reopen(&s).err(), Some(Left::Io));
        assert!(!below.remove(&s));
    }
    // The refusal gone, the same journal still removes its partial.
    assert_eq!(at.present(&s), Some(4));
    assert_eq!(clean_up(&journal_dir, &s).unwrap(), cleaned(1, 0));
    assert!(!place.join("a.txt.part").exists());
    assert_eq!(journals(&journal_dir), 0);
}

#[test]
fn a_refusal_that_says_the_file_is_another_lets_the_journal_go() {
    for (n, why) in refusals().0.into_iter().enumerate() {
        let (_scratch, place, journal_dir) = scratch(&format!("refused-another-{n}"));
        let at = crashed_with_a_partial(&place, &journal_dir);
        let s = settings();
        seams::REFUSED.set(Some(why));
        assert_eq!(at.try_reopen(&s).err(), Some(Left::Replaced), "{why:?}");
        assert_eq!(at.present(&s), None, "{why:?}");
        assert!(!at.remove(&s), "{why:?}");
        assert_eq!(clean_up(&journal_dir, &s).unwrap(), cleaned(0, 1));
        assert_eq!(journals(&journal_dir), 0, "{why:?}: no later try finds it");
        // What is under the name is never removed for a refusal.
        assert_eq!(std::fs::read(place.join("a.txt.part")).unwrap(), b"half");
        seams::REFUSED.set(None);
    }
}
