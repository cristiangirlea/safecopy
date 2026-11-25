//! A cleanup and a running journal at one segment file. Each window is
//! held open through a seam, never by timing, and both sides run on one
//! thread: a lock is per open file, so two opens in one process meet as
//! two processes would.

use super::*;
use crate::seams::{Point, when};
use crate::tests::{Scratch, partial};
#[cfg(unix)]
use std::cell::RefCell;
use std::rc::Rc;

/// A journal that starts its next segment each time a partial ends.
fn rolling() -> Settings {
    Settings {
        roll_every: 1,
        ..Settings::default()
    }
}

fn with_ending(dir: &Path, ending: &str) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension() == Some(OsStr::new(ending)))
        .collect();
    paths.sort();
    paths
}

fn segments(dir: &Path) -> Vec<PathBuf> {
    with_ending(dir, LOG_EXTENSION)
}

fn cleaned(removed: usize, left: usize) -> Cleaned {
    Cleaned {
        removed,
        left,
        kept: 0,
    }
}

/// The folders of a run: one to write into and one for journals.
fn scratch(name: &str) -> (Scratch, PathBuf, PathBuf) {
    Scratch::folders("jmeet", name)
}

/// A running journal and where it writes.
struct Run {
    scratch: Scratch,
    place: PathBuf,
    journal_dir: PathBuf,
    root: Directory,
    journal: Rc<Journal>,
}

/// `journal`, started in the folders of `scratch`, with one live partial:
/// `a.txt.part`, at place 0.
fn run_of((scratch, place, journal_dir): (Scratch, PathBuf, PathBuf), journal: Journal) -> Run {
    let root = Directory::destination(&place).unwrap();
    drop(partial(&root, &journal, 0, "a.txt.part", b"half"));
    Run {
        scratch,
        place,
        journal_dir,
        root,
        journal: Rc::new(journal),
    }
}

fn start_in(place: &Path, journal_dir: &Path) -> Result<Journal, JournalError> {
    let root = Directory::destination(place).unwrap();
    Journal::start(journal_dir, &root, &rolling())
}

fn run(name: &str) -> Run {
    let folders = scratch(name);
    let journal = start_in(&folders.1, &folders.2).unwrap();
    run_of(folders, journal)
}

impl Run {
    fn seq(&self) -> u64 {
        let bytes = std::fs::read(self.journal.segment_path()).unwrap();
        read_segment(&bytes, &rolling()).unwrap().seq()
    }

    /// What starts and ends the partial of `place`: with [`rolling`], the
    /// journal then starts its next segment.
    fn ender(&self, place: u32) -> impl FnOnce() + 'static {
        let (journal, dir) = (self.journal.clone(), self.place.clone());
        let root = self.root.try_clone().unwrap();
        move || {
            let name = format!("{place:02}.txt.part");
            drop(partial(&root, &journal, place, &name, b"data"));
            journal.ended(place).unwrap();
            std::fs::remove_file(dir.join(&name)).unwrap();
        }
    }

    /// A cleanup of the journals, which must find nothing to do.
    fn cleanup(&self) -> impl FnOnce() + 'static {
        let dir = self.journal_dir.clone();
        move || {
            assert_eq!(clean_up(&dir, &rolling()).unwrap(), Cleaned::default());
        }
    }

    /// What must hold after a cleanup met the journal: no live partial and
    /// no live segment was removed, the journal still records and still
    /// rolls, and what it recorded after is kept for the cleanup that
    /// follows a crash.
    fn still_whole(self) {
        assert!(seams::AT.take().is_none(), "the point was reached");
        assert!(self.place.join("a.txt.part").exists(), "a live partial");
        assert!(self.journal.segment_path().exists(), "a live segment");
        let before = self.seq();
        drop(partial(&self.root, &self.journal, 7, "z.txt.part", b"more"));
        self.ender(8)();
        assert!(self.seq() > before, "it still rolls");
        self.ender(9)();
        assert!(self.seq() > before + 1, "and again");
        let Run {
            scratch: _scratch,
            place,
            journal_dir,
            root,
            journal,
        } = self;
        let journal = Rc::try_unwrap(journal).expect("no hook holds it still");
        let places: Vec<u32> = journal.partials().iter().map(|s| s.place).collect();
        assert_eq!(places, [0, 7]);
        journal.crash();
        drop(root);
        let got = recover(&journal_dir, &rolling()).unwrap();
        assert_eq!(got.len(), 1);
        let places: Vec<u32> = got[0].partials().iter().map(|s| s.place).collect();
        assert_eq!(places, [0, 7], "what was recorded after is kept");
        assert_eq!(clean_up(&journal_dir, &rolling()).unwrap(), cleaned(2, 0));
        assert!(!place.join("a.txt.part").exists());
        assert!(!place.join("z.txt.part").exists());
        assert_eq!(std::fs::read_dir(&journal_dir).unwrap().count(), 0);
    }
}

