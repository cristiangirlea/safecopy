//! The target `names`. What it runs is in `safecopy-names`, where the tests run
//! every seed and every saved crash through the same function.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| safecopy::names::fuzzing::names(data));
