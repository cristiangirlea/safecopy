//! The checks on both sides that decide whether a file continues, starts
//! again from zero, or is refused.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Seek, SeekFrom};

use safecopy_chunks::{CHUNK, Hasher, Settings};
use safecopy_folder::{FileId, Time, identity};

use crate::{Held, PeerId, Receiving, Resumable, ResumeError, Sending, TransferKey};

/// What the receiver does with a stream, from [`receiver_takes`] or
/// [`Resent::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Take {
    /// A new file: no record and no mark under its key, from zero. It goes
    /// through the program's consent and intake as any new file.
    New,
    /// Continues the kept partial: cut it to `have` and append.
    Append,
    /// Stop this stream with [`crate::Verdict::Restart`]: the record and
    /// its partial are removed, the key marked ([`Resent::mark`]), and the
    /// sender sends from zero, once.
    Restart,
    /// The one resend from zero under this key: taken without a second
    /// question within the session or a rejoin of it. The record and its
    /// partial, if any, are removed first; any further stream under the
    /// key is refused.
    Resend,
    /// Refused ([`crate::Verdict::Refused`]): off a chunk, past the size,
    /// another `same`, a key not kept, or one stream too many. A record
    /// whose `same` differs is removed with its partial.
    Refuse,
}

fn on_chunk(n: u64) -> bool {
    n.is_multiple_of(CHUNK)
}

/// What the receiver does with a stream under `stream.key`, given its
/// record for that key and peer (`None` when it keeps none) and the
/// stream's `same` bytes. A stream from past its size or off a chunk is
/// refused; one from past zero needs a record; a record's `same` and size
/// must be repeated exactly; one from zero under a kept key is the one
/// resend; one from another place than `have` makes the sender restart.
///
/// A mark of [`Resent`] comes first: use [`Resent::check`], which calls
/// this.
pub fn receiver_takes(record: Option<&Receiving>, stream: &Resumable, same: &[u8]) -> Take {
    if stream.from > stream.size || !on_chunk(stream.from) {
        return Take::Refuse;
    }
    let Some(record) = record else {
        return if stream.from == 0 {
            Take::New
        } else {
            Take::Refuse
        };
    };
    if record.key != stream.key || record.same != same || record.size != stream.size {
        return Take::Refuse;
    }
    if stream.from == 0 {
        Take::Resend
    } else if stream.from == record.have {
        Take::Append
    } else {
        Take::Restart
    }
}

#[derive(Debug, Clone)]
struct Mark {
    same: Vec<u8>,
    spent: bool,
}

/// The receiver's per-session marks of keys it stopped with
/// [`crate::Verdict::Damaged`] or [`crate::Verdict::Restart`], kept in
/// memory (and over a rejoin of the session). A marked key takes one
/// resend from zero with the same `same`, and refuses any stream after it:
/// a second failure, or a third stream, is refused, so a sender cannot
/// loop resends.
#[derive(Debug, Clone, Default)]
pub struct Resent {
    marks: HashMap<(PeerId, TransferKey), Mark>,
}

impl Resent {
    /// No marks.
    pub fn new() -> Self {
        Self::default()
    }

    /// The file under `key` was stopped as damaged or to restart: its one
    /// resend from zero, with the same `same`, will be taken. A key already
    /// marked stays as it is: a second failure is not a second resend.
    pub fn mark(&mut self, peer: PeerId, key: TransferKey, same: &[u8]) {
        self.marks.entry((peer, key)).or_insert(Mark {
            same: same.to_vec(),
            spent: false,
        });
    }

    /// Whether `key` is marked.
    pub fn marked(&self, peer: PeerId, key: TransferKey) -> bool {
        self.marks.contains_key(&(peer, key))
    }

    /// What the receiver does with a stream from `peer`: a marked key's
    /// one resend ([`Take::Resend`], which spends the mark) or a refusal,
    /// and otherwise [`receiver_takes`]. A [`Take::Restart`] marks the key;
    /// a [`Take::Resend`] under a kept key marks it spent at once.
    pub fn check(
        &mut self,
        peer: PeerId,
        record: Option<&Receiving>,
        stream: &Resumable,
        same: &[u8],
    ) -> Take {
        if let Some(mark) = self.marks.get_mut(&(peer, stream.key)) {
            let resend = !mark.spent && stream.from == 0 && mark.same == same;
            mark.spent = true;
            return if resend { Take::Resend } else { Take::Refuse };
        }
        // A record is only ever another peer's by mistake: not this one's.
        let record = record.filter(|r| r.peer == peer);
        let take = receiver_takes(record, stream, same);
        match take {
            Take::Restart => self.mark(peer, stream.key, same),
            Take::Resend => {
                self.marks.insert(
                    (peer, stream.key),
                    Mark {
                        same: same.to_vec(),
                        spent: true,
                    },
                );
            }
            _ => {}
        }
        take
    }
}

