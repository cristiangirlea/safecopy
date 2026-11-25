//! The target `resume_handshake`. What it runs is in `safecopy-resume`, where the tests run
//! every seed and every saved crash through the same function.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| safecopy::resume::fuzzing::resume_handshake(data));
