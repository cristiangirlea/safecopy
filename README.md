# safecopy

A Rust library for safe, resumable, verified file transfer, for use in your
own software. It moves files over any async byte stream (tokio's
`AsyncRead` and `AsyncWrite`), so it works with any transport.

**Status:** `safecopy-names`, `safecopy-folder`, `safecopy-manifest`,
`safecopy-chunks`, `safecopy-journal` and `safecopy-resume` are built;
`safecopy-bundle` is empty and comes next.

## What it is for

- Saving files that came from somewhere else without trusting their names:
  no links followed, nothing replaced, nothing written outside the folder.
- Checking every byte: SHA-256 on both sides, verified before a file gets
  its final name.
- Continuing a transfer that was cut off, instead of starting again, and
  starting again whenever there is any doubt.
- Cleaning up after a crash, removing only the files it wrote.

## Crates

Small crates with one job each. Use the umbrella crate, or only the one
you need.

| Crate | Its job |
|---|---|
| [`safecopy-names`](crates/safecopy-names) | Safe file names, numbered names, collisions in one folder |
| [`safecopy-folder`](crates/safecopy-folder) | Writing through a held folder: no links followed, nothing replaced |
| [`safecopy-manifest`](crates/safecopy-manifest) | A bounded list of files to send, and "the source changed" |
| [`safecopy-chunks`](crates/safecopy-chunks) | Verified copying with SHA-256, verify before publish |
| [`safecopy-journal`](crates/safecopy-journal) | Crash records and cleanup by identity |
| [`safecopy-resume`](crates/safecopy-resume) | Resume records and the checks on both sides |
| [`safecopy-bundle`](crates/safecopy-bundle) | Many small files in one stream |
| [`safecopy`](crates/safecopy) | Re-exports all of the above |

Dependencies run one way: `names` needs no other crate; `folder` uses
`names`; `manifest` and `chunks` use `folder`; `journal` uses `folder`
and `manifest` (for its progress); `resume` uses `chunks` and
`journal`; `bundle` uses `chunks` and `resume`.

## Design rules

- Any byte stream; no transport built in.
- Limits are settings with defaults, not constants.
- Errors are typed enums with the numbers they need, never sentences; your
  program words them.
- No program's paths, settings or interface: the caller passes every
  folder and every limit.
- Linux, Windows and macOS are tested in CI; Android is built.
- Minimum Rust version 1.89, edition 2024.
- Permissive dependency licences only, checked by `cargo deny`.
- Parsers of bytes from the other side are fuzzed.

## Licence

MIT. See [LICENSE](LICENSE).
