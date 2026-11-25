//! What the fuzz targets `journal` and `journal_log` run, kept in the crate
//! so its tests run every saved input through the same code. Built only for
//! the tests and under `cfg(fuzzing)`, which a fuzzer's build sets: an
//! ordinary build has none of it.

use safecopy_manifest::GroupLimits;

use crate::{Record, Second, Settings, read_segment};

/// A small limit, so the fuzzer reaches it.
fn record_settings() -> Settings {
    Settings {
        record_bytes: 4096,
        ..Settings::default()
    }
}

/// Small limits, so the fuzzer reaches them.
fn log_settings() -> Settings {
    Settings {
        segment_bytes: 1 << 16,
        partials: 16,
        groups: GroupLimits {
            entries: 16,
            min_entries: 4,
            ..GroupLimits::default()
        },
        ..Settings::default()
    }
}

/// A journal file of the whole-record format as cleanup reads it from the
/// disk: never a panic, never past the limit it was given, and what reads
/// writes back to a record that reads the same.
pub fn journal(data: &[u8]) {
    let settings = record_settings();
    let Ok(record) = Record::decode(data, &settings) else {
        return;
    };
    assert!(data.len() <= settings.record_bytes);
    // A longer form of a number than needed is taken, so the bytes may
    // differ; the record may not.
    let bytes = record.encode(&settings).expect("a record read writes back");
    assert!(bytes.len() <= data.len());
    assert_eq!(Record::decode(&bytes, &settings).as_ref(), Ok(&record));
}

