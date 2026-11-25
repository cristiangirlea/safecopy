# safecopy-bundle

Many small files in one stream.

Sends several small files in one stream over any async byte stream
(tokio's `AsyncRead` and `AsyncWrite`), each with its own size, transfer
key and hash, so one bad file never spoils the others.

- `plan`: which files of a list go bundled (at most `small`, 1 MiB),
  overlapped (up to `alone_over`, 8 MiB) or alone; bundles of at most
  `files` (64) files or `bytes` (8 MiB).
- `BundleWriter::new` writes the table of every file first (ending the
  bundle early rather than pass `table_bytes`, 128 KiB), then `write` or
  `give_up` for each entry in order, then `finish`. A file that changed or
  could not be read is given up with its mark and padded, never resized.
- `BundleReader::open` reads and checks the whole table before any byte of
  any file; `Table::read` takes one `Take { Save, SaveUnlessSame(Digest),
  Skip }` per entry. Admission is one batch: every taken entry's partial
  file is created and `BundleHooks::admitted` called once (journal them in
  one write) before any file's byte is read. Each file is verified before
  it is published under its name or a numbered one, never replacing a
  file. `SaveUnlessSame` gives a candidate's hash (a file already at the
  destination): an equal file's partial is removed and it is
  `AlreadyThere`.
- `ReadOptions::publish` is a `Publish`: the downloaded-file mark, the
  publish's retries and the names it tries. No published file is held
  open: there is no `hold` to set, and `Outcome::Saved` says where each
  file is.
- `BundleHooks::admitted` and `before_publish` get the read's own handle
  of a partial file: reading or seeking it, there or later through a
  clone kept, on any thread, moves no byte, since every byte is written
  at its own offset in the file (a positioned write) and never at the
  handle's position; where the disk makes the file be copied into its
  name, that copy reads at its own offsets too. Writing is another
  matter: an entry's hash is of the bytes as they came off the stream,
  never read back from the disk, so what a hook writes through the
  handle, or cuts from the file, is published as the hook left it, and
  what `before_publish` writes is not flushed by the read. A clone kept
  is a handle the read never closes: the file stays open past the read,
  and on Windows a partial file that was removed (by the read, or by a
  publish that copied it into its name) stays in its folder until the
  clone is closed. `before_create` and `ended` are given no handle.
- `Outcome` per file: `Saved`, `AlreadyThere`, `Damaged`, `Refused`,
  `GivenUp`, `NotReceived`, `Failed(kind)`, `Folder(Refusal)` (the
  folder crate refused its partial or its name), `NoPartialName { tried }`
  (every partial name tried for it was taken), `NoNameLeft { tried }`
  (every name tried for the checked file was taken). A damaged, skipped,
  given-up or failed file, or one left without a name, is read past and
  the bundle reads on. A stream that ends
  early, a bad mark, a byte after the last hash (checked before the last
  file is published) or a cancel ends the bundle: the files published
  stay, every other partial file is removed, and the rest taken are
  `NotReceived`. The cancel is watched while
  `Pace` holds a write too; dropping the read also removes its partial
  files. A writer torn part way through an entry (a stream error, a
  dropped write) refuses every later call (`Torn`), so the stream only
  ever ends short, never misframed.
- In a cut bundle, an entry the receiver skipped stays `Refused` and one
  whose partial file could not be made stays `Failed`, `Folder` or
  `NoPartialName`, where the design
  note calls every entry not published `NotReceived`: the sender resends
  only the `NotReceived` ones, never what was refused.
- `Gate`: at most `at_once` (2) bundles of one transfer read at a time.

Errors are `BundleError` kinds with their numbers (`TooMany`, `TooLarge`,
`TooManyBytes`, `TableTooLong`, `MetaTooLong`, `CountMismatch`, `BadMark`,
`CutShort`, `TooLong`, `Torn`, ...), never sentences.

## The framing

All numbers little-endian: the framing version (one byte); the table's
length (`u32`) and the table: the count (`u32`), then per file its size
(`u64`), its transfer key (16 bytes), its metadata block's length (`u32`)
and the block, which this crate does not read; then per file, in order,
exactly its size in bytes, a mark (0 whole, 1 given up) and 32 bytes (its
SHA-256, or zeros when given up); then the end of the stream. Every bound
is a setting (`BundleLimits`); only the framing is fixed.

## The `quic` feature

