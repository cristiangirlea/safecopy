//! Safe, resumable, verified file transfer.
//!
//! Re-exports the safecopy crates, each under a short name. Use this crate
//! for all of them, or depend on the one crate you need.
//!
//! The `quic` feature (off by default) adds `bundle::quic`, helpers for
//! quinn's streams.

pub use safecopy_bundle as bundle;
pub use safecopy_chunks as chunks;
pub use safecopy_folder as folder;
pub use safecopy_journal as journal;
pub use safecopy_manifest as manifest;
pub use safecopy_names as names;
pub use safecopy_resume as resume;

#[cfg(test)]
mod tests {
    #[test]
    fn every_crate_is_reexported() {
        let limits = crate::bundle::BundleLimits::default();
        assert_eq!(limits.files, 64);
        assert_eq!(crate::chunks::CHUNK, 4 << 20);
    }
}
