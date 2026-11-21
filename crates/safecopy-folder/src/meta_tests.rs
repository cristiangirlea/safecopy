//! Tests of a file's time and execute bit, the candidate and the existing
//! folder.

use std::io::Write;
use std::time::SystemTime;

use super::tests::{
    files_in, junctions_required, link_folder, no_link_here, os, remove_link, temp,
};
use super::*;
use crate::seams::{FINISH_ERRORS, NO_HARD_LINKS, NO_RENAME};

/// 2001-09-09 01:46:40.1234567 UTC: whole 100 ns steps, which every
/// system's disk keeps exactly (NTFS counts in them).
const SENT: Time = Time {
    secs: 1_000_000_000,
    nanos: 123_456_700,
};

fn modified(path: &Path) -> Time {
    Time::from_system(std::fs::metadata(path).unwrap().modified().unwrap()).unwrap()
}

fn meta(executable: bool) -> Meta {
    Meta {
        modified: Some(SENT),
        executable,
    }
}

fn finish(executable: bool, meta_rules: MetaRules) -> Finish {
    Finish {
        meta: meta(executable),
        meta_rules,
        sync: true,
    }
}

#[cfg(windows)]
fn zone(path: &Path) -> Option<String> {
    let mut zone = path.as_os_str().to_os_string();
    zone.push(":Zone.Identifier");
    std::fs::read_to_string(zone).ok()
}

#[test]
fn a_time_reads_back_the_same_on_both_sides_of_the_epoch() {
    // Whole 100 ns steps: Windows' system time counts in them.
    for time in [
        SENT,
        Time { secs: 0, nanos: 0 },
        Time {
            secs: -1,
            nanos: 999_999_900,
        },
        Time {
            secs: -86_400,
            nanos: 0,
        },
        Time {
            secs: -86_401,
            nanos: 100,
        },
    ] {
        assert_eq!(Time::from_system(time.to_system().unwrap()), Some(time));
    }
    assert_eq!(
        Time {
            secs: 0,
            nanos: 1_000_000_000
        }
        .to_system(),
        None
    );
}

/// The time is set on the partial file, after the downloaded-file mark
/// (which changes the time on Windows) and before its name; publishing,
/// by rename, link or copy, keeps it.
#[tokio::test]
async fn the_modified_time_is_set_on_the_partial_after_the_mark_and_survives_publish() {
    let rules = PublishRules {
        mark: true,
        ..PublishRules::default()
    };
    for copied in [false, true] {
        let to = temp(&format!("time-{copied}"));
        let dir = Directory::open(&to).unwrap();
        let file = dir.create(os("a.exe.part")).unwrap();
        std::io::Write::write_all(&mut &file, b"MZ!").unwrap();
        NO_HARD_LINKS.set(copied);
        NO_RENAME.set(copied);
        let (published, applied) = dir
            .publish_with(
                &file,
                os("a.exe.part"),
                "a.exe",
                &rules,
                &finish(false, MetaRules::default()),
                std::future::pending(),
            )
            .await
            .unwrap();
        NO_HARD_LINKS.set(false);
        NO_RENAME.set(false);
        assert_eq!(applied.time, TimeApplied::Set);
        // Every handle closed first: a time the system writes at close would show.
        drop((file, dir));
        assert_eq!(files_in(&to), ["a.exe"], "copied: {copied}");
        assert_eq!(modified(&published.path), SENT, "copied: {copied}");
        assert_eq!(std::fs::read(&published.path).unwrap(), b"MZ!");
        #[cfg(windows)]
        assert_eq!(zone(&published.path).as_deref(), Some(win::INTERNET_ZONE));
        std::fs::remove_dir_all(&to).unwrap();
    }
    // A publish that fails has set it on the partial file already, after its mark.
    let to = temp("time-partial");
    let dir = Directory::open(&to).unwrap();
    let file = dir.create(os("a.exe.part")).unwrap();
    #[cfg(windows)]
    let error = 87;
    #[cfg(unix)]
    let error = libc::EIO;
    FINISH_ERRORS.with_borrow_mut(|errors| errors.push_back(error));
    let failed = dir
        .publish_with(
            &file,
            os("a.exe.part"),
            "a.exe",
            &rules,
            &finish(false, MetaRules::default()),
            std::future::pending(),
        )
        .await;
    assert!(matches!(failed, Err(PublishError::Io(_))), "{failed:?}");
    assert_eq!(modified(&to.join("a.exe.part")), SENT);
    #[cfg(windows)]
    assert_eq!(
        zone(&to.join("a.exe.part")).as_deref(),
        Some(win::INTERNET_ZONE)
    );
    dir.discard(&file, os("a.exe.part")).unwrap();
    drop((file, dir));
    std::fs::remove_dir_all(&to).unwrap();
}

