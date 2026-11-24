# safecopy-folder

Writing through a held folder.

A `Directory` is a folder held open by handle; everything below it is
opened relative to that handle, one name at a time:

- links and junctions are never followed below it (`is_link` tells the
  refusal apart);
- files are only created new, never opened over one already there;
- `Directory::publish` gives a finished file its name without replacing a
  file that took the name meanwhile, and takes `name (2).ext` instead;
- removal never goes through a link or outside the folder; on Windows it
  goes through a handle checked to be the file meant, and on Unix a name
  is removed for an open file only while its device and inode are still
  that file's;
- the downloaded-file mark (Windows' Mark of the Web) goes on before the
  name, when the caller turns it on in `PublishRules`.

Linux and Android publish by linking the open file through procfs, macOS
with its own no-replace rename (`RENAME_EXCL`), Windows by renaming the
open handle; disks that can do none of these get a copy from the open
handle into a name created new. The crate docs say what each system does.

## Differences from the code this came from

This crate was ported from code written for one program. Where the
original code behaved as below, this crate does not:

- Where linking through procfs is denied (Android apps, some
  containers), the original code tried and waited out its retries again
  for every name, 250 ms each with the default rules. A folder handle
  here remembers the failure and goes straight to the rename after it.
- The original code took error code 32 as "file in use" on every system.
  That is Windows' code; on Unix 32 is `EPIPE`, which is now reported at
  once instead of retried.
- The original code removed the staging name after linking, and in
  `discard`, whatever file had the name then, and removed the final name
  when a no-replace rename found a swapped file there. Here a name is
  removed only while it is still the open file; another file is left
  (the swapped one given its old name back) and the call fails with
  `Refusal::Replaced`. `publish` counts a file named before that refusal
  as published. On Unix `publish_path` removes a staging file or an empty
  claim only while it is still the file linked or created; on Windows it
  still removes them by path.
- On Unix, opening a folder whose own name is a symbolic link gave the
  system's raw `ENOTDIR`; it is now refused with `Refusal::Link`.
- `Directory::mark` took any name; it now refuses one that is not one
  part, as every other call does.
- `Directory::finish` checked only the final name, not the staging name
  it removes after linking, so a staging name such as `../x` could remove
  a file outside the folder. It now refuses both with
  `Refusal::LeavesFolder`, before touching anything.

## What is already there, and what a file keeps

- `Meta { modified, executable }`, read at the source with `Meta::read`,
  and `Directory::apply` on the partial file: the modified time by handle
  (a time the disk cannot hold leaves the arrival time and says so), and
  the owner's execute bit, only when the caller's `MetaRules` turn it on
  and only on Linux and macOS. Both are best effort: a disk that refuses
  them (exFAT keeps no Unix permissions) still takes the file, and
  `Applied` says what was set. `Meta::read` on Windows never says
  executable. `Directory::publish_with` takes them in a `Finish`, marks
  the file, then applies these, then names it; when the disk makes it copy
  the file into its name, the copy gets them too, and with `Finish::sync`
  its bytes are flushed to the disk before the publish succeeds.
- `Directory::candidate(name, size)`: a regular file of exactly that name
  (not another case or form of it), no link followed, of exactly that
  size, opened for reading by handle (on Linux and Android, only after a
  path handle showed it is one). `Candidate::unchanged` says whether
  its size, time and the file its name leads to are still the same;
  `safecopy-chunks` hashes it that way.
- `Directory::existing(name)`: a folder of exactly that name, never
  through a link or junction, opened only when this user may add files and
  folders to it.

## Changes

- **The copy into a name reads at its own offsets.** Where a disk has
  neither hard links nor renames, `publish` copies the open file into
  its name; it did so through the file's position, set to the start
  once and shared with every clone of the handle, so a clone read or
  moved on another thread during the copy changed what was published,
  after the caller had checked the bytes. Each slice is now read and
  written at an explicit offset (`pread` and `pwrite`, or their Windows
  forms), and no handle's position is relied on
  (`a_clone_moved_during_a_folder_copy_fallback_changes_no_byte`).
- `Meta::of(&Metadata)`: the `Meta` of metadata already read, as
  `Meta::read` gives it, for a caller that holds it from a listing and
  opens no file (`meta_read_gives_the_owners_execute_bit_on_unix`,
  `meta_read_on_windows_never_says_executable`).
- `Directory::reopen(name)`: a regular file already in the folder, opened
  for reading and writing with no link followed and nothing created, so a
  kept partial file can be appended to after a drop; a folder, a device, a
  link or a junction is refused
  (`reopen_opens_only_a_regular_file_already_there_and_never_a_link`).
  On Windows it is now opened with the right to delete, as a new partial
  is: a reopened partial is published and removed through its handle,
  where both were refused before (the same test).
- A publish dropped while it copies the file into its name (a disk with
  neither hard links nor renames) removes that copy, as a cancel already
  did: nothing is left under the final name
  (`a_publish_dropped_during_a_folder_copy_fallback_leaves_no_copy`).
- **That drop leaves no slice of the copy behind.** Each slice is copied
  on the blocking pool, and one handed there outlived the dropped
  publish, holding the file and the copy open: on Windows, where a name
  goes only with its file's last handle, the copy stayed under the final
  name until the slice had run, and the partial could not be removed
  from its folder until then either; under a busy pool that is as long
  as the pool takes. The copy's slices now share a gate with its claim:
  a slice takes it, copies nothing when it is shut, and holds it while it
  copies; dropping the publish shuts it and closes both handles before
  the copy is removed. The drop so waits for a slice under way, 64 KiB,
  where it never waited before: on a disk that stalls, for as long as it
  stalls (the same test, which holds a slice on the pool, and
  `a_slice_still_waiting_when_its_publish_is_dropped_copies_nothing`).
  The flush of a finished copy is not behind the gate: one still on the
  pool at a drop or a cancel ends there, and on Windows the removed copy
  stays in the folder until it has.
- **A publish that fails says which name it was trying.**
  `PublishError::Name { path, error }` is new: the name being tried when
  the system refused (the one asked for, or `name (2).ext` and so on once
  those before it were taken), as `Published::path` would give it, with
  the system's `io::Error` whole, its kind and raw code. `publish` and
  `publish_with` gave `PublishError::Io` for it before, which is now only
  a failure before any name is tried (the file's time or execute bit;
  `a_publish_that_fails_before_any_name_is_tried_is_an_io_error`).
  `publish_path` returns `Result<Published, PublishError>` where it
  returned `io::Result<Published>`, and every error of its own is a
  `Name` (`a_publish_that_fails_says_which_numbered_name_it_was_trying`,
  `by_path_a_publish_that_fails_says_which_numbered_name_it_was_trying`).

Uses `safecopy-names`.

Licence: MIT.
