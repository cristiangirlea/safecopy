//! What the fuzz target `bundle_table` runs, kept in the crate so its tests
//! run every saved input through the same code. Built only for the tests
//! and under `cfg(fuzzing)`, which a fuzzer's build sets: an ordinary build
//! has none of it.

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use safecopy_chunks::{Digest, Progress};
use safecopy_folder::{Directory, Meta};

use crate::{BundleHooks, BundleLimits, BundleReader, Outcome, Partial, ReadOptions, Take, Target};

fn run<F: Future>(future: F) -> F::Output {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("a runtime")
        })
        .block_on(future)
}

/// The folder the target's folder is made in: the system's temporary one
/// in the fuzzer; the build's in the tests, where their other files are.
fn scratch() -> PathBuf {
    #[cfg(not(test))]
    return std::env::temp_dir();
    #[cfg(test)]
    {
        // The test binary is in target/<profile>/deps.
        let exe = std::env::current_exe().expect("this program");
        let target = exe.ancestors().nth(3).expect("the build's folder");
        target.join("test-tmp")
    }
}

/// An empty folder for this process, emptied after every run.
fn folder() -> &'static PathBuf {
    static FOLDER: OnceLock<PathBuf> = OnceLock::new();
    FOLDER.get_or_init(|| {
        let path = scratch().join(format!("safecopy-fuzz-bundle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a temporary folder");
        path
    })
}

