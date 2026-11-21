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
resume record of `safecopy-resume`).

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
header is whole. A transfer without groups starts its next segment after
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
how long and how many journals cleanup keeps for a later try (30 days, 64).

## Systems

- **Windows** records the volume by its `\\?\Volume{...}\` path and serial
  number, and each folder by its file ID; cleanup opens the volume, checks
  its serial, opens the folder by ID and the partial by its name in it,
  compares the file ID and deletes through that handle. A folder on a
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
`safecopy-resume` keeps its records with them.

## Changes

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
  names where one is, removes it by identity and reopens it, and
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
  `Settings::keep_for` (30 days) or past `Settings::keep_most` (64) kept
  journals, the oldest first
  (`a_kept_journal_past_its_age_is_removed_and_leaves_its_partials`,
  `past_the_most_kept_journals_the_oldest_are_removed`).
- Cleanup opened whatever volume path a journal held, a network share or
  a pipe included. It now opens only `\\?\Volume{GUID}\` and leaves the
  partials of any other
  (`a_recorded_volume_that_is_not_a_volume_guid_path_is_never_opened`).
- A partial, or its folder, on another volume than the journal's is
  refused with `JournalError::OtherVolume`
  (`a_partial_on_another_volume_than_the_journal_s_is_refused`).
- A panic in another thread while it held the journal made every later
  call panic too; the journal now carries on
  (`a_panic_while_the_journal_is_held_does_not_stop_it`).

The log segment is fuzzed: `fuzz/fuzz_targets/journal_log.rs`; the
whole-record format: `fuzz/fuzz_targets/journal.rs`.

Uses `safecopy-names`, `safecopy-folder` and `safecopy-manifest`.

Licence: MIT.