/// A time no disk here can hold, before 1601 (Windows' first day; before
/// what Linux's and macOS' disks keep) or far past what any keeps, leaves
/// the time the file arrived with and says so; it is not an error. So does
/// a time with too many nanoseconds.
#[test]
fn a_time_the_file_system_cannot_hold_leaves_the_arrival_time() {
    let to = temp("time-unheld");
    let dir = Directory::open(&to).unwrap();
    let file = dir.create(os("a.part")).unwrap();
    let arrival = modified(&to.join("a.part"));
    for time in [
        Time {
            secs: -11_644_473_601,
            nanos: 0,
        },
        Time {
            secs: i64::MAX,
            nanos: 0,
        },
        Time {
            secs: 1_000_000_000,
            nanos: 1_000_000_000,
        },
    ] {
        let meta = Meta {
            modified: Some(time),
            executable: false,
        };
        let applied = dir.apply(&file, &meta, &MetaRules::default()).unwrap();
        // Never a time the disk changed as it stored it: the time sent, or the arrival time.
        match applied.time {
            TimeApplied::Set => assert_eq!(modified(&to.join("a.part")), time),
            TimeApplied::CannotHold => {
                assert_eq!(modified(&to.join("a.part")), arrival, "{time:?}");
            }
            other => panic!("{other:?} for {time:?}"),
        }
        // Windows cannot hold either; ext4 and APFS neither (btrfs can).
        if cfg!(windows) || time.nanos >= 1_000_000_000 {
            assert_eq!(applied.time, TimeApplied::CannotHold, "{time:?}");
        }
        // Back to the arrival time for the next one.
        file.set_modified(arrival.to_system().unwrap()).unwrap();
    }
    // No time sent, or none asked for, leaves it as well.
    let none = Meta::default();
    let applied = dir.apply(&file, &none, &MetaRules::default()).unwrap();
    assert_eq!(applied.time, TimeApplied::NoTime);
    let not_asked = MetaRules {
        times: false,
        executable: false,
    };
    let applied = dir.apply(&file, &meta(false), &not_asked).unwrap();
    assert_eq!(applied.time, TimeApplied::NotAsked);
    assert_eq!(modified(&to.join("a.part")), arrival);
    dir.discard(&file, os("a.part")).unwrap();
    drop((file, dir));
    std::fs::remove_dir_all(&to).unwrap();
}

/// The execute bit is set only when the caller's rules turn it on and the
/// sender's file had it, and then only the owner's. Never on Windows.
#[tokio::test]
async fn the_executable_bit_is_set_only_when_the_rules_allow_and_only_for_the_owner() {
    assert!(!MetaRules::default().executable, "never set by default");
    let allowed = MetaRules {
        times: true,
        executable: true,
    };
    let here = if cfg!(unix) {
        BitApplied::Set
    } else {
        BitApplied::NotHere
    };
    for (executable, rules, bit) in [
        (true, MetaRules::default(), BitApplied::NotAsked),
        (false, allowed, BitApplied::NotAsked),
        (true, allowed, here),
    ] {
        let to = temp(&format!("bit-{executable}-{}", rules.executable));
        let dir = Directory::open(&to).unwrap();
        let file = dir.create(os("run.part")).unwrap();
        let (published, applied) = dir
            .publish_with(
                &file,
                os("run.part"),
                "run",
                &PublishRules::default(),
                &finish(executable, rules),
                std::future::pending(),
            )
            .await
            .unwrap();
        assert_eq!(applied.executable, bit, "{executable} {rules:?}");
        drop((file, dir));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&published.path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o100 != 0, bit == BitApplied::Set, "{mode:o}");
            // The group's and others' bits are never set.
            assert_eq!(mode & 0o011, 0, "{mode:o}");
        }
        #[cfg(windows)]
        assert!(
            !Meta::read(&File::open(&published.path).unwrap())
                .unwrap()
                .executable
        );
        std::fs::remove_dir_all(&to).unwrap();
    }
}