fn names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(folder())
        .expect("the folder")
        .map(|e| {
            e.expect("an entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

fn empty() {
    for name in names() {
        let _ = std::fs::remove_file(folder().join(name));
    }
}

/// Small limits, so the fuzzer reaches them.
fn limits() -> BundleLimits {
    BundleLimits {
        small: 4096,
        files: 8,
        bytes: 16 << 10,
        table_bytes: 2048,
        meta_bytes: 256,
        ..BundleLimits::default()
    }
}

#[derive(Default)]
struct Counts {
    created: AtomicUsize,
    admissions: AtomicUsize,
    written: AtomicU64,
}

impl BundleHooks for Counts {
    fn admitted(&self, partials: &[Partial<'_>]) -> io::Result<()> {
        self.admissions.fetch_add(1, Ordering::SeqCst);
        self.created.fetch_add(partials.len(), Ordering::SeqCst);
        Ok(())
    }
}

impl Progress for Counts {
    fn advanced(&self, bytes: u64) {
        self.written.fetch_add(bytes, Ordering::SeqCst);
    }
}

/// Runs one input: its first eight bytes choose what is done with each
/// entry, two bits each, and the rest is the bundle. Whether it was whole:
/// its table was taken and every entry came to its end.
fn ran(data: &[u8]) -> bool {
    // One run at a time: they share the folder.
    static ALONE: Mutex<()> = Mutex::new(());
    let _alone = ALONE.lock().unwrap_or_else(PoisonError::into_inner);
    // What a run that failed left would fail this one too.
    empty();
    let (head, stream) = data.split_at(data.len().min(8));
    let mut choices = [0; 8];
    choices[..head.len()].copy_from_slice(head);
    let choices = u64::from_le_bytes(choices);
    let limits = limits();
    let dir = Directory::open(folder()).expect("the folder");
    let counts = Counts::default();
    let whole = run(async {
        let Ok(table) = BundleReader::open(stream, &limits).await else {
            assert!(names().is_empty(), "a bad table created a file");
            return false;
        };
        let entries = table.entries().to_vec();
        assert!(entries.len() <= limits.files as usize);
        let decisions: Vec<Take<'_>> = (0..entries.len())
            .map(|i| {
                let target = Target {
                    dir: &dir,
                    name: format!("e{i}"),
                    meta: Meta::default(),
                };
                match (choices >> (2 * i)) & 3 {
                    0 => Take::Save(target),
                    1 => Take::SaveUnlessSame(target, Digest([i as u8; 32])),
                    _ => Take::Skip,
                }
            })
            .collect();
        let taken_bytes: u64 = decisions
            .iter()
            .zip(&entries)
            .filter(|(take, _)| !matches!(take, Take::Skip))
            .map(|(_, entry)| entry.size)
            .sum();
        let options = ReadOptions {
            progress: &counts,
            sync: false,
            ..ReadOptions::default()
        };
        let report = table
            .read(decisions, &options, &counts, std::future::pending())
            .await;
        assert_eq!(report.outcomes.len(), entries.len());
        assert!(counts.admissions.load(Ordering::SeqCst) <= 1);
        assert!(counts.created.load(Ordering::SeqCst) <= limits.files as usize);
        assert!(counts.written.load(Ordering::SeqCst) <= taken_bytes);
        let saved = report
            .outcomes
            .iter()
            .filter(|o| matches!(o, Outcome::Saved(_)))
            .count();
        let left = names();
        assert!(
            left.iter().all(|n| !n.ends_with(".part")),
            "a partial file was left"
        );
        assert_eq!(left.len(), saved);
        // A skipped entry is settled before its bytes come: the error says
        // whether the stream then ended where the table said.
        report.error.is_none() && !report.outcomes.contains(&Outcome::NotReceived)
    });
    empty();
    whole
}

/// A bundle as the other side sends it: `BundleReader::open` on the input's
/// bytes, then the whole bundle read with the input's decisions into a real
/// folder. Never a panic; a bad table creates no partial file; never more
/// than `files` partial files, never more bytes written than the table
/// declares for the files taken, and no partial file left behind.
pub fn bundle_table(data: &[u8]) {
    ran(data);
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::{BundleWriter, TableEntry, TransferKey, WriteOptions};

    /// A seed or a saved input: its file's name and its bytes.
    type Input = (String, Vec<u8>);

    /// One fuzz target: what it runs, whether bytes are a whole input of
    /// the format as it is now, the seeds the format writes, and the seed
    /// a test cuts short.
    struct FuzzTarget {
        name: &'static str,
        run: fn(&[u8]),
        whole: fn(&[u8]) -> bool,
        seeds: fn() -> Vec<Input>,
        cut: &'static str,
    }

    const TARGETS: &[FuzzTarget] = &[FuzzTarget {
        name: "bundle_table",
        run: bundle_table,
        whole: ran,
        seeds,
        // Its last entry is skipped: only the stream's end says it is cut.
        cut: "each-decision",
    }];

    /// A file of a bundle: its bytes, or how many of them the sender gives
    /// up after it said their count.
    enum File {
        Whole(Vec<u8>),
        GivenUp(u64),
    }

    /// `len` bytes that differ by `seed`.
    fn data(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    /// The decisions, then the bundle a sender writes of `files` under the
    /// target's limits, the metadata block of each a little longer.
    fn bundle(choices: u64, files: &[File]) -> Vec<u8> {
        let entries: Vec<TableEntry> = files
            .iter()
            .enumerate()
            .map(|(i, file)| TableEntry {
                key: TransferKey([i as u8 + 1; 16]),
                meta: data(i * 5, 100),
                size: match file {
                    File::Whole(bytes) => bytes.len() as u64,
                    File::GivenUp(size) => *size,
                },
            })
            .collect();
        let mut out = choices.to_le_bytes().to_vec();
        run(async {
            let mut writer =
                BundleWriter::new(&mut out, &entries, &limits(), WriteOptions::default())
                    .await
                    .expect("a table within the limits");
            assert_eq!(writer.entries(), files.len());
            for file in files {
                match file {
                    File::Whole(bytes) => {
                        writer.write(&mut &bytes[..]).await.expect("it is written");
                    }
                    File::GivenUp(_) => writer.give_up().await.expect("it is given up"),
                }
            }
            writer.finish().await.expect("the bundle ends");
        });
        out
    }

    /// Bundles as the writer writes them: none to the most files, an empty
    /// file, files given up, and each decision a receiver can make.
    fn seeds() -> Vec<Input> {
        let whole = |len, seed| File::Whole(data(len, seed));
        let bundles = [
            ("no-file", 0, Vec::new()),
            ("one-file", 0, vec![whole(40, 1)]),
            // Saved, saved unless the same, skipped.
            (
                "each-decision",
                0b10_01_00,
                vec![whole(64, 2), whole(0, 3), whole(100, 4)],
            ),
            (
                "given-up",
                0b00_01_00,
                vec![whole(10, 5), File::GivenUp(48), File::GivenUp(0)],
            ),
            ("most-files", 0, (0..8).map(|i| whole(i * 3, 6)).collect()),
        ];
        bundles
            .into_iter()
            .map(|(name, choices, files)| (name.to_owned(), bundle(choices, &files)))
            .collect()
    }

    #[test]
    fn a_bundle_cut_or_with_a_byte_more_is_not_whole() {
        for (name, seed) in seeds() {
            assert!(ran(&seed), "{name}");
            let mut longer = seed.clone();
            longer.push(0);
            assert!(!ran(&longer), "{name} with a byte more");
            assert!(!ran(&seed[..seed.len() - 1]), "{name} cut");
        }
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
