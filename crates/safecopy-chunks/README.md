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
  is. `created` and `whole` get the handle the save writes through:
  reading or seeking it there moves no byte, since each chunk is written
  at its own offset, but a clone kept past the hook's return must not be
  moved.
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
`Damaged`, `Cancelled`, `PartialShort { have, len }`, ...) and
`CandidateError { Changed, Unreadable }`, never sentences.

What both sides must agree on is fixed by the framing: the framing version
(`FRAMING`), the trailer, and the chunk size (`CHUNK`, 4 MiB, a constant,
not a setting). The rest are settings with defaults: a receiver keeps 1
boundary, a sender 4, and the metadata block holds at most 4 KiB.

The framing is fuzzed: `fuzz/fuzz_targets/framing.rs`.

## Changes

- **`save_resumable` takes a `Target { dir, name, size }`** where it took
  the three as arguments: `save_resumable(reader, target, start, keep,
  options, hooks, cancel)`. `save_verified` is as it was.
- **Each chunk is written at its own offset.** The writer shared its
  position with the handle given to `Hooks::created` and `Hooks::whole`,
  and with the one a `Kept` came with: a hook that read or sought the
  file, or a `Kept` with no byte kept whose handle stood past the start,
  moved where the next bytes landed, under a hash that still matched
  (`a_hook_that_moves_the_files_position_moves_no_byte`,
  `a_resumed_save_from_zero_writes_from_the_files_first_byte`).
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
  or `Left { kind, raw }` when the save failed and the system refused to
  remove the partial, so a caller keeps its record of it without looking
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