/// A journal segment as cleanup and recovery read it from the disk: never a
/// panic, never past the limits given, a torn or damaged entry stops the
/// reading there, and what reads writes back to a snapshot that reads the
/// same. A journal is not written by the other side, but a torn or damaged
/// one must never make cleanup remove the wrong file.
pub fn journal_log(data: &[u8]) {
    let settings = log_settings();
    let Ok(segment) = read_segment(data, &settings) else {
        return;
    };
    assert!(segment.read <= data.len().min(settings.segment_bytes));
    assert!(segment.partials().len() <= settings.partials);
    // Only the second format holds a second identity, and a time's
    // nanoseconds stay below a second.
    for started in segment.partials() {
        match started.second {
            Second::None => {}
            Second::Born { nanos, .. } | Second::Both { nanos, .. } => {
                assert!(segment.format() == 2 && nanos < 1_000_000_000);
            }
            Second::Generation(_) => assert_eq!(segment.format(), 2),
        }
    }
    let progress = segment.progress();
    assert!(progress.open_groups().len() <= settings.groups.open);
    for group in progress.open_groups() {
        assert!(group.len <= settings.groups.entries);
    }
    // A partial is found only through folders the segment names, each once.
    for (started, at) in segment.partials().iter().zip(segment.places()) {
        if let Some(at) = at {
            assert_eq!(at.partial, started.partial);
            assert_eq!(at.file, started.file);
            assert_eq!(at.second, started.second);
            assert!(at.location.levels.len() <= segment.folders().count());
        }
        assert!(!progress.skips(started.place) || !segment.grouped());
    }
    // Reading only the bytes read gives the same; a snapshot of it reads back the same.
    let again = read_segment(&data[..segment.read], &settings).expect("the bytes read read again");
    assert_eq!(again.partials(), segment.partials());
    let snapshot = segment.snapshot();
    let back = read_segment(&snapshot, &settings).expect("a snapshot reads");
    assert_eq!(back.format(), segment.format());
    assert_eq!(back.partials(), segment.partials());
    assert_eq!(back.progress(), segment.progress());
    assert_eq!(back.entries, 0);
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::log::{Entry, FORMAT, FORMAT_SECOND, State, frame};
    use crate::{Folder, Partial, Root, Started};

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

    const TARGETS: &[Target] = &[
        Target {
            name: "journal",
            run: journal,
            whole: |bytes| Record::decode(bytes, &record_settings()).is_ok(),
            seeds: records,
            cut: "two-partials",
        },
        Target {
            name: "journal_log",
            run: journal_log,
            // Every frame read: none torn, damaged or out of place.
            whole: |bytes| {
                read_segment(bytes, &log_settings()).is_ok_and(|read| read.read == bytes.len())
            },
            seeds: segments,
            cut: "format-1-grouped",
        },
    ];

    /// Records of the whole-record format: no partial, and partials with
    /// numbers of every length.
    fn records() -> Vec<Input> {
        let volume = r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\";
        let partial = |folder, name: &str, file| Partial {
            folder,
            name: name.into(),
            file,
        };
        let all = [
            ("no-partial", String::new(), 0, Vec::new()),
            (
                "one-partial",
                volume.to_owned(),
                0x1234_5678,
                vec![partial(9, "a.txt.part", 300)],
            ),
            (
                "two-partials",
                volume.to_owned(),
                u64::MAX,
                vec![
                    partial(u128::MAX, "文書.pdf.part", 1 << 64),
                    partial(0, "", 127),
                ],
            ),
        ];
        all.into_iter()
            .map(|(name, volume, serial, partials)| {
                let record = Record {
                    volume,
                    serial,
                    partials,
                };
                let bytes = record.encode(&record_settings()).expect("it encodes");
                (name.to_owned(), bytes)
            })
            .collect()
    }

    fn unix() -> Root {
        Root::Unix {
            path: b"/srv/in".to_vec(),
            device: 7,
            inode: 9,
        }
    }

    fn windows() -> Root {
        Root::Windows {
            volume: r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\".into(),
            serial: 0x1234_5678,
            folder: 1 << 70,
        }
    }

    fn named(n: u32, parent: u32, name: &str) -> Entry {
        Entry::Folder(Folder {
            n,
            parent,
            name: name.into(),
            device: 7,
            id: 10 + u128::from(n),
        })
    }

    fn started(place: u32, folder: u32, second: Second) -> Started {
        Started {
            place,
            folder,
            name: format!("{place}.jpg"),
            partial: format!("{place}.jpg.part"),
            size: 5 + u64::from(place),
            file: 100 + u128::from(place),
            second,
        }
    }

    /// The bytes of a segment that starts from `state` and goes on with
    /// `entries`, each one applied: one that could not follow fails here.
    fn segment(state: &mut State, entries: Vec<Entry>) -> Vec<u8> {
        let mut bytes = frame(&state.header());
        for entry in entries {
            bytes.extend_from_slice(&frame(&entry.encode(state.format)));
            state.apply(entry).expect("it follows");
        }
        bytes
    }

    /// Segments of both formats, as the journal writes them: both roots, a
    /// new header and one holding a snapshot, every kind of entry, with and
    /// without groups, and every second identity.
    fn segments() -> Vec<Input> {
        let settings = log_settings();
        let mut seeds = Vec::new();
        for (format, label) in [(FORMAT, "format-1"), (FORMAT_SECOND, "format-2")] {
            // The second identities a partial of this format ends with.
            let second = |n: u32| match (format, n % 4) {
                (FORMAT, _) | (_, 0) => Second::None,
                (_, 1) => Second::Born {
                    secs: 1_700_000_000,
                    nanos: 999_999_999,
                },
                (_, 2) => Second::Generation(u32::MAX),
                _ => Second::Both {
                    secs: -1,
                    nanos: 0,
                    generation: 3,
                },
            };
            let new = |root| State::new(root, &settings, format);
            seeds.push((
                format!("{label}-new"),
                segment(&mut new(unix()), Vec::new()),
            ));
            let mut grouped = new(unix());
            let entries = vec![
                Entry::Group {
                    index: 0,
                    first: 0,
                    len: 4,
                },
                named(1, 0, "Photos"),
                Entry::StartedMany((0..3).map(|p| started(p, 1, second(p))).collect()),
                Entry::DoneMany(vec![0, 2]),
                Entry::Ended { place: 1 },
                Entry::Started(started(3, 0, second(3))),
            ];
            seeds.push((format!("{label}-grouped"), segment(&mut grouped, entries)));
            // The next segment of the same journal: its header holds the
            // open group, and the partial still live.
            grouped.seq += 1;
            let entries = vec![Entry::Done { place: 3 }];
            seeds.push((format!("{label}-snapshot"), segment(&mut grouped, entries)));
            let mut plain = new(windows());
            let entries = vec![
                named(1, 0, "Photos"),
                named(2, 1, "2024"),
                Entry::Started(started(0, 2, second(1))),
                Entry::Started(started(1, 1, second(2))),
                Entry::Started(started(2, 0, second(3))),
                Entry::Done { place: 1 },
            ];
            seeds.push((format!("{label}-folders"), segment(&mut plain, entries)));
            plain.seq += 1;
            let entries = vec![Entry::Ended { place: 0 }, Entry::DoneMany(vec![2])];
            seeds.push((
                format!("{label}-folders-snapshot"),
                segment(&mut plain, entries),
            ));
            // A folder known by its identity alone.
            let mut unnamed = new(unix());
            let entries = vec![named(1, 0, ""), Entry::Started(started(0, 1, second(0)))];
            seeds.push((format!("{label}-unnamed"), segment(&mut unnamed, entries)));
        }
        seeds
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
