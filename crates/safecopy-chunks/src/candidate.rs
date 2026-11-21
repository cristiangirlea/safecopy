//! The hash of a file already at the destination.

use std::fmt;
use std::future::Future;

use safecopy_folder::Candidate;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::{Digest, Progress, SLICE, digest};

/// Why a [`Candidate`] gave no hash. Either way it is no match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CandidateError {
    /// It changed before or while it was read: its size, its modified
    /// time, or the file its name leads to.
    Changed,
    /// It could not be read (locked against reading, a disk error).
    Unreadable,
}

impl fmt::Display for CandidateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for CandidateError {}

/// Hashing a [`Candidate`].
pub trait CandidateHash {
    /// The SHA-256 of the whole file, read through its handle from byte 0.
    /// It counts only when the file was [`Candidate::unchanged`] before the
    /// read and after it, and exactly its size was read. `progress` hears
    /// of each slice read, so a long hash is not taken for a stall. This
    /// hash stays on this side: it is compared with the sender's, never
    /// sent. It takes the candidate mutably: the read goes through the
    /// candidate's own file position.
    fn hash(
        &mut self,
        progress: &dyn Progress,
    ) -> impl Future<Output = Result<Digest, CandidateError>> + Send;
}

impl CandidateHash for Candidate {
    async fn hash(&mut self, progress: &dyn Progress) -> Result<Digest, CandidateError> {
        if !self.unchanged() {
            return Err(CandidateError::Changed);
        }
        let handle = self
            .file()
            .try_clone()
            .map_err(|_| CandidateError::Unreadable)?;
        let mut file = tokio::fs::File::from_std(handle);
        file.rewind()
            .await
            .map_err(|_| CandidateError::Unreadable)?;
        let mut context = ring::digest::Context::new(&ring::digest::SHA256);
        let mut buffer = vec![0; SLICE];
        let size = self.size();
        let mut at = 0;
        while at < size {
            let want = usize::try_from(size - at).map_or(SLICE, |left| left.min(SLICE));
            let n = file
                .read(&mut buffer[..want])
                .await
                .map_err(|_| CandidateError::Unreadable)?;
            if n == 0 {
                // Shorter than it was.
                return Err(CandidateError::Changed);
            }
            context.update(&buffer[..n]);
            at += n as u64;
            progress.advanced(n as u64);
        }
        if !self.unchanged() {
            return Err(CandidateError::Changed);
        }
        Ok(digest(context))
    }
}