Off by default. quinn's streams already are tokio streams, so a bundle
needs nothing more; `quic::stop` stops a stream with a program's code (a
refused bundle, say) and `quic::stopped` reads the code back. The tests
run over `tokio::io::duplex` and, with the feature, over a local QUIC
pair.

The feature takes quinn with no features of its own: the program turns on
the runtime and the crypto provider it uses (`runtime-tokio`,
`rustls-ring` or `rustls-aws-lc-rs`), so it never gets a second rustls
provider from here. `quic` re-exports quinn (`pub use quinn;`), so the
helpers are tied to quinn 0.11: moving to quinn 0.12 is a major release
of this crate.

The table and a whole bundle read are fuzzed:
`fuzz/fuzz_targets/bundle_table.rs`.

## Changes

- **A bundle takes publish rules with nothing to hold.**
  `ReadOptions::publish` took the folder crate's `&PublishRules`, whose
  `hold` asks for each published file back held open
  (`Directory::take_back`). The read passed it on, every entry's publish
  opened the second handle, and `Outcome::Saved` had no place for it: it
  was closed at once, and a caller that set `hold` got nothing to take
  back, with no error.
  - `ReadOptions::publish` is now a `Publish` (of `safecopy-chunks`,
    exported here too): `mark`, `retries`, `retry_wait` and
    `name_tries`, with the folder crate's defaults. It has no `hold`, so
    the option cannot be set and then not given.
  - Why not hand the files back: a held file is a handle kept open, and
    on Unix a second one of its folder. The sender decides how many
    files a bundle has, up to `BundleLimits::files`: 64 by default, so
    128 descriptors held for a bundle and 256 for two, for as long as
    their reports are kept. 256 is the usual limit on open files of a
    macOS process, and a receiver that raised `files` would need a
    second limit, on what is held. On Windows every one of those files could be renamed, deleted
    or opened without delete sharing by no one else, and no folder above
    them renamed, for as long as the report lived. And `Report`,
    `Outcome` and `Saved` are values to keep, copy and compare (`Clone`,
    `PartialEq`, `Eq`), given by reference to `BundleHooks::ended`: with
    open files in them they could be none of those.
  - What the read does with each option, for every entry it saves:
    `publish.mark` marks the file as downloaded before its name;
    `publish.retries` and `publish.retry_wait` try again a publish that
    something else holds a moment; `publish.name_tries` bounds the names
    tried (`NoNameLeft`); `meta_rules` say which of `Target::meta`'s
    time and execute bit are set; `progress` hears of every slice
    written and `pace` is awaited before each; `sync` flushes the file
    before its name, and a copy into the name too; `partial_tries`
    bounds the partial names tried (`NoPartialName`). None is taken and
    left unused.
  - **Breaking:** `ReadOptions::publish` is a `Publish`, by value, where
    it was a `&PublishRules`. A caller that passed rules builds a
    `Publish` from the same four fields; one that set `hold` drops it
    (it never held anything). One that left the field to
    `..ReadOptions::default()` changes nothing. `ReadOptions` keeps its
    lifetime, for `progress` and `pace`.

  Tests: `a_bundle_s_publish_rules_are_a_save_s_and_hold_nothing`,
  `every_entry_is_marked_as_the_publish_rules_ask`,
  `no_published_entry_is_held_open_when_the_read_returns`, and the
  examples in `ReadOptions::publish`'s docs, two of which must not
  build: a `hold` field, and the folder crate's rules passed as the
  option.
