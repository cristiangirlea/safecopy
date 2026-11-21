//! Many small files in one stream.
//!
//! Sends several small files in one stream, each with its own size and hash,
//! so one bad file never spoils the others.
//!
//! Nothing is built yet; see the repository's README.

#[cfg(test)]
mod tests {
    // Keeps the test job running for this crate until it has real tests.
    #[test]
    fn crate_builds() {}
}
