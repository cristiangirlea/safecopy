# AGENTS.md

Guide for any agent (or person) working in this repository.

## What this is

safecopy: a Rust library for safe, resumable, verified file transfer over
any async byte stream, MIT licensed, for anyone to use in their own
software. A Cargo workspace of small crates with one job each. See
[README.md](README.md) for the crates and how they depend on each other.

## Layout

- `crates/<name>/`: one crate each (`src/lib.rs`, `README.md`, `Cargo.toml`
  inheriting from the workspace). `crates/safecopy` re-exports the rest.
- `Cargo.toml`: the workspace: edition 2024, `rust-version = "1.89"`,
  shared metadata and path dependencies. `Cargo.lock` is committed.
- `fuzz/`: cargo-fuzz targets, its own workspace, nightly only. Each
  calls its crate's `fuzzing` module (built for tests and `cfg(fuzzing)`
  alone). `seeds/` and `regressions/` are committed, `corpus/` is not.
- `deny.toml`: licence, advisory, ban and source rules for `cargo deny`.
- `.github/workflows/ci.yml`: CI on GitHub's runners (the repository is
  public): Linux, Windows and macOS. `fuzz-weekly.yml`: the long fuzz run.

## What CI runs

On every pull request, and on pushes to `main`:

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets --features quic -- -D warnings  # Linux
cargo test --locked --workspace --features quic                                 # Linux
cargo check --locked -p safecopy-bundle --features quic                         # Linux, the library alone
cargo deny check
cargo +1.89 check --locked --workspace --all-targets        # minimum Rust version
cargo +1.89 check --locked --workspace --all-targets --features quic
cargo check --locked --target aarch64-linux-android --workspace
cargo +nightly fuzz run <target> fuzz/corpus/<target> fuzz/seeds/<target> -- -max_total_time=60 -rss_limit_mb=2048   # Linux, per target
```

fmt, clippy, test and the minimum Rust version run on Linux, Windows and
macOS; the rest on Linux. The `quic` feature (quinn, off by default) is
linted and tested once, on Linux; its minimum Rust version check runs on
all three. On Windows the tests run with
`SAFECOPY_REQUIRE_JUNCTIONS=1` and `SAFECOPY_REQUIRE_SYMLINKS=1`, so a
link test that cannot make its link fails instead of skipping. Set the
first locally too; set the second only where file symbolic links can be
made (Developer Mode or an administrator). On Linux they run with
`SAFECOPY_REQUIRE_SECOND=1`: a test fails when a file's birth time and
inode generation are not both read. Set it only where the build folder
is on ext4, xfs, btrfs or f2fs.
The Android check compiles `ring`'s C with the NDK: point
`CC_aarch64_linux_android` and `AR_aarch64_linux_android` at the NDK's
`aarch64-linux-android24-clang` and `llvm-ar` (CI uses the runner's NDK).
The fuzz nightly is pinned by date in both workflows. Run the same before
every push.

Fuzzing (commands in the README's section): `cargo test` runs every seed
and every input of `fuzz/regressions/<target>/` through the target's
function, and fails when a seed is not what the format writes now
(`cargo test --workspace write_fuzz_seeds -- --ignored` writes them
again). A crash's input is committed there with its fix. CI caches each
target's corpus; `fuzz-weekly.yml` runs 30 minutes per target on `main`,
and a crash is the failed run's artifact `fuzz-artifacts-<target>`.

## Working rules

- One purpose per branch. Larger work is a feature branch with branches
  merged into it.
- A branch finishes everything its issue asks.
- Every defect, security point or missing test that review finds in what
  the branch touches is fixed in the branch before the push, blocking or
  not. Nits (wording, naming, style, "could also") are dropped. Only real
  work outside the branch, or a decision for the owner, becomes an issue.
- Reviews are text, commits are changes: findings and the reasons for
  fixes go into the review on the pull request, not into a commit per
  finding or per round.
- Merge `main` into the branch once before the final checks, so the local
  checks see what CI tests.
- Changes reach `main` only through a pull request with CI green. No
  pushes to `main`, no force pushes, no auto-merge.
- No AI attribution lines in commits or pull requests.
- Public repository: no local paths, user names, emails, private
  repositories or other products in code, docs, tests or commit messages.

## Library rules

- **No program's text, paths or UI.** No settings folder, no Downloads, no
  environment variables read, no logging of file names. The caller passes
  every folder and every limit.
- **Typed errors.** Every error is an enum kind with the numbers it needs,
  never a sentence a person reads; each program words them itself.
- **Limits are settings with defaults**, fields of a settings struct, never
  constants a user must fork to change. Only what both sides of the
  framing must agree on is fixed.
- **Transport-agnostic.** Bytes move through tokio's `AsyncRead` and
  `AsyncWrite`; transport helpers sit behind optional features, off by
  default.
- **Dependencies run one way**, as the README's graph says; no crate
  depends on a program that uses it.
- Every parser of bytes from the other side has a fuzz target in `fuzz/`,
  with seeds, and its body in the crate's `fuzzing` module.
- Where a system lacks a guarantee, the crate's docs say what it does
  instead.
- New dependencies carry permissive licences only (see `deny.toml`).
- Comments are short and say why.
