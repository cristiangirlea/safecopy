//! Safe, resumable, verified file transfer.
//!
//! Re-exports the safecopy crates, each under a short name. Use this crate
//! for all of them, or depend on the one crate you need.
//!
//! [`names`] and [`folder`] are built; the others are still empty.

pub use safecopy_bundle as bundle;
pub use safecopy_chunks as chunks;
pub use safecopy_folder as folder;
pub use safecopy_journal as journal;
pub use safecopy_manifest as manifest;
pub use safecopy_names as names;
pub use safecopy_resume as resume;

#[cfg(test)]
mod tests {
    // Keeps the test job running for this crate until it has real tests.
    #[test]
    fn crate_builds() {}
}
