//! A journal file as cleanup reads it from the disk: never a panic, never
//! past the limit it was given, and what reads writes back to a record
//! that reads the same.
#![no_main]

use libfuzzer_sys::fuzz_target;
use safecopy::journal::{Record, Settings};

fuzz_target!(|data: &[u8]| {
    // A small limit, so the fuzzer reaches it.
    let settings = Settings {
        record_bytes: 4096,
        ..Settings::default()
    };
    let Ok(record) = Record::decode(data, &settings) else {
        return;
    };
    assert!(data.len() <= settings.record_bytes);
    // A longer form of a number than needed is taken, so the bytes may
    // differ; the record may not.
    let bytes = record.encode(&settings).expect("a record read writes back");
    assert!(bytes.len() <= data.len());
    assert_eq!(Record::decode(&bytes, &settings).as_ref(), Ok(&record));
});