/// A disk that refuses the time or the execute bit (exFAT keeps no Unix
/// permissions; a share may deny times) still takes the whole, checked
/// file: what was not set is reported, and the file is published.
#[tokio::test]
async fn a_refused_time_or_bit_still_publishes_the_file() {
    #[cfg(unix)]
    let denied = libc::EPERM;
    #[cfg(windows)]
    let denied = 5;
    let allowed = MetaRules {
        times: true,
        executable: true,
    };
    let to = temp("meta-refused");
    let dir = Directory::open(&to).unwrap();
    let file = dir.create(os("run.part")).unwrap();
    let arrival = modified(&to.join("run.part"));
    crate::seams::SET_TIMES_ERROR.set(Some(denied));
    crate::seams::CHMOD_ERROR.set(Some(denied));
    let published = dir
        .publish_with(
            &file,
            os("run.part"),
            "run",
            &PublishRules::default(),
            &finish(true, allowed),
            std::future::pending(),
        )
        .await;
    crate::seams::SET_TIMES_ERROR.set(None);
    crate::seams::CHMOD_ERROR.set(None);
    let (published, applied) = published.unwrap();
    assert_eq!(
        applied.time,
        TimeApplied::Failed(io::ErrorKind::PermissionDenied)
    );
    #[cfg(unix)]
    assert_eq!(
        applied.executable,
        BitApplied::Failed(io::ErrorKind::PermissionDenied)
    );
    #[cfg(windows)]
    assert_eq!(applied.executable, BitApplied::NotHere);
    drop((file, dir));
    assert_eq!(files_in(&to), ["run"]);
    assert_eq!(modified(&published.path), arrival);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&published.path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0, "{mode:o}");
    }
    std::fs::remove_dir_all(&to).unwrap();
}

/// Windows has no execute bit: whatever a file's name, its meta never says
/// it can be run.
#[cfg(windows)]
#[test]
fn meta_read_on_windows_never_says_executable() {
    let to = temp("meta-windows");
    for name in ["setup.exe", "run.bat", "run.cmd", "tool.ps1", "plain"] {
        std::fs::write(to.join(name), b"x").unwrap();
        let file = File::open(to.join(name)).unwrap();
        let meta = Meta::read(&file).unwrap();
        assert!(!meta.executable, "{name}");
        assert_eq!(meta.modified, Some(modified(&to.join(name))));
    }
    std::fs::remove_dir_all(&to).unwrap();
}

/// On Unix the owner's execute bit is what `Meta::read` says, on a
/// regular file only; the group's and others' do not count.
#[cfg(unix)]
#[test]
fn meta_read_gives_the_owners_execute_bit_on_unix() {
    use std::os::unix::fs::PermissionsExt;
    let to = temp("meta-unix");
    for (mode, executable) in [(0o644, false), (0o744, true), (0o655, false), (0o700, true)] {
        let path = to.join(format!("f{mode:o}"));
        std::fs::write(&path, b"x").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let meta = Meta::read(&File::open(&path).unwrap()).unwrap();
        assert_eq!(meta.executable, executable, "{mode:o}");
    }
    // A folder is searchable, not a program.
    let folder = File::open(&to).unwrap();
    assert!(!Meta::read(&folder).unwrap().executable);
    std::fs::remove_dir_all(&to).unwrap();
}

/// Makes `link` a file symbolic link to `target`; false where this computer
/// cannot (Windows without Developer Mode or an administrator).
fn link_file(target: &Path, link: &Path) -> bool {
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link).is_ok()
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
}

