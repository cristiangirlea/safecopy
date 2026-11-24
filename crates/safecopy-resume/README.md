# safecopy-resume

Resuming an interrupted transfer.

A file cut off by a lost connection keeps what it received and continues
from its last whole chunk; on any doubt it starts again from zero, and a
wrong byte is never kept: every byte is checked against the whole-file
hash before the file gets its name (`safecopy-chunks`).

- **`Store`**: the resume records of one program, in a folder it passes,
  one file per transfer. A receiving record (`Receiving`) keeps, for a file
  of at least one whole chunk (4 MiB), where its partial file is (found
  again by identity, as `safecopy-journal` finds it), `have` (whole
  chunks, flushed to the disk before they are recorded), `prefix` (the
  SHA-256 of those bytes) and the caller's `same` bytes, which every later
  stream must repeat. A sending record (`Sending`) keeps the path, identity,
  size and modified time of the file being sent. A record file has two
  slots, each checksummed with SHA-256; a rewrite goes to the older one, so
  a torn write leaves the other whole.
  - `receiving`, `keep` (rewritten once 64 MiB more is whole and a second
    has passed, or at once at a drop), `sending`, `received`, `sent`,
    `forget`, `remove_all`, `kept_bytes`.
  - `kept_by(peer) -> Result<Vec<Held>, ResumeError>`: what is kept from
    one peer, from the records and the folders' listings alone: each
    record whose partial is still the recorded file under its name and
    at least `have` bytes long, oldest first, at most 64. No byte of a
    partial is read and no unchanged record file is opened, so its cost
    does not grow with the bytes kept: it is the list to give the other
    side at each connection. It removes nothing.
  - `reopen_hashed(peer, key)`: the kept partial as a save continues it
    (`safecopy_chunks::Kept`), opened by identity, its prefix read once
    through that handle and checked against the record, then cut to
    `have`. `reopen(peer, key)` is the same without reading the bytes.
    Either takes the record first and keeps it held while the file is
    continued.
  - `held_from(peer)` and `held_hashed(peer)`: `kept_by` with each
    partial opened and its prefix hashed, the second with the `Hasher`
    that built. They read every kept file, so they are for a check the
    program itself decides to run, not for each connection. A record is
    removed, with its partial, only when the partial is gone or replaced,
    too short, or has other bytes in its prefix; one that is open
    elsewhere or not reachable now stays, and one the store itself holds
    is passed by. No handle is kept after the check: a hasher from
    `held_hashed` is good only where nothing else can have written the
    partial since.
  - `release(peer, key)` and `release_peer(peer)`: stop holding those
    records without removing them. A record a store wrote or reopened is
    in use by it until then: it will not reopen that partial again
    (`ResumeError::Busy`), so one key on two connections never has its
    partial cut under the save appending to it. Release when the save
    has ended without publishing, after the last `keep`; `forget` when
    it is published. A release says the save is over: `release_peer`
    while another connection of that peer still saves takes that
    protection from its files.
  - `expire(now, keep_for, live)`: removes what is past its time, past 64
    files of one peer (its oldest), past 256 in all (the oldest of the peer
    keeping the most), past a quarter of a disk's free space and what
    they hold (the oldest there), or past 256 sending records (the
    oldest); never a record of `live`, the `(peer, key)` of each file of
    the sessions still running, moving or paused.
  - `clean_up(journals, ..)`: the journals' cleanup, leaving the partials a
    valid record keeps and the journals that list them, so a partial whose
    record is later gone or torn is still removed.
- **The checks:** `receiver_takes(record, &Resumable, same) -> Take`
  (`New`, `Append`, `Restart`, `Resend`, `Refuse`), `sender_checks(record,
  source, &Held, hasher, &Settings) -> (Next, Option<Hasher>)` (`Continue { from }`
  with the hasher standing at `from`, or `Restart`, `Dropped`,
  `Changed`), and `Resent`, the receiver's per-session marks:
  a key stopped as damaged or to restart takes one resend from zero with
  the same `same`, and refuses any stream after it.