/// What holds the file just made in `dir` with the ending `ending`, as a
/// cleanup has it for the moment it looks, and where it is kept until the
/// test lets it go.
#[cfg(unix)]
fn holder(
    dir: &Path,
    ending: &'static str,
) -> (Rc<RefCell<Option<File>>>, impl FnOnce() + 'static) {
    let held = Rc::new(RefCell::new(None));
    let (keep, dir) = (held.clone(), dir.to_path_buf());
    (held, move || {
        let made = with_ending(&dir, ending)
            .into_iter()
            .find(|p| std::fs::metadata(p).unwrap().len() == 0)
            .expect("an empty file, just made");
        *keep.borrow_mut() = Some(files::take(&made, 0).unwrap().0);
    })
}

// The cleanup that takes a segment its journal has just let go of.

/// A cleanup that opened a segment before its journal rolled, and locks it
/// after, holds a file with no name: the journal is on its next segment,
/// which the cleanup never listed, and nothing of it is touched.
#[cfg(unix)]
#[test]
fn a_cleanup_that_opened_a_segment_before_its_journal_rolled_takes_nothing() {
    let run = run("opened-before-roll");
    when(Point::TakeOpened, run.ender(1));
    assert_eq!(
        clean_up(&run.journal_dir, &rolling()).unwrap(),
        Cleaned::default()
    );
    run.still_whole();
}

/// A file removed, or removed and made again under its name, between a
/// take's open and its lock is not taken: the lock is on a file its name
/// no longer reaches.
#[cfg(unix)]
#[test]
fn a_file_removed_while_it_is_being_taken_is_not_taken() {
    let (_scratch, place, _) = scratch("take-removed");
    let path = place.join("a.1.jlog");
    std::fs::write(&path, b"one").unwrap();
    let gone = path.clone();
    when(Point::TakeOpened, move || {
        std::fs::remove_file(&gone).unwrap()
    });
    let taken = files::take(&path, 16).unwrap_err();
    assert_eq!(taken.kind(), io::ErrorKind::NotFound);
    assert!(!files::busy(&taken) && !files::refused(&taken));
    std::fs::write(&path, b"one").unwrap();
    let (gone, other) = (path.clone(), place.join("b.1.jlog"));
    std::fs::write(&other, b"two").unwrap();
    when(Point::TakeOpened, move || {
        std::fs::rename(&other, &gone).unwrap()
    });
    let taken = files::take(&path, 16).unwrap_err();
    assert_eq!(taken.kind(), io::ErrorKind::NotFound);
    // The one under the name now is taken.
    assert_eq!(files::take(&path, 16).unwrap().1, b"two");
}

#[cfg(unix)]
#[test]
fn a_record_filled_before_its_creators_lock_is_left_alone() {
    use std::io::Write;
    let (_scratch, place, _) = scratch("create-filled");
    let path = place.join("record.resume");
    let other = path.clone();
    when(Point::Made, move || {
        files::hold(&other).unwrap().write_all(b"record").unwrap();
    });
    assert!(files::busy(&files::create(&path).unwrap_err()));
    assert_eq!(std::fs::read(path).unwrap(), b"record");
}

// The cleanup that locks a segment before its journal does: in a folder
// that gives a file no second name, a segment is still made under its own
// name and then locked.

/// A cleanup that ran whole between a first segment's making and its lock
/// removed it as an empty journal: the start takes another name.
#[cfg(unix)]
#[test]
fn where_no_second_name_can_be_made_a_cleanup_at_a_journals_start_does_not_stop_it() {
    let folders = scratch("start-cleaned");
    let dir = folders.2.clone();
    seams::NO_LINKS.set(true);
    when(Point::Made, move || {
        assert_eq!(clean_up(&dir, &rolling()).unwrap(), Cleaned::default());
    });
    let journal = start_in(&folders.1, &folders.2).unwrap();
    assert!(journal.segment_path().exists(), "held under its name");
    let run = run_of(folders, journal);
    run.still_whole();
    seams::NO_LINKS.set(false);
}