/// Whether a file symbolic link that cannot be made fails its test: CI sets
/// `SAFECOPY_REQUIRE_SYMLINKS` on Windows, whose runners are administrators.
fn symlinks_required() -> bool {
    std::env::var_os("SAFECOPY_REQUIRE_SYMLINKS").is_some_and(|v| !v.is_empty())
}

/// A candidate is exactly the name asked (not another case of it), a
/// regular file (not a folder nor a link) and exactly the size asked.
#[test]
fn a_candidate_needs_the_exact_name_a_regular_file_and_the_same_size() {
    let (to, outside) = (temp("candidate"), temp("candidate-outside"));
    std::fs::write(to.join("report.pdf"), b"12345").unwrap();
    std::fs::create_dir(to.join("folder")).unwrap();
    std::fs::write(outside.join("secret.pdf"), b"12345").unwrap();
    let dir = Directory::open(&to).unwrap();

    let found = dir.candidate(os("report.pdf"), 5).unwrap();
    assert_eq!(found.size(), 5);
    assert_eq!(found.name(), os("report.pdf"));
    assert!(found.unchanged());
    assert_eq!(
        identity(found.file()).unwrap(),
        identity(&File::open(to.join("report.pdf")).unwrap()).unwrap()
    );
    drop(found);

    assert!(dir.candidate(os("report.pdf"), 4).is_none(), "another size");
    assert!(dir.candidate(os("report.pdf"), 6).is_none(), "another size");
    // Opens on Windows and macOS, which ignore case; not the exact name.
    assert!(dir.candidate(os("Report.pdf"), 5).is_none(), "another case");
    assert!(dir.candidate(os("REPORT.PDF"), 5).is_none(), "another case");
    assert!(dir.candidate(os("missing.pdf"), 5).is_none());
    let folder_size = std::fs::metadata(to.join("folder")).unwrap().len();
    assert!(
        dir.candidate(os("folder"), folder_size).is_none(),
        "a folder"
    );
    assert!(dir.candidate(os("../candidate-outside"), 5).is_none());

    if link_file(&outside.join("secret.pdf"), &to.join("linked.pdf")) {
        assert!(dir.candidate(os("linked.pdf"), 5).is_none(), "a link");
    } else {
        assert!(
            !symlinks_required(),
            "SAFECOPY_REQUIRE_SYMLINKS is set, but no file symbolic link can be made"
        );
        eprintln!("skipped the link part: no file symbolic link can be made here");
    }
    if link_folder(&outside, &to.join("junction")) {
        assert!(dir.candidate(os("junction"), 0).is_none(), "a junction");
        remove_link(&to.join("junction"));
    } else {
        no_link_here(junctions_required(), "a junction as a candidate");
    }
    drop(dir);
    for path in [to, outside] {
        std::fs::remove_dir_all(path).unwrap();
    }
}

/// A candidate whose file changed since it was opened, in size, time or
/// the file its name leads to, is not unchanged.
#[test]
fn a_candidate_is_unchanged_only_while_its_name_size_and_time_are() {
    let to = temp("candidate-changed");
    let dir = Directory::open(&to).unwrap();
    let path = to.join("a.txt");
    let fresh = || {
        std::fs::write(&path, b"same").unwrap();
        dir.candidate(os("a.txt"), 4).unwrap()
    };

    let grown = fresh();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"+")
        .unwrap();
    assert!(!grown.unchanged(), "grown");
    drop(grown);

    let touched = fresh();
    let earlier = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(earlier)
        .unwrap();
    assert!(!touched.unchanged(), "another time");
    drop(touched);

    let replaced = fresh();
    std::fs::write(to.join("b.txt"), b"same").unwrap();
    // The candidate shares delete, so another file can take its name.
    std::fs::rename(to.join("b.txt"), &path).unwrap();
    assert!(!replaced.unchanged(), "replaced");
    drop(replaced);

    let gone = fresh();
    std::fs::remove_file(&path).unwrap();
    assert!(!gone.unchanged(), "gone");
    drop((gone, dir));
    std::fs::remove_dir_all(&to).unwrap();
}