- **`handshake::{answer, offer}`**: the exchange that decides where each
  file continues, over one bidirectional stream, for a program with no
  control channel of its own. The receiver states what it holds, the sender
  checks each and replies `Continue { from }` or a `Verdict`.
- **The wire types:** `TransferKey` (16 random bytes from the system's
  secure source), `PeerId` (the program's proven identity of the other
  side, opaque here), `Resumable { key, from, size }` (a stream's prefix,
  `write_prefix` / `read_prefix`), `Held { key, have, prefix }` and
  `Verdict { Damaged, Restart, Refused, Dropped, AlreadyThere, Paused }`.
  Their encoding (the `postcard` encoding of their values) is fixed for a
  framing version and held by golden-bytes tests.

Every limit is a field of `Limits`, whose `Default` is: 64 files per peer,
256 in all, a quarter of a disk, 24 hours, a rewrite every 64 MiB and 1
second, 4 KiB of `same`, 256 sending records, and 1024 record files in a
store's index. Only what both sides must agree on is fixed: the chunk
(`CHUNK`, 4 MiB) and the wire types' encoding.

Errors are `ResumeError` kinds (`PastSize`, `OffChunk`, `NotHeld`,
`WrongSize`, `Gone`, `Unreachable`, `PrefixChanged`, `Busy`, `Damaged`,
`TooMany`, ...);
each program words them, and maps each `Verdict` to its own codes.

## How to resume a file

With `safecopy_chunks::save_resumable` and a journal, in order.

The first time, on the receiver:

1. `Resent::check(peer, None, &stream, same)` says `Take::New`.
2. `save_resumable(.., Start::New, &keep, ..)` with a rule that keeps a
   cut stream, and hooks that call `Journal::started` in `created` and,
   in `whole`, `Store::receiving` for the first whole chunk and
   `Store::keep(.., false)` for each later one.
3. It ends with `Stopped { kept: Some(kept), .. }`: call
   `Store::keep(peer, &Held { key, have: kept.have(), prefix:
   kept.prefix() }, now, &kept.file, true)`, and nothing on the journal.
   Then drop `kept` and `Store::release(peer, key)`: until then the
   store takes the file to be in use and will not reopen it.
   `Store::release_peer(peer)` lets go of every record of the peer at
   once, whichever connection was saving it: use it only when no save of
   that peer's still runs.

The sender keeps `Store::sending` for the file from its first stream.

At any later start, on the receiver: `Store::expire`, then
`Store::clean_up(journals, ..)`.

When the peer is back:

4. The receiver lists `Store::kept_by(peer)`, which reads no file, and
   the `Held`s go to the sender (`handshake::answer`, or the program's
   own channel).
5. The sender, for each: `Store::sent`, `Source::open`, then
   `sender_checks(record, source, &held, hasher, &settings)`. On `Next::Continue {
   from }` it sends the `Resumable { key, from, size }` prefix and
   `copy_exact` from `from` with the hasher that came back, then the
   trailer.