/// A cleanup that holds a first segment in the moment its journal would
/// lock it: the start takes another name, and the one it left is removed
/// by the next cleanup.
#[cfg(unix)]
#[test]
fn where_no_second_name_can_be_made_a_first_segment_held_by_a_cleanup_is_stepped_past() {
    let folders = scratch("start-held");
    seams::NO_LINKS.set(true);
    let (held, hold) = holder(&folders.2, LOG_EXTENSION);
    when(Point::Made, hold);
    let journal = start_in(&folders.1, &folders.2).unwrap();
    assert!(seams::AT.take().is_none(), "the point was reached");
    assert!(journal.segment_path().exists(), "held under its name");
    assert_eq!(segments(&folders.2).len(), 2, "its own and the one it left");
    drop(held.take());
    let run = run_of(folders, journal);
    run.still_whole();
    seams::NO_LINKS.set(false);
}

/// The same while the journal rolls: the next segment is held by a
/// cleanup in the moment the journal would lock it. The journal takes the
/// number after, and every later roll goes on.
#[cfg(unix)]
#[test]
fn where_no_second_name_can_be_made_a_next_segment_held_by_a_cleanup_is_stepped_past() {
    let run = run("roll-held");
    seams::NO_LINKS.set(true);
    let (held, hold) = holder(&run.journal_dir, LOG_EXTENSION);
    when(Point::Made, hold);
    run.ender(1)();
    assert!(held.borrow().is_some(), "the point was reached");
    assert_eq!(run.seq(), 3, "past the number a cleanup had");
    drop(held.take());
    run.still_whole();
    seams::NO_LINKS.set(false);
}

/// A cleanup that ran whole in that moment of a roll found the old
/// segment held, and left the journal alone.
#[cfg(unix)]
#[test]
fn where_no_second_name_can_be_made_a_cleanup_during_a_roll_takes_nothing() {
    let run = run("roll-cleaned");
    seams::NO_LINKS.set(true);
    when(Point::Made, run.cleanup());
    run.ender(1)();
    assert_eq!(run.seq(), 2);
    run.still_whole();
    seams::NO_LINKS.set(false);
}

// A segment is held before it has its name.

/// A cleanup that ran whole while a first segment had only its temporary
/// name, not yet locked, removed that name: the start makes another, and
/// no segment was there for the cleanup to take.
#[cfg(unix)]
#[test]
fn a_cleanup_before_a_first_segment_is_locked_does_not_stop_the_start() {
    let folders = scratch("temp-cleaned");
    let dir = folders.2.clone();
    when(Point::TempMade, move || {
        assert!(segments(&dir).is_empty(), "no name before the lock");
        assert_eq!(with_ending(&dir, NEW_EXTENSION).len(), 1);
        assert_eq!(clean_up(&dir, &rolling()).unwrap(), Cleaned::default());
        assert!(with_ending(&dir, NEW_EXTENSION).is_empty(), "removed");
    });
    let journal = start_in(&folders.1, &folders.2).unwrap();
    assert!(journal.segment_path().exists(), "held under its name");
    assert!(with_ending(&folders.2, NEW_EXTENSION).is_empty());
    run_of(folders, journal).still_whole();
}

/// A cleanup that holds the temporary name in the moment its journal
/// would lock it: the start makes another, and the next cleanup removes
/// the one it left.
#[cfg(unix)]
#[test]
fn a_temporary_name_held_by_a_cleanup_is_given_up_for_another() {
    let folders = scratch("temp-held");
    let (held, hold) = holder(&folders.2, NEW_EXTENSION);
    when(Point::TempMade, hold);
    let journal = start_in(&folders.1, &folders.2).unwrap();
    assert!(held.borrow().is_some(), "the point was reached");
    assert!(journal.segment_path().exists(), "held under its name");
    assert_eq!(segments(&folders.2).len(), 1);
    assert_eq!(with_ending(&folders.2, NEW_EXTENSION).len(), 1, "left");
    // Held, it is in use for all a cleanup knows.
    let run = run_of(folders, journal);
    run.cleanup()();
    assert_eq!(with_ending(&run.journal_dir, NEW_EXTENSION).len(), 1);
    drop(held.take());
    run.cleanup()();
    assert!(with_ending(&run.journal_dir, NEW_EXTENSION).is_empty());
    run.still_whole();
}

