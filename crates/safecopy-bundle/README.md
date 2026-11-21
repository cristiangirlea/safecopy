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
- `Outcome` per file: `Saved`, `AlreadyThere`, `Damaged`, `Refused`,
  `GivenUp`, `NotReceived`, `Failed(kind)`. A damaged, skipped, given-up or
  failed file is read past and the bundle reads on. A stream that ends
  early, a bad mark, a byte after the last hash (checked before the last
  file is published) or a cancel ends the bundle: the files published
  stay, every other partial file is removed, and the rest taken are
  `NotReceived`. The cancel is watched while
  `Pace` holds a write too; dropping the read also removes its partial
  files. A writer torn part way through an entry (a stream error, a
  dropped write) refuses every later call (`Torn`), so the stream only
  ever ends short, never misframed.
- In a cut bundle, an entry the receiver skipped stays `Refused` and one
  whose partial file could not be made stays `Failed`, where the design
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

Uses `safecopy-folder`, `safecopy-chunks` and `safecopy-resume`; quinn
only with `quic`.

Licence: MIT.
