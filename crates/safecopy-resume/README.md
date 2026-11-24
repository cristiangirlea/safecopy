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
  - `saving(peer, key) -> Result<Saving, ResumeError>`: one save's claim
    on a file being received, taken before anything is done for a stream
    that brings it and held around the save. `ResumeError::Busy` when a
    save of this store is on that file already (another `Saving`, or its
    receiving record held). `Saving::kept(&held, now, &file)` ends a save
    that kept its partial (the record written at once, then let go),
    `Saving::forget()` one that published or kept nothing; dropped
    without either (the save's future dropped, a panic) it lets the
    record go and removes nothing, so the file is continued later.
  - `forget(peer, key)` is `ResumeError::Busy`, removing nothing, for a
    file this store is receiving: a `Saving` is on it, or its receiving
    record is held. The save's own `Saving::forget` removes it then;
    so do `remove_all` (the person's word) and `expire` for a key left
    out of its `live` list. A sending record is forgotten first, whatever
    is being received: the other side knows the key of a file sent to
    it, and a stream it holds open under that key does not keep the
    sender's record. When one was removed, the answer is `Ok` and a
    file coming under the same key is left to its save. A record the
    call found and could not remove (taken by a reopening meanwhile, or
    held by another store) is `Busy` too: `Ok` says none is left.
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
    the sessions still running, moving or paused. A file that reads as
    no record, and a receiving record whose file is not named as a
    partial, go at once by their own name alone, whatever their age:
    neither counts toward a cap. `kept_bytes` leaves such a record out
    too, and its sum stops at the largest `u64`.
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
`TooMany`, `NotPartial`, ...);
each program words them, and maps each `Verdict` to its own codes.

## How to resume a file

With `safecopy_chunks::save_resumable` and a journal, in order.

**Before anything: the store's folder is the program's alone.** Give
`Store::open` a folder only the program that owns it can write, as its
journals' folder is. A record is not sealed and nothing in it proves the
store wrote it, and no check can: a program able to write that folder
could write the journals too. A record file planted there **cannot** have
a byte written through a link, have a file opened, cut, written or
removed that is not named as a partial, or be written over a file that
is not that record; one naming a file that is no partial counts toward
no cap and goes at the next `expire`. It **can** name any file whose name
ends in the partial ending, in any folder the person can write, by the
folder's identity, the name and the file ID (another program's
unfinished file, or on Unix this store's own running save under another
key): step 6's `reopen_hashed` cuts that file and hands it out once its
first `have` bytes hash to the record's `prefix`, both chosen by whoever
planted the record, and `reopen` does without reading; whatever removes
the record removes that file with it, and until then it counts toward
the caps with the `have` it claims. The hash at the reopening and the
whole file's hash at the end still decide what is published: no byte
the sender did not send gets a name.

**For every stream that brings a file, first:** `let saving =
store.saving(peer, key)?`, before a check's answer is acted on.
`ResumeError::Busy` says a save of this store is on that file already
(the same key on a second connection): refuse the stream and touch
nothing. The guard is held until the save has ended, outside the save's
future, and every end below goes through it.

The first time, on the receiver:

1. `Resent::check(peer, None, &stream, same)` says `Take::New`.
2. `save_resumable(.., Start::New, &keep, ..)` with a rule that keeps a
   cut stream, and the first save's hooks (below).
3. It ends, in one of four ways:
   - `Ok(saved)`: `Journal::done`, then `saving.forget()`.
   - `Stopped { kept: Some(kept), .. }`: `saving.kept(&Held { key, have:
     kept.have(), prefix: kept.prefix() }, now, &kept.file)`, which
     writes the record at once and lets it go; nothing on the journal;
     then drop `kept`.
   - `Stopped { kept: None, .. }`, for whatever reason (a cancel, an
     error the rule does not keep, less than one whole chunk, a wrong
     offset, a partial that could not be cut back): `saving.forget()`,
     and `Journal::ended` if the partial was listed. Without it the
     record would stay held, and offered by `kept_by`, until an expiry.
   - The save's future is dropped, or it panics: nothing to call. The
     guard, dropped with the task, lets the record go as it was last
     written, and the partial is continued or expired later. When the
     save's drop returns, no write of it is running and none can start,
     so nothing of the dropped stream lands in the partial after a later
     reopening; the drop waits for a write already under way, for as
     long as that one write takes (64 KiB at most; on a disk that
     stalls, as long as it stalls).

The hooks of the two saves differ:

| | first save (`Start::New`) | continued save (`Start::Resume`) |
|---|---|---|
| `before_create` | called | not called |
| `created` | `Journal::started` | not called: the caller lists the partial itself (step 6) |
| `whole`, first whole chunk | `Store::receiving(&record, file)` | `Store::keep(peer, &held, now, file, false)` |
| `whole`, each later one | `Store::keep(peer, &held, now, file, false)` | `Store::keep(peer, &held, now, file, false)` |

A continued save has its record already: `Store::receiving` in its
`whole` would write the record anew, with the first save's `created`
time lost. A save that starts new after a restart is a first save.

The record's `at` is `PartialAt::of(&root, &dir, file, partial)` with
the `file` and the `partial` name the `created` hook was given. The
store keeps a record only for a file named as a partial: a name that
ends in its journal settings' `partial_ending` (`.part`, which is what
`save_resumable` names its partials; a program with another ending
passes it with `Store::with_journal_settings`). For any other name
`Store::receiving` is `ResumeError::NotPartial` and writes nothing.

The sender keeps `Store::sending` for the file from its first stream.

At every start, on the receiver: `Store::expire`, then
`Store::clean_up(journals, ..)`. The expiry is also what removes a file
under a record's name that is not that record (see Systems).

When the peer is back:

4. The receiver lists `Store::kept_by(peer)`, which reads no file, and
   the `Held`s go to the sender (`handshake::answer`, or the program's
   own channel).
5. The sender, for each: `Store::sent`, `Source::open`, then
   `sender_checks(record, source, &held, hasher, &settings)`. On `Next::Continue {
   from }` it sends the `Resumable { key, from, size }` prefix and
   `copy_exact` from `from` with the hasher that came back, then the
   trailer.