/// Takes away the right to add files and folders to `path`; false where
/// this user keeps it anyway (root on Unix).
fn read_only(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: no arguments, no memory.
        if unsafe { libc::geteuid() } == 0 {
            return false;
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o555)).unwrap();
        true
    }
    #[cfg(windows)]
    {
        // Everyone denied adding a file (WD) or a folder (AD) here.
        let denied = std::process::Command::new("icacls")
            .arg(path)
            .args(["/deny", "*S-1-1-0:(WD,AD)"])
            .output()
            .unwrap();
        assert!(denied.status.success(), "{denied:?}");
        true
    }
}

fn writable_again(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(windows)]
    {
        let allowed = std::process::Command::new("icacls")
            .arg(path)
            .args(["/remove:d", "*S-1-1-0"])
            .output()
            .unwrap();
        assert!(allowed.status.success(), "{allowed:?}");
    }
}

/// `existing` opens a folder to continue into only by its exact name, only
/// when this user may add to it, and never through a link or junction.
#[test]
fn existing_opens_only_a_writable_folder_of_that_exact_name_and_never_a_link() {
    let (to, outside) = (temp("existing"), temp("existing-outside"));
    std::fs::create_dir(to.join("Photos")).unwrap();
    std::fs::write(to.join("Photos").join("a.jpg"), b"a").unwrap();
    std::fs::write(to.join("notes"), b"a file").unwrap();
    std::fs::create_dir(to.join("locked")).unwrap();
    let dir = Directory::open(&to).unwrap();

    let photos = dir.existing(os("Photos")).unwrap();
    assert_eq!(
        photos.identity().unwrap(),
        Directory::open(&to.join("Photos"))
            .unwrap()
            .identity()
            .unwrap()
    );
    assert_eq!(photos.path(), to.join("Photos"));
    // It can be added to; the file there is left as it is.
    let added = photos.create(os("b.jpg")).unwrap();
    photos.discard(&added, os("b.jpg")).unwrap();
    drop((added, photos));
    assert_eq!(files_in(&to.join("Photos")), ["a.jpg"]);

    assert!(dir.existing(os("photos")).is_none(), "another case");
    assert!(dir.existing(os("PHOTOS")).is_none(), "another case");
    assert!(dir.existing(os("notes")).is_none(), "a file");
    assert!(dir.existing(os("missing")).is_none());
    assert!(dir.existing(os("..")).is_none());

    if link_folder(&outside, &to.join("linked")) {
        assert!(dir.existing(os("linked")).is_none(), "a link");
        remove_link(&to.join("linked"));
    } else {
        no_link_here(junctions_required(), "a link as an existing folder");
    }

    if read_only(&to.join("locked")) {
        assert!(dir.existing(os("locked")).is_none(), "cannot be added to");
        writable_again(&to.join("locked"));
        assert!(dir.existing(os("locked")).is_some());
    } else {
        eprintln!("skipped the read-only part: this user may write anywhere");
    }
    drop(dir);
    for path in [to, outside] {
        std::fs::remove_dir_all(path).unwrap();
    }
}

/// "café" with its é as one character (NFC), and as an e and an accent
/// after it (NFD).
const NFC: &str = "caf\u{e9}";
const NFD: &str = "cafe\u{301}";

/// The one name in `dir`, as the disk stored it.
fn stored(dir: &Path) -> String {
    let names = files_in(dir);
    assert_eq!(names.len(), 1, "{names:?}");
    names.into_iter().next().unwrap()
}

