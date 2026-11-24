# safecopy-journal

Crash records for partial files, and cleanup that removes only the file it
recorded.

A `Journal` is kept in a folder you pass, one per transfer (a folder, a
paste, a session's single files): an append-only log of what the transfer
did, in short segment files. From the moment each partial file is created,
and before it gets a byte, the journal records its folder, its name, its
own ID and the entry it is for, synced to the disk. A transfer that goes
by groups (`safecopy-manifest`'s `groups`) also records the groups it opens
and the entries it settles, so its `Progress` stays bounded whatever the
number of files, and can be read back after a crash (`recover`).

After a crash, `clean_up` removes a recorded partial only through a handle
checked to be that very file, never by its name alone: a file renamed
since, another file under the name, a folder or a disk that is gone, is
left alone. A journal still held by a running transfer, in this process or
another, is left alone too, and so is one whose disk is not there: it is
kept for a later cleanup, within the age and count `Settings` give.
`clean_up_keeping` leaves the partials the caller still keeps (a valid
resume record of `safecopy-resume`), and the journals that list them.

## A partial kept to continue later

A caller that keeps a partial file when its stream is cut calls nothing
on the journal for it then, neither `done` nor `ended`: the journal goes
on listing it and stays on the disk when it is dropped. `clean_up_keeping`
leaves the partial while the caller's rule keeps it, and keeps the journal
with it. Once the rule lets go (the resume record expired, was torn or is
gone), the next cleanup still knows the file: it removes the partial, and
then the journal. A transfer that continues the partial in a later run
lists it in that run's journal: `started` takes a file that already has
bytes. In the same run it is still listed and needs no second call. A
caller that removes a kept partial itself calls `ended` if that journal
still runs.

Journals kept this way count toward `Settings::keep_most` and
`Settings::keep_for` together with the ones kept for a disk that is away.
Both bounds must fit what the caller keeps: at least as many journals as
it keeps partials (each may be alone in its journal), for at least as
long as it keeps one. A partial cut and continued in `n` runs is listed
in `n` kept journals, one per run, so `keep_most` is reached with fewer
partials than it counts; the oldest journals go first and the newest that
lists the file stays, so the file is still known to a cleanup. The defaults (512 journals, 30 days) fit a
`safecopy-resume` store's (256 files, 24 hours) with as many journals
again to spare, and `Store::clean_up` raises the two to its own limits
when they are set lower. A journal past either bound is removed and its
partials left: cleanup removes only what a journal proves it wrote. On
Linux and macOS, where an inode is reused once its file is deleted, a
journal kept longer can meet a new partial of the same name and inode in
the same folder; run a cleanup after forgetting kept files so such a
journal does not outlast them. A journal that lists a kept partial now
lives as long as that partial is kept (up to the caller's keep time, and
512 journals by default) where it went at the next cleanup, so the time
in which an old journal can meet such a new partial is that much longer.

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
journal.started(0, &root, &file, OsStr::new("a.txt.part"), "a.txt", 1234)?;
// ... write, verify, publish ...
journal.done(0)?;
```

## The log

A segment starts with a header: the format, its sequence number, the
destination's root and a snapshot of the progress (the mark, the open
groups' settled entries, the live partials and the folders they are in, up
to the root). Then come entries: `Folder` the first time a folder below the
root is used (one level each), `Group` when a group opens, `Started` (or
`StartedMany` for a bundle) when partials are created, `Done` (or
`DoneMany`) when entries are settled, and `Ended` when a partial goes
without being settled. Each entry is framed with its length and a CRC-32C;
a reader stops at the first one cut short, damaged, or that could not
follow the ones before it, so a torn append loses only that entry.

A journal syncs once per `started` or `started_many`, and once per mark
move: then it writes the next segment's snapshot and syncs it (on Unix
its folder too, so the new name outlasts a crash), and only then deletes
the old segment. `group` is written with the next entry,
`done` and `ended` without a sync of their own. A crash between the two
segments leaves both; a reader takes the highest-numbered segment whose
header is whole. A file already under the next segment's name is left as
it is and the number after is taken (up to 64 tries), so a journal's
numbers grow and need not follow one another. A transfer without groups starts its next segment after
every `Settings::roll_every` (256) partials ended or settled. An append
that would pass `Settings::segment_bytes` starts the next segment first,
whose snapshot holds only what is live, and goes there: starts and ends
that never move the mark cannot fill a segment for good.

A published file whose `Done` was lost in a crash is found by `recover`:
its recorded ID under its expected name or a numbered form of it, with its
size, never a name with the partial ending. On Windows that settles it; on
Unix, where an inode is reused as soon as a file is deleted, it is only a
candidate (`Recovered::candidates`), to be settled by a hash comparison,
and the entry is fetched again until then.

`Settings` holds the longest segment (4 MiB) and old record (1 MiB), the
groups and partials a journal holds (`GroupLimits::default()`, 134), the
ending a name must have to be removed (`.part`), the name rules a numbered
name was made by, how many names of a folder `recover` reads (10,000), and
how long and how many journals cleanup keeps for a later try (30 days,
512).

## Systems

- **Windows** records the volume by its `\\?\Volume{...}\` path and serial
  number, and each folder by its file ID; cleanup opens the volume, checks
  its serial, opens the folder by ID, checks it is still below the root,
  opens the partial by its name in it, compares the file ID and deletes
  through that handle. A folder on a
  network share has no volume path, and `Journal::start` refuses it.
- **Linux, macOS and Android** record the root's path, device and inode,
  and each folder below it by name with its device and inode. Cleanup opens
  the root and compares it, then each level by name relative to the one
  above, no link followed, comparing each: a level renamed, replaced or
  turned into a link stops the walk. The partial is opened without
  following a link, its inode compared, and its name removed while it is
  still that file; a swap in that moment is not caught. A journal is held
  by an exclusive `flock`, which other programs see only if they lock.

`files` holds the helpers that create, hold, take and delete such files;
`safecopy-resume` keeps its records with them. A file held (`create`,
`hold`) has one writer and is still read by other processes (`open_read`);
`busy` tells the error of a file another holder has from any other.
`hold` refuses a link and a file with more than one name (a hard link,
counted through the open handle: `st_nlink`, or the number of links
`GetFileInformationByHandle` gives): what it wrote would land in a file
another name shows. `take`, which only reads and removes the name, takes
such a file. `refused` tells those refusals from the system's errors.

## A cleanup beside a running journal

A cleanup may run in one process while a journal is written in another. It
goes by one rule: **a running journal holds every segment it made, from
before the segment has its name until after that name is gone.** A segment
a cleanup holds under its name is therefore no running journal's, and what
it lists was left behind. A journal keeps its side while it starts its
next segment: the new one is held before it is named, so both are named
and held before the old one's name is removed, and that name is removed
before the old one is let go. An old segment whose name cannot be removed
stays held, and the journal starts no further segment until it can be:
entries still go into the segment it has, and one that needs the next
segment to fit fails with the system's error for the removal.

- **Windows** makes or opens a file and holds it in one step, an open that
  shares no writing or deleting. A name is never there with its file not
  yet held, and a cleanup's open of a held segment is refused. Neither
  side has a moment between finding a file and holding it, so nothing more
  is done there.
- **Linux, macOS and Android** lock a file after opening it, and the lock
  is on the file, not on its name. A cleanup (`files::take`) checks, once
  it has the lock, that the file is still the one its name reaches (the
  handle's device and inode against the name's, no link followed, and at
  least one name left): a segment its journal let go of since the cleanup
  opened it has lost its name, and the journal is left alone. A journal
  makes a segment under a temporary name (`<process>-<count>.jnew`), locks
  it, gives it its own name beside the first, never in place of a file
  already there, and removes the temporary one.
- **A temporary name a crash left** is removed by the next cleanup, by
  this rule: it is named as one (`<process>-<count>.jnew`, both numbers),
  the cleanup can lock it, and it is still under that name.
  No age is asked of it. A journal whose temporary name a cleanup removes
  or holds before the journal's own lock sees that under its lock and
  makes another, so the rule costs a running journal one more try and
  never its segment. A temporary name is never read as a journal,
  whatever it holds, and one that is the second name of a segment (a crash
  between the two steps) is removed without touching the segment.
- **Where a folder gives a file no second name** (a file system without
  hard links; a system whose rules refuse them, as Android's do to an
  app), a segment is made under its own name and then locked. A cleanup
  may lock the empty file in between. It lists nothing, so nothing is
  removed for it, and the journal takes the next name (a first segment) or
  the next number (a later one). The empty file stays until a cleanup
  takes it with no journal running under its name.

`files::create`, which `safecopy-resume` makes its records with, still
names a file and then locks it on Unix, and fails with the error `busy`
tells when another had it in between; its caller handles that.
**A journals folder on a network file system** has weaker locks on Unix:
Linux turns this lock into a lock a process holds, so a cleanup in the
process that runs the journal is given it too, and whether another
computer sees it is the server's matter. Keep the journals on a local
disk, or run no cleanup in a process that has a journal open there.

## A folder renamed while its transfer runs

`started` walks from the root's handle down to the folder it is given, by
the path that folder was opened at, no link followed. When that path no
longer reaches the held folder (renamed, moved, another folder or a link
put under its name), the journal reads where the handle is now, walks
that instead, and records the folder under its present names: the
transfer carries on. A folder no longer below the root, or never below
it, is refused (`JournalError::NotBelow`). A folder opened through a link
below the root's path is judged the same way, by where it really is:
recorded under its real names when that is below the root, refused with
`NotBelow` when the link leads outside.

A folder on another volume than the root's is refused on Windows
(`OtherVolume`), where a file ID means a file on one volume only. Unix
does not check it today: a folder on a disk mounted below the root is
walked and recorded like any other, each level with its own device, and
cleanup finds it again by those names and devices.

What the journal does when it does not read where a held folder is now
depends on why:

| | Renamed below the root | Moved out of the root | Place not read |
|---|---|---|---|
| Windows, Linux, Android (procfs mounted) | recorded under its present names | `NotBelow` | `NotBelow`, renamed or moved alike |
| macOS, the BSDs; Linux and Android with no procfs | recorded by identity alone | recorded by identity alone | always: the system cannot say |

- **Windows, Linux and Android read the place.** A reading that fails (a
  file system or a volume that gives a handle no path, a Linux folder
  whose name ends in " (deleted)", procfs mounted and refusing) shows
  nothing, so the folder is not taken on trust: `NotBelow`. An error of
  the first walk that is no sign of a rename stands as it is, a link
  under the name for one (`Folder(Refusal::Link)`).
- **macOS and the BSDs have no way to read it, nor Linux and Android
  with no procfs mounted.** There a folder whose name is gone, or names
  a file or another folder now, is recorded by its identity alone, as
  one `Folder` with an empty name directly below the root. A folder
  moved out of the root cannot be told from one renamed inside it and is
  recorded the same way; cleanup never finds such a folder on these
  systems, so nothing is removed there. A link under its name is refused
  as before (`Folder(Refusal::Link)`), since the link could be how the
  folder was opened.

After a crash, cleanup:

- removes a partial recorded after the rename on Windows, Linux and
  Android, finding its folder under its present names;
- removes a partial recorded before the rename on Windows, and leaves it
  on Unix, which cannot open a folder by its ID: the recorded names no
  longer reach the folder. The same goes for a folder recorded by
  identity alone;
- on Windows, where an ID opens a folder anywhere on its volume, checks
  the folder is still below the root before touching anything: by its
  recorded names, or by where the root and the folder are now, walked
  from the root with no link followed. A folder moved out of the root
  since the crash is left as one that is gone, and its record ends with
  its journal; one whose place cannot be read then is left and its
  journal kept for a later cleanup. A move in the moment between that
  check and the removal is not caught. A
  journal of the whole-record format of before names no root, and is
  cleaned as it always was, by the folder's ID alone;
- checks the partial's own name and ID as ever, so a file of the same
  name where the folder used to be is left: it is another file, in
  another folder.

## Changes

- **A cleanup and a running journal no longer meet at a segment (Unix).**
  Two things could happen with a cleanup in one process and a journal in
  another. A cleanup that opened a segment just before its journal started
  the next one, and locked it just after, read it as an ended journal's
  and removed partials still being written. And a cleanup could lock a
  segment between its making and its journal's lock: a journal then failed
  to start, or left its next segment behind and failed every later one
  with "already exists". Now `files::take` leaves a file that is no longer
  under its name once locked, a segment is locked before it has its name,
  an old segment whose name cannot be removed stays held, and a file under
  the next segment's name is stepped past: see "A cleanup beside a running
  journal". Windows was not affected by either: its open holds the file in
  the same step. Tested with each moment held open by a seam, in one
  process (`meet_tests`). The on-disk format is the same. What is new on
  the disk is the temporary name (`*.jnew`) a segment has for a moment on
  Unix: an older version of this crate that meets one left by a crash
  neither reads nor removes it, and it stays until a cleanup of this
  version runs. A segment number may now be skipped; every version reads
  the highest.
- **Tested:** a file given a second name between `files::hold`'s open
  and its count of names is not held: the count is read from the handle
  after the open
  (`a_file_given_a_second_name_while_it_is_being_held_is_not_held`).
- **Only a name that is a partial's is opened.** `PartialAt::reopen`,
  `try_reopen` and `present` opened whatever name was recorded, where
  `remove` checked it first: a record read from a disk can name any file
  of the person's, and a caller that cut or wrote what `reopen` gave it
  did so to that file. All three now take the `Settings`, as `remove`
  does, and open nothing for a name that does not end in
  `Settings::partial_ending`, is the ending alone, has a separator or
  would open a stream of a file (`name:stream`): `try_reopen` is
  `Left::Name`, the other two `None`. They take the settings, and are
  not left to a check the caller makes first, so that no caller can
  leave it out. New: **`PartialAt::is_partial(&settings)`**, the same
  rule for a caller that opens nothing
  (`a_recorded_name_that_is_not_a_partials_is_never_opened`).
- **`PartialAt::present(&settings)`** gives a kept partial's length when it is
  still the recorded file under its name: no byte read, and no conflict
  with a transfer writing the file. Windows opens it for its attributes
  alone, Linux and Android as a path only; macOS, which has neither,
  opens it for reading without waiting and reads nothing
  (`a_partial_is_found_present_without_being_opened_for_its_bytes`).
  **`PartialAt::try_reopen(&settings)`** is `reopen` with why it failed, and
  `Left` has one more kind, `Busy`: the file is there and open elsewhere
  (Windows), which `reopen` gave as `None`, the same as gone.
- **Cleanup keeps a journal while a partial it lists is kept.**
  `clean_up_keeping` counted a kept partial as left and removed the
  journal, so nothing knew the file once its resume record was gone. The
  journal now stays until the partial is removed, gone or replaced, within
  `keep_for` and `keep_most`; `Cleaned::kept` counts it
  (`cleanup_keeps_a_journal_while_a_partial_it_lists_is_kept`,
  `a_journal_kept_for_the_caller_is_held_to_the_most_kept`).
- **`Settings::keep_most` is 512** by default, where it was 64: room for
  the 256 files a resume store keeps, each alone in its journal.
- **`started` and `started_many` take a partial that has bytes**: one
  kept and opened again is listed by the new run's journal
  (`a_reopened_partial_with_bytes_is_listed_by_the_new_runs_journal`).
  `ended` is for a partial that was removed, not for one kept.
- **`files::hold` refuses a file with more than one name**, where it
  held any regular file: a hard link put under a record's name is no
  longer written through. New: **`files::refused(&error)`**, whether an
  error says the name holds a link, something other than a regular file
  or a file with a second name
  (`a_file_with_a_second_name_is_not_held_and_is_still_taken`). A file
  removed in the moment it was opened, with no name left, is `NotFound`
  as before.
- **Tested:** a junction and a file symbolic link under a partial's name
  are not `present` on Windows
  (`a_partial_is_found_present_without_being_opened_for_its_bytes`).
- **`files::hold(path)`** holds a record file that is already there as
  `files::create` holds a new one, and **`files::busy(&error)`** says
  whether an error means another holder has it
  (`a_held_record_file_is_read_by_others_and_written_by_one`, and on Unix
  `a_record_file_removed_while_it_is_being_held_is_not_held`).

From the first version of this crate (the format and behaviour of the code
it was ported from):

- **The journal is an append-only log of segments** (extension `jlog`),
  not one record rewritten in place: a crash in the middle of a write loses
  at most the entry being written, never the partials before it
  (`a_torn_journal_append_leaves_the_earlier_entries_usable`). Journals of
  the whole-record format (extension `journal`) are still read and cleaned
  up (`cleanup_still_reads_a_journal_in_todays_format`).
- **Linux, macOS and Android record and clean up** partials, by the walk
  above, where they recorded nothing before
  (`cleanup_finds_a_partial_in_a_nested_folder_on_linux`,
  `cleanup_stops_at_a_level_that_became_a_link_on_linux`,
  `cleanup_leaves_a_renamed_or_replaced_file_alone`,
  `a_root_that_cannot_be_found_keeps_its_journal_until_it_can`). There, a
  journal file of the whole-record format is still removed touching
  nothing, now counted as `left` per partial
  (`elsewhere_a_journal_of_the_whole_record_format_is_removed_and_touches_nothing`).
- **The API names entries, not files:** `started(place, dir, file, partial,
  name, size)`, `started_many`, `group`, `done`, `done_many` and
  `ended(place)`, with `progress` and `partials` to read it back. `dir`
  must be the root or a folder opened below it by name: anything else is
  `JournalError::NotBelow` (`a_folder_not_below_the_root_is_refused`). A
  change that does not follow the groups is `JournalError::Progress`.
- **A folder renamed while its transfer runs no longer stops it.**
  `started`, `started_many` and `PartialAt::of` failed for every later
  file in a folder renamed, moved or replaced by name since it was
  opened. They now record it where it is, as the section above says
  (`a_folder_renamed_during_a_transfer_does_not_stop_it`,
  `a_crash_after_a_folder_was_renamed_removes_only_its_own_partials`,
  `a_renamed_folder_whose_place_cannot_be_read_is_recorded_by_identity_alone`,
  `a_link_put_under_a_renamed_folders_name_is_not_followed`,
  `a_file_put_under_a_renamed_folders_name_does_not_stop_it`). What was
  refused still is (`a_folder_not_below_the_root_is_refused`,
  `a_folder_moved_out_of_the_root_is_refused`,
  `a_folder_reached_through_a_link_that_leads_outside_the_root_is_refused`,
  and on Windows
  `a_partial_on_another_volume_than_the_journal_s_is_refused`), with two
  differences where a folder was opened through a link, on Windows, Linux
  and Android: one that leads outside the root is now `NotBelow`, where
  it was `Folder(Refusal::Link)`, and one that leads to a folder really
  below the root is recorded under that folder's real names, where it was
  refused (`a_folder_reached_through_a_link_inside_the_root_is_recorded_where_it_is`). A `Level`
  or `Folder` with an empty name is new in what a segment may hold; the
  format is the same, and a reader of before leaves such a partial on
  Unix and removes it by the folder's ID on Windows. It is only ever one
  level, directly below the root: a segment with an empty name anywhere
  else is read as damaged from there on, and nothing it lists after is
  removed (`an_empty_folder_name_that_is_not_the_only_level_is_damaged`,
  `cleanup_removes_nothing_for_an_empty_folder_name_out_of_place`).
- **A place that cannot be read is not taken for a rename.** Where the
  system reads a held folder's place and the reading fails, `started`
  refuses the folder with `NotBelow` instead of recording it by its
  identity alone, which is kept for the systems that cannot read a place
  at all (`a_folder_whose_place_cannot_be_read_is_not_taken_on_trust`,
  and on Linux
  `a_folder_named_as_deleted_and_moved_out_of_the_root_is_refused`).
- **Cleanup on Windows stays below the root.** A recorded folder's ID
  opened it anywhere on its volume, so a folder moved out of the root
  between a crash and the cleanup had its partial removed there. The
  folder must now still be below the root, or the partial is left
  (`cleanup_leaves_a_partial_whose_folder_was_moved_out_of_the_root`,
  `cleanup_leaves_a_partial_whose_folders_place_cannot_be_read`); one
  renamed below the root is still cleaned
  (`a_crash_after_a_folder_was_renamed_removes_only_its_own_partials`).
- **`JournalError::Io { kind, raw }`** where it was `Io(kind)`: `raw` is
  the system's own error code (`errno`, a Windows error code), `None` for
  an error that has none (`a_system_error_keeps_its_raw_code`).
- **The progress stays bounded:** two open groups' bits and 134 partials,
  a segment within its bound, one sync per start and per mark move
  (`the_progress_stays_bounded_for_10000_entries_flat_and_nested`,
  `a_bundle_is_journalled_in_one_write`,
  `a_crash_after_started_many_before_the_first_byte_removes_every_empty_partial`).
- **Recovery** (`recover`, `Recovered`):
  `a_crash_before_the_mark_moves_resumes_from_the_last_snapshot`,
  `a_crash_after_a_publish_whose_done_was_lost_counts_the_file_settled`,
  `a_reused_inode_does_not_settle_a_lost_done`,
  `a_finished_file_deleted_meanwhile_is_not_fetched_again`,
  `a_crash_between_two_segments_leaves_both_and_cleanup_removes_each_partial_once`,
  `the_highest_numbered_whole_segment_is_read`,
  `a_torn_snapshot_leaves_the_old_segment_in_charge`.
- An append that fails is cut back off the segment, so the entries before
  it stay readable and later appends follow them; one that cannot be cut
  back stops the journal (`JournalError::Broken`)
  (`a_record_write_that_fails_keeps_the_partials_on_the_disk`). An entry
  that would pass `Settings::segment_bytes` even in a fresh segment is
  refused with `TooLarge`
  (`a_partial_whose_entry_would_pass_the_segment_limit_is_not_recorded`);
  one that fits there starts it
  (`a_grouped_journal_full_of_churn_starts_its_next_segment_and_takes_more`,
  `a_journal_without_groups_starts_its_next_segment_after_roll_every_ends`),
  and a longer segment is read only that far
  (`a_segment_longer_than_the_limit_is_read_only_that_far`).
- `clean_up_keeping` leaves the partials the caller keeps; `PartialAt`
  names where one is, removes it by identity and reopens it (only under
  a partial's name, by the settings each call takes), and
  `Location::open` finds a recorded folder again or says why not (`Left`).
- Streams that start partials at once in new folders each get their own
  folder entry (`partials_started_at_once_in_new_folders_are_all_recorded`).
  A next segment that fails to begin loses nothing: the entry that moved
  the mark is written, and the segment begins at the next append
  (`a_next_segment_that_fails_to_begin_is_tried_again_and_loses_nothing`).
  A folder entry numbered after its child is damaged
  (`a_folder_numbered_after_its_child_is_refused`).
- Journal names carry a random part after the process number, so a later
  process with a reused ID never takes the name of a journal kept from
  before.

## Differences from the code this came from

This crate was ported from code written for one program. Where the
original code behaved as below, this crate does not:

- A record was written at any length, while cleanup read only its first
  1 MiB, so a longer record was thrown away and its partial files stayed.
  An entry that would pass the segment's limit is now not recorded, and
  the journal keeps the partials it had.
- A journal file longer than the limit had its first 1 MiB read; it is
  now removed and touches nothing
  (`a_journal_longer_than_the_limit_is_removed_and_touches_nothing`).
- Bytes after a record's last partial were ignored; such a record is now
  damaged, and its journal removed without touching anything
  (`bytes_after_the_last_partial_make_a_record_damaged`).
- A new journal whose name an earlier process had left failed to start;
  it now takes the next name, up to 64 tries
  (`a_journal_name_left_by_an_earlier_process_is_stepped_past`).
- `Journal::ended` logged a record it could not write; it now returns the
  error, and logs nothing.
- Cleanup opened a journal file through a symbolic link and deleted what
  it pointed to. A link or other reparse point named as a journal is now
  opened as itself and left (`a_link_named_as_a_journal_is_never_followed`,
  which on Windows needs a computer that can make file symbolic links and
  skips elsewhere unless `SAFECOPY_REQUIRE_SYMLINKS` is set;
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
  `Settings::keep_for` (30 days) or past `Settings::keep_most` (512) kept
  journals, the oldest first
  (`a_kept_journal_past_its_age_is_removed_and_leaves_its_partials`,
  `past_the_most_kept_journals_the_oldest_are_removed`).
- Cleanup opened whatever volume path a journal held, a network share or
  a pipe included. It now opens only `\\?\Volume{GUID}\` and leaves the
  partials of any other
  (`a_recorded_volume_that_is_not_a_volume_guid_path_is_never_opened`).
- On Windows a partial, or its folder, on another volume than the
  journal's is refused with `JournalError::OtherVolume`; a partial on
  another volume than its own folder's is refused on every system
  (`a_partial_on_another_volume_than_the_journal_s_is_refused`).
- A panic in another thread while it held the journal made every later
  call panic too; the journal now carries on
  (`a_panic_while_the_journal_is_held_does_not_stop_it`).

The log segment is fuzzed: `fuzz/fuzz_targets/journal_log.rs`; the
whole-record format: `fuzz/fuzz_targets/journal.rs`.

Uses `safecopy-names`, `safecopy-folder` and `safecopy-manifest`.

Licence: MIT.
