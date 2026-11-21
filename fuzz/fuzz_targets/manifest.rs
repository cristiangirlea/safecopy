//! A list as the other side sends it: never a panic, never past the limits
//! it was given, and what decodes encodes back to a list equal to it.
#![no_main]

use libfuzzer_sys::fuzz_target;
use safecopy::manifest::{Kind, Limits, Manifest};

fuzz_target!(|data: &[u8]| {
    // Small limits, so the fuzzer reaches each of them.
    let limits = Limits {
        entries: 64,
        depth: 4,
        path_chars: 64,
        bytes: 1 << 20,
        encoded: 4096,
        ..Limits::default()
    };
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
});