- **A refusal of `safecopy-folder` keeps its kind.** The folder crate
  refuses some names and opens itself, with a `Refusal`. An entry so
  refused was `Outcome::Failed(ErrorKind::Other)`, whatever the refusal.
  - `Outcome::Folder(Refusal)` is new: the folder crate refused the
    entry's partial file at its creation, or the checked file on its way
    to its name. The bundle reads on as after any failed entry, and in a
    cut bundle an entry refused at its creation stays `Folder`, as one
    that is `Failed` does. An arm for `Failed(ErrorKind::Other)` that
    stood for a refusal no longer gets it: match `Folder(why)`; `why` is
    `safecopy_folder::Refusal`, itself `#[non_exhaustive]`. The name is
    `Folder` because `Outcome::Refused` is an entry the receiver
    skipped. `Failed(kind)` is still the system's error, and a hook's
    (its error's kind, whatever it carries).
  - `BundleError::Folder(Refusal)` is new, for an error that carries a
    refusal where `Io(kind)` took its kind. No call of this crate ends
    a bundle so today: its errors are the stream's.
  - `Outcome` and `BundleError` were `#[non_exhaustive]` already, so a
    `match` on either has a wildcard arm and still builds.
    `NoPartialName` and `NoNameLeft` are as they were.
  - Met in practice: a `Target::name` with a separator in it
    (`LeavesFolder`) or a colon on Windows (`Stream`).

  Tests: `an_entry_the_folder_crate_refuses_says_the_refusal_and_the_others_are_saved`
  (the publish through a test-only switch),
  `a_hook_refusing_one_publish_or_one_partial_that_cannot_be_made_fails_only_that_file`,
  `an_error_of_the_system_keeps_its_kind_and_a_refusal_its_own`.
- **The search for a free partial name has a limit and sees a cancel.**
  Admission claims `name.part`, then `name.2.part` and so on for each
  taken entry; it tried them without end, and looked at the cancel only
  once every partial file was made. `ReadOptions` has a new field,
  `partial_tries`, the most names tried for one entry, 10 001 by default
  (`safecopy_folder::NAME_TRIES`). An entry whose names are all taken is
  the new `Outcome::NoPartialName { tried }`, with nothing created for
  it, and the bundle reads on as for an entry that is `Failed`
  (`an_entry_tries_only_as_many_partial_names_as_the_options_say`). A
  cancel given while names are taken ends the bundle with `Cancelled`
  before the next name is tried and before `BundleHooks::admitted`: the
  partial files made for earlier entries are removed and
  `BundleHooks::ended` hears the report; after each taken name the read
  yields to the runtime, so a cancel from another task of its thread is
  seen too
  (`a_cancel_between_partial_names_ends_the_bundle_with_nothing_created`,
  `a_cancel_given_by_another_task_is_seen_between_partial_names`).
  A checked entry for which every name up to
  `PublishRules::name_tries` is taken is the new
  `Outcome::NoNameLeft { tried }`, its partial removed
  (`an_entry_whose_names_are_all_taken_says_how_many_it_tried`).
  **Breaking** for a caller that writes `ReadOptions` out in full: add
  `partial_tries: safecopy_folder::NAME_TRIES`, or end the literal with
  `..ReadOptions::default()`.
- **A hook's "already exists" is never a taken name.** Nothing changed
  here, and a test now holds it: the hooks are called outside the
  search, and `before_create` or `admitted` failing with that kind ends
  the bundle with `Hook(AlreadyExists)`, nothing left on the disk
  (`a_hook_that_says_already_exists_ends_the_bundle_and_is_no_taken_name`).
- **A bundle that ends early leaves no write behind, and no partial
  file.** Each slice of a file is written on the blocking pool, and
  `Table::read` returned at a cut, a cancel or a stream error (or was
  dropped) while its last write was still there, holding the partial
  file open. On Windows, where a name goes only with its file's last
  handle, the partial file that had just been removed was still in its
  folder when the read returned, until that write had run; under a busy
  pool that is as long as the pool takes. On every system the write
  could still start after the read was over. Each entry's writer now
  shares a gate with its writes: a write takes it, writes nothing when
  it is shut, and holds it while it writes. A read that ends early waits
  for the write under way before it removes the partial file, and a
  dropped read shuts the gate, which waits for that write too: one slice
  (64 KiB) at most, where neither waited before, and on a disk that
  stalls, for as long as it stalls
  (`a_cut_bundle_returns_with_its_last_write_done_and_no_partial_left`,
  `a_read_dropped_with_a_write_under_way_leaves_no_partial_behind`, each
  holding a write on the pool;
  `a_write_still_waiting_when_its_read_is_dropped_writes_nothing`). A
  flush to the disk is not behind the gate: one still on the pool when
  `cancel` came or the read was dropped ends there, and holds its file
  open until it has.
- **Every byte is written at its own offset**, by a positioned write.
  The writer shared its position with the handle given to
  `BundleHooks::admitted`: a hook that read or sought the file, or kept
  a clone of it and moved that while the file was written, moved where
  the bytes landed, under a hash that still matched. Nothing done to the
  handle's position does now
  (`a_hook_that_moves_a_partial_files_position_moves_no_byte`,
  `a_clone_of_an_entrys_file_moved_on_another_thread_moves_no_byte`). Where
  that position stands when a hook is called is promised by nothing: it
  stood after the last byte written, and now differs by system.

Uses `safecopy-folder`, `safecopy-chunks` and `safecopy-resume`; quinn
only with `quic`.

Licence: MIT.
