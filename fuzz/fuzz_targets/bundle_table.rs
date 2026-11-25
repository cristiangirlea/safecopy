//! The target `bundle_table`. What it runs is in `safecopy-bundle`, where the tests run
//! every seed and every saved crash through the same function.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| safecopy::bundle::fuzzing::bundle_table(data));