/// The same while the journal rolls: the next segment keeps its number,
/// whether the cleanup removed its temporary name or holds it.
#[cfg(unix)]
#[test]
fn a_cleanup_before_a_next_segment_is_locked_does_not_stop_the_roll() {
    let run = run("roll-temp-cleaned");
    when(Point::TempMade, run.cleanup());
    run.ender(1)();
    assert!(seams::AT.take().is_none(), "the point was reached");
    assert_eq!(run.seq(), 2);
    let (held, hold) = holder(&run.journal_dir, NEW_EXTENSION);
    when(Point::TempMade, hold);
    run.ender(2)();
    assert!(held.borrow().is_some(), "the point was reached");
    assert_eq!(run.seq(), 3);
    assert_eq!(segments(&run.journal_dir).len(), 1);
    drop(held.take());
    run.still_whole();
}

// A cleanup while a journal rolls.

/// A cleanup between the next segment's making and the old one's removal
/// finds both held.
#[test]
fn a_cleanup_while_both_segments_have_their_names_takes_nothing() {
    let run = run("rolled");
    let dir = run.journal_dir.clone();
    let cleanup = run.cleanup();
    when(Point::Rolled, move || {
        assert_eq!(segments(&dir).len(), 2);
        cleanup();
        assert_eq!(segments(&dir).len(), 2, "no live segment is removed");
    });
    run.ender(1)();
    assert_eq!(run.seq(), 2);
    run.still_whole();
}

/// A cleanup between the old segment's removal and its being let go
/// finds the new one held.
#[test]
fn a_cleanup_once_the_old_segments_name_is_gone_takes_nothing() {
    let run = run("unlinked");
    let dir = run.journal_dir.clone();
    let cleanup = run.cleanup();
    when(Point::Unlinked, move || {
        cleanup();
        assert!(!segments(&dir).is_empty(), "no live segment is removed");
    });
    run.ender(1)();
    assert_eq!(run.seq(), 2);
    assert_eq!(segments(&run.journal_dir).len(), 1);
    run.still_whole();
}

/// An old segment whose name could not be removed still lists partials
/// being written: the journal goes on holding it, so no cleanup takes it
/// for a journal that ended, and removes it before its next segment.
#[test]
fn a_segment_whose_name_cannot_be_removed_stays_held() {
    let run = run("stale");
    seams::FAIL_DELETES.set(1);
    run.ender(1)();
    assert_eq!(seams::FAIL_DELETES.get(), 0);
    assert_eq!(run.seq(), 2);
    assert_eq!(segments(&run.journal_dir).len(), 2, "the old one stays");
    let old = segments(&run.journal_dir).remove(0);
    assert_ne!(old, run.journal.segment_path());
    assert!(files::busy(&files::take(&old, 16).unwrap_err()), "held");
    run.cleanup()();
    assert!(run.place.join("a.txt.part").exists());
    assert_eq!(segments(&run.journal_dir).len(), 2);
    // While it cannot be removed, no further segment is started; what is
    // recorded goes into the one there is.
    seams::FAIL_DELETES.set(1);
    run.ender(2)();
    assert_eq!(seams::FAIL_DELETES.get(), 0);
    assert_eq!(run.seq(), 2);
    run.cleanup()();
    // Once it can be, it goes, and the journal rolls: the roll that was
    // due, and the one this partial's end brings.
    run.ender(3)();
    assert_eq!(run.seq(), 4);
    assert_eq!(segments(&run.journal_dir).len(), 1);
    run.still_whole();
}

