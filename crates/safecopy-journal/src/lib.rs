//! Crash records and safe cleanup.
//!
//! Records each partial file while it is written, so that after a crash
//! cleanup removes only the file it recorded, checked by its identity.
//!
//! Nothing is built yet; see the repository's README.

#[cfg(test)]
mod tests {
    // Keeps the test job running for this crate until it has real tests.
    #[test]
    fn crate_builds() {}
}
