use super::*;
use crate::seams::{FINISH_ERRORS, NO_HARD_LINKS, NO_RENAME};
use std::collections::VecDeque;
use std::io::Write;
use std::time::Duration;

/// A new, empty folder for one test, under the build's target folder.
pub(crate) fn temp(name: &str) -> PathBuf {
    // The test binary is in target/<profile>/deps.
    let exe = std::env::current_exe().unwrap();
    let target = exe.ancestors().nth(3).unwrap();
    let dir = target
        .join("test-tmp")
        .join(format!("folder-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub(crate) fn os(name: &str) -> &OsStr {
    OsStr::new(name)
}

/// Makes `link` a junction (Windows) or symbolic link to the folder
/// `target`; false where this computer cannot.
pub(crate) fn link_folder(target: &Path, link: &Path) -> bool {
    #[cfg(windows)]
    {
        // mklink takes backslashes only.
        let link: PathBuf = link.components().collect();
        let target: PathBuf = target.components().collect();
        let made = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output();
        matches!(made, Ok(output) if output.status.success() && link.exists())
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
}

pub(crate) fn remove_link(path: &Path) {
    #[cfg(windows)]
    std::fs::remove_dir(path).unwrap();
    #[cfg(unix)]
    std::fs::remove_file(path).unwrap();
}

/// Whether a link that cannot be made fails its test: CI sets
/// `SAFECOPY_REQUIRE_JUNCTIONS` where it must make them.
pub(crate) fn junctions_required() -> bool {
    std::env::var_os("SAFECOPY_REQUIRE_JUNCTIONS").is_some_and(|v| !v.is_empty())
}

/// Reports a part that needs a link this computer cannot make: a failure
/// where links are required, otherwise a skip that says so.
pub(crate) fn no_link_here(required: bool, part: &str) {
    assert!(
        !required,
        "SAFECOPY_REQUIRE_JUNCTIONS is set, but no junction or link can be made for {part}"
    );
    eprintln!("skipped {part}: no junction or link can be made here");
}

#[test]
#[should_panic(expected = "SAFECOPY_REQUIRE_JUNCTIONS is set")]
fn a_missing_junction_fails_where_junctions_are_required() {
    no_link_here(true, "the junction part");
}

#[test]
fn a_missing_junction_is_skipped_where_not_required() {
    no_link_here(false, "the junction part");
}

pub(crate) fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[cfg(windows)]
fn zone(path: &Path) -> Option<String> {
    let mut zone = path.as_os_str().to_os_string();
    zone.push(":Zone.Identifier");
    std::fs::read_to_string(zone).ok()
}

#[test]
fn only_a_sure_sign_says_a_folder_was_removed() {
    let here = || Ok(PathBuf::from("Work"));
    assert!(!removed_from(false, here()));
    assert!(removed_from(true, here()));
    let recycled: PathBuf = ["$Recycle.Bin", "S-1-5-21", "$R0K2J1X"].iter().collect();
    assert!(removed_from(false, Ok(recycled)));
    assert!(in_recycle_bin(Path::new("disk/$recycle.bin/x")));
    assert!(!in_recycle_bin(Path::new("disk/Recycle/x")));
    assert!(removed_from(false, Err(io::ErrorKind::NotFound.into())));
    // A file system that cannot say where the folder is says nothing.
    for unknown in [
        io::Error::from(io::ErrorKind::Unsupported),
        io::Error::from_raw_os_error(1),
        io::Error::from_raw_os_error(50),
        io::Error::from(io::ErrorKind::PermissionDenied),
    ] {
        assert!(!removed_from(false, Err(unknown)));
    }
}

#[test]
fn a_taken_folder_name_gets_a_number() {
    let dir = temp("claim");
    let held = Directory::destination(&dir).unwrap();
    let claim = |name: &str| held.claim(name).unwrap().path().to_path_buf();
    assert_eq!(claim("Photos"), dir.join("Photos"));
    assert_eq!(claim("Photos"), dir.join("Photos (2)"));
    assert_eq!(claim("Photos"), dir.join("Photos (3)"));
    // A file of that name takes it too.
    std::fs::write(dir.join("Notes"), "a file").unwrap();
    assert_eq!(claim("Notes"), dir.join("Notes (2)"));
    // A folder's name has no extension to keep last.
    assert_eq!(claim("v1.2"), dir.join("v1.2"));
    assert_eq!(claim("v1.2"), dir.join("v1.2 (2)"));
    assert!(dir.join("Photos (3)").is_dir());
    assert!(held.claim("a/b").is_err());
    drop(held);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_taken_name_gets_a_number() {
    let dir = temp("unique");
    assert_eq!(unique(&dir, "a.txt"), dir.join("a.txt"));
    std::fs::write(dir.join("a.txt"), "1").unwrap();
    assert_eq!(unique(&dir, "a.txt"), dir.join("a (2).txt"));
    std::fs::write(dir.join("a (2).txt.part"), "").unwrap();
    assert_eq!(unique(&dir, "a.txt"), dir.join("a (3).txt"));
    assert_eq!(unique(&dir, ".hidden"), dir.join(".hidden"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_name_must_be_one_part_of_the_folder() {
    let dir = temp("one-part");
    let held = Directory::open(&dir).unwrap();
    for name in ["", ".", "..", "a/b", "../x"] {
        let e = held.create(os(name)).unwrap_err();
        assert_eq!(refusal(&e), Some(Refusal::LeavesFolder), "{name:?}");
    }
    #[cfg(windows)]
    {
        // "a:" alone reads as a drive; "ab:" as a stream.
        let e = held.create(os("ab:stream")).unwrap_err();
        assert_eq!(refusal(&e), Some(Refusal::Stream));
        assert_eq!(
            refusal(&held.create(os("a\\b")).unwrap_err()),
            Some(Refusal::LeavesFolder)
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let e = held.create(OsStr::from_bytes(b"a\0b")).unwrap_err();
        assert_eq!(refusal(&e), Some(Refusal::Nul));
    }
    #[cfg(windows)]
    {
        // More than one system call can carry (32,767 UTF-16 units).
        let e = held.create(os(&"x".repeat(40_000))).unwrap_err();
        assert_eq!(refusal(&e), Some(Refusal::TooLong));
        let e = held.create(os("a\0b")).unwrap_err();
        assert_eq!(refusal(&e), Some(Refusal::Nul));
    }
    assert!(files_in(&dir).is_empty());
    drop(held);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Linux removes the staging name after linking, so `finish` refuses a
/// staging name that could reach outside the folder, before it touches
/// anything.
#[test]
fn a_finish_refuses_a_staging_name_that_is_not_one_part() {
    let base = temp("finish-partial");
    std::fs::create_dir_all(base.join("in/a")).unwrap();
    std::fs::write(base.join("x"), b"outside").unwrap();
    std::fs::write(base.join("in/a/b"), b"below").unwrap();
    let dir = Directory::open(&base.join("in")).unwrap();
    let file = partial(&dir, "a.part", b"ours");
    let absolute = base.join("x");
    for staging in [os("../x"), os("a/b"), absolute.as_os_str()] {
        let e = dir.finish(&file, staging, os("a.txt")).unwrap_err();
        assert_eq!(refusal(&e), Some(Refusal::LeavesFolder), "{staging:?}");
    }
    assert_eq!(std::fs::read(base.join("x")).unwrap(), b"outside");
    assert_eq!(std::fs::read(base.join("in/a/b")).unwrap(), b"below");
    assert_eq!(files_in(&base.join("in")), ["a", "a.part"]);
    drop((file, dir));
    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn a_file_is_created_only_new_and_opened_again_by_name() {
    let dir = temp("create");
    let held = Directory::open(&dir).unwrap();
    let mut file = held.create(os("a.txt")).unwrap();
    file.write_all(b"one").unwrap();
    let again = held.create(os("a.txt")).unwrap_err();
    assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"one");
    let opened = held.entry(os("a.txt")).unwrap();
    assert_eq!(identity(&opened).unwrap(), identity(&file).unwrap());
    assert!(
        held.inspect(os("a.txt"))
            .unwrap()
            .metadata()
            .unwrap()
            .is_file()
    );
    let sub = held.claim("sub").unwrap();
    assert!(held.entry(os("sub")).unwrap().metadata().unwrap().is_dir());
    assert_ne!(sub.identity().unwrap(), held.identity().unwrap());
    let e = Directory::from_file(opened, dir.join("a.txt")).unwrap_err();
    assert_eq!(refusal(&e), Some(Refusal::NotFolder));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = file.metadata().unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0, "{mode:o}");
    }
    drop((file, sub, held));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn names_are_listed_up_to_the_limit() {
    let dir = temp("names");
    for name in ["a", "b", "c"] {
        std::fs::write(dir.join(name), name).unwrap();
    }
    std::fs::create_dir(dir.join("d")).unwrap();
    let held = Directory::open(&dir).unwrap();
    let mut all: Vec<OsString> = held
        .names(100)
        .unwrap()
        .into_iter()
        .map(Result::unwrap)
        .collect();
    all.sort();
    assert_eq!(all, ["a", "b", "c", "d"]);
    assert_eq!(held.names(2).unwrap().len(), 2);
    // Listing twice from one handle starts over each time.
    assert_eq!(held.names(100).unwrap().len(), 4);
    drop(held);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_held_folder_and_its_identity_and_place() {
    let dir = temp("identity");
    let held = Directory::writable(&dir, true).unwrap();
    let again = held.try_clone().unwrap();
    assert_eq!(held.identity().unwrap(), again.identity().unwrap());
    assert_eq!(
        held.identity().unwrap(),
        Directory::open(&dir).unwrap().identity().unwrap()
    );
    assert_eq!(held.path(), dir);
    assert!(!held.removed());
    assert!(held.unfit::<&Path>(&[]).is_none());
    drop((held, again));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The folder's place is read from its handle, so a rename is followed
/// and a deletion is seen (macOS keeps the path it was opened at).
#[cfg(any(target_os = "linux", target_os = "android", windows))]
#[test]
fn a_held_folder_follows_a_rename_and_knows_when_it_is_gone() {
    let dir = temp("follows");
    std::fs::create_dir(dir.join("before")).unwrap();
    let held = Directory::open(&dir.join("before")).unwrap();
    std::fs::rename(dir.join("before"), dir.join("after")).unwrap();
    assert_eq!(
        held.final_path().unwrap().file_name(),
        Some(os("after")),
        "{:?}",
        held.final_path()
    );
    assert!(!held.removed());
    std::fs::remove_dir(dir.join("after")).unwrap();
    assert!(held.removed());
    drop(held);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_kept_out_folder_and_the_folders_inside_it_are_unfit() {
    let dir = temp("kept-out");
    let kept = dir.join("kept");
    std::fs::create_dir_all(kept.join("inner")).unwrap();
    std::fs::create_dir(dir.join("other")).unwrap();
    let open = |path: &Path| Directory::writable(path, true).unwrap();
    assert_eq!(open(&kept).unfit(&[&kept]), Some(Unfit::KeptOut));
    assert_eq!(
        open(&kept.join("inner")).unfit(&[&kept]),
        Some(Unfit::KeptOut)
    );
    assert_eq!(open(&dir.join("other")).unfit(&[&kept]), None);
    // A kept-out folder not made yet holds nothing; an empty path holds everything.
    assert_eq!(open(&dir.join("other")).unfit(&[dir.join("missing")]), None);
    assert_eq!(
        open(&dir.join("other")).unfit(&[PathBuf::new()]),
        Some(Unfit::KeptOut)
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

/// No link or junction below a held folder is followed, whichever way it
/// is opened; one at the top is followed only when asked.
#[test]
fn a_link_or_junction_is_never_followed() {
    let (dir, outside) = (temp("links"), temp("links-outside"));
    std::fs::write(outside.join("secret.txt"), b"outside").unwrap();
    let link = dir.join("link");
    if !link_folder(&outside, &link) {
        no_link_here(junctions_required(), "a link below a held folder");
        for path in [dir, outside] {
            std::fs::remove_dir_all(path).unwrap();
        }
        return;
    }
    let held = Directory::open(&dir).unwrap();
    assert!(is_link(&held.directory(os("link")).unwrap_err()));
    assert!(is_link(&held.entry(os("link")).unwrap_err()));
    assert!(is_link(&held.inspect(os("link")).unwrap_err()));
    assert!(is_link(&Directory::open(&link).unwrap_err()));
    assert_ne!(
        held.create(os("link")).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    // Followed only when the caller asks for it.
    let followed = Directory::followed(&link).unwrap();
    assert!(followed.entry(os("secret.txt")).is_ok());
    assert_eq!(files_in(&outside), ["secret.txt"]);
    drop((held, followed));
    remove_link(&link);
    for path in [dir, outside] {
        std::fs::remove_dir_all(path).unwrap();
    }
}

/// A symbolic link to a file is refused, and never written through.
#[cfg(unix)]
#[test]
fn a_link_to_a_file_is_never_followed() {
    let (dir, outside) = (temp("file-link"), temp("file-link-outside"));
    std::fs::write(outside.join("a.txt"), b"outside").unwrap();
    std::os::unix::fs::symlink(outside.join("a.txt"), dir.join("a.txt")).unwrap();
    let held = Directory::open(&dir).unwrap();
    assert!(is_link(&Directory::open(&dir.join("a.txt")).unwrap_err()));
    assert!(is_link(&held.entry(os("a.txt")).unwrap_err()));
    assert!(is_link(&held.inspect(os("a.txt")).unwrap_err()));
    assert_eq!(
        held.create(os("a.txt")).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    // Removing it removes the link, not the file it points to.
    held.remove(os("a.txt")).unwrap();
    assert_eq!(std::fs::read(outside.join("a.txt")).unwrap(), b"outside");
    assert!(files_in(&dir).is_empty());
    drop(held);
    for path in [dir, outside] {
        std::fs::remove_dir_all(path).unwrap();
    }
}

/// A pipe is neither a file nor a folder: refused, never opened to read.
#[cfg(unix)]
#[test]
fn a_special_file_is_never_opened() {
    let dir = temp("special");
    let fifo = std::ffi::CString::new(dir.join("pipe").as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a valid C string.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let held = Directory::open(&dir).unwrap();
    for refused in [held.entry(os("pipe")), held.inspect(os("pipe"))] {
        assert_eq!(
            refusal(&refused.unwrap_err()),
            Some(Refusal::NotFileOrFolder)
        );
    }
    drop(held);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A destination that is itself a link (a redirected folder) is followed;
/// a link put inside it later is not.
#[test]
fn a_linked_destination_is_followed_but_links_inside_it_are_not() {
    let (to, actual, outside) = (
        temp("redirected"),
        temp("redirected-actual"),
        temp("redirected-outside"),
    );
    let destination = to.join("Received");
    if !link_folder(&actual, &destination) {
        no_link_here(junctions_required(), "a linked destination");
        for path in [to, actual, outside] {
            std::fs::remove_dir_all(path).unwrap();
        }
        return;
    }
    let held = Directory::destination(&destination).unwrap();
    let claimed = held.claim("Photos").unwrap();
    assert!(actual.join("Photos").is_dir());
    let sub = claimed.claim("sub").unwrap();
    sub.create(os("first.txt")).unwrap();
    drop(sub);
    assert!(actual.join("Photos/sub/first.txt").is_file());
    let link = actual.join("Photos/sub");
    std::fs::rename(&link, actual.join("Photos/kept")).unwrap();
    assert!(link_folder(&outside, &link));
    assert!(is_link(&claimed.directory(os("sub")).unwrap_err()));
    assert!(files_in(&outside).is_empty());
    drop((held, claimed));
    remove_link(&link);
    remove_link(&destination);
    for path in [to, actual, outside] {
        std::fs::remove_dir_all(path).unwrap();
    }
}

/// A folder replaced by a link between two entries is refused when it is
/// opened again by name, and nothing is written outside; a folder held
/// open goes on writing where it really is.
#[test]
fn a_destination_directory_replaced_by_a_link_between_entries_does_not_write_outside() {
    for replace_root in [false, true] {
        let label = if replace_root {
            "swap-root"
        } else {
            "swap-child"
        };
        let (to, outside) = (temp(label), temp(&format!("{label}-outside")));
        std::fs::write(outside.join("second.txt"), b"untouched").unwrap();
        let held = Directory::destination(&to).unwrap();
        let photos = held.claim("Photos").unwrap();
        // Windows renames no folder with a handle open inside it.
        drop(
            photos
                .claim("sub")
                .unwrap()
                .create(os("first.txt"))
                .unwrap(),
        );
        let replaced = if replace_root {
            to.join("Photos")
        } else {
            to.join("Photos/sub")
        };
        std::fs::rename(&replaced, to.join("original")).unwrap();
        if !link_folder(&outside, &replaced) {
            no_link_here(
                junctions_required(),
                "a destination folder replaced between entries",
            );
            drop((held, photos));
            for dir in [to, outside] {
                std::fs::remove_dir_all(dir).unwrap();
            }
            continue;
        }
        if replace_root {
            assert!(is_link(&held.directory(os("Photos")).unwrap_err()));
            // The held handle still writes where it was, not through the link.
            photos.create(os("second.txt")).unwrap();
            assert!(to.join("original/second.txt").is_file());
        } else {
            assert!(is_link(&photos.directory(os("sub")).unwrap_err()));
            assert!(to.join("original/first.txt").is_file());
        }
        assert_eq!(
            std::fs::read(outside.join("second.txt")).unwrap(),
            b"untouched"
        );
        assert_eq!(files_in(&outside), ["second.txt"]);
        drop((held, photos));
        remove_link(&replaced);
        for dir in [to, outside] {
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}

/// `inside` bytes written to a new partial file of `dir`.
fn partial(dir: &Directory, name: &str, inside: &[u8]) -> File {
    let mut file = dir.create(os(name)).unwrap();
    file.write_all(inside).unwrap();
    file
}

#[tokio::test(start_paused = true)]
async fn finishing_in_a_folder_retries_scanners_at_most_five_times() {
    #[cfg(windows)]
    let errors = [5, 32];
    #[cfg(unix)]
    let errors = [libc::EACCES];
    for error in errors {
        for failures in [5, 6] {
            let to = temp(&format!("finish-retry-{error}-{failures}"));
            let dir = Directory::open(&to).unwrap();
            let file = dir.create(os("a.part")).unwrap();
            FINISH_ERRORS.with_borrow_mut(|errors| errors.extend(vec![error; failures]));
            let before = tokio::time::Instant::now();
            let rules = PublishRules::default();
            let result = dir
                .publish(&file, os("a.part"), "a.txt", &rules, std::future::pending())
                .await;
            assert_eq!(before.elapsed(), Duration::from_millis(250));
            // On Unix, EACCES after the last try also means procfs is denied: it falls back.
            let fallback = cfg!(unix) && error != 32;
            assert_eq!(
                result.is_ok(),
                failures == 5 || fallback,
                "{error} {failures}"
            );
            assert!(FINISH_ERRORS.with_borrow(|errors| errors.is_empty()));
            if result.is_err() {
                dir.discard(&file, os("a.part")).unwrap();
            }
            drop(file);
            assert!(!to.join("a.part").exists());
            drop(dir);
            std::fs::remove_dir_all(to).unwrap();
        }
    }
}

/// Linking that fails for the folder (here procfs denied) is found once:
/// the next name, and the next file, go straight to the rename without
/// trying to link or waiting again.
#[tokio::test(start_paused = true)]
async fn a_folder_that_cannot_link_is_found_once_per_handle() {
    // Unix waits out the retries once, since EACCES may be a passing hold.
    #[cfg(unix)]
    let (denied, tries, wait) = (libc::EACCES, 6, 250);
    #[cfg(windows)]
    let (denied, tries, wait) = (50, 1, 0);
    let to = temp("no-link-cached");
    let dir = Directory::open(&to).unwrap();
    std::fs::write(to.join("a.txt"), b"taken").unwrap();
    let file = partial(&dir, "a.part", b"a");
    FINISH_ERRORS.with_borrow_mut(|errors| errors.extend(vec![denied; 3 * tries]));
    let before = tokio::time::Instant::now();
    let rules = PublishRules::default();
    let published = dir
        .publish(&file, os("a.part"), "a.txt", &rules, std::future::pending())
        .await;
    assert_eq!(published.unwrap().path, to.join("a (2).txt"));
    assert_eq!(before.elapsed(), Duration::from_millis(wait));
    // Only the first name tried to link; "a (2).txt" went straight on.
    assert_eq!(FINISH_ERRORS.with_borrow(VecDeque::len), 2 * tries);
    let second = partial(&dir, "b.part", b"b");
    let again = dir
        .publish(
            &second,
            os("b.part"),
            "b.txt",
            &rules,
            std::future::pending(),
        )
        .await;
    assert_eq!(before.elapsed(), Duration::from_millis(wait));
    assert_eq!(
        FINISH_ERRORS.with_borrow_mut(std::mem::take).len(),
        2 * tries
    );
    // A second handle to the folder knows it too.
    let clone = dir.try_clone().unwrap();
    let unsupported = clone.finish(&second, os("b.part"), os("c.txt"));
    assert_eq!(unsupported.unwrap_err().kind(), io::ErrorKind::Unsupported);
    assert_eq!(again.unwrap().path, to.join("b.txt"));
    drop((file, second, dir, clone));
    std::fs::remove_dir_all(to).unwrap();
}

/// A link failure that belongs to one file, not the folder, is not
/// remembered: the next file still links.
#[tokio::test(start_paused = true)]
async fn a_link_failure_of_one_file_does_not_stop_linking_in_the_folder() {
    let to = temp("no-link-one-file");
    let dir = Directory::open(&to).unwrap();
    let file = partial(&dir, "a.part", b"a");
    // Unix: the partial deleted, so procfs has nothing to link (ENOENT).
    // Windows: ERROR_NOT_SAME_DEVICE, a file on another volume.
    #[cfg(unix)]
    let error = {
        std::fs::remove_file(to.join("a.part")).unwrap();
        libc::ENOENT
    };
    #[cfg(windows)]
    let error = 17;
    FINISH_ERRORS.with_borrow_mut(|errors| errors.push_back(error));
    let rules = PublishRules::default();
    let published = dir
        .publish(&file, os("a.part"), "a.txt", &rules, std::future::pending())
        .await;
    assert_eq!(published.unwrap().path, to.join("a.txt"));
    assert!(!dir.no_link.load(std::sync::atomic::Ordering::Relaxed));
    drop((file, dir));
    std::fs::remove_dir_all(to).unwrap();
}

/// 32 is Windows' "file in use"; on Unix it is EPIPE, a real failure that
/// is reported at once.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn on_unix_a_broken_pipe_is_not_taken_for_a_held_file() {
    let to = temp("epipe-not-held");
    let dir = Directory::open(&to).unwrap();
    let file = partial(&dir, "a.part", b"a");
    FINISH_ERRORS.with_borrow_mut(|errors| errors.extend([libc::EPIPE; 6]));
    let before = tokio::time::Instant::now();
    let result = dir
        .publish(
            &file,
            os("a.part"),
            "a.txt",
            &PublishRules::default(),
            std::future::pending(),
        )
        .await;
    let Err(PublishError::Name { error, .. }) = result else {
        panic!("{result:?}");
    };
    assert_eq!(error.raw_os_error(), Some(libc::EPIPE));
    assert_eq!(before.elapsed(), Duration::ZERO);
    assert_eq!(FINISH_ERRORS.with_borrow_mut(std::mem::take).len(), 5);
    dir.discard(&file, os("a.part")).unwrap();
    drop((file, dir));
    std::fs::remove_dir_all(to).unwrap();
}

/// A cancel while waiting for a held file to be let go ends the publish
/// then, without waiting for the other tries.
#[tokio::test(start_paused = true)]
async fn a_cancel_during_a_retry_ends_the_publish_at_once() {
    let to = temp("cancel-retry");
    let dir = Directory::open(&to).unwrap();
    let file = partial(&dir, "a.part", b"a");
    #[cfg(windows)]
    let held = 32;
    #[cfg(unix)]
    let held = libc::EACCES;
    FINISH_ERRORS.with_borrow_mut(|errors| errors.extend([held; 5]));
    let before = tokio::time::Instant::now();
    let cancel = tokio::time::sleep(Duration::from_millis(75));
    let result = dir
        .publish(
            &file,
            os("a.part"),
            "a.txt",
            &PublishRules::default(),
            cancel,
        )
        .await;
    assert!(matches!(result, Err(PublishError::Cancelled)), "{result:?}");
    assert_eq!(before.elapsed(), Duration::from_millis(75));
    assert_eq!(FINISH_ERRORS.with_borrow_mut(std::mem::take).len(), 3);
    assert_eq!(files_in(&to), ["a.part"]);
    dir.discard(&file, os("a.part")).unwrap();
    drop((file, dir));
    std::fs::remove_dir_all(to).unwrap();
}

#[tokio::test]
async fn a_cancel_during_a_folder_copy_fallback_removes_both_open_files() {
    use std::task::Poll;
    let to = temp("folder-copy-cancel");
    let dir = Directory::open(&to).unwrap();
    let file = partial(&dir, "a.txt.part", b"new");
    let cancel = std::future::poll_fn(|_| {
        if to.join("a.txt").exists() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    });
    NO_HARD_LINKS.set(true);
    NO_RENAME.set(true);
    let rules = PublishRules::default();
    let result = dir
        .publish(&file, os("a.txt.part"), "a.txt", &rules, cancel)
        .await;
    NO_RENAME.set(false);
    NO_HARD_LINKS.set(false);
    assert!(matches!(result, Err(PublishError::Cancelled)), "{result:?}");
    // The copy is gone; the partial is the caller's to discard.
    assert_eq!(files_in(&to), ["a.txt.part"]);
    dir.discard(&file, os("a.txt.part")).unwrap();
    drop(file);
    assert!(files_in(&to).is_empty());
    drop(dir);
    std::fs::remove_dir_all(to).unwrap();
}

/// The caller checked the file's bytes before publishing, and may have
/// given a clone of the file away (a save's hooks): where the file is
/// copied into its name, that clone moved mid-copy changes no byte.
#[tokio::test]
async fn a_clone_moved_during_a_folder_copy_fallback_changes_no_byte() {
    use std::io::{Seek, SeekFrom};
    let to = temp("folder-copy-moved");
    let dir = Directory::open(&to).unwrap();
    // Several slices, no two alike.
    let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let file = partial(&dir, "a.bin.part", &bytes);
    let mut clone = file.try_clone().unwrap();
    // Before the first slice nothing; before the second, the clone is
    // moved back into what was copied already.
    crate::seams::on("copy slice", || {});
    crate::seams::on("copy slice", move || {
        clone.seek(SeekFrom::Start(7)).unwrap();
    });
    NO_HARD_LINKS.set(true);
    NO_RENAME.set(true);
    let rules = PublishRules::default();
    let published = dir
        .publish(
            &file,
            os("a.bin.part"),
            "a.bin",
            &rules,
            std::future::pending(),
        )
        .await;
    NO_RENAME.set(false);
    NO_HARD_LINKS.set(false);
    published.unwrap();
    assert!(std::fs::read(to.join("a.bin")).unwrap() == bytes);
    // Windows lets the partial's name go with its last handle.
    drop(file);
    assert_eq!(files_in(&to), ["a.bin"]);
    drop(dir);
    std::fs::remove_dir_all(to).unwrap();
}

/// Lets a held slice go when the test ends, however it ends.
struct Release(std::sync::Arc<std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.lock().unwrap().take();
    }
}

/// Hears every step of the slices of this thread's next copy into a name,
/// and holds each slice at `held` until the test lets go or the copy's
/// claim is dropped.
fn hold_slices_at(
    held: crate::seams::Step,
) -> (std::sync::mpsc::Receiver<crate::seams::Step>, Release) {
    use crate::seams::Step;
    use std::sync::{Arc, Mutex, mpsc};
    let (heard, steps) = mpsc::channel();
    let (release, go) = mpsc::channel::<()>();
    let go = Mutex::new(go);
    let release = Arc::new(Mutex::new(Some(release)));
    let guard = Release(release.clone());
    crate::seams::COPIES.set(Some(Arc::new(move |step| {
        let _ = heard.send(step);
        if step == held {
            let _ = go.lock().unwrap().recv();
        } else if step == Step::Closing && held == Step::Copying {
            // Let go only once the drop is at the gate.
            drop(release.lock().unwrap().take());
        }
    })));
    (steps, guard)
}

/// A publish copying `a.txt.part` into `a.txt`, polled once: the first
/// slice is handed to the blocking pool, and the publish waits for it.
macro_rules! copying_into_its_name {
    ($to:expr, $dir:ident, $file:ident, $publishing:ident) => {
        let $dir = Directory::open($to).unwrap();
        let $file = partial(&$dir, "a.txt.part", &vec![7; 1 << 20]);
        NO_HARD_LINKS.set(true);
        NO_RENAME.set(true);
        let rules = PublishRules::default();
        let finish = Finish::default();
        // Never cancelled: only the drop ends it.
        let mut $publishing = Box::pin($dir.publish_with(
            &$file,
            os("a.txt.part"),
            "a.txt",
            &rules,
            &finish,
            std::future::pending(),
        ));
        let polled =
            std::future::poll_fn(|cx| std::task::Poll::Ready($publishing.as_mut().poll(cx))).await;
        NO_RENAME.set(false);
        NO_HARD_LINKS.set(false);
        assert!(polled.is_pending(), "its first slice is still to be copied");
    };
}

/// A slice of the copy is under way on the blocking pool, where it holds
/// the file and the copy open, when the publish is dropped. The drop
/// waits for it: when it returns, the copy is gone and nothing holds the
/// partial.
#[tokio::test]
async fn a_publish_dropped_during_a_folder_copy_fallback_leaves_no_copy() {
    use crate::seams::Step;
    let to = temp("folder-copy-drop");
    let (steps, _release) = hold_slices_at(Step::Copying);
    copying_into_its_name!(&to, dir, file, publishing);
    assert_eq!(steps.recv().unwrap(), Step::Queued);
    assert_eq!(steps.recv().unwrap(), Step::Copying);
    assert_eq!(files_in(&to), ["a.txt", "a.txt.part"]);
    drop(publishing);
    // Read at once, with no wait for the pool: the slice had ended.
    let heard: Vec<Step> = steps.try_iter().collect();
    assert_eq!(heard, [Step::Closing, Step::Ended]);
    assert_eq!(files_in(&to), ["a.txt.part"]);
    // The partial is the caller's again: no slice holds it open.
    dir.discard(&file, os("a.txt.part")).unwrap();
    drop(file);
    assert_eq!(files_in(&to), [""; 0]);
    drop(dir);
    std::fs::remove_dir_all(to).unwrap();
}

/// A slice that had not started when its publish was dropped copies
/// nothing, however late it runs, and holds neither file open meanwhile.
#[tokio::test]
async fn a_slice_still_waiting_when_its_publish_is_dropped_copies_nothing() {
    use crate::seams::Step;
    let to = temp("folder-copy-drop-waiting");
    let (steps, release) = hold_slices_at(Step::Queued);
    copying_into_its_name!(&to, dir, file, publishing);
    assert_eq!(steps.recv().unwrap(), Step::Queued);
    drop(publishing);
    assert_eq!(steps.recv().unwrap(), Step::Closing);
    // The slice is still held on the pool, and the folder is as it was.
    assert_eq!(files_in(&to), ["a.txt.part"]);
    dir.discard(&file, os("a.txt.part")).unwrap();
    drop(file);
    assert_eq!(files_in(&to), [""; 0]);
    drop(release);
    assert_eq!(steps.recv().unwrap(), Step::Ended, "it never copied");
    assert!(steps.try_recv().is_err());
    drop(dir);
    std::fs::remove_dir_all(to).unwrap();
}

#[tokio::test]
async fn a_cancel_before_publishing_names_nothing() {
    let to = temp("cancel-first");
    let dir = Directory::open(&to).unwrap();
    let file = partial(&dir, "a.part", b"a");
    let rules = PublishRules::default();
    let result = dir
        .publish(&file, os("a.part"), "a.txt", &rules, std::future::ready(()))
        .await;
    assert!(matches!(result, Err(PublishError::Cancelled)));
    assert_eq!(files_in(&to), ["a.part"]);
    dir.discard(&file, os("a.part")).unwrap();
    drop((file, dir));
    std::fs::remove_dir_all(to).unwrap();
}

#[test]
fn a_cut_short_file_in_a_folder_discards_its_open_staging_file() {
    let to = temp("discard");
    let dir = Directory::open(&to).unwrap();
    let file = partial(&dir, "a.txt.part", b"new");
    dir.discard(&file, os("a.txt.part")).unwrap();
    drop(file);
    assert!(files_in(&to).is_empty());
    drop(dir);
    std::fs::remove_dir_all(to).unwrap();
}

/// Every way a publish can claim a name: by handle, by a no-replace rename,
/// and by copying into a name created new.
#[derive(Debug, Clone, Copy)]
enum Route {
    Native,
    Rename,
    Copy,
}

const ROUTES: [Route; 3] = [Route::Native, Route::Rename, Route::Copy];

impl Route {
    fn set(self, on: bool) {
        NO_HARD_LINKS.set(on && !matches!(self, Route::Native));
        NO_RENAME.set(on && matches!(self, Route::Copy));
    }
}

/// B was given `same.txt` and is still arriving when A finishes under it:
/// B steps aside to the next name and never replaces A.
#[tokio::test]
async fn a_name_taken_while_receiving_is_not_overwritten() {
    for route in ROUTES {
        let to = temp(&format!("taken-{route:?}"));
        let dir = Directory::open(&to).unwrap();
        let file = partial(&dir, "same.txt.part", b"bbb");
        // A finishes under the name B was given.
        std::fs::write(to.join("same.txt"), "aaa").unwrap();
        std::fs::write(to.join("same (2).txt"), "a2").unwrap();
        route.set(true);
        let rules = PublishRules::default();
        let published = dir
            .publish(
                &file,
                os("same.txt.part"),
                "same.txt",
                &rules,
                std::future::pending(),
            )
            .await;
        route.set(false);
        let published = published.unwrap();
        assert_eq!(published.path, to.join("same (3).txt"), "{route:?}");
        assert_eq!(published.marked, Marked::NotAsked);
        assert_eq!(std::fs::read_to_string(to.join("same.txt")).unwrap(), "aaa");
        assert_eq!(
            std::fs::read_to_string(to.join("same (2).txt")).unwrap(),
            "a2"
        );
        assert_eq!(std::fs::read_to_string(&published.path).unwrap(), "bbb");
        drop(file);
        assert_eq!(
            files_in(&to),
            ["same (2).txt", "same (3).txt", "same.txt"],
            "{route:?}"
        );
        drop(dir);
        std::fs::remove_dir_all(&to).unwrap();
    }
}

/// Two files published under one name at the same time, from two threads,
/// both stay: one under the name, the other numbered.
#[test]
fn two_publishes_of_one_name_at_once_keep_both_files() {
    for route in ROUTES {
        let to = temp(&format!("same-time-{route:?}"));
        let dir = Directory::open(&to).unwrap();
        let start = std::sync::Barrier::new(2);
        let published: Vec<PathBuf> = std::thread::scope(|s| {
            let tasks: Vec<_> = [("one.part", b"one"), ("two.part", b"two")]
                .map(|(staging, inside)| {
                    let file = partial(&dir, staging, inside);
                    let (dir, start) = (&dir, &start);
                    s.spawn(move || {
                        // The switches are per thread: each sets its own.
                        route.set(true);
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_time()
                            .build()
                            .unwrap();
                        start.wait();
                        let rules = PublishRules::default();
                        let published = runtime.block_on(dir.publish(
                            &file,
                            os(staging),
                            "same.txt",
                            &rules,
                            std::future::pending(),
                        ));
                        route.set(false);
                        published.unwrap().path
                    })
                })
                .into_iter()
                .collect();
            tasks.into_iter().map(|t| t.join().unwrap()).collect()
        });
        let mut names: Vec<_> = published.iter().map(|p| p.file_name().unwrap()).collect();
        names.sort();
        assert_eq!(names, [os("same (2).txt"), os("same.txt")], "{route:?}");
        let mut inside: Vec<_> = published
            .iter()
            .map(|p| std::fs::read(p).unwrap())
            .collect();
        inside.sort();
        assert_eq!(inside, [b"one", b"two"], "{route:?}");
        assert_eq!(files_in(&to), ["same (2).txt", "same.txt"], "{route:?}");
        drop(dir);
        std::fs::remove_dir_all(&to).unwrap();
    }
}

/// On a disk without hard links (FAT, exFAT) a file that took the name
/// is kept, and the file arriving takes the next name.
#[tokio::test]
async fn a_folder_on_a_disk_without_hard_links_keeps_both_files_of_the_same_name() {
    let to = temp("folder-no-hard-links");
    let dir = Directory::open(&to).unwrap();
    std::fs::write(to.join("a.txt"), b"original").unwrap();
    let file = partial(&dir, "a.txt.part", b"new");
    NO_HARD_LINKS.set(true);
    let rules = PublishRules {
        mark: true,
        ..PublishRules::default()
    };
    let saved = dir
        .publish(
            &file,
            os("a.txt.part"),
            "a.txt",
            &rules,
            std::future::pending(),
        )
        .await;
    NO_HARD_LINKS.set(false);
    let saved = saved.unwrap();
    assert_eq!(saved.path, to.join("a (2).txt"));
    assert_eq!(std::fs::read(&saved.path).unwrap(), b"new");
    assert_eq!(std::fs::read(to.join("a.txt")).unwrap(), b"original");
    drop(file);
    assert!(!to.join("a.txt.part").exists());
    #[cfg(windows)]
    {
        assert_eq!(saved.marked, Marked::Marked);
        assert_eq!(zone(&saved.path).as_deref(), Some(win::INTERNET_ZONE));
    }
    #[cfg(unix)]
    assert_eq!(saved.marked, Marked::NotOnThisSystem);
    drop(dir);
    std::fs::remove_dir_all(to).unwrap();
}

/// On a disk without hard links a file that took the name meanwhile is
/// never replaced, by path either: the file arriving takes the next name,
/// still marked as downloaded when asked.
#[tokio::test]
async fn without_hard_links_a_taken_name_is_not_replaced() {
    let to = temp("no-links");
    let marking = PublishRules {
        mark: true,
        ..PublishRules::default()
    };
    let partial = to.join("same.txt.part");
    std::fs::write(&partial, "second").unwrap();
    // Finished first, after this file was given the name.
    std::fs::write(to.join("same.txt"), "first").unwrap();
    NO_HARD_LINKS.set(true);
    let saved = publish_path(&partial, &to.join("same.txt"), &to, "same.txt", &marking).await;
    NO_HARD_LINKS.set(false);
    let saved = saved.unwrap();
    assert_eq!(saved.path, to.join("same (2).txt"));
    assert_eq!(
        std::fs::read_to_string(to.join("same.txt")).unwrap(),
        "first"
    );
    assert_eq!(std::fs::read_to_string(&saved.path).unwrap(), "second");
    assert!(!partial.exists());
    #[cfg(windows)]
    assert_eq!(zone(&saved.path).as_deref(), Some(win::INTERNET_ZONE));
    // A free name is taken as it is.
    std::fs::write(&partial, "third").unwrap();
    NO_HARD_LINKS.set(true);
    let saved = publish_path(
        &partial,
        &to.join("new.txt"),
        &to,
        "new.txt",
        &PublishRules::default(),
    )
    .await;
    NO_HARD_LINKS.set(false);
    let saved = saved.unwrap();
    assert_eq!(saved.path, to.join("new.txt"));
    assert_eq!(saved.marked, Marked::NotAsked);
    assert_eq!(std::fs::read_to_string(&saved.path).unwrap(), "third");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&saved.path).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0, "{mode:o}");
    }
    // A rename that fails leaves no empty claim under the name.
    NO_HARD_LINKS.set(true);
    let gone = publish_path(
        &to.join("gone.txt.part"),
        &to.join("gone.txt"),
        &to,
        "gone.txt",
        &PublishRules::default(),
    )
    .await;
    NO_HARD_LINKS.set(false);
    assert!(gone.is_err());
    assert!(!to.join("gone.txt").exists());
    std::fs::remove_dir_all(&to).unwrap();
}

/// By path, with hard links, a taken name is skipped too.
#[tokio::test]
async fn a_name_taken_by_path_is_not_overwritten() {
    let to = temp("taken-by-path");
    let partial = to.join("same.txt.part");
    std::fs::write(&partial, "bbb").unwrap();
    std::fs::write(to.join("same.txt"), "aaa").unwrap();
    let rules = PublishRules::default();
    let saved = publish_path(&partial, &to.join("same.txt"), &to, "same.txt", &rules)
        .await
        .unwrap();
    assert_eq!(saved.path, to.join("same (2).txt"));
    assert_eq!(std::fs::read_to_string(to.join("same.txt")).unwrap(), "aaa");
    assert_eq!(std::fs::read_to_string(&saved.path).unwrap(), "bbb");
    assert!(!partial.exists());
    std::fs::remove_dir_all(&to).unwrap();
}

/// A received file, also one numbered because its name was taken, is
/// marked as downloaded on Windows when asked, and cannot be run on Unix.
#[tokio::test]
async fn a_received_file_is_marked_as_downloaded() {
    let to = temp("marked");
    let dir = Directory::open(&to).unwrap();
    let rules = PublishRules {
        mark: true,
        ..PublishRules::default()
    };
    let mut saved = Vec::new();
    for _ in 0..2 {
        let file = partial(&dir, "setup.exe.part", b"MZ!");
        let published = dir
            .publish(
                &file,
                os("setup.exe.part"),
                "setup.exe",
                &rules,
                std::future::pending(),
            )
            .await
            .unwrap();
        saved.push(published);
    }
    assert_eq!(saved[1].path, to.join("setup (2).exe"));
    for published in &saved {
        assert_eq!(std::fs::read(&published.path).unwrap(), b"MZ!");
        #[cfg(windows)]
        {
            assert_eq!(published.marked, Marked::Marked);
            assert_eq!(zone(&published.path).as_deref(), Some(win::INTERNET_ZONE));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(published.marked, Marked::NotOnThisSystem);
            let mode = std::fs::metadata(&published.path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0, "{mode:o}");
        }
    }
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

/// The mark goes on the partial file before any name is claimed, so the
/// file is never there under its name unmarked: a publish that fails has
/// already marked it.
#[tokio::test]
async fn the_mark_is_on_the_partial_before_it_gets_a_name() {
    let to = temp("mark-first");
    let dir = Directory::open(&to).unwrap();
    let file = partial(&dir, "a.exe.part", b"MZ");
    // An error that is neither a held file nor a disk without links.
    #[cfg(windows)]
    let error = 87;
    #[cfg(unix)]
    let error = libc::EIO;
    FINISH_ERRORS.with_borrow_mut(|errors| errors.push_back(error));
    let rules = PublishRules {
        mark: true,
        ..PublishRules::default()
    };
    let failed = dir
        .publish(
            &file,
            os("a.exe.part"),
            "a.exe",
            &rules,
            std::future::pending(),
        )
        .await;
    assert!(
        matches!(&failed, Err(PublishError::Name { path, .. }) if *path == to.join("a.exe")),
        "{failed:?}"
    );
    assert_eq!(files_in(&to), ["a.exe.part"]);
    #[cfg(windows)]
    assert_eq!(
        zone(&to.join("a.exe.part")).as_deref(),
        Some(win::INTERNET_ZONE)
    );
    dir.discard(&file, os("a.exe.part")).unwrap();
    drop((file, dir));
    std::fs::remove_dir_all(&to).unwrap();
}

/// A library marks nothing unless its caller asks.
#[tokio::test]
async fn nothing_is_marked_unless_asked() {
    let to = temp("unmarked");
    let dir = Directory::open(&to).unwrap();
    let file = partial(&dir, "a.exe.part", b"MZ");
    let published = dir
        .publish(
            &file,
            os("a.exe.part"),
            "a.exe",
            &PublishRules::default(),
            std::future::pending(),
        )
        .await
        .unwrap();
    assert_eq!(published.marked, Marked::NotAsked);
    #[cfg(windows)]
    assert_eq!(zone(&published.path), None);
    let partial = to.join("b.exe.part");
    std::fs::write(&partial, "MZ").unwrap();
    let by_path = publish_path(
        &partial,
        &to.join("b.exe"),
        &to,
        "b.exe",
        &PublishRules::default(),
    )
    .await
    .unwrap();
    assert_eq!(by_path.marked, Marked::NotAsked);
    #[cfg(windows)]
    assert_eq!(zone(&by_path.path), None);
    drop((file, dir));
    std::fs::remove_dir_all(&to).unwrap();
}

/// The name a publish was trying when the system refused, and the error's
/// kind.
#[cfg(unix)]
fn failed_at(result: Result<Published, PublishError>) -> (PathBuf, io::ErrorKind) {
    match result {
        Err(PublishError::Name { path, error }) => (path, error.kind()),
        other => panic!("{other:?}"),
    }
}

/// The caller knows only its first choice: a failure after numbering says
/// which name it was at, with the system's error whole.
#[tokio::test]
async fn a_publish_that_fails_says_which_numbered_name_it_was_trying() {
    let to = temp("names-its-try");
    let dir = Directory::open(&to).unwrap();
    let file = partial(&dir, "a.part", b"a");
    // The first name is taken, the second refused.
    #[cfg(windows)]
    let (taken, error) = (183, 87);
    #[cfg(unix)]
    let (taken, error) = (libc::EEXIST, libc::EIO);
    FINISH_ERRORS.with_borrow_mut(|errors| errors.extend([taken, error]));
    let failed = dir
        .publish(
            &file,
            os("a.part"),
            "a.txt",
            &PublishRules::default(),
            std::future::pending(),
        )
        .await;
    let second = safecopy_names::candidates("a.txt").nth(1).unwrap();
    assert_eq!(second, "a (2).txt");
    let Err(PublishError::Name { path, error: said }) = failed else {
        panic!("{failed:?}");
    };
    assert_eq!(path, to.join(&second));
    assert_eq!(said.raw_os_error(), Some(error));
    assert_eq!(files_in(&to), ["a.part"]);
    dir.discard(&file, os("a.part")).unwrap();
    drop((file, dir));
    std::fs::remove_dir_all(to).unwrap();
}

#[tokio::test]
async fn by_path_a_publish_that_fails_says_which_numbered_name_it_was_trying() {
    let to = temp("path-names-its-try");
    std::fs::write(to.join("a.txt"), b"theirs").unwrap();
    // No partial: the second name is claimed, and the rename has nothing to move.
    NO_HARD_LINKS.set(true);
    let failed = publish_path(
        &to.join("a.txt.part"),
        &to.join("a.txt"),
        &to,
        "a.txt",
        &PublishRules::default(),
    )
    .await;
    NO_HARD_LINKS.set(false);
    let Err(PublishError::Name { path, error }) = failed else {
        panic!("{failed:?}");
    };
    assert_eq!(path, to.join("a (2).txt"));
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    // ERROR_FILE_NOT_FOUND, ENOENT.
    assert_eq!(error.raw_os_error(), Some(2));
    assert_eq!(files_in(&to), ["a.txt"]);
    std::fs::remove_dir_all(to).unwrap();
}

#[cfg(windows)]
#[test]
fn an_open_staging_file_cannot_be_swapped_on_windows() {
    let to = temp("staging-locked");
    let dir = Directory::open(&to).unwrap();
    let file = partial(&dir, "a.part", b"inside");
    std::fs::write(to.join("replacement"), b"outside").unwrap();
    assert_eq!(
        std::fs::rename(to.join("a.part"), to.join("moved.part"))
            .unwrap_err()
            .raw_os_error(),
        Some(32)
    );
    assert_eq!(
        std::fs::remove_file(to.join("a.part"))
            .unwrap_err()
            .raw_os_error(),
        Some(32)
    );
    assert!(std::fs::rename(to.join("replacement"), to.join("a.part")).is_err());
    dir.finish(&file, os("a.part"), os("a.txt")).unwrap();
    assert_eq!(std::fs::read(to.join("a.txt")).unwrap(), b"inside");
    assert_eq!(std::fs::read(to.join("replacement")).unwrap(), b"outside");
    // A taken name is refused, not replaced.
    std::fs::write(to.join("b.txt"), b"taken").unwrap();
    let taken = dir.finish(&file, os("a.txt"), os("b.txt")).unwrap_err();
    assert_eq!(taken.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(to.join("b.txt")).unwrap(), b"taken");
    drop((file, dir));
    std::fs::remove_dir_all(to).unwrap();
}

/// A staging name swapped for a link to another file cannot supply the
/// finished content: the file published is the one written.
#[cfg(unix)]
#[tokio::test]
async fn a_staging_file_replaced_by_a_link_cannot_supply_the_finished_content() {
    let (to, outside) = (temp("swap-staging"), temp("swap-staging-outside"));
    let dir = Directory::open(&to).unwrap();
    let file = partial(&dir, "a.part", b"inside");
    std::fs::write(outside.join("a.txt"), b"outside").unwrap();
    std::fs::rename(to.join("a.part"), to.join("original.part")).unwrap();
    std::os::unix::fs::symlink(outside.join("a.txt"), to.join("a.part")).unwrap();
    // Linux links the open file through procfs; macOS has none.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let replaced = dir.finish(&file, os("a.part"), os("b.txt")).unwrap_err();
        assert_eq!(refusal(&replaced), Some(Refusal::Replaced));
        assert_eq!(std::fs::read(to.join("b.txt")).unwrap(), b"inside");
        // The planted link is not the staging file: it stays.
        assert!(
            std::fs::symlink_metadata(to.join("a.part"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(outside.join("a.txt")).unwrap(), b"outside");
    }
    let published = dir
        .publish(
            &file,
            os("a.part"),
            "a.txt",
            &PublishRules::default(),
            std::future::pending(),
        )
        .await
        .unwrap();
    assert_eq!(published.path, to.join("a.txt"));
    assert_eq!(std::fs::read(to.join("a.txt")).unwrap(), b"inside");
    assert_eq!(std::fs::read(outside.join("a.txt")).unwrap(), b"outside");
    assert!(
        !std::fs::symlink_metadata(to.join("a.txt"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    drop((file, dir));
    for dir in [to, outside] {
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_replaced_staging_name_falls_back_to_copying_the_open_file() {
    let to = temp("rename-replaced-staging");
    let dir = Directory::open(&to).unwrap();
    let file = partial(&dir, "a.part", b"inside");
    std::fs::write(to.join("outside"), b"outside").unwrap();
    std::fs::write(to.join("a.txt"), b"taken").unwrap();
    std::fs::rename(to.join("a.part"), to.join("original.part")).unwrap();
    std::os::unix::fs::symlink(to.join("outside"), to.join("a.part")).unwrap();
    NO_HARD_LINKS.set(true);
    let result = dir
        .publish(
            &file,
            os("a.part"),
            "a.txt",
            &PublishRules::default(),
            std::future::pending(),
        )
        .await;
    NO_HARD_LINKS.set(false);
    assert_eq!(result.unwrap().path, to.join("a (2).txt"));
    assert_eq!(std::fs::read(to.join("a (2).txt")).unwrap(), b"inside");
    assert_eq!(std::fs::read(to.join("outside")).unwrap(), b"outside");
    assert_eq!(std::fs::read(to.join("a.txt")).unwrap(), b"taken");
    // The planted link is not the staging file, so the discard leaves it.
    assert!(
        std::fs::symlink_metadata(to.join("a.part"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    drop((file, dir));
    std::fs::remove_dir_all(to).unwrap();
}

/// Linked, the file is finished: a staging name that cannot be removed
/// is left, not taken for a failure that would link the next name too.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[tokio::test]
async fn a_staging_name_that_cannot_be_removed_does_not_fail_the_finish() {
    let to = temp("finish-remove-fails");
    let dir = Directory::open(&to).unwrap();
    let file = partial(&dir, "a.part", b"ours");
    crate::seams::REMOVE_ERRORS.with_borrow_mut(|errors| errors.push_back(libc::EPERM));
    let published = dir
        .publish(
            &file,
            os("a.part"),
            "a.txt",
            &PublishRules::default(),
            std::future::pending(),
        )
        .await;
    assert_eq!(published.unwrap().path, to.join("a.txt"));
    assert!(crate::seams::REMOVE_ERRORS.with_borrow(VecDeque::is_empty));
    assert_eq!(files_in(&to), ["a.part", "a.txt"]);
    drop((file, dir));
    std::fs::remove_dir_all(to).unwrap();
}

/// A file swapped in between the rename's check and the rename is given
/// its old name back, not deleted, and never keeps the final name.
#[cfg(unix)]
#[test]
fn a_file_swapped_in_during_a_rename_goes_back_to_its_name() {
    let to = temp("rename-swapped-back");
    let dir = Directory::open(&to).unwrap();
    let file = partial(&dir, "a.part", b"ours");
    let at = to.clone();
    crate::seams::on("rename", move || {
        std::fs::rename(at.join("a.part"), at.join("moved")).unwrap();
        std::fs::write(at.join("a.part"), b"theirs").unwrap();
    });
    let replaced = dir.rename(&file, os("a.part"), os("a.txt")).unwrap_err();
    assert_eq!(refusal(&replaced), Some(Refusal::Replaced));
    assert_eq!(files_in(&to), ["a.part", "moved"]);
    assert_eq!(std::fs::read(to.join("a.part")).unwrap(), b"theirs");
    drop((file, dir));
    std::fs::remove_dir_all(to).unwrap();
}

/// When the staging name is taken again before the swapped-in file can go
/// back, nothing is replaced: that file stays under the final name, and
/// the rename still fails.
#[cfg(unix)]
#[test]
fn a_swapped_in_file_stays_when_its_old_name_was_taken_again() {
    let to = temp("rename-swap-back-taken");
    let dir = Directory::open(&to).unwrap();
    let file = partial(&dir, "a.part", b"ours");
    let at = to.clone();
    crate::seams::on("rename", move || {
        std::fs::rename(at.join("a.part"), at.join("moved")).unwrap();
        std::fs::write(at.join("a.part"), b"theirs").unwrap();
    });
    let at = to.clone();
    crate::seams::on("renamed", move || {
        std::fs::write(at.join("a.part"), b"again").unwrap();
    });
    let replaced = dir.rename(&file, os("a.part"), os("a.txt")).unwrap_err();
    assert_eq!(refusal(&replaced), Some(Refusal::Replaced));
    assert_eq!(files_in(&to), ["a.part", "a.txt", "moved"]);
    assert_eq!(std::fs::read(to.join("a.txt")).unwrap(), b"theirs");
    assert_eq!(std::fs::read(to.join("a.part")).unwrap(), b"again");
    assert_eq!(std::fs::read(to.join("moved")).unwrap(), b"ours");
    drop((file, dir));
    std::fs::remove_dir_all(to).unwrap();
}

/// By path, a partial swapped after it was linked is not the file linked,
/// and stays.
#[cfg(unix)]
#[tokio::test]
async fn by_path_a_partial_swapped_after_linking_stays() {
    let to = temp("path-linked-swap");
    std::fs::write(to.join("a.part"), b"ours").unwrap();
    let at = to.clone();
    crate::seams::on("linked", move || {
        std::fs::remove_file(at.join("a.part")).unwrap();
        std::fs::write(at.join("a.part"), b"theirs").unwrap();
    });
    let rules = PublishRules::default();
    let published = publish_path(&to.join("a.part"), &to.join("a.txt"), &to, "a.txt", &rules)
        .await
        .unwrap();
    assert_eq!(published.path, to.join("a.txt"));
    assert_eq!(std::fs::read(to.join("a.txt")).unwrap(), b"ours");
    assert_eq!(std::fs::read(to.join("a.part")).unwrap(), b"theirs");
    // Unswapped, the partial goes.
    std::fs::rename(to.join("a.part"), to.join("b.part")).unwrap();
    publish_path(&to.join("b.part"), &to.join("b.txt"), &to, "b.txt", &rules)
        .await
        .unwrap();
    assert_eq!(files_in(&to), ["a.txt", "b.txt"]);
    std::fs::remove_dir_all(to).unwrap();
}

/// By path without hard links, a failed rename removes the empty claim it
/// made, but not a file put in the claim's place.
#[cfg(unix)]
#[tokio::test]
async fn by_path_a_file_put_in_place_of_the_claim_stays() {
    let to = temp("path-claim-swap");
    std::fs::write(to.join("a.part"), b"ours").unwrap();
    let at = to.clone();
    crate::seams::on("claimed", move || {
        // Written before the claim goes, so it cannot reuse the claim's inode.
        std::fs::write(at.join("theirs"), b"theirs").unwrap();
        std::fs::rename(at.join("theirs"), at.join("a.txt")).unwrap();
        // The rename then fails: nothing to rename.
        std::fs::rename(at.join("a.part"), at.join("moved")).unwrap();
    });
    NO_HARD_LINKS.set(true);
    let rules = PublishRules::default();
    let failed = publish_path(&to.join("a.part"), &to.join("a.txt"), &to, "a.txt", &rules).await;
    assert_eq!(
        failed_at(failed),
        (to.join("a.txt"), io::ErrorKind::NotFound)
    );
    assert_eq!(std::fs::read(to.join("a.txt")).unwrap(), b"theirs");
    // Unswapped, the claim goes.
    let failed = publish_path(&to.join("b.part"), &to.join("b.txt"), &to, "b.txt", &rules).await;
    NO_HARD_LINKS.set(false);
    assert_eq!(
        failed_at(failed),
        (to.join("b.txt"), io::ErrorKind::NotFound)
    );
    assert_eq!(files_in(&to), ["a.txt", "moved"]);
    std::fs::remove_dir_all(to).unwrap();
}

/// The no-replace rename every Unix publish falls back to (and macOS's
/// only way): a taken name is refused, a swapped staging name too.
#[cfg(unix)]
#[test]
fn the_no_replace_rename_never_replaces_a_taken_name() {
    let to = temp("no-replace-rename");
    let dir = Directory::open(&to).unwrap();
    let file = partial(&dir, "a.part", b"inside");
    std::fs::write(to.join("a.txt"), b"taken").unwrap();
    let taken = dir.rename(&file, os("a.part"), os("a.txt")).unwrap_err();
    assert_eq!(taken.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(to.join("a.txt")).unwrap(), b"taken");
    dir.rename(&file, os("a.part"), os("b.txt")).unwrap();
    assert_eq!(std::fs::read(to.join("b.txt")).unwrap(), b"inside");
    assert!(!to.join("a.part").exists());
    // A name that is not the open file is never renamed.
    std::fs::write(to.join("other.part"), b"other").unwrap();
    let other = dir
        .rename(&file, os("other.part"), os("c.txt"))
        .unwrap_err();
    assert_eq!(refusal(&other), Some(Refusal::Replaced));
    assert!(!to.join("c.txt").exists());
    assert_eq!(std::fs::read(to.join("other.part")).unwrap(), b"other");
    drop((file, dir));
    std::fs::remove_dir_all(to).unwrap();
}

/// macOS has no procfs to link an open file: it publishes with its own
/// no-replace rename (`RENAME_EXCL`), and never replaces a taken name.
#[cfg(target_vendor = "apple")]
#[tokio::test]
async fn on_macos_a_publish_renames_without_replacing() {
    let to = temp("macos-publish");
    let dir = Directory::open(&to).unwrap();
    let file = partial(&dir, "a.part", b"new");
    let unsupported = dir.finish(&file, os("a.part"), os("x.txt")).unwrap_err();
    assert_eq!(unsupported.kind(), io::ErrorKind::Unsupported);
    std::fs::write(to.join("a.txt"), b"taken").unwrap();
    let published = dir
        .publish(
            &file,
            os("a.part"),
            "a.txt",
            &PublishRules::default(),
            std::future::pending(),
        )
        .await
        .unwrap();
    assert_eq!(published.path, to.join("a (2).txt"));
    assert_eq!(std::fs::read(to.join("a.txt")).unwrap(), b"taken");
    assert_eq!(std::fs::read(to.join("a (2).txt")).unwrap(), b"new");
    assert_eq!(files_in(&to), ["a (2).txt", "a.txt"]);
    drop((file, dir));
    std::fs::remove_dir_all(to).unwrap();
}

/// A file is removed through a handle opened without following a link:
/// a file that took its name after it was opened stays.
#[cfg(windows)]
#[test]
fn removal_goes_only_through_a_checked_handle() {
    let (to, outside) = (temp("remove-by-handle"), temp("remove-outside"));
    std::fs::write(to.join("a.txt"), b"meant").unwrap();
    let dir = Directory::open(&to).unwrap();
    let handle = dir.open_for_delete(os("a.txt")).unwrap();
    // Another file takes the name after the handle was opened.
    std::fs::rename(to.join("a.txt"), to.join("moved.txt")).unwrap();
    std::fs::write(to.join("a.txt"), b"newcomer").unwrap();
    delete_by_handle(&handle).unwrap();
    drop(handle);
    assert_eq!(files_in(&to), ["a.txt"]);
    assert_eq!(std::fs::read(to.join("a.txt")).unwrap(), b"newcomer");
    // Nothing is deleted through a junction.
    std::fs::write(outside.join("b.txt"), b"outside").unwrap();
    if link_folder(&outside, &to.join("link")) {
        // A junction is a folder, which a file's delete refuses before
        // reading its tag; either way it is never opened through.
        assert!(dir.open_for_delete(os("link")).is_err());
        assert!(dir.remove(os("link")).is_err());
        assert_eq!(files_in(&outside), ["b.txt"]);
        remove_link(&to.join("link"));
    } else {
        no_link_here(junctions_required(), "removing through a junction");
    }
    // Remove opens the name the same way, then deletes by that handle.
    dir.remove(os("a.txt")).unwrap();
    assert!(files_in(&to).is_empty());
    drop(dir);
    for path in [to, outside] {
        std::fs::remove_dir_all(path).unwrap();
    }
}

/// A held folder that moves, with a junction put at its old path, still
/// removes inside the moved folder only, never through the junction.
#[cfg(windows)]
#[test]
fn removal_in_a_moved_folder_never_goes_through_a_junction_at_its_old_path() {
    let (to, outside) = (temp("remove-moved"), temp("remove-moved-outside"));
    std::fs::write(outside.join("a.txt"), b"outside").unwrap();
    std::fs::create_dir(to.join("sub")).unwrap();
    std::fs::write(to.join("sub/a.txt"), b"meant").unwrap();
    let dir = Directory::open(&to.join("sub")).unwrap();
    std::fs::rename(to.join("sub"), to.join("moved")).unwrap();
    if link_folder(&outside, &to.join("sub")) {
        dir.remove(os("a.txt")).unwrap();
        assert!(files_in(&to.join("moved")).is_empty());
        assert_eq!(files_in(&outside), ["a.txt"]);
        assert_eq!(std::fs::read(outside.join("a.txt")).unwrap(), b"outside");
        remove_link(&to.join("sub"));
    } else {
        no_link_here(junctions_required(), "a junction at a moved folder's path");
    }
    drop(dir);
    for path in [to, outside] {
        std::fs::remove_dir_all(path).unwrap();
    }
}

/// Unix has no delete by handle: a discard checks the name is still the
/// open file, and leaves a file put under it after it was created.
#[cfg(unix)]
#[test]
fn on_unix_a_discard_leaves_a_file_that_took_the_name() {
    let to = temp("discard-by-name");
    let dir = Directory::open(&to).unwrap();
    let file = partial(&dir, "a.part", b"ours");
    std::fs::rename(to.join("a.part"), to.join("moved")).unwrap();
    std::fs::write(to.join("a.part"), b"theirs").unwrap();
    let replaced = dir.discard(&file, os("a.part")).unwrap_err();
    assert_eq!(refusal(&replaced), Some(Refusal::Replaced));
    assert_eq!(files_in(&to), ["a.part", "moved"]);
    assert_eq!(std::fs::read(to.join("a.part")).unwrap(), b"theirs");
    // Its own name it still removes.
    std::fs::remove_file(to.join("a.part")).unwrap();
    std::fs::rename(to.join("moved"), to.join("a.part")).unwrap();
    dir.discard(&file, os("a.part")).unwrap();
    assert!(files_in(&to).is_empty());
    drop((file, dir));
    std::fs::remove_dir_all(to).unwrap();
}

/// After linking the open file, Linux removes the staging name only while
/// it is still that file: a file swapped in under it stays, and the
/// finish reports it.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn a_finish_leaves_a_file_swapped_in_under_the_staging_name() {
    let to = temp("finish-swapped-staging");
    let dir = Directory::open(&to).unwrap();
    let file = partial(&dir, "a.part", b"ours");
    std::fs::rename(to.join("a.part"), to.join("moved")).unwrap();
    std::fs::write(to.join("a.part"), b"theirs").unwrap();
    let replaced = dir.finish(&file, os("a.part"), os("a.txt")).unwrap_err();
    assert_eq!(refusal(&replaced), Some(Refusal::Replaced));
    assert_eq!(std::fs::read(to.join("a.txt")).unwrap(), b"ours");
    assert_eq!(std::fs::read(to.join("a.part")).unwrap(), b"theirs");
    // Unswapped, the staging name goes.
    std::fs::remove_file(to.join("a.part")).unwrap();
    std::fs::rename(to.join("moved"), to.join("a.part")).unwrap();
    dir.finish(&file, os("a.part"), os("b.txt")).unwrap();
    assert_eq!(files_in(&to), ["a.txt", "b.txt"]);
    drop((file, dir));
    std::fs::remove_dir_all(to).unwrap();
}

/// Unix removes by name, relative to the held folder: never through a
/// link, and never outside the folder.
#[cfg(unix)]
#[test]
fn removal_goes_only_through_a_checked_handle() {
    let (to, outside) = (temp("remove-by-name"), temp("remove-outside"));
    std::fs::write(outside.join("b.txt"), b"outside").unwrap();
    std::fs::create_dir(to.join("sub")).unwrap();
    let dir = Directory::open(&to.join("sub")).unwrap();
    std::fs::write(to.join("sub/a.txt"), b"meant").unwrap();
    // The folder moves; the held handle still removes inside it.
    std::fs::rename(to.join("sub"), to.join("moved")).unwrap();
    std::os::unix::fs::symlink(&outside, to.join("sub")).unwrap();
    std::fs::write(outside.join("a.txt"), b"outside").unwrap();
    dir.remove(os("a.txt")).unwrap();
    assert!(files_in(&to.join("moved")).is_empty());
    assert_eq!(files_in(&outside), ["a.txt", "b.txt"]);
    drop(dir);
    for path in [to, outside] {
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[cfg(windows)]
#[test]
fn a_local_folder_has_a_volume_and_keeps_marks() {
    let to = temp("volume");
    let dir = Directory::open(&to).unwrap();
    let volume = dir.volume_path().unwrap();
    assert!(
        volume.starts_with(r"\\?\Volume{") && volume.ends_with("}\\"),
        "{volume}"
    );
    assert_eq!(platform::unfit(dir.handle()), None);
    drop(dir);
    std::fs::remove_dir_all(to).unwrap();
}

#[test]
fn a_refusal_is_read_back_from_the_error() {
    let e = refused(Refusal::Link);
    assert!(is_link(&e));
    assert_eq!(refusal(&e), Some(Refusal::Link));
    assert_eq!(refusal(&io::Error::other("other")), None);
    assert_eq!(refusal(&io::ErrorKind::NotFound.into()), None);
    assert!(!is_link(&refused(Refusal::NotFileOrFolder)));
}

#[test]
fn reopen_opens_only_a_regular_file_already_there_and_never_a_link() {
    use std::io::{Read, Seek, SeekFrom};
    let base = temp("reopen");
    let dir = Directory::open(&base).unwrap();
    // Nothing is created.
    let missing = dir.reopen(os("none.part")).unwrap_err();
    assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
    assert!(!base.join("none.part").exists());
    // A file there opens for reading and writing.
    std::fs::write(base.join("a.part"), b"abc").unwrap();
    let mut file = dir.reopen(os("a.part")).unwrap();
    file.seek(SeekFrom::End(0)).unwrap();
    file.write_all(b"d").unwrap();
    let mut read = String::new();
    file.seek(SeekFrom::Start(0)).unwrap();
    file.read_to_string(&mut read).unwrap();
    assert_eq!(read, "abcd");
    // It gets its name, or is removed, through this handle as a new partial is.
    if dir.finish(&file, os("a.part"), os("a.txt")).is_err() {
        dir.rename(&file, os("a.part"), os("a.txt")).unwrap();
    }
    drop(file);
    assert_eq!(std::fs::read(base.join("a.txt")).unwrap(), b"abcd");
    assert!(!base.join("a.part").exists());
    std::fs::write(base.join("b.part"), b"abc").unwrap();
    let file = dir.reopen(os("b.part")).unwrap();
    dir.discard(&file, os("b.part")).unwrap();
    drop(file);
    assert!(!base.join("b.part").exists());
    std::fs::write(base.join("a.part"), b"abcd").unwrap();
    // A folder, a name leaving the folder, and a link are refused.
    std::fs::create_dir(base.join("Sub")).unwrap();
    assert!(dir.reopen(os("Sub")).is_err());
    assert_eq!(
        refusal(&dir.reopen(os("../a.part")).unwrap_err()),
        Some(Refusal::LeavesFolder)
    );
    let link = base.join("link.part");
    if link_folder(&base.join("Sub"), &link) {
        let error = dir.reopen(os("link.part")).unwrap_err();
        assert!(is_link(&error), "{error:?}");
        remove_link(&link);
    } else {
        no_link_here(junctions_required(), "a link named as a partial");
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(base.join("a.part"), &link).unwrap();
        assert!(is_link(&dir.reopen(os("link.part")).unwrap_err()));
        assert_eq!(std::fs::read(base.join("a.part")).unwrap(), b"abcd");
    }
    let _ = std::fs::remove_dir_all(&base);
}
