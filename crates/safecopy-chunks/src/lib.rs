//! Verified copying of one file.
//!
//! Copies exactly the declared number of bytes over any async byte stream,
//! hashes them with SHA-256 on both sides, and verifies the whole file before
//! it is published.
//!
//! Nothing is built yet; see the repository's README.

#[cfg(test)]
mod tests {
    // Keeps the test job running for this crate until it has real tests.
    #[test]
    fn crate_builds() {}
}