6. The receiver: `Resent::check(peer, record, &stream, same)` says
   `Take::Append`; `Store::reopen_hashed(peer, key)` gives the folder and
   the `Kept`: the partial open, its prefix read and checked through
   that handle, and its name, `kept.partial`, which is the one the first
   save gave it (`name.part`, or `name.N.part`) as the record keeps it.
   `Journal::started(place, &dir, &kept.file, &kept.partial, name, size)`
   lists it in this run's journal; then `save_resumable(reader, Target {
   dir: &dir, name, size }, Start::Resume(Box::new(kept)), &keep, ..)`.
7. Published: `Journal::done`, then `Store::forget(peer, key)` on both
   sides. Cut again: step 3 again.

The prefix is hashed at step 6 and not at step 4 for two reasons. A list
that hashed would read every kept file each time the other side
connects. And a hash taken at the listing says nothing of the partial
between then and the reopening, when no handle is on it: bytes changed
in that window would be published under a hash that matches the
sender's.

**What the matching hash proves.** A new file's bytes are hashed as they
are written; a resumed file's kept bytes as they are read at
`reopen_hashed`, through the handle the save then appends through, and
the rest as they are written. Nothing is read back after that, before
or after the file gets its name: a byte changed on the disk later, by a
failing disk or by another program writing the partial, is not seen.

Any other answer of a check (`Restart`, `Resend`, `Refuse`, `Dropped`,
`Changed`, `ResumeError::PrefixChanged`), and a save that ends with
`ChunkError::Damaged`, is the file from zero or not at all:
`Store::forget`, and `Journal::ended` if the partial was listed.
`ResumeError::Busy` from a reopening or a write means the file is in
use, by another process or by a save of this one (the same key on a
second connection): leave it alone, and refuse the stream.
`ResumeError::Unreachable` means its disk is away: nothing is removed,
and it may be tried again.

## Systems

**One folder, several processes.** A record file is held by the store
that wrote it or reopened its partial, as its only writer, until that
store is dropped or lets it go (`release`, `release_peer`): on Windows
open with read sharing only, on Unix under an exclusive `flock`, which
readers do not ask for and which other programs see only if they lock.
Another store on the folder still reads the record, so its lookups,
`keeps`, `kept_by` and `held_from` count the partial as kept; it cannot
write the record nor reopen its partial (`ResumeError::Busy`), and
removes neither. Before a store writes a record it does not hold, or
reopens its partial, it takes the record and reads it again through
that handle; before it removes one, it reads it again too, and leaves
one that changed since its last reading.

**A record file's name** is the SHA-256, in hex, of its kind (receiving
or sending), peer and key, with the ending `resume`. One transfer so
has one name: of two stores that first write a record for the same peer
and key at the same moment, the second finds the file made and is told
`ResumeError::Busy`, as it is for any record another store holds. Records
are still found by what they hold, not by their name, so one under
another name (the 32 random hex digits of before) is read, rewritten in
its place and removed as ever.

**A partial open elsewhere.** On Windows a partial file being written is
open without delete sharing, which shuts out the reopening a check or a
continuation needs; Unix has no such thing. `kept_by` asks for the
file's attributes only and is shut out by nothing. `held_from`,
`held_hashed` and the reopenings take a partial they cannot open for one
in use, never for one that is gone: its record stays. On Unix they open
it, and what keeps a second save off it is the record: held by this
store or another, it is `Busy`.

**Lookups** go through an index in memory, built when the store opens.
Each lookup lists the folder once and opens only the record files the
listing shows as new or changed, by their length or modified time, and
drops the ones that are gone; a record the store holds is never read
again. Every write of a record changes its file's length (its two 16 KiB
slots and up to 255 bytes), because a listing shows a new length at once
on every system, and on Windows shows the new time of a file still held
only later. The index remembers at most `Limits::indexed` files (1024);
a folder with more has the rest read at each lookup.

On Unix a new record's folder is synced
after it is made, so its name outlasts a crash. A sender's file is
opened again without waiting (a pipe put under its path is refused, not
waited on) and must be a regular file. Free space is read with `GetDiskFreeSpaceExW` on
Windows and `fstatvfs` on Unix; a disk that does not say is not held to its
share; removing kept files frees their bytes, so the share is of free
space and what is kept together, counted once per disk. A sending
record's path is kept as the system holds it (UTF-16 on
Windows, bytes on Unix), so a record is read on the system that wrote it; with the rest it must fit
one 16 KiB slot. A handshake list naming one key twice is refused.

Records hold no file contents and are not sealed: whoever can write the
records' folder runs as the person. A tampered receiving record can only
make a file start from zero.

The handshake and the checks are fuzzed: `fuzz/fuzz_targets/resume_handshake.rs`.

## Changes

- **`Store::kept_by(peer)`** lists what is kept without reading it; the
  "how to resume" flow lists with it and hashes at `reopen_hashed`
  (`what_is_kept_is_listed_without_a_byte_of_it_read`).
- **A check no longer removes a file in use.** `held_from` and
  `held_hashed` removed the record of any partial they could not open,
  and took it out of the store's own hands to do so: on Windows, a file
  still being received, or just cut, lost its record when the other side
  connected again. They now remove only a partial that is gone, replaced,
  too short or changed, and pass by every record their store holds
  (`a_check_of_what_is_kept_leaves_a_partial_in_use_and_its_record`,
  `a_peer_that_connects_again_while_its_file_comes_does_not_stop_the_save`).
  Release a record before expecting them to list it.
- **A partial being continued is not reopened a second time.** `reopen`
  and `reopen_hashed` are `ResumeError::Busy` for a record their store
  already holds, where on Unix a second reopening cut the partial under
  the save appending to it
  (`a_partial_being_continued_is_not_reopened_a_second_time`). A
  reopening that fails leaves nothing held: the record is removed with
  its partial when that is gone or too short, and let go otherwise
  (`a_reopening_that_fails_leaves_no_record_held`,
  `a_reopening_dropped_while_it_reads_leaves_the_record_not_held`). New kind:
  `ResumeError::Unreachable`, for a partial whose disk is away, which
  was `Gone`.
- **Record files are named by their transfer, on the disk:**
  `<sha-256 of kind, peer and key>.resume`, where the name was 32 random
  hex digits, so two stores never make two files for one transfer
  (`two_stores_that_first_write_one_record_at_once_make_one_file`,
  `a_record_written_twice_at_once_by_one_store_keeps_the_later_write`).
  Nothing else in a record file changed. This version reads, rewrites
  and removes the files of the version before under their random names.
  The version before reads, rewrites and removes the new files too, since
  it also finds a record by what it holds; it still names its own new
  records at random, so only a folder shared with it can get two files
  for one transfer.
- **`Store::expire`'s `live` is `&[(PeerId, TransferKey)]`**, where it was
  `&[TransferKey]`: a key names a file to one peer only. Sending records
  are capped by count too, `Limits::sending` (256), the oldest first
  (`expire_takes_live_files_by_peer_and_key_and_caps_sending_records`).
- **`sender_checks` returns `(Next, Option<Hasher>)`**, where it returned
  `Next`: with `Continue`, the hasher it checked the prefix with, standing
  at `have`. It takes the `safecopy_chunks::Settings` a hasher it reads
  from the file is made with, as a fifth argument.
- **`Store::reopen` holds the record first** and is `ResumeError::Busy`
  when another store has it, so a partial is never cut under the process
  appending to it.
- **New on `Store`:** `held_hashed`, `reopen_hashed`, `release`,
  `release_peer`, `with_chunk_settings`. `Limits` has two more fields, `sending` and `indexed`;
  `ResumeError` two more kinds, `PrefixChanged` and `Busy`
  (`a_cut_save_keeps_its_partial_and_a_later_one_continues_and_publishes`,
  `a_kept_partial_whose_bytes_changed_is_found_out_and_not_appended_to`,
  `a_released_record_is_another_stores_to_take_over`).
- **A record another store holds is read.** An existing record was taken
  with no sharing, so another process's store counted its partial as not
  kept and its cleanup removed the file. It is now held for writing only
  (`two_stores_on_one_folder_read_each_others_records_and_write_only_their_own`).
- **Lookups no longer read every record file**: the index above
  (`a_lookup_among_many_records_opens_only_the_files_that_changed`). A
  record file is up to 255 bytes longer than its two slots; a version of
  this crate from before does not read such a file, and its `expire`
  removes one it can take: do not share a folder with it
  (`a_record_is_not_removed_by_an_older_reading_of_it`,
  `a_record_write_that_fails_leaves_no_file_behind`).
- **`Store::clean_up` keeps the journal of a kept partial**, and holds the
  journals kept to at least `Limits::total` and `Limits::keep_for`
  (`a_kept_partial_whose_record_is_torn_leaves_no_orphan_after_cleanup`).

Uses `safecopy-folder`, `safecopy-chunks`, `safecopy-journal` and
`ring` (SHA-256 and the random source); its tests use
`safecopy-manifest`.

Licence: MIT.
