# safecopy-journal

Crash records for partial files, and cleanup that removes only the file it
recorded.

A `Journal` is kept in a folder you pass, one file per transfer. From the
moment each partial file is created, and before it gets a byte, the journal
records the file's folder, name and file ID, with the folder's volume.
After a crash, `clean_up` removes a recorded partial only through a handle
checked to be that very file, never by its name alone: a file renamed
since, another file under the name, a folder or a disk that is gone, is
left alone. A journal still held by a running transfer, in this process or
another, is left alone too, and so is one whose disk is not plugged in:
it is kept for a later cleanup, within the age and count `Settings` give.

```rust
use std::ffi::OsStr;
use std::path::Path;
use safecopy_folder::Directory;
use safecopy_journal::{Journal, Settings, clean_up};

let settings = Settings::default();
let journals = Path::new("journals"); // your program's own folder
clean_up(journals, &settings)?;       // after a crash, at the next start

let root = Directory::destination(Path::new("target folder"))?;
let journal = Journal::start(journals, &root, &settings)?;
let file = root.create(OsStr::new("a.txt.part"))?;
journal.started(&root, &file, OsStr::new("a.txt.part"))?;
// ... write, verify, publish ...
journal.ended(&root, OsStr::new("a.txt.part"))?;
```

## Systems

- **Windows** records and cleans up: the volume is opened again by its
  `\\?\Volume{...}\` path and its serial number checked, the folder by its
  file ID, the partial by its name in that folder; its file ID is compared
  and it is deleted through that handle. A folder on a network share has
  no volume path, and `Journal::start` refuses it.
- **Linux, macOS and Android** cannot open a folder by its ID: a journal
  there records nothing and writes no file, and `clean_up` only removes
  the journal files it finds.

Each record is rewritten whole on every change: the volume path, its serial
number and the partials, as unsigned LEB128 varints and length-prefixed
UTF-8. `Settings` holds the longest record (1 MiB), the ending a name
must have to be removed (`.part`), and how long and how many journals
cleanup keeps for a later try (30 days, 64).

## Differences from the code this came from

This crate was ported from code written for one program. Where the
original code behaved as below, this crate does not:

- A record was written at any length, while cleanup read only its first
  1 MiB, so a longer record was thrown away and its partial files stayed.
  A partial whose record would pass `Settings::record_bytes` is now not
  recorded: `Journal::started` fails with `JournalError::TooLarge` and the
  journal keeps the partials it had, on the disk and in memory
  (`a_partial_whose_record_would_pass_its_limit_is_not_recorded`).
- Bytes after a record's last partial were ignored; such a record is now
  damaged, and its journal removed without touching anything
  (`bytes_after_the_last_partial_make_a_record_damaged`). So is a journal
  file longer than the limit, of which the first 1 MiB was read before
  (`a_journal_longer_than_the_limit_is_removed_and_touches_nothing`).
- A new journal whose name an earlier process had left failed to start;
  it now takes the next name, up to 64 tries
  (`a_journal_name_left_by_an_earlier_process_is_stepped_past`). Journal
  files are named `<process>-<count>.journal`; cleanup still reads any
  `.journal` file in the folder.
- `Journal::ended` logged a record it could not write, and did nothing
  when the folder's identity could not be read; it now returns either as
  an error, and logs nothing
  (`a_record_write_that_fails_keeps_the_partials_on_the_disk`).
- A record was rewritten by emptying the file first, so a write that
  failed (a full disk) lost every partial it held. The room is now taken
  before the old bytes change, and a failed write puts the earlier record
  back (`a_record_write_that_fails_keeps_the_partials_on_the_disk`). A
  crash in the middle of a rewrite can still leave a damaged record, which
  cleanup removes, leaving its partials: the record is rewritten in place,
  as before.
- Cleanup opened a journal file through a symbolic link and deleted what
  it pointed to. A link or other reparse point named as a journal is now
  opened as itself and left (`a_link_named_as_a_journal_is_never_followed`,
  which needs a computer that can make file symbolic links and skips
  elsewhere unless `SAFECOPY_REQUIRE_SYMLINKS` is set;
  `a_junction_named_as_a_journal_is_never_followed`).
- Cleanup removed a journal whatever became of its partials, so one on a
  removable disk not plugged in at that start was never removed. A journal
  whose volume cannot be opened, or with a partial that could not be
  removed for another reason than being gone or replaced (a file in use),
  is now kept for a later cleanup and removed once each partial is
  removed, gone or replaced
  (`a_journal_whose_volume_cannot_be_opened_is_kept_until_it_can`,
  `a_partial_in_use_keeps_its_journal_for_a_later_cleanup`). A kept
  journal is removed, its partials left, once it is older than
  `Settings::keep_for` (30 days) or past `Settings::keep_most` (64) kept
  journals, the oldest first
  (`a_kept_journal_past_its_age_is_removed_and_leaves_its_partials`,
  `past_the_most_kept_journals_the_oldest_are_removed`).
- Cleanup opened whatever volume path a journal held, a network share or
  a pipe included. It now opens only `\\?\Volume{GUID}\` and leaves the
  partials of any other
  (`a_recorded_volume_that_is_not_a_volume_guid_path_is_never_opened`).
- `Journal::started` recorded a partial on any volume, though a file ID
  names a file on one volume only. A partial, or its folder, on another
  volume than the journal's is now refused with
  `JournalError::OtherVolume`
  (`a_partial_on_another_volume_than_the_journal_s_is_refused`).
- A panic in another thread while it held the record made every later
  call panic too; the journal now carries on with the record as it was
  (`a_panic_while_the_record_is_held_does_not_stop_the_journal`).

Uses `safecopy-folder`.

Licence: MIT.
