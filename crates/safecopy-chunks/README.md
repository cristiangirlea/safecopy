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
  removes the partial file.
- `CandidateHash::hash`: the hash of a file already at the destination
  (`Directory::candidate`), counted only when the file was unchanged before
  and after.

Errors are `ChunkError` kinds (`CutShort { got, size }` for the file's
bytes, `TrailerCutShort { got }` for the trailer's, `TooLong`,
`Damaged`, `Cancelled`, ...) and `CandidateError { Changed, Unreadable }`,
never sentences.

What both sides must agree on is fixed by the framing: the framing version
(`FRAMING`), the trailer, and the chunk size (`CHUNK`, 4 MiB, a constant,
not a setting). The rest are settings with defaults: a receiver keeps 1
boundary, a sender 4, and the metadata block holds at most 4 KiB.

The framing is fuzzed: `fuzz/fuzz_targets/framing.rs`.

Uses `safecopy-folder` and `ring` (SHA-256).

Licence: MIT.
