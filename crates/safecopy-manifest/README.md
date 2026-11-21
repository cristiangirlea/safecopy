# safecopy-manifest

A bounded list of files to send.

- `Manifest` is a checked list of files and folders: each path safe for
  the other side, each folder before what it holds, no two names the same
  apart from case or Unicode form, within the caller's `Limits`. It
  encodes to bytes and decodes from them, checked whole before it is used;
  the count is checked before any room is set aside for it.
- `Sources::list` lists the files and folders a person picked, walking
  folders by handle, and keeps the folders they are in open.
  `Sources::open` opens an entry again from those folders without
  following a link, and refuses anything not as it was listed (another
  file or folder in its place, gone, another size or modified time) with
  `ManifestError::Changed`; `Opened::unchanged` checks again while and
  after it is read.
- `Sources::list_keeping_out` also refuses anything inside, holding or
  linked into folders the caller keeps out, judged by the identity of all
  they hold and by where the folders really are.
- `Listing` walks one folder by handle for sending it whole, and says what
  it skips (links, what cannot be read, what is too deep). `open_entry`
  opens a listed file again from the held folder and refuses one that grew.

Every limit is a field of `Limits`, whose `Default` is: 10,000 entries,
32 parts deep, 1,024 characters a path, 1 TiB of files, 16 MiB encoded,
256 source folders, 10,000 entries walked in the kept-out folders, and
`NameRules::default()` for each name. `Limits::fits` takes the caller's
own check on each path, such as its framing's header limit.

Errors are `ManifestError` kinds; nothing is worded or logged here.

On Windows an online-only file (kept by a cloud service) is listed from
its metadata and fetched only when opened; one a storage service took
offline is refused, and so is a source on a network share. Other systems
have neither.

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

Uses `safecopy-names` and `safecopy-folder`.

Licence: MIT.
