//! Writing through a held folder.
//!
//! Writes files into a folder the caller holds open: links are never
//! followed, nothing already there is replaced, a finished file is published
//! under a new name, and removal goes through a checked handle.
//!
//! Nothing is built yet; see the repository's README.

#[cfg(test)]
mod tests {
    // Keeps the test job running for this crate until it has real tests.
    #[test]
    fn crate_builds() {}
}