/// A file already under the next segment's name is none the journal made:
/// it is left as it is and the number after is taken, so the journal
/// rolls now and at every later turn.
#[test]
fn a_file_under_the_next_segments_name_is_stepped_past() {
    let run = run("name-taken");
    let first = run.journal.segment_path();
    let name = log_name(&first).unwrap().0;
    let numbered = |seq: u64| run.journal_dir.join(segment_name(&name, seq));
    std::fs::write(numbered(2), b"").unwrap();
    std::fs::write(numbered(3), b"another's").unwrap();
    run.ender(1)();
    assert_eq!(run.seq(), 4);
    assert_eq!(run.journal.segment_path(), numbered(4));
    assert!(!first.exists());
    assert_eq!(std::fs::read(numbered(2)).unwrap(), b"");
    assert_eq!(std::fs::read(numbered(3)).unwrap(), b"another's");
    // The journal runs: a cleanup leaves all of it.
    run.cleanup()();
    assert_eq!(segments(&run.journal_dir).len(), 3);
    run.still_whole();
}

// What a crash leaves of a segment in the making.

/// A crash before the segment had its own name leaves the temporary one:
/// the next cleanup removes it, and never reads it as a journal, whatever
/// it holds.
#[cfg(unix)]
#[test]
fn a_temporary_name_left_by_a_crash_is_removed_and_never_read_as_a_journal() {
    let run = run("temp-left");
    let (place, journal_dir) = (run.place.clone(), run.journal_dir.clone());
    let segment = run.journal.segment_path();
    let Run {
        scratch: _scratch,
        journal,
        ..
    } = run;
    Rc::try_unwrap(journal).unwrap().crash();
    // A whole segment that lists a partial, under a temporary name only.
    let left = journal_dir.join(format!("1-1.{NEW_EXTENSION}"));
    std::fs::rename(&segment, &left).unwrap();
    // One held is a segment in the making: left.
    let making = journal_dir.join(format!("1-2.{NEW_EXTENSION}"));
    std::fs::write(&making, b"").unwrap();
    let held = files::take(&making, 0).unwrap();
    // One that is a second name of a file elsewhere loses only that name.
    let (second, other) = (
        journal_dir.join(format!("1-4.{NEW_EXTENSION}")),
        place.join("other.txt"),
    );
    std::fs::write(&other, b"other").unwrap();
    std::fs::hard_link(&other, &second).unwrap();
    // One not named as a segment in the making is not a cleanup's.
    let notes = journal_dir.join(format!("notes.{NEW_EXTENSION}"));
    std::fs::write(&notes, b"notes").unwrap();
    // A link under such a name is not followed.
    let (link, target) = (
        journal_dir.join(format!("1-3.{NEW_EXTENSION}")),
        place.join("kept.txt"),
    );
    std::fs::write(&target, b"kept").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert_eq!(
        clean_up(&journal_dir, &rolling()).unwrap(),
        Cleaned::default()
    );
    assert!(recover(&journal_dir, &rolling()).unwrap().is_empty());
    assert!(!left.exists(), "removed");
    assert!(place.join("a.txt.part").exists(), "not read as a journal");
    assert!(making.exists(), "one in use is left");
    assert!(!second.exists());
    assert_eq!(std::fs::read(&other).unwrap(), b"other");
    assert_eq!(std::fs::read(&notes).unwrap(), b"notes");
    assert!(link.symlink_metadata().is_ok());
    assert_eq!(std::fs::read(&target).unwrap(), b"kept");
    drop(held);
    clean_up(&journal_dir, &rolling()).unwrap();
    assert!(!making.exists());
}

/// A crash between the segment's own name and the temporary one's removal
/// leaves one file under both: the next cleanup removes the temporary
/// name and cleans the journal as any other.
#[cfg(unix)]
#[test]
fn a_temporary_name_left_beside_its_segment_does_not_keep_the_journal() {
    let run = run("temp-beside");
    let (place, journal_dir) = (run.place.clone(), run.journal_dir.clone());
    let segment = run.journal.segment_path();
    let Run {
        scratch: _scratch,
        journal,
        ..
    } = run;
    let left = journal_dir.join(format!("1-1.{NEW_EXTENSION}"));
    std::fs::hard_link(&segment, &left).unwrap();
    // While the journal runs, its second name is held with it.
    clean_up(&journal_dir, &rolling()).unwrap();
    assert!(left.exists() && place.join("a.txt.part").exists());
    Rc::try_unwrap(journal).unwrap().crash();
    assert_eq!(clean_up(&journal_dir, &rolling()).unwrap(), cleaned(1, 0));
    assert!(!place.join("a.txt.part").exists());
    assert_eq!(std::fs::read_dir(&journal_dir).unwrap().count(), 0);
}

