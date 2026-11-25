# safecopy-chunks

Verified copying of one file.

Copies exactly the declared number of bytes over any async byte stream
(tokio's `AsyncRead` and `AsyncWrite`), hashes them with SHA-256 on both
sides, carries the whole-file hash after the last byte, and verifies the
file before it gets its name.

- `Hasher`: SHA-256 over a whole file, keeping the hash state at its last
  `keep` chunk boundaries, so the hash of a prefix is at hand after a cut
  (`Hasher::at`), and a copy can go on from there (`Hasher::rewound`);
  `hash_prefix` and `Hasher::read` read it from the disk when it is not.
- `copy_exact`: the bytes from `from` to `size`, in slices of at most
  64 KiB. Before each slice it awaits the caller's `Pace`, the one hook a
  program uses to put its files behind other traffic or to pause them;
  this crate never sets a stream's priority.
- `write_trailer` / `read_trailer`: the 32-byte hash after the bytes;
  `read_end` checks nothing follows.
- `write_version` / `read_version` and `write_meta` / `read_meta`: the
  framing version byte, and the caller's metadata block (a length and up
  to `Settings::meta_bytes` bytes this crate does not read).
- `save_verified`: a whole file into a held folder (`safecopy-folder`):
  the partial file created new, the bytes written and hashed, the trailer
  compared, the bytes flushed to the disk (on tokio's blocking pool), then
  the downloaded-file mark, the modified time and the execute bit (as the
  caller's rules allow), then the name, never replacing a file; a file the
  disk makes be copied into its name is flushed again as that copy. A file
  that fails anywhere, its hash included, is removed before it gets a
  name. `Hooks` (`before_create`, `created`, `before_publish`,
  `ended`) let the caller keep a crash record or refuse; an error from any
  removes the partial file. `ended` is not called for a save whose future
  is dropped: its partial file stays for the crash cleanup. `ended` hears
  what became of the partial file
  (`PartialFate`): published, removed, never created, replaced by another
  file under its name, or left because the system refused to remove it,
  with the error's kind and raw code.
- `save_resumable(reader, Target { dir, name, size }, start, keep,
  options, hooks, cancel)`: the same save from `Start::New` or from a
  partial file kept before (`Start::Resume(Box<Kept>)`: the open file,
  its name and a `Hasher` standing after the bytes kept). A resumed save
  cuts the partial to those bytes and appends the stream's from there,
  wherever the handle's position stood; the stream carries the bytes
  from there to the size, and the hash compared with the trailer is of
  the whole file. The caller passes a rule, a closure over the error,
  that says which errors keep the partial file: then it is not removed
  but cut back to its last whole chunk, and `Stopped::kept` holds it,
  open, with `Kept::have` and `Kept::prefix` (the running hash there). A
  cancel and a wrong hash never keep, whatever the rule says, nor does a
  save with no whole chunk written, nor one that failed after the hash
  matched (the flush, the hook before publishing, the publish), nor a
  resumed one whose partial is shorter than its hasher says or whose
  hasher is off a chunk, nor one that cannot be cut back. A write that
  fails within a chunk keeps the whole chunks before it.
  `Hooks::whole(file, have, prefix)` hears of each whole chunk once it
  is written and before the next byte is read, for a resume record kept
  from the first chunk on; when it fails and the rule keeps a hook's
  error, the chunk it was told of is kept too, written and hashed as it
  is. `created` and `whole` get the save's own handle: reading or
  seeking it, there or later through a clone kept, on any thread, moves
  no byte, since every byte is written at its own offset in the file
  (a positioned write) and never at the handle's position; where the
  disk makes the file be copied into its name, that copy reads at its
  own offsets too. A save whose future is dropped leaves no write to
  its partial behind: when the drop returns, none is running and none
  can start, and the handle it wrote through is closed; the drop waits
  for a write already under way on the blocking pool, for as long as
  that one write takes (one slice, 64 KiB, at most; on a disk that
  stalls, as long as it stalls).
  `save_verified` is this save from `Start::New`, keeping nothing. The
  caller answers for a handed-in hasher matching the partial's bytes
  (`safecopy-resume` gives one read from the disk and checked).
- `CandidateHash::hash`: the hash of a file already at the destination
  (`Directory::candidate`), counted only when the file was unchanged before
  and after.

**What a matching hash proves.** The hash is of the bytes as they came
off the stream and were handed to the system to write; a save never
reads its own file back, before or after it gets its name. A match says
the stream was whole and unchanged and that those bytes were written,
not what a disk that fails silently holds, nor what another program
wrote into the partial file meanwhile. For a resumed file the kept bytes
are covered through the hasher handed in, and the caller answers for it
being the hash of what the partial holds then: `safecopy-resume`'s
`Store::reopen_hashed` reads them from the disk through the handle the
save then writes, so they are proven as read at that reopening and the
rest as written.

Errors are `ChunkError` kinds (`CutShort { got, size }` for the file's
bytes, `TrailerCutShort { got }` for the trailer's, `TooLong`,
`Damaged`, `Cancelled`, `PartialShort { have, len }`,
`Folder(Refusal)` for a refusal of `safecopy-folder`'s own, ...) and
`CandidateError { Changed, Unreadable }`, never sentences.

What both sides must agree on is fixed by the framing: the framing version
(`FRAMING`), the trailer, and the chunk size (`CHUNK`, 4 MiB, a constant,
not a setting). The rest are settings with defaults: a receiver keeps 1
boundary, a sender 4, and the metadata block holds at most 4 KiB.

The framing is fuzzed: `fuzz/fuzz_targets/framing.rs`.

## Changes

- **A refusal of `safecopy-folder` keeps its kind.** The folder crate
  refuses some names and opens itself, with a `Refusal` (a name that is
  not one part of a path or names a stream, a link in the way, and the
  rest). A save gave `ChunkError::Io(ErrorKind::Other)` for every one,
  and `PartialFate::Left { kind: Other, raw: None }` for a removal so
  refused.
  - `ChunkError::Folder(Refusal)` is new: what `save_verified` and
    `save_resumable` fail with when the folder crate refuses the
    partial file's name at its creation, or the checked file on its way
    to its name. An arm for `Io(ErrorKind::Other)` that stood for a
    refusal no longer gets it: match `Folder(why)`; `why` is
    `safecopy_folder::Refusal`, itself `#[non_exhaustive]`.
    `ChunkError` was `#[non_exhaustive]` already, so a `match` on it has
    a wildcard arm and still builds. An error of the system is still
    `Io(kind)`.
  - `NoPartialName { tried }` and `NoNameLeft { tried }` are as they
    were. Neither comes from a `Refusal`: the first is this crate's own
    search for a partial name, the second the publish's
    (`PublishError::NoNameLeft`). `Refusal::NoNameLeft` is the folder
    crate's search for a new folder's name, which no save makes.
  - `PartialFate::Left` has a new field, `refused: Option<Refusal>`:
    what the folder crate refused the removal with, `None` when the
    system did. **Breaking** for a caller that names every field of
    `Left` in a pattern or builds one: add `refused`, or end the pattern
    with `..`. `Refusal::Replaced` is still `PartialFate::Replaced`.
    The partial is still there either way.
  - Met in practice: a `Target::name` with a separator in it
    (`LeavesFolder`, at the creation) or a colon on Windows (`Stream`),
    and a `Kept::partial` handed to `Start::Resume` that is not one part
    of a path (`LeavesFolder` at the publish, and again at the removal).

  Tests: `a_save_that_cannot_create_its_partial_says_none_was_created`,
  `a_refused_publish_and_a_refused_removal_say_the_refusal` (a
  test-only switch for the refusals no name gives),
  `an_error_of_the_system_keeps_its_kind_and_a_refusal_its_own`.
- **The search for a free partial name has a limit and sees a cancel.**
  A new save claims `name.part`, then `name.2.part` and so on; it tried
  them without end and looked at its cancel only once a name was found,
  so in a folder holding very many partial files of one name it tried
  them all before its first byte. `SaveOptions` has a new field,
  `partial_tries`, the most names tried, 10 001 by default
  (`safecopy_folder::NAME_TRIES`, as far as a file's own name is
  numbered: a partial name is taken only by another save of that name
  under way or by one a crash left). When every one is taken the save
  fails with the new `ChunkError::NoPartialName { tried }`, and a
  cancel given while names are taken ends it with `Cancelled` before
  the next name is tried; after each taken name the save yields to the
  runtime, so a cancel from another task of its thread is seen too.
  Either way nothing was created: `Hooks::created` is not called and
  `Hooks::ended` hears `PartialFate::NotCreated`
  (`a_save_tries_only_as_many_partial_names_as_its_options_say`,
  `a_cancel_between_partial_names_ends_the_save_with_nothing_created`,
  `a_cancel_given_by_another_task_is_seen_between_partial_names`).
  `save_verified` and `save_resumable` from `Start::New` alike;
  `Start::Resume` creates no file and searches for no name.
  The publish's own search is bounded by
  `PublishRules::name_tries` (see `safecopy-folder`); when it runs out
  the save fails with the new `ChunkError::NoNameLeft { tried }` and its
  partial is removed
  (`a_save_whose_names_are_all_taken_says_how_many_it_tried`); it was
  `Io(Other)` for any publish error this crate did not know.
  **Breaking** for a caller that writes `SaveOptions` out in full: add
  `partial_tries: safecopy_folder::NAME_TRIES`, or end the literal with
  `..SaveOptions::default()`.
- **A hook's "already exists" is never a taken name.** Nothing changed
  here, and a test now holds it: the search moves to the next name only
  on the system's own "already exists" for the partial file; the hooks
  are called outside it, and an error of `before_create` or `created`
  of that kind ends the save with `Hook(AlreadyExists)` at once, the
  partial just made removed and no second one tried
  (`a_hook_that_says_already_exists_fails_the_save_and_is_no_taken_name`).
- **`save_resumable` takes a `Target { dir, name, size }`** where it took
  the three as arguments: `save_resumable(reader, target, start, keep,
  options, hooks, cancel)`. `save_verified` is as it was.
- **A dropped save leaves no write behind.** Each write runs on the
  blocking pool, and one handed there outlived the save's future: under
  a busy pool it could start much later, and on Unix land in the partial
  after a later save had reopened and hashed it, so a sender that sent
  other bytes on the dropped connection and came back at once got them
  written over checked bytes. The writer now shares a gate with each
  write: a write takes it, writes nothing when it is shut, and holds it
  while it writes; dropping the writer takes it and shuts it. Dropping a
  save so waits for a write under way, one slice, where it never
  waited before: on a disk that stalls, for as long as it stalls. Every other end (published,
  cut and kept, failed) shuts the gate before the partial is cut back,
  handed back or published
  (`a_write_still_waiting_when_its_save_is_dropped_writes_nothing`,
  `a_write_under_way_when_its_save_is_dropped_is_whole_when_the_drop_returns`).
- **Every byte is written at its own offset**, by a positioned write
  (`pwrite`, or a write with its offset on Windows). The writer shared
  its position with the handle given to `Hooks::created` and
  `Hooks::whole`, and with the one a `Kept` came with: a hook that read
  or sought the file, a clone of it kept and moved on another thread
  while a chunk was written, or a `Kept` with no byte kept whose handle
  stood past the start, moved where the next bytes landed, under a hash
  that still matched. Nothing done to the handle's position does now
  (`a_hook_that_moves_the_files_position_moves_no_byte`,
  `a_clone_of_the_file_moved_on_another_thread_moves_no_byte`,
  `a_resumed_save_from_zero_writes_from_the_files_first_byte`). The
  copy into the name, where a disk needs one, reads at its own offsets
  as well (`safecopy-folder`).
- **What is kept, pinned by tests:**
  `a_write_that_fails_within_a_chunk_keeps_the_whole_chunks_before_it`,
  `a_partial_that_cannot_be_cut_back_is_removed`,
  `a_flush_or_a_publish_that_fails_after_the_hash_matched_keeps_nothing`,
  `a_short_partial_and_a_wrong_offset_are_removed_whatever_the_rule_says`,
  `an_error_from_whole_that_the_rule_keeps_keeps_the_chunk_it_was_told_of`.
- **A save that starts new or resumes, and can keep its partial.** New:
  `save_resumable(..) -> Result<Saved, Stopped>`,
  `Start { New, Resume(Box<Kept>) }`,
  `Kept { file, partial, hasher }` with `have()` and `prefix()`,
  `Stopped { error, kept }`, `Hooks::whole(file, have, prefix)` (default:
  nothing), `PartialFate::Kept { have }` and
  `ChunkError::PartialShort { have, len }`. `save_verified` is unchanged
  in what it does; it now calls `Hooks::whole` at each whole chunk too.
  `Hooks::before_create` and `Hooks::created` are not called for a
  resumed save, whose caller holds the file already.
  Tests: `a_cut_stream_keeps_whole_chunks_and_a_later_save_appends_and_publishes`,
  `a_cancel_and_a_damaged_hash_keep_nothing_whatever_the_rule_says`,
  `a_cut_before_the_first_whole_chunk_keeps_nothing`,
  `whole_fires_once_per_whole_chunk_and_its_error_fails_the_save`,
  `a_resumed_save_checks_its_stream_and_its_partial_as_a_new_one_does`,
  `a_resumed_save_keeps_nothing_after_a_cancel_a_wrong_hash_or_a_matched_one`,
  `a_resumed_save_that_is_cut_keeps_its_partial_again`.
- **`Hooks::ended` is told what became of the partial file.** It takes a
  second argument, `PartialFate`: `NotCreated`, `Published`, `Removed`,
  `Replaced` (Linux and macOS: another file had its name, and was left),
  or `Left { kind, raw, refused }` when the save failed and the system
  or `safecopy-folder` refused to remove the partial, so a caller keeps its record of it without looking
  for the file by name. `save_verified` still returns the error that
  stopped the save. `Published` says the file has its name, not that the
  partial name is gone: one the system would not remove after the file
  was linked or copied into its name stays, unreported.
  Tests: `a_partial_that_cannot_be_removed_is_reported_left_and_is_still_there`,
  `a_partial_whose_name_another_file_took_is_reported_replaced`,
  `a_save_that_cannot_create_its_partial_says_none_was_created`,
  `hooks_run_in_order_and_an_error_in_any_removes_the_partial`.

Uses `safecopy-folder` and `ring` (SHA-256).

Licence: MIT.
