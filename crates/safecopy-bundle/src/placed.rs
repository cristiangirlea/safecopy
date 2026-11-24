//! A partial file's writer: every write behind a gate its reader shuts.

use std::fs::File;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::task::JoinHandle;

/// Writes a file from its start, each slice at its own offset and on the
/// blocking pool; a write's error comes with the next write or the end.
///
/// A write handed to the pool outlives the future that asked for it, and
/// holds the file open while it waits there. So the writer and its writes
/// share a gate: dropped, the writer waits for the write that holds it
/// and shuts every later one out, and no handle of the file is left.
pub(crate) struct Placed {
    gate: Arc<Gate>,
    /// Where the next byte goes.
    at: u64,
    writing: Option<JoinHandle<io::Result<()>>>,
    #[cfg(test)]
    seam: Option<crate::seams::Writes>,
    #[cfg(test)]
    fail_at: Option<u64>,
}

/// The handle a writer's writes go through: held for the length of each
/// write, and empty once the writer is gone.
struct Gate(Mutex<Option<File>>);

impl Gate {
    fn lock(&self) -> MutexGuard<'_, Option<File>> {
        // A write that panicked left the file as whole as it will be.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Placed {
    pub(crate) fn new(file: &File) -> io::Result<Self> {
        Ok(Self {
            gate: Arc::new(Gate(Mutex::new(Some(file.try_clone()?)))),
            at: 0,
            writing: None,
            #[cfg(test)]
            seam: crate::seams::WRITES.with_borrow(Clone::clone),
            #[cfg(test)]
            fail_at: crate::seams::WRITE_ERROR_AT.take(),
        })
    }

    /// Hands `bytes` to the pool once the write before is done, and gives
    /// that one's error.
    pub(crate) async fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.done().await?;
        let (gate, at, owned) = (self.gate.clone(), self.at, bytes.to_vec());
        #[cfg(test)]
        let (seam, fail_at) = (self.seam.clone(), self.fail_at);
        self.writing = Some(tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            let step = |step| {
                if let Some(seam) = &seam {
                    seam(step);
                }
            };
            #[cfg(test)]
            step(crate::seams::Step::Queued);
            let file = gate.lock();
            let written = match &*file {
                Some(file) => {
                    #[cfg(test)]
                    step(crate::seams::Step::Writing);
                    #[cfg(test)]
                    if fail_at.is_some_and(|fail_at| at + owned.len() as u64 > fail_at) {
                        return Err(io::ErrorKind::StorageFull.into());
                    }
                    write_at(file, &owned, at)
                }
                // The writer is gone, and its read with it.
                None => Ok(()),
            };
            #[cfg(test)]
            step(crate::seams::Step::Ended);
            drop(file);
            written
        }));
        self.at += bytes.len() as u64;
        Ok(())
    }

    /// The write under way, ended: its error, when it had one.
    async fn done(&mut self) -> io::Result<()> {
        let Some(writing) = &mut self.writing else {
            return Ok(());
        };
        let done = writing.await;
        self.writing = None;
        done.unwrap_or_else(|_| Err(io::ErrorKind::Other.into()))
    }

    /// Waits for the write under way and gives its error, for a read
    /// whose bytes have ended: after it, nothing of this writer is on the
    /// pool.
    pub(crate) async fn settle(&mut self) -> io::Result<()> {
        #[cfg(test)]
        if let Some(seam) = &self.seam {
            seam(crate::seams::Step::Settling);
        }
        self.done().await
    }
}

impl Drop for Placed {
    fn drop(&mut self) {
        #[cfg(test)]
        if let Some(seam) = &self.seam {
            seam(crate::seams::Step::Closing);
        }
        // Waits for the write that holds the gate, one slice, as long as
        // the disk takes. A write that has not started finds no file.
        self.gate.lock().take();
    }
}

/// Writes all of `bytes` at the offset `at` of `file`.
fn write_at(file: &File, bytes: &[u8], at: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.write_all_at(bytes, at)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let (mut bytes, mut at) = (bytes, at);
        while !bytes.is_empty() {
            match file.seek_write(bytes, at) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    bytes = &bytes[n..];
                    at += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}
