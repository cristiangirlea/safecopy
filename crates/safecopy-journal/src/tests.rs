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

#[cfg(not(windows))]
#[test]
fn elsewhere_a_journal_records_nothing_and_cleanup_only_removes_journals() {
    let (_scratch, place, journal_dir) = scratch("elsewhere");
    let root = Directory::destination(&place).unwrap();
    let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
    let file = root.create(OsStr::new("a.txt.part")).unwrap();
    journal
        .started(&root, &file, OsStr::new("a.txt.part"))
        .unwrap();
    assert_eq!(journals(&journal_dir), 0, "no journal file is written");
    std::fs::write(
        journal_dir.join("1-1.journal"),
        sample().encode(&settings()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        clean_up(&journal_dir, &settings()).unwrap(),
        Cleaned::default()
    );
    assert_eq!(journals(&journal_dir), 0);
    assert!(place.join("a.txt.part").exists());
    assert_eq!(remove_by_identity(&sample(), &settings()), cleaned(0, 2));
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::os::windows::fs::MetadataExt;
    use std::process::Command;

    fn partial(dir: &Directory, journal: &Journal, name: &str, bytes: &[u8]) -> File {
        let mut file = dir.create(OsStr::new(name)).unwrap();
        journal.started(dir, &file, OsStr::new(name)).unwrap();
        file.write_all(bytes).unwrap();
        file
    }

    /// The test binary run again as another process, in `role`.
    fn child(role: &str, place: &Path, journal_dir: &Path) -> String {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "tests::windows::as_child",
                "--exact",
                "--ignored",
                "--nocapture",
            ])
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
                let _a = partial(&root, &journal, "a.txt.part", b"half");
                let _b = partial(&sub, &journal, "b.jpg.part", b"half");
                let _c = partial(&root, &journal, "c.txt.part", b"half");
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
        assert_eq!(journals(&journal_dir), 1, "the crash leaves its record");
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
        std::fs::remove_file(place.join("Photos/b.jpg.part")).unwrap();
        std::fs::write(place.join("Photos/b.jpg.part"), b"theirs").unwrap();
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
        drop(partial(&root, &journal, "a.txt.part", b"half"));
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
        drop(partial(&root, &journal, "a.txt.part", b"half"));
        drop(partial(&sub, &journal, "b.jpg.part", b"half"));
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
        drop(partial(&root, &journal, "a.txt.part", b"whole"));
        assert_eq!(journals(&journal_dir), 1);
        journal.ended(&root, OsStr::new("a.txt.part")).unwrap();
        std::fs::rename(place.join("a.txt.part"), place.join("a.txt")).unwrap();
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
        drop(partial(&root, &journal, "a.txt.part", b"half"));
        drop(journal);
        assert_eq!(journals(&journal_dir), 1);
        assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(1, 0));
        assert!(!place.join("a.txt.part").exists());
    }

    #[test]
    fn cleanup_leaves_another_file_of_the_name_a_renamed_one_and_another_volume() {
        let (_scratch, place, journal_dir) = scratch("left");
        let root = Directory::destination(&place).unwrap();
        let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
        drop(partial(&root, &journal, "same.txt.part", b"mine"));
        drop(partial(&root, &journal, "renamed.txt.part", b"mine"));
        // Another file now has the recorded name.
        std::fs::remove_file(place.join("same.txt.part")).unwrap();
        std::fs::write(place.join("same.txt.part"), b"theirs").unwrap();
        // The other one was renamed.
        std::fs::rename(place.join("renamed.txt.part"), place.join("kept.txt")).unwrap();
        journal.crash();
        drop(root);
        assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(0, 2));
        assert_eq!(
            std::fs::read(place.join("same.txt.part")).unwrap(),
            b"theirs"
        );
        assert_eq!(std::fs::read(place.join("kept.txt")).unwrap(), b"mine");

        // A volume whose serial is not the recorded one is another disk.
        let root = Directory::destination(&place).unwrap();
        let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
        drop(partial(&root, &journal, "c.txt.part", b"mine"));
        {
            let open = journal.inner.as_ref().unwrap();
            let mut record = open.record();
            record.serial ^= 1;
            open.write(&record).unwrap();
        }
        journal.crash();
        drop(root);
        assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(0, 1));
        assert!(place.join("c.txt.part").exists());
        assert_eq!(journals(&journal_dir), 0);
    }

    #[test]
    fn a_name_without_the_partial_ending_is_never_removed() {
        let (_scratch, place, journal_dir) = scratch("pattern");
        let root = Directory::destination(&place).unwrap();
        let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
        drop(partial(&root, &journal, "report.docx", b"theirs"));
        journal.crash();
        drop(root);
        assert_eq!(clean_up(&journal_dir, &settings()).unwrap().left, 1);
        assert!(place.join("report.docx").exists());
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
            Err(win::Left::Name)
        );
        assert_eq!(remove_by_identity(&record, &settings()), cleaned(0, 1));
        assert_eq!(
            std::fs::read(place.join("report.docx:x.part")).unwrap(),
            b"stream"
        );
        assert_eq!(std::fs::read(place.join("report.docx")).unwrap(), b"theirs");
    }

    #[test]
    fn a_partial_whose_record_would_pass_its_limit_is_not_recorded() {
        let (_scratch, place, journal_dir) = scratch("limit");
        let root = Directory::destination(&place).unwrap();
        // Room for the volume and exactly this one partial.
        let mut a = root.create(OsStr::new("a.part")).unwrap();
        let one = Record {
            volume: root.volume_path().unwrap(),
            serial: root.identity().unwrap().volume,
            partials: vec![Partial {
                folder: root.identity().unwrap().file,
                name: "a.part".to_string(),
                file: safecopy_folder::identity(&a).unwrap().file,
            }],
        };
        let tight = Settings {
            record_bytes: one.encode(&settings()).unwrap().len(),
            ..settings()
        };
        let journal = Journal::start(&journal_dir, &root, &tight).unwrap();
        journal.started(&root, &a, OsStr::new("a.part")).unwrap();
        a.write_all(b"half").unwrap();
        drop(a);
        let b = root.create(OsStr::new("b.part")).unwrap();
        assert_eq!(
            journal.started(&root, &b, OsStr::new("b.part")),
            Err(JournalError::TooLarge {
                most: tight.record_bytes
            })
        );
        root.discard(&b, OsStr::new("b.part")).unwrap();
        drop(b);
        journal.crash();
        drop(root);
        // The journal on the disk still holds the one partial it took.
        assert_eq!(clean_up(&journal_dir, &tight).unwrap(), cleaned(1, 0));
        assert!(!place.join("a.part").exists());
    }

    #[test]
    fn a_journal_longer_than_the_limit_is_removed_and_touches_nothing() {
        let (_scratch, place, journal_dir) = scratch("too-long");
        let root = Directory::destination(&place).unwrap();
        let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
        drop(partial(&root, &journal, "a.part", b"half"));
        journal.crash();
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
    fn a_journal_name_left_by_an_earlier_process_is_stepped_past() {
        use std::sync::atomic::AtomicU64;
        let (_scratch, _, journal_dir) = scratch("taken-name");
        let pid = std::process::id();
        let name = |n: u64| journal_dir.join(format!("{pid}-{n}.{EXTENSION}"));
        for n in 1..=4 {
            std::fs::write(name(n), b"old").unwrap();
        }
        let next = AtomicU64::new(1);
        drop(win::create_named(&journal_dir, &next).unwrap());
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
            win::create_named(&journal_dir, &next).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert!(!name(70).exists());
    }

    #[test]
    fn a_link_named_as_a_journal_is_never_followed() {
        let (_scratch, place, journal_dir) = scratch("linked-journal");
        let target = place.join("thesis.docx");
        std::fs::write(&target, b"theirs").unwrap();
        let link = journal_dir.join("1-1.journal");
        // A file symbolic link needs Developer Mode or an administrator, as
        // on CI's runners; a junction cannot name a file.
        if std::os::windows::fs::symlink_file(&target, &link).is_err() {
            assert!(
                !symlinks_required(),
                "SAFECOPY_REQUIRE_SYMLINKS is set, but no file symbolic link can be made \
                 (it needs Developer Mode or an administrator)"
            );
            eprintln!("skipped: no symbolic link can be made here");
            return;
        }
        assert_eq!(
            clean_up(&journal_dir, &settings()).unwrap(),
            Cleaned::default()
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"theirs");
    }

    #[test]
    fn a_junction_named_as_a_journal_is_never_followed() {
        let (_scratch, place, journal_dir) = scratch("junction-journal");
        let target = place.join("Thesis");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("thesis.docx"), b"theirs").unwrap();
        let link = journal_dir.join("1-1.journal");
        // A junction needs no privilege: made everywhere.
        let made = Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        assert_eq!(
            clean_up(&journal_dir, &settings()).unwrap(),
            Cleaned::default()
        );
        assert_eq!(
            std::fs::read(target.join("thesis.docx")).unwrap(),
            b"theirs"
        );
        assert!(
            std::fs::symlink_metadata(&link).unwrap().file_attributes()
                & ::windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT.0
                != 0,
            "the junction is still there"
        );
        assert_eq!(journals(&journal_dir), 1);
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
        drop(partial(&root, &journal, "a.part", b"half"));
        drop(partial(&root, &journal, "b.part", b"half"));
        journal.crash();
        drop(root);
        // The disk is not plugged in at this start.
        win::seams::NO_VOLUME.set(true);
        let done = clean_up(&journal_dir, &settings());
        win::seams::NO_VOLUME.set(false);
        assert_eq!(
            done.unwrap(),
            Cleaned {
                removed: 0,
                left: 2,
                kept: 1
            }
        );
        assert!(place.join("a.part").exists());
        assert!(place.join("b.part").exists());
        assert_eq!(journals(&journal_dir), 1, "the journal is kept");
        // Back at a later start, with one partial gone meanwhile.
        std::fs::remove_file(place.join("a.part")).unwrap();
        assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(1, 1));
        assert!(!place.join("b.part").exists());
        assert_eq!(journals(&journal_dir), 0, "then it is removed");
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
            Err(win::Left::Unreachable)
        );
        assert!(place.join("a.part").exists());
    }

    #[test]
    fn a_partial_in_a_folder_that_is_gone_ends_its_journal() {
        let (_scratch, place, journal_dir) = scratch("folder-gone");
        let root = Directory::destination(&place).unwrap();
        let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
        let sub = root.claim("Photos").unwrap();
        drop(partial(&sub, &journal, "a.part", b"half"));
        journal.crash();
        drop((root, sub));
        std::fs::remove_dir_all(place.join("Photos")).unwrap();
        assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(0, 1));
        assert_eq!(journals(&journal_dir), 0);
    }

    #[test]
    fn a_partial_in_use_keeps_its_journal_for_a_later_cleanup() {
        use std::os::windows::fs::OpenOptionsExt;
        let (_scratch, place, journal_dir) = scratch("in-use");
        let root = Directory::destination(&place).unwrap();
        let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
        drop(partial(&root, &journal, "a.part", b"half"));
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
        drop(partial(&root, &journal, "a.part", b"half"));
        journal.crash();
        drop(root);
        let week = Settings {
            keep_for: 7 * DAY,
            ..settings()
        };
        let path = journal_path(&journal_dir);
        changed(&path, 6 * DAY);
        win::seams::NO_VOLUME.set(true);
        let young = clean_up(&journal_dir, &week);
        changed(&path, 8 * DAY);
        let old = clean_up(&journal_dir, &week);
        win::seams::NO_VOLUME.set(false);
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
            drop(partial(&root, &journal, &format!("{n}.part"), b"half"));
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
        win::seams::NO_VOLUME.set(true);
        let done = clean_up(&journal_dir, &two);
        let mut names: Vec<String> = std::fs::read_dir(&journal_dir)
            .unwrap()
            .map(|e| {
                let bytes = std::fs::read(e.unwrap().path()).unwrap();
                Record::decode(&bytes, &settings()).unwrap().partials[0]
                    .name
                    .clone()
            })
            .collect();
        win::seams::NO_VOLUME.set(false);
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
                Err(win::Left::Volume)
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
        // The journal's volume acted out as another one.
        journal.inner.as_ref().unwrap().record().serial ^= 1;
        let file = root.create(OsStr::new("a.part")).unwrap();
        assert_eq!(
            journal.started(&root, &file, OsStr::new("a.part")),
            Err(JournalError::OtherVolume)
        );
        assert!(journal.inner.as_ref().unwrap().record().partials.is_empty());
        root.discard(&file, OsStr::new("a.part")).unwrap();
    }

    #[test]
    fn a_record_write_that_fails_keeps_the_partials_on_the_disk() {
        let (_scratch, place, journal_dir) = scratch("failed-write");
        let root = Directory::destination(&place).unwrap();
        let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
        drop(partial(&root, &journal, "a.part", b"half"));
        let b = root.create(OsStr::new("b.part")).unwrap();
        win::seams::FAIL_WRITES.set(1);
        assert!(matches!(
            journal.started(&root, &b, OsStr::new("b.part")),
            Err(JournalError::Io(_))
        ));
        root.discard(&b, OsStr::new("b.part")).unwrap();
        drop(b);
        // The disk holds the earlier record again, whole.
        let path = std::fs::read_dir(&journal_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let on_disk = Record::decode(&std::fs::read(&path).unwrap(), &settings()).unwrap();
        let names: Vec<_> = on_disk.partials.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["a.part"]);
        // An ended partial whose record cannot be written stays recorded.
        win::seams::FAIL_WRITES.set(1);
        assert!(matches!(
            journal.ended(&root, OsStr::new("a.part")),
            Err(JournalError::Io(_))
        ));
        journal.crash();
        drop(root);
        assert_eq!(clean_up(&journal_dir, &settings()).unwrap(), cleaned(1, 0));
        assert!(!place.join("a.part").exists());
    }

    #[test]
    fn a_partial_name_that_is_not_text_is_refused() {
        use std::os::windows::ffi::OsStringExt;
        let (_scratch, place, journal_dir) = scratch("not-text");
        let root = Directory::destination(&place).unwrap();
        let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
        let file = root.create(OsStr::new("a.part")).unwrap();
        // A lone surrogate: a Windows name, but no Unicode text.
        let name = std::ffi::OsString::from_wide(&[0xD800, u16::from(b'.')]);
        assert_eq!(
            journal.started(&root, &file, &name),
            Err(JournalError::NameNotText)
        );
        root.discard(&file, OsStr::new("a.part")).unwrap();
    }

    #[test]
    fn a_panic_while_the_record_is_held_does_not_stop_the_journal() {
        let (_scratch, place, journal_dir) = scratch("poisoned");
        let root = Directory::destination(&place).unwrap();
        let journal = Journal::start(&journal_dir, &root, &settings()).unwrap();
        let open = journal.inner.as_ref().unwrap();
        let panicked = std::thread::scope(|s| {
            s.spawn(|| {
                let _held = open.record();
                panic!("a panic while the record is held");
            })
            .join()
        });
        assert!(panicked.is_err());
        drop(partial(&root, &journal, "a.part", b"half"));
        journal.ended(&root, OsStr::new("a.part")).unwrap();
        root.remove(OsStr::new("a.part")).unwrap();
        drop(journal);
        assert_eq!(journals(&journal_dir), 0);
    }

    #[test]
    fn a_journal_that_records_nothing_writes_nothing() {
        let (_scratch, place, _) = scratch("none");
        let root = Directory::destination(&place).unwrap();
        let journal = Journal::none();
        let file = root.create(OsStr::new("a.part")).unwrap();
        journal.started(&root, &file, OsStr::new("a.part")).unwrap();
        journal.ended(&root, OsStr::new("a.part")).unwrap();
    }
}