/// What the sender does with a file the receiver holds, from
/// [`sender_checks`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Send from `from`, still hashing from byte 0.
    Continue {
        /// The receiver's `have`.
        from: u64,
    },
    /// The receiver's prefix or `have` is not right: send from zero, once,
    /// under the same key.
    Restart,
    /// No record of it: tell the receiver ([`crate::Verdict::Dropped`]).
    Dropped,
    /// The file is not the one recorded: it fails, and the receiver is
    /// told [`crate::Verdict::Dropped`]. A changed file is a different
    /// file; it is not sent again by itself.
    Changed,
}

/// The sender's file, opened again to continue it: its identity, size and
/// modified time as they are now.
#[derive(Debug)]
pub struct Source {
    /// The file, open for reading.
    pub file: File,
    /// Its identity.
    pub id: FileId,
    /// Its size.
    pub size: u64,
    /// Its modified time.
    pub modified: Option<Time>,
}

impl Source {
    /// `file` as it is now.
    pub fn read(file: File) -> Result<Self, ResumeError> {
        let meta = file.metadata()?;
        Ok(Self {
            id: identity(&file)?,
            size: meta.len(),
            modified: meta.modified().ok().and_then(Time::from_system),
            file,
        })
    }

    /// The file at the record's path, opened again; `None` when it cannot
    /// be opened (gone, renamed) or is no regular file. On Unix it opens
    /// without waiting, so a pipe put under the path never holds it.
    pub fn open(record: &Sending) -> Option<Self> {
        let file = open_source(&record.path).ok()?;
        if !file.metadata().ok()?.is_file() {
            return None;
        }
        #[cfg(unix)]
        blocking(&file);
        Self::read(file).ok()
    }
}

fn open_source(path: &std::path::Path) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Opening a pipe for reading waits for a writer; this does not.
        options.custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    options.open(path)
}

/// Back to blocking reads for a regular file's bytes.
#[cfg(unix)]
fn blocking(file: &File) {
    use std::os::fd::AsRawFd;
    // SAFETY: plain fcntl calls on a descriptor `file` owns.
    unsafe {
        let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
    }
}

/// What the sender does with a file the receiver holds: its record looked
/// up by the peer **and** the key (`None` when it has none), its file
/// opened again, and the receiver's [`Held`]. In order: the file must be
/// the recorded one (identity, size, modified time), `have` at most its
/// size and a whole number of chunks, and its own first `have` bytes must
/// hash to `prefix`, from `hasher` when it still keeps that boundary or
/// read again from the file, once.
///
/// With [`Next::Continue`] comes the hasher that check used, standing at
/// `have`: the sender goes on from it (`safecopy_chunks::copy_exact`) and
/// does not read the prefix again. It is `hasher` rewound, or one read
/// from the file, made as `settings` makes a sender's. `None` with any
/// other answer.
pub async fn sender_checks(
    record: Option<&Sending>,
    source: Option<&Source>,
    held: &Held,
    hasher: Option<&Hasher>,
    settings: &Settings,
) -> Result<(Next, Option<Hasher>), ResumeError> {
    let Some(record) = record.filter(|r| r.key == held.key) else {
        return Ok((Next::Dropped, None));
    };
    let Some(source) = source else {
        return Ok((Next::Changed, None));
    };
    if source.id != record.file || source.size != record.size || source.modified != record.modified
    {
        return Ok((Next::Changed, None));
    }
    if held.have > source.size || !on_chunk(held.have) {
        return Ok((Next::Restart, None));
    }
    let ours = match hasher.and_then(|h| h.rewound(held.have)) {
        Some(rewound) => rewound,
        None => {
            let mut file = source.file.try_clone()?;
            file.seek(SeekFrom::Start(0))?;
            let mut file = tokio::fs::File::from_std(file);
            #[cfg(test)]
            crate::tests::seams::PREFIX_BYTES
                .set(crate::tests::seams::PREFIX_BYTES.get() + held.have);
            let mut read = settings.sending();
            match read.read(&mut file, held.have).await {
                Ok(()) => read,
                // Shorter than recorded now: changed under us.
                Err(safecopy_chunks::ChunkError::CutShort { .. }) => {
                    return Ok((Next::Changed, None));
                }
                Err(e) => return Err(e.into()),
            }
        }
    };
    Ok(if ours.clone().finish() == held.prefix {
        (Next::Continue { from: held.have }, Some(ours))
    } else {
        (Next::Restart, None)
    })
}