/// A file or folder stored in one form of its characters is neither a
/// candidate nor an existing folder when asked for in the other: macOS
/// opens both forms, so the folder is read for the exact bytes there.
#[test]
fn another_form_of_the_characters_is_neither_a_candidate_nor_an_existing_folder() {
    for (made, other) in [(NFC, NFD), (NFD, NFC)] {
        let to = temp(&format!("forms-{}", made.len()));
        std::fs::write(to.join(made), b"12345").unwrap();
        let dir = Directory::open(&to).unwrap();
        // A disk that stores one form only (HFS+) has the other: ask by what it holds.
        let name = stored(&to);
        let other = if name == made { other } else { made };
        assert!(dir.candidate(os(&name), 5).is_some(), "{name:?}");
        assert!(dir.candidate(os(other), 5).is_none(), "{other:?}");
        drop(dir);
        std::fs::remove_dir_all(&to).unwrap();

        let to = temp(&format!("forms-folder-{}", made.len()));
        std::fs::create_dir(to.join(made)).unwrap();
        let dir = Directory::open(&to).unwrap();
        let name = stored(&to);
        let other = if name == made { other } else { made };
        assert!(dir.existing(os(&name)).is_some(), "{name:?}");
        assert!(dir.existing(os(other)).is_none(), "{other:?}");
        drop(dir);
        std::fs::remove_dir_all(&to).unwrap();
    }
}

/// Where the disk makes the file be copied into its name, the copy gets
/// the owner's execute bit (no group or other execute bit) and, with
/// `sync`, is flushed to the disk; a linked or renamed file is the
/// caller's to flush. A copy whose time and bit cannot be applied is
/// removed, and the partial file is left for the caller.
#[tokio::test]
async fn the_copy_fallback_gets_the_bit_is_flushed_and_is_removed_when_apply_fails() {
    let allowed = MetaRules {
        times: true,
        executable: true,
    };
    for (copied, sync) in [(true, true), (true, false), (false, true)] {
        let to = temp(&format!("copy-bit-{copied}-{sync}"));
        let dir = Directory::open(&to).unwrap();
        let file = dir.create(os("run.part")).unwrap();
        std::io::Write::write_all(&mut &file, b"#!").unwrap();
        NO_HARD_LINKS.set(copied);
        NO_RENAME.set(copied);
        crate::seams::SYNCS.set(0);
        let published = dir
            .publish_with(
                &file,
                os("run.part"),
                "run",
                &PublishRules::default(),
                &Finish {
                    sync,
                    ..finish(true, allowed)
                },
                std::future::pending(),
            )
            .await;
        NO_HARD_LINKS.set(false);
        NO_RENAME.set(false);
        let (published, applied) = published.unwrap();
        assert_eq!(
            crate::seams::SYNCS.get(),
            u32::from(copied && sync),
            "copied: {copied}, sync: {sync}"
        );
        drop((file, dir));
        assert_eq!(files_in(&to), ["run"]);
        assert_eq!(std::fs::read(&published.path).unwrap(), b"#!");
        assert_eq!(modified(&published.path), SENT);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(applied.executable, BitApplied::Set);
            let mode = std::fs::metadata(&published.path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o100, "{mode:o}");
        }
        #[cfg(windows)]
        assert_eq!(applied.executable, BitApplied::NotHere);
        std::fs::remove_dir_all(&to).unwrap();
    }

    let to = temp("copy-apply-fails");
    let dir = Directory::open(&to).unwrap();
    let file = dir.create(os("run.part")).unwrap();
    std::io::Write::write_all(&mut &file, b"#!").unwrap();
    NO_HARD_LINKS.set(true);
    NO_RENAME.set(true);
    // The partial's apply goes through; the copy's fails.
    crate::seams::APPLY_ERRORS.with_borrow_mut(|errors| errors.extend([None, Some(5)]));
    let failed = dir
        .publish_with(
            &file,
            os("run.part"),
            "run",
            &PublishRules::default(),
            &finish(true, allowed),
            std::future::pending(),
        )
        .await;
    NO_HARD_LINKS.set(false);
    NO_RENAME.set(false);
    assert!(crate::seams::APPLY_ERRORS.with_borrow(|errors| errors.is_empty()));
    assert!(matches!(failed, Err(PublishError::Io(_))), "{failed:?}");
    assert_eq!(files_in(&to), ["run.part"]);
    dir.discard(&file, os("run.part")).unwrap();
    drop((file, dir));
    std::fs::remove_dir_all(&to).unwrap();
}
