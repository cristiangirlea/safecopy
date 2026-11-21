//! A bounded list of files to send.
//!
//! Builds and encodes a list of files within limits the caller sets, keeps
//! the opened sources, and says when a source changed after it was listed.
//!
//! Nothing is built yet; see the repository's README.

#[cfg(test)]
mod tests {
    // Keeps the test job running for this crate until it has real tests.
    #[test]
    fn crate_builds() {}
}
