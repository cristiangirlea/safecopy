//! A journal segment as cleanup and recovery read it from the disk: never a
//! panic, never past the limits given, a torn or damaged entry stops the
//! reading there, and what reads writes back to a snapshot that reads the
//! same. A journal is not written by the other side, but a torn or damaged
//! one must never make cleanup remove the wrong file.
#![no_main]

use libfuzzer_sys::fuzz_target;
use safecopy::journal::{Settings, read_segment};
use safecopy::manifest::GroupLimits;

fuzz_target!(|data: &[u8]| {
    // Small limits, so the fuzzer reaches them.
    let settings = Settings {
        segment_bytes: 1 << 16,
        partials: 16,
        groups: GroupLimits {
            entries: 16,
            min_entries: 4,
            ..GroupLimits::default()
        },
        ..Settings::default()
    };
    let Ok(segment) = read_segment(data, &settings) else {
        return;
    };
    assert!(segment.read <= data.len().min(settings.segment_bytes));
    assert!(segment.partials().len() <= settings.partials);
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
            assert!(at.location.levels.len() <= segment.folders().count());
        }
        assert!(!progress.skips(started.place) || !segment.grouped());
    }
    // Reading only the bytes read gives the same; a snapshot of it reads back the same.
    let again = read_segment(&data[..segment.read], &settings).expect("the bytes read read again");
    assert_eq!(again.partials(), segment.partials());
    let snapshot = segment.snapshot();
    let back = read_segment(&snapshot, &settings).expect("a snapshot reads");
    assert_eq!(back.partials(), segment.partials());
    assert_eq!(back.progress(), segment.progress());
    assert_eq!(back.entries, 0);
});
