//! Placeholder target so the fuzz job runs from the start; real parsers get their own targets.
#![no_main]

use libfuzzer_sys::fuzz_target;

// Links the library so the target builds against it.
use safecopy as _;

fuzz_target!(|data: &[u8]| {
    let _ = data.len();
});