/// The same seen from a cleanup that opened the old segment before the
/// roll and listed no other: it locks a file its journal still holds, and
/// removes nothing the segment lists.
#[cfg(unix)]
#[test]
fn a_cleanup_that_opened_a_segment_whose_name_then_stays_finds_it_held() {
    let run = run("stale-opened");
    let roll = run.ender(1);
    when(Point::TakeOpened, move || {
        seams::FAIL_DELETES.set(1);
        roll();
        assert_eq!(seams::FAIL_DELETES.get(), 0);
    });
    assert_eq!(
        clean_up(&run.journal_dir, &rolling()).unwrap(),
        Cleaned::default()
    );
    assert_eq!(segments(&run.journal_dir).len(), 2, "the old one stays");
    run.still_whole();
}

/// A journal that ends with nothing recorded removes the segment it still
/// held from before with its last.
#[test]
fn a_journal_that_ends_removes_the_segment_it_still_held() {
    let (_scratch, place, journal_dir) = scratch("stale-ended");
    let journal = start_in(&place, &journal_dir).unwrap();
    let root = Directory::destination(&place).unwrap();
    drop(partial(&root, &journal, 1, "b.txt.part", b"data"));
    seams::FAIL_DELETES.set(1);
    journal.ended(1).unwrap();
    assert_eq!(segments(&journal_dir).len(), 2);
    drop(journal);
    assert_eq!(std::fs::read_dir(&journal_dir).unwrap().count(), 0);
}

/// A next segment that could be neither written nor removed stays held
/// under its name, the old one in charge: the next roll removes it and
/// takes its number.
#[test]
fn a_next_segment_that_failed_and_kept_its_name_stays_held() {
    let run = run("stale-new");
    seams::FAIL_SNAPSHOT.set(true);
    seams::FAIL_DELETES.set(1);
    run.ender(1)();
    assert_eq!(seams::FAIL_DELETES.get(), 0);
    assert_eq!(run.seq(), 1);
    let both = segments(&run.journal_dir);
    assert_eq!(both.len(), 2);
    for path in &both {
        assert!(files::busy(&files::take(path, 16).unwrap_err()), "held");
    }
    run.cleanup()();
    // The roll that was due, then the one this partial's end brings.
    run.ender(2)();
    assert_eq!(run.seq(), 3);
    assert_eq!(segments(&run.journal_dir).len(), 1);
    run.still_whole();
}

/// A cleanup that opened a journal's segment before the journal ended,
/// and locks it after, takes nothing: the name went first.
#[cfg(unix)]
#[test]
fn a_cleanup_that_opened_a_segment_before_its_journal_ended_takes_nothing() {
    let (_scratch, place, journal_dir) = scratch("opened-before-end");
    let journal = start_in(&place, &journal_dir).unwrap();
    when(Point::TakeOpened, move || drop(journal));
    assert_eq!(
        clean_up(&journal_dir, &rolling()).unwrap(),
        Cleaned::default()
    );
    assert!(seams::AT.take().is_none(), "the point was reached");
    assert_eq!(std::fs::read_dir(&journal_dir).unwrap().count(), 0);
}

/// Temporary names an earlier process of this number left are stepped
/// past, more of them than one try at a name goes through.
#[cfg(unix)]
#[test]
fn temporary_names_left_by_an_earlier_process_are_stepped_past() {
    use std::sync::atomic::Ordering;
    let folders = scratch("temp-names");
    // Other tests count on too, in their own folders: a wide run of names.
    let from = files::NEW.load(Ordering::Relaxed);
    let left = |n: u64| {
        folders
            .2
            .join(format!("{}-{n}.{NEW_EXTENSION}", std::process::id()))
    };
    for n in from..from + 200 {
        std::fs::write(left(n), b"left").unwrap();
    }
    let journal = start_in(&folders.1, &folders.2).unwrap();
    assert!(journal.segment_path().exists());
    assert_eq!(std::fs::read(left(from)).unwrap(), b"left", "not written");
    // The next cleanup removes them.
    let run = run_of(folders, journal);
    run.cleanup()();
    assert!(with_ending(&run.journal_dir, NEW_EXTENSION).is_empty());
    run.still_whole();
}
