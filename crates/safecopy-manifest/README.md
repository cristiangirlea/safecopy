# safecopy-manifest

A bounded list of files to send.

- `Manifest` is a checked list of files and folders: each path safe for
  the other side, each folder before what it holds, no two names the same
  apart from case or Unicode form, within the caller's `Limits`. It
  encodes to bytes and decodes from them, checked whole before it is used;
  the count is checked before any room is set aside for it.
- `Sources::list` lists the files and folders a person picked, walking
  folders by handle, and keeps the folders they are in open.
  `Sources::meta` gives an entry's modified time and execute bit
  (`safecopy_folder::Meta`) as the listing read them, with no file opened.
  `Sources::open` opens an entry again from those folders without
  following a link, and refuses anything not as it was listed (another
  file in its place, gone, another size, modified time or execute bit)
  with `ManifestError::Changed`, and a file that is no longer a regular file
  with `ManifestError::NotAFile`; `Opened::unchanged` checks again while
  and after it is read.
- `Sources::list_keeping_out` also refuses anything inside, holding or
  linked into folders the caller keeps out, judged by the identity of all
  they hold and by where the folders really are.
- `Listing` walks one folder by handle for sending it whole, and says what
  it skips (links, what cannot be read, what is too deep); each entry
  carries its time and execute bit (`Listed::meta`). `open_entry` opens a
  listed file again from the held folder and refuses one that grew or
  whose execute bit changed (`Changed`), or is no longer a regular file
  (`NotAFile`); an error's `at` is the part of the path that failed.

- `groups` cuts a list into groups, in order, as a pure function of the
  list, so both sides compute the same: a group ends where the parent
  folder changes once it holds at least 64 entries or 64 MiB, and never
  holds more than 256 entries or 256 MiB (a larger file is a group of its
  own). `Progress` counts what is done of it: the mark (groups settled from
  the first), the open groups' settled entries, one bit each, and the
  places being written. `Groups::may_start(place, &Progress)` applies the
  open-groups rule: the next group opens once every entry of the newest
  open one has started, and at most two are open, so a slow file holds up
  new work by at most one group and the progress stays bounded whatever
  the list's length. The values are `GroupLimits`' defaults.

Every limit is a field of `Limits`, whose `Default` is: 10,000 entries,
32 parts deep, 1,024 characters a path, 1 TiB of files, 16 MiB encoded,
256 source folders, 10,000 entries walked in the kept-out folders, and
`NameRules::default()` for each name. `Limits::fits` takes the caller's
own check on each path, such as its framing's header limit.

Errors are `ManifestError` kinds; nothing is worded or logged here.

On Windows an online-only file (kept by a cloud service) is listed from
its metadata by `Sources`, its time included, and fetched only when
opened; one a storage service took offline is refused, and so is a source
on a network share. Other systems have neither.

The encoding is the id, the count, then each entry's path, kind and size,
as unsigned LEB128 varints and length-prefixed UTF-8: the same bytes as
`postcard` gives for those values.

## Differences from the code this came from

This crate was ported from code written for one program. Where the
original code behaved as below, this crate does not:

- The original always refused a path its own stream header could not
  carry. That check is the caller's here (`Limits::fits`), and none is set
  by default, so a path within the character limit is taken
  (`without_a_check_of_the_callers_a_path_within_the_characters_is_taken`).
- The byte count of a folder's `Listing` could overflow (a panic in a
  debug build); it now stops at `u64::MAX`
  (`a_listing_s_byte_count_never_wraps`).
- A device, pipe or socket in a folder's `Listing` was skipped as
  unreadable; it is now skipped as `Skip::NotFileOrFolder`
  (`a_special_file_in_a_listed_folder_is_skipped_as_one`).
- A picked device, pipe or socket was refused as a link; it is now
  `ManifestError::NotFileOrFolder`
  (`a_picked_special_file_is_refused_as_one`).

## Changes

- **A name over 200 bytes is refused in a list.** `Limits::names` follows
  `safecopy-names`, whose default rules now limit a name to 200 bytes of
  UTF-8 as well as 200 UTF-16 units. A listing cuts such a name (90
  Chinese characters to 66) and numbers it within the limit; a list from
  a sender that kept it whole, as an earlier version did, is refused with
  `ManifestError::Name`. A caller that only writes where names are
  counted in units takes such lists with
  `names: NameRules::default().with_max_bytes(usize::MAX)`.
- **A file's time and execute bit at listing time.** `Sources::meta(n)`
  and the new field `Listed::meta` give `safecopy_folder::Meta
  { modified, executable }` from the metadata the listing already read,
  with nothing more opened for them. `Sources` lists from metadata alone,
  so no file is opened for its data and an online-only file is not
  fetched; a `Listing` opens each entry for reading as it walks, as it
  did before, and reads the `Meta` from that handle. The
  execute bit is the owner's on Linux and macOS and never set on Windows,
  as `Meta::read` has it. A folder has its time and is never executable;
  `Sources::meta` is `None` for a number the list does not have
  (`a_list_gives_each_entry_s_time_and_execute_bit_without_opening_it`,
  `an_online_only_file_s_time_is_given_without_fetching_it`,
  `a_folder_s_listing_gives_each_entry_s_time_and_execute_bit`).
- **A changed execute bit alone is `Changed`.** `Sources::open`,
  `Opened::unchanged` and `Listing::open_entry` refuse a file whose
  owner's execute bit is no longer the listed one, though its size, time
  and identity are the same: the other side was told what the listing
  gave. `open_entry` still does not compare the modified time
  (`a_source_whose_execute_bit_changed_is_changed`).
- New: `groups`, `Groups`, `GroupLimits`, `Progress` and `ProgressError`
  (`a_flat_folder_of_10000_entries_is_cut_into_groups_of_at_most_256_entries_or_256_mib`,
  `a_nested_tree_is_cut_where_the_parent_changes_and_small_folders_share_a_group`,
  `the_mark_moves_only_when_the_oldest_open_group_is_settled`,
  `a_slow_file_holds_up_new_work_by_at_most_one_group`,
  `a_progress_read_back_must_be_one_the_calls_could_make`).
- **`ManifestError::NotAFile { at }`** is new: a listed file that is a
  folder now, or a device, a pipe or a socket, from `Listing::open_entry`
  and `Sources::open`. Both gave `Changed` for a folder before, and
  `open_entry` gave `NotFileOrFolder` for the rest; `Changed` is now a
  file or folder that is still one, but not the one listed
  (`a_listed_file_that_became_a_folder_is_no_longer_a_file`,
  `a_listed_file_that_became_a_special_file_is_no_longer_a_file`,
  `a_source_changed_in_size_time_or_identity_is_changed`,
  `a_source_that_became_a_special_file_is_no_longer_a_file`).
- **`at` names the part that failed.** From `Listing::open_entry`,
  `Link { at }` and `Unreadable { at, .. }` named the whole entry
  whichever part of its path failed. `at` is now that part: the folder on
  the way that became a link or is gone, or the entry itself. A link
  whose `at` is the entry's own path is a swapped file; any shorter path
  is a swapped folder above it
  (`a_directory_replaced_by_a_link_after_listing_does_not_send_outside_content`,
  `a_file_replaced_by_a_link_after_listing_is_refused_with_a_reason`).
- **`ManifestError::Unreadable` has a `raw` field**: the system's own
  error code (`errno`, a Windows error code) beside the kind, `None` for
  an error that has none (`a_whole_drive_or_nothing_is_not_a_list`).

Uses `safecopy-names` and `safecopy-folder`.

Licence: MIT.
