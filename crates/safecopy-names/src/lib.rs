//! Safe file names.
//!
//! Checks a name a peer sent before it touches the disk: refuses unsafe and
//! reserved names and paths that leave their folder, makes numbered names
//! (`report (2).pdf`) and finds collisions within one folder.
//!
//! Nothing is built yet; see the repository's README.

#[cfg(test)]
mod tests {
    // Keeps the test job running for this crate until it has real tests.
    #[test]
    fn crate_builds() {}
}
