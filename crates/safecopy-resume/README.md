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
    `forget`, `remove_all`, `kept_bytes`, `reopen` (the partial by
    identity, cut to `have`).
  - `held_from(peer)`: what is kept from one peer, each partial checked
    first (there, long enough, its prefix hashing right) or removed.
  - `expire(now, keep_for, live)`: removes what is past its time, past 64
    files of one peer (its oldest), past 256 in all (the oldest of the peer
    keeping the most), or past a quarter of a disk's free space and what
    they hold (the oldest there); never a record of `live`, the sessions
    still running, moving or paused.
  - `clean_up(journals, ..)`: the journals' cleanup, leaving the partials a
    valid record keeps.
- **The checks:** `receiver_takes(record, &Resumable, same) -> Take`
  (`New`, `Append`, `Restart`, `Resend`, `Refuse`), `sender_checks(record,
  source, &Held, hasher) -> Next` (`Continue { from }`, `Restart`,
  `Dropped`, `Changed`), and `Resent`, the receiver's per-session marks:
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
second, and 4 KiB of `same`. Only what both sides must agree on is fixed:
the chunk (`CHUNK`, 4 MiB) and the wire types' encoding.

Errors are `ResumeError` kinds (`PastSize`, `OffChunk`, `NotHeld`,
`WrongSize`, `Gone`, `Damaged`, `TooMany`, ...);
each program words them, and maps each `Verdict` to its own codes.

## Systems

A record file stays open while its store lasts: on Windows without write
or delete sharing, on Unix under an exclusive `flock`, which other programs
see only if they lock. A store removes a record, and its partial, only
once it holds the record file itself, so a second store on the same
folder leaves the first's alone. On Unix a new record's folder is synced
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

Uses `safecopy-folder`, `safecopy-chunks`, `safecopy-journal` and
`ring` (SHA-256 and the random source); its tests use
`safecopy-manifest`.

Licence: MIT.
