//! What the fuzz target `manifest` runs, kept in the crate so its tests run
//! every saved input through the same code. Built only for the tests and
//! under `cfg(fuzzing)`, which a fuzzer's build sets: an ordinary build has
//! none of it.

use crate::{Kind, Limits, Manifest};

/// Small limits, so the fuzzer reaches each of them.
fn limits() -> Limits {
    Limits {
        entries: 64,
        depth: 4,
        path_chars: 64,
        bytes: 1 << 20,
        encoded: 4096,
        ..Limits::default()
    }
}

/// A list as the other side sends it: never a panic, never past the limits
/// it was given, and what decodes encodes back to a list equal to it.
pub fn manifest(data: &[u8]) {
    let limits = limits();
    let Ok(manifest) = Manifest::decode(data, &limits) else {
        return;
    };
    let entries = manifest.entries();
    assert!(!entries.is_empty() && entries.len() <= limits.entries);
    assert!(manifest.bytes() <= limits.bytes);
    let files = entries.iter().filter(|e| e.kind == Kind::File).count();
    assert_eq!(manifest.files() as usize, files);
    for entry in entries {
        assert!(entry.path.chars().count() <= limits.path_chars);
        assert!(entry.path.split('/').count() <= limits.depth);
        assert!(entry.kind == Kind::File || entry.size == 0);
    }
    // A longer form of a number than needed is taken, so the bytes may
    // differ; the list may not.
    let bytes = manifest.encode(&limits).expect("a decoded list encodes");
    assert!(bytes.len() <= data.len());
    assert_eq!(Manifest::decode(&bytes, &limits).as_ref(), Ok(&manifest));
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::Entry;

    /// A seed or a saved input: its file's name and its bytes.
    type Input = (String, Vec<u8>);

    /// One fuzz target: what it runs, whether bytes are a whole input of
    /// the format as it is now, the seeds the format writes, and the seed
    /// a test cuts short.
    struct Target {
        name: &'static str,
        run: fn(&[u8]),
        whole: fn(&[u8]) -> bool,
        seeds: fn() -> Vec<Input>,
        cut: &'static str,
    }

    const TARGETS: &[Target] = &[Target {
        name: "manifest",
        run: manifest,
        whole: |bytes| Manifest::decode(bytes, &limits()).is_ok(),
        seeds,
        cut: "one-file",
    }];

    fn file(path: &str, size: u64) -> Entry {
        Entry {
            path: path.into(),
            kind: Kind::File,
            size,
        }
    }

    fn folder_entry(path: &str) -> Entry {
        Entry {
            path: path.into(),
            kind: Kind::Folder,
            size: 0,
        }
    }

    /// Lists as [`Manifest::encode`] writes them under the target's limits:
    /// both kinds of entry, the deepest path, wide names, the largest sizes
    /// and numbers that take every length of their encoding.
    fn seeds() -> Vec<Input> {
        let lists: Vec<(&str, u64, Vec<Entry>)> = vec![
            ("one-file", 1, vec![file("a.txt", 5)]),
            ("empty-file", 0, vec![file("empty", 0)]),
            ("one-folder", 300, vec![folder_entry("Empty")]),
            (
                "tree",
                u64::MAX,
                vec![
                    folder_entry("Photos"),
                    folder_entry("Photos/2024"),
                    file("Photos/2024/beach.jpg", 70_000),
                    file("Photos/2024/hill.jpg", 127),
                    folder_entry("Photos/Empty"),
                    file("notes.txt", 128),
                ],
            ),
            (
                "deepest",
                1 << 40,
                vec![
                    folder_entry("a"),
                    folder_entry("a/b"),
                    folder_entry("a/b/c"),
                    file("a/b/c/d", 1 << 20),
                ],
            ),
            (
                "wide-names",
                7,
                vec![
                    folder_entry("文書"),
                    file("文書/café.txt", 16_384),
                    file("文書/😀 (2).png", 1),
                    file("_CON", 2),
                ],
            ),
            (
                "many",
                64,
                (0..64).map(|i| file(&format!("f{i:02}"), i)).collect(),
            ),
        ];
        lists
            .into_iter()
            .map(|(name, id, entries)| {
                let list = Manifest::new(id, entries, &limits()).expect("a list within the limits");
                let bytes = list.encode(&limits()).expect("it encodes");
                (name.to_owned(), bytes)
            })
            .collect()
    }

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

    /// The first of `inputs` that is not a whole input now.
    fn stale(inputs: &[Input], whole: fn(&[u8]) -> bool) -> Option<&str> {
        let found = inputs.iter().find(|(_, bytes)| !whole(bytes));
        found.map(|(name, _)| name.as_str())
    }

    const WRITE: &str = "write them again: cargo test --workspace write_fuzz_seeds -- --ignored";

    #[test]
    fn the_seeds_are_what_the_format_writes_now() {
        for target in TARGETS {
            let saved = saved(&folder("seeds", target.name));
            assert!(!saved.is_empty(), "{}: no seeds", target.name);
            assert_eq!(
                stale(&saved, target.whole),
                None,
                "{}: {WRITE}",
                target.name
            );
            let mut made = (target.seeds)();
            made.sort();
            assert!(saved == made, "{}: {WRITE}", target.name);
        }
    }

    #[test]
    fn a_seed_cut_short_is_found() {
        for target in TARGETS {
            let mut copy = saved(&folder("seeds", target.name));
            let seed = copy.iter_mut().find(|(name, _)| name == target.cut);
            seed.expect("the seed").1.pop();
            assert_eq!(stale(&copy, target.whole), Some(target.cut));
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
