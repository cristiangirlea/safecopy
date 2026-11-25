# safecopy

A Rust library for safe, resumable, verified file transfer, for use in your
own software. It moves files over any async byte stream (tokio's
`AsyncRead` and `AsyncWrite`), so it works with any transport.

**Status:** every crate is built: `safecopy-names`, `safecopy-folder`,
`safecopy-manifest`, `safecopy-chunks`, `safecopy-journal`,
`safecopy-resume` and `safecopy-bundle`.

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
`journal`; `bundle` uses `folder`, `chunks` and `resume`.

The optional `quic` feature (off by default) adds helpers for quinn's
streams in `safecopy-bundle`; nothing else needs a transport.

## Design rules

- Any byte stream; no transport built in (QUIC helpers only behind the
  optional `quic` feature).
- Limits are settings with defaults, not constants.
- Errors are typed enums with the numbers they need, never sentences; your
  program words them.
- No program's paths, settings or interface: the caller passes every
  folder and every limit.
- Linux, Windows and macOS are tested in CI; Android is built.
- Minimum Rust version 1.89, edition 2024.
- Permissive dependency licences only, checked by `cargo deny`.
- Parsers of bytes from the other side are fuzzed.

## Fuzzing

Each parser has a target in `fuzz/fuzz_targets/` (cargo-fuzz, a nightly
pinned by date as `FUZZ_NIGHTLY` in the workflows). A target is one call
into the `fuzzing` module of the crate that owns the format. That module
is built only for the crate's tests and under `cfg(fuzzing)`, which a
fuzzer's build sets: the tests run what the fuzzer runs, and an ordinary
build has none of it.

- `fuzz/seeds/<target>/`: valid inputs, written by the formats' own
  encoders. A test in each crate fails when a seed no longer decodes or
  is not what the format writes now. Write them again with
  `cargo test --workspace write_fuzz_seeds -- --ignored` and commit them.
- `fuzz/regressions/<target>/`: inputs that once made a target fail. The
  ordinary tests run every seed and every regression input through the
  target's function, on Linux, Windows and macOS.
- `fuzz/corpus/<target>/`: what the fuzzer found, not in git. CI keeps it
  in a cache between runs.

To run a target from its seeds (Linux or macOS, `cargo install
cargo-fuzz@0.13.2 --locked`):

```sh
mkdir -p fuzz/corpus/journal_log
cargo +nightly fuzz run journal_log fuzz/corpus/journal_log fuzz/seeds/journal_log -- -max_total_time=60 -rss_limit_mb=2048
```

When a run fails, libFuzzer leaves the input in `fuzz/artifacts/<target>/`
(in CI: the failed run's artifact `fuzz-artifacts-<target>`). Keep it:

```sh
cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<input>    # fails again
cp fuzz/artifacts/<target>/<input> fuzz/regressions/<target>/<what-it-shows>
cargo test --workspace fuzzing                                      # fails until it is fixed
```

For a smaller input, `cargo +nightly fuzz tmin <target>
fuzz/artifacts/<target>/<input>` writes `minimized-from-<hash>` beside
it: copy that one instead. Commit the input with the fix.

In CI every pull request runs each target for a minute, from its seeds
and the newest cached corpus, and saves what it found as a new cache. A
pull request's cache belongs to that pull request alone; only a run on
`main` adds to the corpus the others start from. Once a week
(`.github/workflows/fuzz-weekly.yml`, Mondays, or started by hand on
`main`) each target runs for 30 minutes, the corpus is minimised and
saved. A crash fails that run and its input is the run's artifact
`fuzz-artifacts-<target>`, kept 14 days; nothing else is posted.

## Licence

MIT. See [LICENSE](LICENSE).
