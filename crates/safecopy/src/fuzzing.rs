//! What the fuzz target `smoke` runs, kept in the crate so its tests run
//! every saved input through the same code. Built only for the tests and
//! under `cfg(fuzzing)`, which a fuzzer's build sets: an ordinary build has
//! none of it.

/// Any bytes: the target only shows that the fuzzer builds against the
/// library and runs. Each parser has a target of its own.
pub fn smoke(data: &[u8]) {
    let _ = data.len();
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    /// A seed or a saved input: its file's name and its bytes.
    type Input = (String, Vec<u8>);

    /// One fuzz target: what it runs and its seeds. Every input is whole to
    /// this one, so no seed can be found stale by what it holds.
    struct Target {
        name: &'static str,
        run: fn(&[u8]),
        seeds: fn() -> Vec<Input>,
    }

    const TARGETS: &[Target] = &[Target {
        name: "smoke",
        run: smoke,
        seeds: || vec![("any".to_owned(), b"safecopy".to_vec())],
    }];

    /// `fuzz/<kind>/<target>` in the repository.
    fn folder(kind: &str, target: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fuzz")
            .join(kind)
            .join(target)
    }

    /// The inputs a folder holds, by name. A file whose name starts with a
    /// dot is none: the one that keeps an empty folder, or a system's own.
    fn saved(dir: &Path) -> Vec<Input> {
        let mut all: Vec<Input> = std::fs::read_dir(dir)
            .expect("the folder")
            .map(|entry| entry.expect("an entry"))
            .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                (name, std::fs::read(entry.path()).expect("an input"))
            })
            .collect();
        all.sort();
        all
    }

    const WRITE: &str = "write them again: cargo test --workspace write_fuzz_seeds -- --ignored";

    #[test]
    fn the_seeds_are_what_the_target_is_given_now() {
        for target in TARGETS {
            let mut made = (target.seeds)();
            made.sort();
            assert!(!made.is_empty(), "{}: no seeds", target.name);
            let saved = saved(&folder("seeds", target.name));
            assert!(saved == made, "{}: {WRITE}", target.name);
        }
    }

    #[test]
    fn every_saved_input_runs_as_the_fuzz_target_runs_it() {
        for target in TARGETS {
            let mut inputs = saved(&folder("seeds", target.name));
            inputs.extend(saved(&folder("regressions", target.name)));
            for (name, bytes) in &inputs {
                let ran = std::panic::catch_unwind(|| (target.run)(bytes));
                assert!(ran.is_ok(), "{}: the input {name}", target.name);
            }
        }
    }

    #[test]
    #[ignore = "writes fuzz/seeds: run when a format changes"]
    fn write_fuzz_seeds() {
        for target in TARGETS {
            let dir = folder("seeds", target.name);
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("the folder");
            for (name, bytes) in (target.seeds)() {
                std::fs::write(dir.join(name), bytes).expect("a seed");
            }
        }
    }
}
