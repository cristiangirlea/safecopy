---
name: check
description: Run locally what CI runs (fmt, clippy, tests, cargo deny, minimum Rust version, Android build, fuzz smoke run). Use before every push.
---

# check

Run these from the repository root, in order, and stop at the first
failure. Report each command and whether it passed.

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo deny check
cargo +1.89 check --locked --workspace --all-targets
cargo check --locked --target aarch64-linux-android --workspace
```

Then the fuzz job, once per target in `fuzz/fuzz_targets/`, with the
nightly that `FUZZ_NIGHTLY` in `.github/workflows/ci.yml` names:

```sh
cargo +<FUZZ_NIGHTLY> fuzz run <target> -- -max_total_time=60 -rss_limit_mb=2048
```

Notes:

- A tool that is missing (`cargo-deny`, `cargo-fuzz`, the 1.89 toolchain,
  the `aarch64-linux-android` target, the pinned nightly) is reported as
  not run, never skipped silently. Install with `cargo install cargo-deny`,
  `cargo install cargo-fuzz`, `rustup toolchain install 1.89`,
  `rustup target add aarch64-linux-android`,
  `rustup toolchain install <FUZZ_NIGHTLY>`.
- CI runs fuzz on Linux only. Elsewhere, at least build the targets:
  `cargo check --manifest-path fuzz/Cargo.toml`.
- CI runs fmt, clippy and test on Linux, Windows and macOS; locally they
  cover only the system you are on. Say which. On Windows, run the tests
  with `SAFECOPY_REQUIRE_JUNCTIONS=1`, as CI does.
