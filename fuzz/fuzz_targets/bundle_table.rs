//! A bundle as the other side sends it: `BundleReader::open` on arbitrary
//! bytes, then the whole bundle read with arbitrary decisions into a real
//! folder. Never a panic; a bad table creates no partial file; never more
//! than `files` partial files, never more bytes written than the table
//! declares for the files taken, and no partial file left behind.
#![no_main]

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use libfuzzer_sys::arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use safecopy::bundle::{
    BundleHooks, BundleLimits, BundleReader, Outcome, Partial, ReadOptions, Take, Target,
};
use safecopy::chunks::{Digest, Progress};
use safecopy::folder::{Directory, Meta};

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

/// An empty folder for this process, emptied after every run.
fn folder() -> &'static PathBuf {
    static FOLDER: OnceLock<PathBuf> = OnceLock::new();
    FOLDER.get_or_init(|| {
        let path =
            std::env::temp_dir().join(format!("safecopy-fuzz-bundle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("a temporary folder");
        path
    })
}

fn names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(folder())
        .expect("the folder")
        .map(|e| e.expect("an entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
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

fuzz_target!(|data: &[u8]| {
    let mut input = Unstructured::new(data);
    let choices = u64::arbitrary(&mut input).unwrap_or(0);
    let stream = input.take_rest();
    let limits = limits();
    let dir = Directory::open(folder()).expect("the folder");
    let counts = Counts::default();
    run(async {
        let Ok(table) = BundleReader::open(stream, &limits).await else {
            assert!(names().is_empty(), "a bad table created a file");
            return;
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
    });
    for name in names() {
        let _ = std::fs::remove_file(folder().join(name));
    }
});
