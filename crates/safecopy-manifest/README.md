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
- What `safecopy-folder` refuses to open reaches the caller as that
  refusal: a link as `Link`, a device, pipe or socket as
  `NotFileOrFolder` or `NotAFile`, and every other
  `safecopy_folder::Refusal` as `ManifestError::Refused { why, root,
  part }`. What the system itself refused is `Unreadable`, with its kind
  and code.

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

- **`ManifestError::Refused { why, root, part }`** is new: a refusal of
  `safecopy-folder`'s that has no kind of its own here. `Sources::list`,
  `Sources::list_keeping_out`, `Listing::list` and `Listing::open_entry`
  gave `Unreadable { kind: Other, raw: None }` for it before, whatever
  the refusal was. A caller sees:
  - `Unreadable` is now only what the system said. An arm for
    `Unreadable { kind: ErrorKind::Other, .. }` that stood for these
    refusals no longer gets them: match `Refused { why, .. }`.
  - `why` is the `safecopy_folder::Refusal`, itself `#[non_exhaustive]`.
    It is never `Link` (still `ManifestError::Link`) nor
    `NotFileOrFolder` (still `ManifestError::NotFileOrFolder`, or
    `NotAFile` from `open_entry` for the entry itself): those are
    unchanged.
  - Where it was is two numbers, not a path: `root` is which picked path
    it was for, from 0 (always 0 for a `Listing`), and `part` is how many
    names lead to what was refused from the folder held open, which is
    the listed folder for a `Listing` (0 is the folder itself, otherwise
    that many of `Listed::parts`) and the folder a picked path is in for
    `Sources` (0 is that folder, 1 the picked path, 2 a name in it).
  - `ManifestError` was `#[non_exhaustive]` already, so a `match` on it
    has a wildcard arm and still builds; the new kind lands in that arm
    until it is given one.
  - Met in practice: on Windows a file given as the folder to list, or
    as the folder a picked path is in (`NotFolder`; other systems refuse
    that open themselves, and so does Windows for a folder below the one
    held, which stays `Unreadable` with `NotADirectory`); a picked name
    with a NUL (`Nul`); on Windows one naming a stream (`Stream`) or
    longer than one call passes (`TooLong`), and a folder whose listing
    the system returns damaged (`BadListing`, from `Listing::list` for
    the listed folder and from `Sources` for any folder it walks). An
    entry a `Listing` listed is not refused for these when opened again,
    since its names were opened once already; the mapping holds for it
    all the same.
  - Not changed: `Sources::open` gives `Changed` for whatever is refused
    there, the kept-out folders `KeptOut`, and what a `Listing` skips
    inside the listed folder `Skip::Unreadable` (or `Skip::Link`,
    `Skip::NotFileOrFolder`), which carries no kind for a system error
    either.

  (`every_refusal_keeps_its_kind_when_it_is_mapped`,
  `a_listed_entry_refused_when_opened_again_says_the_refusal_and_the_part`,
  `a_name_refused_below_a_picked_folder_counts_the_names_to_it`,
  `a_folder_refused_inside_a_listed_folder_is_skipped_as_unreadable`,
  `a_file_listed_as_a_folder_is_refused_as_no_folder`,
  `a_picked_name_with_a_nul_is_refused_as_one`,
  `a_picked_path_naming_a_stream_is_refused`,
  `a_picked_name_too_long_to_pass_is_refused_as_one`,
  `a_listed_folder_that_became_a_file_is_unreadable_with_the_system_s_code`,
  `a_system_error_is_still_unreadable_with_its_kind_and_code`).
- **A listing keeps the extension of a name it cuts.** `Limits::names`
  follows `safecopy-names`, which now cuts a long name in its stem: 250
  letters and `.txt` are listed as 196 letters and `.txt`, where an
  earlier version listed 200 letters. Two programs on different versions
  list one long name differently, and each takes the other's list: a
  receiver never cuts a listed name again, it refuses one that is not
  safe and within the limits already, and both cuts are. The file is
  saved under the name its sender listed
  (`a_list_cut_by_an_older_or_a_newer_sender_is_taken_by_the_other`).
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