6. The receiver, with its `saving` taken: `Resent::check(peer, record,
   &stream, same)` says `Take::Append`; `Store::reopen_hashed(peer,
   key)` gives the folder and the `Kept`: the partial open, its prefix
   read and checked through that handle, and its name, `kept.partial`,
   which is the one the first save gave it (`name.part`, or
   `name.N.part`) as the record keeps it.
   `Journal::started(place, &dir, &kept.file, &kept.partial, name, size)`
   lists it in this run's journal; then `save_resumable(reader, Target {
   dir: &dir, name, size }, Start::Resume(Box::new(kept)), &keep, ..)`
   with the continued save's hooks.
7. It ends as at step 3. Published: `Journal::done` and
   `saving.forget()` on the receiver, `Store::forget(peer, key)` on the
   sender.

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
`saving.forget()`, and `Journal::ended` if the partial was listed. The
answer is acted on only with the `saving` in hand: a check answers for
the stream it was given, and the same key may be coming on another
connection, whose record and partial a `forget` would remove under it.
For that reason `Store::forget` itself is `Busy` for a file this store
is receiving, and removes nothing; a caller cannot leave that out, as it
could a question asked before the call.
`ResumeError::Busy` from `saving`, a reopening, a write or a `forget`
means the file is in use, by another process or by a save of this one
(the same key on a second connection): leave it alone, and refuse the
stream. `ResumeError::Unreachable` means its disk is away: nothing is
removed, and it may be tried again.

**Where the blocking calls run.** Every call of the store that reads or
changes records, but the three async ones, is synchronous and does file
work, most of it under the store's lock; none belongs on a thread that
must not wait for a disk. Each lists the records' folder at least once
(some two or three times) and opens the record files a listing shows as
new or changed. Besides that:

| call | file work |
|---|---|
| `received`, `sent`, `saving`, `keeps`, `kept_bytes`, `release`, `release_peer` | the listing only (dropping a `Saving` lists nothing) |
| `Store::open` | makes the folder, reads every record |
| `kept_by` | inspects each kept partial's name in its folder |
| `reopen` | takes the record file, finds the partial's folder by identity, opens the partial and cuts it |
| `receiving` | flushes the partial, writes the record and flushes it, each time |
| `sending` | writes the record and flushes it |
| `keep`, `Saving::kept` | as `receiving` when a rewrite is due (every 64 MiB and second) or at a drop; otherwise the lookup only |
| `forget`, `Saving::forget`, `remove_all`, `expire`, `clean_up` | remove record files and partials; `expire` reads each disk's free space, `clean_up` the journals |

On an async runtime, run them on its blocking pool
(`tokio::task::spawn_blocking`, with the store in an `Arc`) or inside
`tokio::task::block_in_place`. A `Saving` borrows its store and does not
move to another thread's task: take it and end it (`kept`, `forget`)
inside `block_in_place`. `Hooks::whole` is called on the thread that
polls the save: the `Store::keep` in it lists the folder each time, and
when a rewrite is due holds that thread for two flushes; on a
multi-threaded runtime wrap it in `tokio::task::block_in_place`, or give
the save a thread of its own. The crate's async functions
(`reopen_hashed`, `held_hashed`, `held_from`) do the same file work on
the thread that polls them, before, between and after their waits (the
folder listed, a record taken, each partial opened, cut or removed with
its record), and read only a partial's bytes on the blocking pool; they
hold the store's lock across no wait, and their futures are `Send`.

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

**A file under a record's name that is not that record.** Anyone can work
out the name, so another program of the same person can put a file under
it first. The store writes a file it did not make only when the file
shows it is the record: its bytes read as a record of that kind, peer
and key, and it has that one name. Anything else is `ResumeError::Busy`
and is not written, cut or reopened: bytes that read as nothing or as
another record, and a file with a second name (a hard link to some other
file, which a write would have overwritten; counted through the open
handle, `st_nlink` on Unix and the number of links on Windows). An empty
file with one name is a record not yet written, and is taken. A
symbolic link to a file was refused before with the system's error, and
is `Busy` now; a folder or a junction there is still the system's error
(`ResumeError::Io`). `expire` removes a regular file among these by its
name alone, never what a second name of it shows; a link or a folder it
leaves, as a cleanup never removes through one, and its key has no
record until the person removes it. Until then that peer and key has no record, so the file is
received without one and starts from zero if it is cut. The same holds
for a record whose **first** write was torn by a crash: its file reads
as nothing, and blocks its key until the next `expire`. Run `expire` at
every start.

**A record that names a file that is no partial.** A record says where
its partial is by folder, name and file ID, and a file that reads as a
record can be put into the store's folder by another program of the same
person, naming any file of theirs. The store acts on a record's file
only under a partial's name, by its journal settings
(`PartialAt::is_partial`: the ending, more than the ending alone, no
separator, no `name:stream`): any other is never opened, so never cut
or handed out to be written. `kept_by` does not list it; `reopen`,
`reopen_hashed`, `held_from` and `held_hashed` take it for
`ResumeError::Gone` and remove the record alone; `receiving` and `keep`
are `ResumeError::NotPartial` and write nothing. `expire`, `forget` and
`remove_all` remove such a record's own file and leave the file it
names, as a journal's cleanup does; `expire` removes it at once,
whatever its age or its `live` list, and neither it nor `kept_bytes`
counts what it says it keeps. Two stores on one folder must so be
given the same ending: each takes the other's records for these, and
its `expire` removes them.

**A save dropped while it writes.** A save's writes run on the blocking
pool, and one handed there does not end with the future that asked for
it. The save's writer shares a gate with each of them: dropped, it waits
for the write that holds the gate (one slice, for as long as the disk
takes over it) and shuts every later one out. So on every system a write of a dropped save never lands
in the partial after a later save reopened and hashed it, whatever the
other side sent on the dropped connection.

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

Records hold no file contents and are not sealed: the store's folder
must be writable only by the program that owns it. What a record planted
there can and cannot do is said at the top of "How to resume a file":
it gets a file named as a partial cut, continued or removed, never a
file under another name, and no byte published that the sender did not
send.

The handshake and the checks are fuzzed: `fuzz/fuzz_targets/resume_handshake.rs`.

## Changes

- **What a record planted in the store's folder can do is said.** The
  docs said a tampered receiving record could only make a file start
  from zero. It can name any file that ends in the partial ending, in
  any folder the person can write, and have it cut, continued or
  removed. The folder must be writable only by its program; "How to
  resume a file" and the crate docs say what such a record can and
  cannot do, and that the hashes still decide what is published.
- **A dropped save leaves no write behind** (`safecopy-chunks`): on Unix
  a write of a dropped save still on the blocking pool could land after
  a later save had reopened and hashed the partial, with whatever the
  other side had sent on the dropped connection. `Saving`'s docs said
  that was harmless; it was only for an honest sender.
- **`Store::forget` lets go of no record it did not take.** It claimed
  the file with a save's guard, and that guard, dropped, let go of the
  receiving record: one taken by a `reopen` while the `forget` ran lost
  its hold, and a second reopening could cut the partial under the
  first. A `forget` that leaves a record it found now answers `Busy`,
  where it answered `Ok`
  (`a_forget_lets_go_of_no_record_a_reopening_took_meanwhile`).
- **`Store::forget` removes a sending record whatever is being
  received.** The claim was by peer and key with no direction, so the
  other side, which knows the key of a file sent to it, could hold a
  stream open under that key and have the sender's `forget` answer
  `Busy`. The sending record now goes first, held or not; when there
  was one the answer is `Ok`, and a file coming under the same key is
  left to its save
  (`a_sending_record_is_forgotten_while_a_file_comes_under_its_key`).
- **`Store::expire` and `Store::kept_bytes` count only records the
  store acts on.** A record whose file is not named as a partial was
  kept, counted toward every cap with the `have` it claimed, had its
  folder opened for its free space, and could push the oldest real
  partials out. `expire` now removes it at once, by its own name, and
  `kept_bytes` leaves it out
  (`a_planted_record_of_no_partial_pushes_nothing_out_and_goes_at_an_expiry`).
  A record that says it keeps more bytes than its size no longer reads
  as a record
  (`a_record_keeping_more_than_its_size_is_not_read_as_a_record`), and
  the sums of kept bytes stop at the largest `u64`, where they wrapped
  or panicked
  (`kept_bytes_past_what_a_number_holds_do_not_overflow`).
- **Where the blocking calls run** names every call that lists the
  folder or does file work: `saving`, `received`, `sent`, `reopen` and
  `receiving` were missing.
- **Tested:** an empty file with a second name under a record's name is
  not written
  (`an_empty_file_with_a_second_name_under_a_records_name_is_not_written`).
- **The store acts only on a record whose file is named as a partial.**
  `reopen` and `reopen_hashed` opened the file a record named, whatever
  its name, and cut it to the record's `have`: a record file put into
  the store's folder, reading as the right peer and key and naming some
  other file of the person's, had that file cut and handed out to be
  written. The store now opens a record's file only under a partial's
  name, by its journal settings: such a record is `ResumeError::Gone`
  at a reopening or a check and goes, alone; `kept_by` does not list
  it; `expire`, `forget` and `remove_all` remove the record and leave
  the file (`a_forged_record_naming_another_file_cuts_and_opens_nothing`,
  `a_forged_record_naming_a_stream_a_path_or_the_ending_alone_opens_nothing`).
  `receiving` and `keep` write no such record: new kind
  **`ResumeError::NotPartial`**
  (`a_record_of_a_file_not_named_as_a_partial_is_not_written`). The
  ending is `Store::with_journal_settings`' `partial_ending`
  (`a_store_keeps_partials_of_its_own_ending_only`).
- **The store writes only records it can tell are its own.** A record
  file's name can be worked out from its peer and key; a file found under
  it was held and written at its first slot, then cut to a record's
  length, whatever it was, so a hard link put there overwrote the file it
  led to. A file not made by the store is now written only when it reads
  as that record and has one name; any other is `ResumeError::Busy` and
  left to `expire`
  (`a_file_under_a_records_name_that_is_not_its_record_is_not_written`).
  A first write torn by a crash so blocks its key until the next
  `expire` (`a_first_write_torn_by_a_crash_waits_for_the_next_expiry`).
  A symbolic link under a record's name is `Busy`, where it was `Io`
  (`a_link_under_a_records_name_is_not_written_through`).
- **New: `Store::saving(peer, key)` and `Saving`**, the guard a save
  holds: one save per file in a store, and a save whose future is
  dropped, or that panics, lets its record go instead of leaving the
  file in use until the store is dropped
  (`a_save_dropped_or_panicked_leaves_its_file_to_be_continued`).
- **`Store::forget` is `ResumeError::Busy` for a file its store is
  receiving**, taking the file as a save does for as long as it
  removes, where it removed the held record and, on Unix, the partial
  under the save writing it: the same key on a second connection, with a
  check that answered "restart", cut the first connection's save. The
  save forgets through `Saving::forget`; a record held with no save left
  is released first. A sending record is forgotten as before
  (`a_peer_that_connects_again_while_its_file_comes_does_not_stop_the_save`,
  `a_sending_record_is_forgotten_while_its_store_holds_it`).
- **The flow above** names the continued save's hooks, every end of a
  save, and where the blocking calls run.
- **Tested:** a junction or a symbolic link under a kept partial's name
  is not listed (`a_link_under_a_kept_partials_name_is_not_listed`), and
  the futures of `reopen_hashed`, `held_hashed` and `held_from` are
  `Send` (`the_stores_futures_can_run_on_another_thread`).
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
