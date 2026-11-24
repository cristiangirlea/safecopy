/// Switches tests use to act out a full disk or a torn write.
// Android's emulated thread locals make Clippy ask for the `const` already there.
#[allow(clippy::missing_const_for_thread_local)]
pub(crate) mod seams {
    use std::cell::Cell;

    thread_local! {
        /// The free space every disk reports on this thread.
        pub static FREE: Cell<Option<u64>> = const { Cell::new(None) };
        /// This thread's next record write stops halfway, as a crash would.
        pub static TEAR: Cell<bool> = const { Cell::new(false) };
        /// How many folder syncs this thread's stores made.
        pub static DIR_SYNCS: Cell<u32> = const { Cell::new(0) };
        /// This thread's next record write fails as a full disk would.
        pub static FULL: Cell<bool> = const { Cell::new(false) };
        /// How many record files this thread's stores opened to read.
        pub static OPENS: Cell<u32> = const { Cell::new(0) };
        /// How many bytes of a prefix this thread read again to hash.
        pub static PREFIX_BYTES: Cell<u64> = const { Cell::new(0) };
        /// Runs once, in this thread's next record write, after it looked
        /// for the record's file and before it makes one.
        pub static LOOKED: Cell<Option<Box<dyn FnOnce()>>> = const { Cell::new(None) };
        /// This thread's hashing of a prefix does not end: a slow disk,
        /// for as long as a test needs it.
        pub static STALL: Cell<bool> = const { Cell::new(false) };
        /// Runs once, in this thread's next `forget`, after it claimed the
        /// file and before it removes anything.
        pub static FORGETTING: Cell<Option<Box<dyn FnOnce()>>> = const { Cell::new(None) };
    }

    pub async fn stall() {
        if STALL.get() {
            std::future::pending::<()>().await;
        }
    }
}

use std::ffi::OsStr;
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::Duration;

use safecopy_chunks::{Hasher, copy_exact, read_trailer, write_trailer};
use safecopy_folder::{Directory, Time};
use safecopy_journal::{Journal, PartialAt};
use tokio::io::AsyncSeekExt;

use super::*;
use crate::handshake::Reply;

const C: u64 = CHUNK;

fn temp(name: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let target = exe.ancestors().nth(3).unwrap();
    let dir = target
        .join("test-tmp")
        .join(format!("resume-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A receiving or sending side: a folder files go into, one for journals,
/// and a store.
struct Side {
    base: PathBuf,
    place: PathBuf,
    journals: PathBuf,
    root: Directory,
    store: Store,
}

impl Drop for Side {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn side(name: &str, limits: Limits) -> Side {
    let base = temp(name);
    let (place, journals) = (base.join("Work"), base.join("journals"));
    std::fs::create_dir_all(&place).unwrap();
    std::fs::create_dir_all(&journals).unwrap();
    Side {
        root: Directory::destination(&place).unwrap(),
        store: Store::open(&base.join("records"), limits).unwrap(),
        base,
        place,
        journals,
    }
}

const PEER: PeerId = PeerId([7; 32]);
const OTHER: PeerId = PeerId([9; 32]);

fn key(n: u8) -> TransferKey {
    TransferKey([n; 16])
}

/// A file's bytes, each chunk unlike the next.
fn data(len: u64) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8 ^ (i / C) as u8).collect()
}

fn hash(bytes: &[u8]) -> Digest {
    let mut hasher = Hasher::new(0);
    hasher.update(bytes);
    hasher.finish()
}

/// The receiver got the first `got` bytes of `bytes` into `name.part` in
/// the root, journalled; with one whole chunk or more, a record keeps the
/// whole chunks of it.
#[allow(clippy::too_many_arguments)]
fn received(
    side: &Side,
    journal: &Journal,
    place: u32,
    key: TransferKey,
    name: &str,
    bytes: &[u8],
    got: u64,
    last: u64,
) -> Option<Receiving> {
    let partial = format!("{name}.part");
    let mut file = side.root.create(OsStr::new(&partial)).unwrap();
    let size = bytes.len() as u64;
    journal
        .started(place, &side.root, &file, OsStr::new(&partial), name, size)
        .unwrap();
    file.write_all(&bytes[..got as usize]).unwrap();
    let have = got / C * C;
    let record = Receiving {
        peer: PEER,
        key,
        same: name.as_bytes().to_vec(),
        size,
        at: PartialAt::of(&side.root, &side.root, &file, OsStr::new(&partial)).unwrap(),
        have,
        prefix: hash(&bytes[..have as usize]),
        created: last,
        last,
    };
    match side.store.receiving(&record, &file) {
        Ok(()) => Some(record),
        Err(e) => {
            assert_eq!(e, ResumeError::OffChunk, "under one chunk only");
            None
        }
    }
}

/// A kept partial of `chunks` whole chunks of zeros, quick to make.
fn kept_zeros(side: &Side, peer: PeerId, key: TransferKey, name: &str, chunks: u64, last: u64) {
    kept_in(&side.store, side, peer, key, name, chunks, last);
}

/// [`kept_zeros`], its record written by `store`.
fn kept_in(
    store: &Store,
    side: &Side,
    peer: PeerId,
    key: TransferKey,
    name: &str,
    chunks: u64,
    last: u64,
) -> Receiving {
    let partial = format!("{name}.part");
    let file = side.root.create(OsStr::new(&partial)).unwrap();
    file.set_len(chunks * C).unwrap();
    let record = Receiving {
        peer,
        key,
        same: Vec::new(),
        size: chunks * C,
        at: PartialAt::of(&side.root, &side.root, &file, OsStr::new(&partial)).unwrap(),
        have: chunks * C,
        prefix: hash(&vec![0; (chunks * C) as usize]),
        created: last,
        last,
    };
    store.receiving(&record, &file).unwrap();
    record
}

/// The sender's file `bytes` and its record.
fn source(side: &Side, key: TransferKey, bytes: &[u8]) -> Sending {
    let path = side.place.join(format!("source-{}.bin", key.0[0]));
    std::fs::write(&path, bytes).unwrap();
    let read = Source::read(File::open(&path).unwrap()).unwrap();
    let record = Sending {
        peer: PEER,
        key,
        path,
        file: read.id,
        size: read.size,
        modified: read.modified,
        created: 1,
        last: 1,
    };
    side.store.sending(&record).unwrap();
    record
}

/// One resumable stream of `bytes` from `from`.
async fn send(key: TransferKey, bytes: &[u8], from: u64) -> Vec<u8> {
    let mut out = Vec::new();
    let size = bytes.len() as u64;
    Resumable { key, from, size }
        .write_prefix(&mut out)
        .await
        .unwrap();
    let mut hasher = Hasher::new(4);
    hasher.update(&bytes[..from as usize]);
    let mut rest = &bytes[from as usize..];
    copy_exact(&mut rest, &mut out, size, from, &mut hasher, &(), &())
        .await
        .unwrap();
    write_trailer(&mut out, &hasher.finish()).await.unwrap();
    out
}

/// The receiver takes a stream into its kept partial: checks it, cuts the
/// partial to `have`, hashes that from the disk, appends the rest and
/// compares the trailer. `Ok` when the whole file's hash matched.
async fn take(side: &Side, resent: &mut Resent, stream: &[u8], same: &[u8]) -> Result<(), Take> {
    let mut reader = stream;
    let prefix = Resumable::read_prefix(&mut reader).await.unwrap();
    let record = side.store.received(PEER, prefix.key);
    let take = resent.check(PEER, record.as_ref(), &prefix, same);
    if take != Take::Append {
        return Err(take);
    }
    let (_, file) = side.store.reopen(PEER, prefix.key).unwrap();
    let mut hasher = Hasher::new(1);
    let mut disk = tokio::fs::File::from_std(file.try_clone().unwrap());
    hasher.read(&mut disk, prefix.from).await.unwrap();
    let mut file = tokio::fs::File::from_std(file);
    file.seek(SeekFrom::Start(prefix.from)).await.unwrap();
    let (size, from) = (prefix.size, prefix.from);
    copy_exact(&mut reader, &mut file, size, from, &mut hasher, &(), &())
        .await
        .unwrap();
    let trailer = read_trailer(&mut reader).await.unwrap();
    if hasher.finish() == trailer {
        Ok(())
    } else {
        Err(Take::Refuse)
    }
}

/// The session ended, or the process did: the store holds no record of
/// the peer, and takes none to be in use.
fn later(side: &Side) {
    side.store.release_peer(PEER);
}

fn journal(side: &Side) -> Journal {
    Journal::start(&side.journals, &side.root, &Default::default()).unwrap()
}

#[tokio::test]
async fn a_kept_file_continues_from_its_last_whole_chunk_and_matches_the_hash() {
    let side = side("continues", Limits::default());
    let bytes = data(3 * C + 1000);
    let journal = journal(&side);
    // The cut came halfway through the third chunk.
    let got = 2 * C + C / 2;
    let record = received(&side, &journal, 0, key(1), "f.bin", &bytes, got, 10).unwrap();
    assert_eq!(record.have, 2 * C);
    drop(journal);
    later(&side);
    let held = side.store.held_from(PEER).await.unwrap();
    let prefix = hash(&bytes[..(2 * C) as usize]);
    let expected = Held {
        key: key(1),
        have: 2 * C,
        prefix,
    };
    assert_eq!(held, [expected]);
    let stream = send(key(1), &bytes, held[0].have).await;
    let mut resent = Resent::new();
    assert_eq!(take(&side, &mut resent, &stream, b"f.bin").await, Ok(()));
    assert_eq!(std::fs::read(side.place.join("f.bin.part")).unwrap(), bytes);
}

#[tokio::test]
async fn a_crash_after_a_record_write_resumes_or_restarts_never_keeps_a_wrong_byte() {
    let bytes = data(3 * C + 10);
    for case in ["lagging", "damaged past have", "shorter", "damaged within"] {
        let side = side(&format!("crash-record-{}", case.len()), Limits::default());
        let journal = journal(&side);
        // The record says one chunk; two and a half were written before the crash.
        let record = received(&side, &journal, 0, key(2), "f.bin", &bytes, C, 10).unwrap();
        drop(journal);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(side.place.join("f.bin.part"))
            .unwrap();
        file.seek(SeekFrom::Start(C)).unwrap();
        file.write_all(&bytes[C as usize..(2 * C + C / 2) as usize])
            .unwrap();
        match case {
            "damaged past have" => {
                file.seek(SeekFrom::Start(C + 5)).unwrap();
                file.write_all(b"XX").unwrap();
            }
            "shorter" => file.set_len(C - 1).unwrap(),
            "damaged within" => {
                file.seek(SeekFrom::Start(5)).unwrap();
                file.write_all(b"XX").unwrap();
            }
            _ => {}
        }
        drop(file);
        later(&side);
        let held = side.store.held_from(PEER).await.unwrap();
        if matches!(case, "shorter" | "damaged within") {
            // Removed with its record: the file comes again from zero.
            assert!(held.is_empty(), "{case}");
            assert!(side.store.received(PEER, key(2)).is_none());
            assert!(!side.place.join("f.bin.part").exists(), "{case}");
            continue;
        }
        assert_eq!(held[0].have, record.have, "{case}");
        let stream = send(key(2), &bytes, held[0].have).await;
        let mut resent = Resent::new();
        assert_eq!(
            take(&side, &mut resent, &stream, b"f.bin").await,
            Ok(()),
            "{case}"
        );
        let on_disk = std::fs::read(side.place.join("f.bin.part")).unwrap();
        assert!(on_disk == bytes, "{case}: a wrong byte was kept");
    }
}

#[tokio::test]
async fn a_changed_prefix_sends_the_file_from_zero() {
    let side = side("changed-prefix", Limits::default());
    let bytes = data(2 * C + 3);
    let sending = source(&side, key(3), &bytes);
    let opened = Source::open(&sending).unwrap();
    // The receiver says it holds a chunk whose hash is not the sender's.
    let held = Held {
        key: key(3),
        have: C,
        prefix: hash(b"something else"),
    };
    let next = sender_checks(
        Some(&sending),
        Some(&opened),
        &held,
        None,
        &Default::default(),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(next, Next::Restart);
    assert_eq!(Reply::of(next), Reply::Stop(Verdict::Restart));
    // The right prefix continues, from the hasher's kept boundary or from the disk.
    let held = Held {
        key: key(3),
        have: C,
        prefix: hash(&bytes[..C as usize]),
    };
    let mut hasher = Hasher::new(4);
    hasher.update(&bytes);
    for kept in [Some(&hasher), None] {
        let next = sender_checks(
            Some(&sending),
            Some(&opened),
            &held,
            kept,
            &Default::default(),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(next, Next::Continue { from: C });
    }
    // A receiver whose partial changed within its have removes it and lists nothing.
    let journal = journal(&side);
    received(&side, &journal, 0, key(4), "g.bin", &bytes, 2 * C, 10).unwrap();
    drop(journal);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(side.place.join("g.bin.part"))
        .unwrap();
    file.write_all(b"changed").unwrap();
    drop(file);
    later(&side);
    assert!(side.store.held_from(PEER).await.unwrap().is_empty());
    assert!(!side.place.join("g.bin.part").exists());
    // Its stream from zero is then a new file.
    let mut resent = Resent::new();
    let zero = Resumable {
        key: key(4),
        from: 0,
        size: 2 * C + 3,
    };
    assert_eq!(resent.check(PEER, None, &zero, b"g.bin"), Take::New);
}

#[tokio::test]
async fn a_changed_source_is_changed_not_continued() {
    let bytes = data(2 * C);
    let held = Held {
        key: key(5),
        have: C,
        prefix: hash(&bytes[..C as usize]),
    };
    for case in ["size", "time", "replaced", "gone"] {
        let side = side(&format!("changed-{case}"), Limits::default());
        let sending = source(&side, key(5), &bytes);
        match case {
            "size" => {
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&sending.path)
                    .unwrap();
                file.write_all(b"more").unwrap();
            }
            "time" => {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&sending.path)
                    .unwrap();
                let at = Time {
                    secs: 1_000_000_000,
                    nanos: 0,
                };
                file.set_modified(at.to_system().unwrap()).unwrap();
            }
            "replaced" => {
                let other = sending.path.with_extension("new");
                std::fs::write(&other, &bytes).unwrap();
                std::fs::remove_file(&sending.path).unwrap();
                std::fs::rename(&other, &sending.path).unwrap();
            }
            _ => std::fs::remove_file(&sending.path).unwrap(),
        }
        let opened = Source::open(&sending);
        let next = sender_checks(
            Some(&sending),
            opened.as_ref(),
            &held,
            None,
            &Default::default(),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(next, Next::Changed, "{case}");
        assert_eq!(Reply::of(next), Reply::Stop(Verdict::Dropped));
    }
    // No record at all: dropped.
    let side = side("changed-none", Limits::default());
    let sending = source(&side, key(5), &bytes);
    let opened = Source::open(&sending).unwrap();
    let next = sender_checks(None, Some(&opened), &held, None, &Default::default())
        .await
        .unwrap()
        .0;
    assert_eq!(next, Next::Dropped);
}

#[tokio::test]
async fn an_offset_past_the_size_or_off_a_chunk_is_refused() {
    let side = side("offsets", Limits::default());
    let bytes = data(2 * C);
    let journal = journal(&side);
    let record = received(&side, &journal, 0, key(6), "f.bin", &bytes, C, 10).unwrap();
    let size = record.size;
    for from in [size + C, C + 1, C - 1, 3 * C] {
        let stream = Resumable {
            key: key(6),
            from,
            size,
        };
        let take = receiver_takes(Some(&record), &stream, b"f.bin");
        assert_eq!(take, Take::Refuse, "from {from}");
    }
    // From past zero under a key not kept.
    let stream = Resumable {
        key: key(60),
        from: C,
        size,
    };
    assert_eq!(receiver_takes(None, &stream, b"f.bin"), Take::Refuse);
    // The sender restarts a have it cannot continue from.
    let sending = source(&side, key(6), &bytes);
    let opened = Source::open(&sending).unwrap();
    for have in [C + 1, 3 * C] {
        let held = Held {
            key: key(6),
            have,
            prefix: hash(b""),
        };
        let next = sender_checks(
            Some(&sending),
            Some(&opened),
            &held,
            None,
            &Default::default(),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(next, Next::Restart, "have {have}");
    }
    // A record is kept only on whole chunks within the size.
    let file = side.root.reopen(OsStr::new("f.bin.part")).unwrap();
    let mut bad = record.clone();
    bad.have = C + 1;
    assert_eq!(
        side.store.receiving(&bad, &file),
        Err(ResumeError::OffChunk)
    );
    bad.have = 3 * C;
    assert_eq!(
        side.store.receiving(&bad, &file),
        Err(ResumeError::PastSize)
    );
    drop((file, journal));
    // And the handshake never continues from anywhere else.
    let (mut a, mut b) = tokio::io::duplex(1 << 16);
    let held = [Held {
        key: key(6),
        have: C,
        prefix: record.prefix,
    }];
    let limits = Limits::default();
    let receiver = handshake::answer(&mut a, &held, &limits);
    let liar = async {
        let mut seen = vec![0u8; 2 + held[0].encode().len()];
        tokio::io::AsyncReadExt::read_exact(&mut b, &mut seen)
            .await
            .unwrap();
        let reply = handshake::Reply::Continue { from: 2 * C }.encode();
        tokio::io::AsyncWriteExt::write_all(&mut b, &reply)
            .await
            .unwrap();
    };
    let (answered, ()) = tokio::join!(receiver, liar);
    assert_eq!(answered, Err(ResumeError::Damaged));
}

#[tokio::test]
async fn a_resume_or_resend_whose_same_bytes_differ_is_refused() {
    let side = side("same", Limits::default());
    let bytes = data(2 * C);
    let journal = journal(&side);
    let record = received(&side, &journal, 0, key(7), "report.pdf", &bytes, C, 10).unwrap();
    let mut resent = Resent::new();
    let resume = Resumable {
        key: key(7),
        from: C,
        size: record.size,
    };
    assert_eq!(
        resent.check(PEER, Some(&record), &resume, b"setup.exe"),
        Take::Refuse
    );
    let zero = Resumable { from: 0, ..resume };
    assert_eq!(
        resent.check(PEER, Some(&record), &zero, b"setup.exe"),
        Take::Refuse
    );
    // Another size is another file too.
    let longer = Resumable {
        size: record.size + C,
        ..resume
    };
    assert_eq!(
        resent.check(PEER, Some(&record), &longer, b"report.pdf"),
        Take::Refuse
    );
    // A marked key's resend must repeat the mark's same bytes.
    let mut resent = Resent::new();
    resent.mark(PEER, key(8), b"report.pdf");
    let resend = Resumable {
        key: key(8),
        from: 0,
        size: 10,
    };
    assert_eq!(
        resent.check(PEER, None, &resend, b"setup.exe"),
        Take::Refuse
    );
    assert_eq!(
        resent.check(PEER, None, &resend, b"report.pdf"),
        Take::Refuse,
        "a refusal spends the mark"
    );
}

#[tokio::test]
async fn a_resent_key_takes_one_resend_and_refuses_the_next() {
    let mut resent = Resent::new();
    // Damaged once: marked.
    resent.mark(PEER, key(9), b"a.txt");
    let zero = Resumable {
        key: key(9),
        from: 0,
        size: 100,
    };
    assert_eq!(resent.check(PEER, None, &zero, b"a.txt"), Take::Resend);
    // Damaged twice: a second mark is no second resend.
    resent.mark(PEER, key(9), b"a.txt");
    assert_eq!(resent.check(PEER, None, &zero, b"a.txt"), Take::Refuse);
    let later = Resumable {
        from: C,
        size: 2 * C,
        ..zero
    };
    assert_eq!(resent.check(PEER, None, &later, b"a.txt"), Take::Refuse);
    // Without a mark, from zero under a key not kept is a new file.
    let fresh = Resumable {
        key: key(10),
        from: 0,
        size: 100,
    };
    assert_eq!(resent.check(PEER, None, &fresh, b"b.txt"), Take::New);
}

#[tokio::test]
async fn a_from_zero_stream_under_a_kept_key_restarts_and_is_the_one_resend() {
    let side = side("from-zero", Limits::default());
    let bytes = data(2 * C);
    let journal = journal(&side);
    let record = received(&side, &journal, 0, key(11), "f.bin", &bytes, C, 10).unwrap();
    drop(journal);
    let mut resent = Resent::new();
    let zero = Resumable {
        key: key(11),
        from: 0,
        size: record.size,
    };
    // Not a new file: the one resend, which the caller takes after removing the record.
    assert_eq!(
        resent.check(PEER, Some(&record), &zero, b"f.bin"),
        Take::Resend
    );
    // The save that wrote the record is over.
    later(&side);
    side.store.forget(PEER, key(11)).unwrap();
    assert!(side.store.received(PEER, key(11)).is_none());
    assert!(
        !side.place.join("f.bin.part").exists(),
        "removed by identity"
    );
    // Any further stream under the key is refused: no second copy, no loop.
    assert_eq!(resent.check(PEER, None, &zero, b"f.bin"), Take::Refuse);
    let resume = Resumable { from: C, ..zero };
    assert_eq!(resent.check(PEER, None, &resume, b"f.bin"), Take::Refuse);
}

#[tokio::test]
async fn a_record_never_matches_another_peer() {
    let side = side("other-peer", Limits::default());
    let bytes = data(2 * C);
    let journal = journal(&side);
    received(&side, &journal, 0, key(12), "f.bin", &bytes, C, 10).unwrap();
    drop(journal);
    let sending = source(&side, key(13), &bytes);
    assert!(side.store.received(OTHER, key(12)).is_none());
    assert!(side.store.sent(OTHER, key(13)).is_none());
    assert!(side.store.held_from(OTHER).await.unwrap().is_empty());
    side.store.forget(OTHER, key(12)).unwrap();
    assert_eq!(side.store.remove_all(Some(OTHER)).unwrap(), 0);
    assert!(side.store.received(PEER, key(12)).is_some());
    assert!(side.place.join("f.bin.part").exists());
    // A stream from past zero from another peer is no resume.
    let mut resent = Resent::new();
    let stream = Resumable {
        key: key(12),
        from: C,
        size: 2 * C,
    };
    assert!(side.store.received(OTHER, key(12)).is_none());
    // Even handed this peer's record by mistake, another peer's stream is no resume.
    let ours = side.store.received(PEER, key(12));
    assert_eq!(
        resent.check(OTHER, ours.as_ref(), &stream, b"f.bin"),
        Take::Refuse
    );
    let zero = Resumable { from: 0, ..stream };
    assert_eq!(
        resent.check(OTHER, ours.as_ref(), &zero, b"f.bin"),
        Take::New
    );
    // A mark is the peer's too.
    resent.mark(PEER, key(14), b"x");
    assert!(!resent.marked(OTHER, key(14)));
    // Another peer's list finds no sending record to continue from.
    let held = Held {
        key: key(13),
        have: C,
        prefix: hash(&bytes[..C as usize]),
    };
    let record = side.store.sent(OTHER, key(13));
    let opened = Source::open(&sending);
    let next = sender_checks(
        record.as_ref(),
        opened.as_ref(),
        &held,
        None,
        &Default::default(),
    )
    .await
    .unwrap()
    .0;
    assert_eq!(next, Next::Dropped);
}

#[tokio::test]
async fn a_crash_before_the_first_chunk_leaves_no_partial_behind() {
    let side = side("before-first-chunk", Limits::default());
    let journal = journal(&side);
    // Under one chunk: no record; and a file of one chunk at least, kept.
    assert!(
        received(
            &side,
            &journal,
            0,
            key(15),
            "small.txt",
            &data(C),
            C - 1,
            10
        )
        .is_none()
    );
    received(
        &side,
        &journal,
        1,
        key(16),
        "large.bin",
        &data(2 * C),
        C,
        10,
    )
    .unwrap();
    // One created, with no byte yet.
    let empty = side.root.create(OsStr::new("empty.txt.part")).unwrap();
    journal
        .started(
            2,
            &side.root,
            &empty,
            OsStr::new("empty.txt.part"),
            "empty.txt",
            5,
        )
        .unwrap();
    drop(empty);
    // The crash: the journal is left with three partials recorded.
    drop(journal);
    let done = side
        .store
        .clean_up(&side.journals, &Default::default())
        .unwrap();
    assert_eq!((done.removed, done.left), (2, 1));
    assert!(!side.place.join("small.txt.part").exists());
    assert!(!side.place.join("empty.txt.part").exists());
    assert!(
        side.place.join("large.bin.part").exists(),
        "a valid record keeps it"
    );
}

#[tokio::test]
async fn a_torn_record_falls_back_to_its_other_slot_or_the_journal() {
    let mut side = side("torn-record", Limits::default());
    let bytes = data(3 * C);
    let journal = journal(&side);
    let got = 2 * C + 5;
    let record = received(&side, &journal, 0, key(17), "f.bin", &bytes, got, 10).unwrap();
    assert_eq!(record.have, 2 * C);
    // A rewrite torn halfway: the other slot still reads.
    let file = side.root.reopen(OsStr::new("f.bin.part")).unwrap();
    let one = hash(&bytes[..C as usize]);
    seams::TEAR.set(true);
    side.store
        .keep(
            PEER,
            &Held {
                key: key(17),
                have: C,
                prefix: one,
            },
            20,
            &file,
            true,
        )
        .unwrap();
    assert_eq!(side.store.received(PEER, key(17)).unwrap().have, 2 * C);
    // The next rewrite goes over the torn slot and is read.
    side.store
        .keep(
            PEER,
            &Held {
                key: key(17),
                have: C,
                prefix: one,
            },
            30,
            &file,
            true,
        )
        .unwrap();
    assert_eq!(side.store.received(PEER, key(17)).unwrap().have, C);
    drop((file, journal));
    // Both slots damaged: no record, and the journal's entry removes the partial.
    let records = side.base.join("records");
    side.store = Store::open(&records, Limits::default()).unwrap();
    let path = std::fs::read_dir(&records)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut raw = std::fs::read(&path).unwrap();
    for at in [20, record::SLOT + 20] {
        raw[at] ^= 0xFF;
    }
    // Another length, as every write of a record leaves: the listing shows it.
    raw.push(0);
    std::fs::write(&path, &raw).unwrap();
    assert!(side.store.received(PEER, key(17)).is_none());
    assert!(side.store.held_from(PEER).await.unwrap().is_empty());
    let done = side
        .store
        .clean_up(&side.journals, &Default::default())
        .unwrap();
    assert_eq!((done.removed, done.left), (1, 0));
    assert!(!side.place.join("f.bin.part").exists());
    // The file that is no record goes at the next expiry.
    let expired = side
        .store
        .expire(40, Duration::from_secs(3600), &[])
        .unwrap();
    assert_eq!(expired.removed, 1);
    assert!(!path.exists());
}

#[tokio::test]
async fn expire_never_removes_a_live_sessions_record() {
    let limits = Limits {
        per_peer: 1,
        ..Limits::default()
    };
    let side = side("expire-live", limits);
    let day = 24 * 60 * 60;
    kept_zeros(&side, PEER, key(20), "live", 1, 0);
    kept_zeros(&side, PEER, key(21), "paused", 1, 0);
    kept_zeros(&side, PEER, key(22), "old", 1, 0);
    source(&side, key(23), b"sent");
    // Long past the keep time, and two past the per-peer cap.
    let live = [(PEER, key(20)), (PEER, key(21)), (PEER, key(23))];
    let expired = side
        .store
        .expire(30 * day, Duration::from_secs(day), &live)
        .unwrap();
    assert_eq!(expired.removed, 1);
    assert!(side.store.received(PEER, key(20)).is_some());
    assert!(side.store.received(PEER, key(21)).is_some());
    assert!(side.store.sent(PEER, key(23)).is_some());
    assert!(side.store.received(PEER, key(22)).is_none());
    assert!(side.place.join("live.part").exists());
    assert!(!side.place.join("old.part").exists());
    // Once the session ended, the time applies.
    side.store
        .expire(30 * day, Duration::from_secs(day), &[])
        .unwrap();
    assert_eq!(side.store.kept_bytes(), 0);
    assert!(side.store.sent(PEER, key(23)).is_none());
}

#[tokio::test]
async fn the_oldest_kept_file_goes_past_the_caps() {
    let limits = Limits {
        per_peer: 2,
        ..Limits::default()
    };
    let side = side("caps", limits);
    for (n, last) in [(30, 300), (31, 100), (32, 200)] {
        kept_zeros(&side, PEER, key(n), &format!("{n}"), 1, last);
    }
    let hour = Duration::from_secs(3600);
    let expired = side.store.expire(400, hour, &[]).unwrap();
    assert_eq!(expired.removed, 1);
    assert!(
        side.store.received(PEER, key(31)).is_none(),
        "the oldest goes"
    );
    assert!(!side.place.join("31.part").exists());
    assert_eq!(side.store.kept_bytes(), 2 * C);
    // Past a quarter of the disk's free space and what they hold, the oldest goes too.
    // Two chunks held and four free: over a quarter of six until one goes.
    seams::FREE.set(Some(4 * C));
    let expired = side.store.expire(400, hour, &[]);
    seams::FREE.set(None);
    let expired = expired.unwrap();
    assert_eq!(expired.removed, 1);
    assert!(side.store.received(PEER, key(32)).is_none());
    assert!(side.store.received(PEER, key(30)).is_some());
    assert_eq!(expired.kept_bytes, C);
}

#[tokio::test]
async fn past_the_overall_cap_the_largest_holder_loses_first() {
    let limits = Limits {
        total: 3,
        ..Limits::default()
    };
    let side = side("overall-cap", limits);
    // The oldest file of all is the small holder's.
    kept_zeros(&side, OTHER, key(40), "theirs", 1, 10);
    for (n, last) in [(41, 20), (42, 30), (43, 40)] {
        kept_zeros(&side, PEER, key(n), &format!("{n}"), 1, last);
    }
    let expired = side
        .store
        .expire(50, Duration::from_secs(3600), &[])
        .unwrap();
    assert_eq!(expired.removed, 1);
    assert!(side.store.received(OTHER, key(40)).is_some());
    assert!(
        side.store.received(PEER, key(41)).is_none(),
        "the largest holder's oldest"
    );
    assert!(side.store.received(PEER, key(42)).is_some());
    assert!(side.store.received(PEER, key(43)).is_some());
}

#[tokio::test]
async fn a_crash_while_a_partial_is_written_continues_only_that_partial() {
    use safecopy_manifest::{Entry, GroupLimits, Kind, groups};
    let side = side("crash-partial", Limits::default());
    let settings = safecopy_journal::Settings {
        groups: GroupLimits {
            entries: 4,
            min_entries: 4,
            ..GroupLimits::default()
        },
        ..Default::default()
    };
    let sizes = [10, 10, 10, 10, 10, 2 * C, 10, 10];
    let list: Vec<Entry> = sizes
        .iter()
        .enumerate()
        .map(|(n, size)| Entry {
            path: format!("F/{n}"),
            kind: Kind::File,
            size: *size,
        })
        .collect();
    let cut = groups(&list, &settings.groups);
    let journal = Journal::start(&side.journals, &side.root, &settings).unwrap();
    let publish = |p: u32| {
        let name = format!("{p}.part");
        let file = side.root.create(OsStr::new(&name)).unwrap();
        journal
            .started(p, &side.root, &file, OsStr::new(&name), &p.to_string(), 10)
            .unwrap();
        drop(file);
        std::fs::rename(side.place.join(&name), side.place.join(p.to_string())).unwrap();
        journal.done(p).unwrap();
    };
    journal.group(0, cut.get(0).unwrap()).unwrap();
    (0..4).for_each(publish);
    journal.group(1, cut.get(1).unwrap()).unwrap();
    publish(4);
    // Place 5, a large file, cut after a chunk and a half; place 6 under one chunk.
    let big = data(2 * C);
    received(&side, &journal, 5, key(50), "5", &big, C + C / 2, 1).unwrap();
    assert!(received(&side, &journal, 6, key(51), "6", b"1234567890", 5, 1).is_none());
    drop(journal);

    let got = safecopy_journal::recover(&side.journals, &settings)
        .unwrap()
        .remove(0);
    for p in 0..5 {
        assert!(got.progress().skips(p), "place {p} is skipped");
    }
    let partials: Vec<u32> = got.partials().iter().map(|s| s.place).collect();
    assert_eq!(partials, [5, 6]);
    assert!(!got.progress().skips(7), "not started: it comes as usual");
    // Cleanup keeps the partial a record keeps, and removes the one under a chunk.
    let done = side.store.clean_up(&side.journals, &settings).unwrap();
    assert_eq!((done.removed, done.left), (1, 1));
    assert!(!side.place.join("6.part").exists(), "from zero");
    later(&side);
    let held = side.store.held_from(PEER).await.unwrap();
    assert_eq!(held[0].have, C, "from its last verified chunk");
    let stream = send(key(50), &big, C).await;
    assert_eq!(take(&side, &mut Resent::new(), &stream, b"5").await, Ok(()));
    assert_eq!(std::fs::read(side.place.join("5.part")).unwrap(), big);
}

#[tokio::test]
async fn the_handshake_resumes_over_an_in_memory_duplex_stream() {
    let receiver = side("handshake-receiver", Limits::default());
    let sender = side("handshake-sender", Limits::default());
    let bytes = data(3 * C + 100);
    let journal = journal(&receiver);
    let got = 2 * C + 1000;
    received(&receiver, &journal, 0, key(60), "f.bin", &bytes, got, 10).unwrap();
    received(&receiver, &journal, 1, key(61), "gone.bin", &data(C), C, 11).unwrap();
    drop(journal);
    source(&sender, key(60), &bytes);
    later(&receiver);
    let held = receiver.store.held_from(PEER).await.unwrap();
    assert_eq!(held.len(), 2);
    let limits = Limits::default();
    // A small buffer, so each side waits on the other.
    let (mut a, mut b) = tokio::io::duplex(64);
    let answering = handshake::answer(&mut a, &held, &limits);
    let offering = handshake::offer(&mut b, &limits, async |held: &Held| {
        let record = sender.store.sent(PEER, held.key);
        let opened = record.as_ref().and_then(Source::open);
        sender_checks(
            record.as_ref(),
            opened.as_ref(),
            held,
            None,
            &Default::default(),
        )
        .await
        .unwrap()
        .0
    });
    let (answered, offered) = tokio::join!(answering, offering);
    let answered = answered.unwrap();
    assert_eq!(answered, offered.unwrap());
    assert_eq!(answered[0].1, Reply::Continue { from: 2 * C });
    assert_eq!(
        answered[1].1,
        Reply::Stop(Verdict::Dropped),
        "the sender has no record of it"
    );
    // The receiver removes the dropped one; the other continues.
    receiver.store.forget(PEER, key(61)).unwrap();
    assert!(!receiver.place.join("gone.bin.part").exists());
    let stream = send(key(60), &bytes, 2 * C).await;
    let mut resent = Resent::new();
    assert_eq!(
        take(&receiver, &mut resent, &stream, b"f.bin").await,
        Ok(())
    );
    assert_eq!(
        std::fs::read(receiver.place.join("f.bin.part")).unwrap(),
        bytes
    );
}

#[tokio::test]
async fn the_wire_types_have_a_fixed_encoding() {
    let key = TransferKey(std::array::from_fn(|i| i as u8 + 1));
    let mut expected: Vec<u8> = (1..=16).collect();
    expected.extend_from_slice(&[0x80, 0x80, 0x80, 0x02, 0xAC, 0x02]);
    let resumable = Resumable {
        key,
        from: C,
        size: 300,
    };
    assert_eq!(resumable.encode(), expected);
    let mut read = expected.as_slice();
    assert_eq!(Resumable::read_prefix(&mut read).await.unwrap(), resumable);

    let held = Held {
        key,
        have: 2 * C,
        prefix: Digest([0xAB; 32]),
    };
    let mut expected: Vec<u8> = (1..=16).collect();
    expected.extend_from_slice(&[0x80, 0x80, 0x80, 0x04]);
    expected.extend_from_slice(&[0xAB; 32]);
    assert_eq!(held.encode(), expected);
    assert_eq!(Held::decode(&expected).unwrap(), held);

    let verdicts = [
        Verdict::Damaged,
        Verdict::Restart,
        Verdict::Refused,
        Verdict::Dropped,
        Verdict::AlreadyThere,
        Verdict::Paused,
    ];
    for (code, verdict) in verdicts.into_iter().enumerate() {
        assert_eq!(verdict.code(), code as u8);
        assert_eq!(Verdict::from_code(code as u8), Some(verdict));
    }
    assert_eq!(Verdict::from_code(6), None);

    assert_eq!(
        Reply::Continue { from: C }.encode(),
        [0, 0x80, 0x80, 0x80, 0x02]
    );
    assert_eq!(Reply::Stop(Verdict::Dropped).encode(), [1, 3]);

    // The receiver's opening: the version, the count, each Held.
    let (mut a, mut b) = tokio::io::duplex(1 << 10);
    let limits = Limits::default();
    let list = [held];
    let answering = handshake::answer(&mut a, &list, &limits);
    let reading = async {
        let mut opening = vec![0u8; 2 + expected.len()];
        tokio::io::AsyncReadExt::read_exact(&mut b, &mut opening)
            .await
            .unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut b, &[1, 1])
            .await
            .unwrap();
        opening
    };
    let (answered, opening) = tokio::join!(answering, reading);
    assert_eq!(opening[..2], [handshake::VERSION, 1]);
    assert_eq!(opening[2..], expected);
    assert_eq!(answered.unwrap()[0].1, Reply::Stop(Verdict::Restart));
}

#[tokio::test]
async fn a_handshake_list_past_the_cap_or_damaged_is_refused() {
    let limits = Limits {
        per_peer: 2,
        ..Limits::default()
    };
    let checked = std::cell::Cell::new(0);
    for bytes in [
        vec![handshake::VERSION, 3],
        vec![2, 0],
        vec![handshake::VERSION, 1, 0],
        vec![handshake::VERSION, 0x80],
    ] {
        let (mut a, mut b) = tokio::io::duplex(1 << 10);
        tokio::io::AsyncWriteExt::write_all(&mut b, &bytes)
            .await
            .unwrap();
        drop(b);
        let offered = handshake::offer(&mut a, &limits, async |_: &Held| {
            checked.set(checked.get() + 1);
            Next::Dropped
        })
        .await;
        assert!(offered.is_err(), "{bytes:?}");
    }
    assert_eq!(checked.get(), 0, "nothing checked before the list is whole");
}

#[tokio::test]
async fn a_disk_keeps_a_quarter_of_its_free_space_and_what_is_kept_there() {
    let side = side("disk-share", Limits::default());
    for (n, last) in [(70, 10), (71, 20), (72, 30)] {
        kept_zeros(&side, PEER, key(n), &format!("{n}"), 1, last);
    }
    let hour = Duration::from_secs(3600);
    // Three chunks held and two free: a quarter of five keeps one, the newest.
    seams::FREE.set(Some(2 * C));
    let expired = side.store.expire(40, hour, &[]);
    seams::FREE.set(None);
    assert_eq!(expired.unwrap().removed, 2);
    assert!(side.store.received(PEER, key(72)).is_some());
    assert!(side.store.received(PEER, key(70)).is_none());
    assert!(!side.place.join("71.part").exists());
    // Nothing free: a quarter of what is held keeps nothing.
    seams::FREE.set(Some(0));
    let expired = side.store.expire(40, hour, &[]);
    seams::FREE.set(None);
    assert_eq!(expired.unwrap().kept_bytes, 0);
    assert!(side.store.received(PEER, key(72)).is_none());
}

#[tokio::test]
async fn a_record_keeps_its_partial_whatever_root_it_was_recorded_from() {
    let side = side("other-root", Limits::default());
    let journal = journal(&side);
    let mut file = side.root.create(OsStr::new("f.bin.part")).unwrap();
    journal
        .started(
            0,
            &side.root,
            &file,
            OsStr::new("f.bin.part"),
            "f.bin",
            2 * C,
        )
        .unwrap();
    let bytes = data(2 * C);
    file.write_all(&bytes[..C as usize]).unwrap();
    // The record names the same folder from the folder above the journal's root.
    let above = Directory::destination(&side.base).unwrap();
    let work = above.directory(OsStr::new("Work")).unwrap();
    let record = Receiving {
        peer: PEER,
        key: key(80),
        same: Vec::new(),
        size: 2 * C,
        at: PartialAt::of(&above, &work, &file, OsStr::new("f.bin.part")).unwrap(),
        have: C,
        prefix: hash(&bytes[..C as usize]),
        created: 1,
        last: 1,
    };
    side.store.receiving(&record, &file).unwrap();
    drop((file, journal));
    let done = side
        .store
        .clean_up(&side.journals, &Default::default())
        .unwrap();
    assert_eq!((done.removed, done.left), (0, 1));
    assert!(side.place.join("f.bin.part").exists());
}

#[tokio::test]
async fn a_handshake_list_naming_a_key_twice_is_refused() {
    let held = Held {
        key: key(90),
        have: C,
        prefix: Digest([1; 32]),
    };
    let mut bytes = vec![handshake::VERSION, 2];
    bytes.extend_from_slice(&held.encode());
    bytes.extend_from_slice(&held.encode());
    let (mut a, mut b) = tokio::io::duplex(1 << 10);
    tokio::io::AsyncWriteExt::write_all(&mut b, &bytes)
        .await
        .unwrap();
    let checked = std::cell::Cell::new(0);
    let offered = handshake::offer(&mut a, &Limits::default(), async |_: &Held| {
        checked.set(checked.get() + 1);
        Next::Dropped
    })
    .await;
    assert_eq!(offered, Err(ResumeError::Damaged));
    assert_eq!(checked.get(), 0, "nothing is hashed for it");
}

#[tokio::test]
async fn a_stream_from_another_whole_chunk_restarts_and_marks_the_key() {
    let side = side("restart-mark", Limits::default());
    let bytes = data(3 * C);
    let journal = journal(&side);
    let record = received(&side, &journal, 0, key(60), "f.bin", &bytes, 2 * C, 10).unwrap();
    drop(journal);
    let mut resent = Resent::new();
    // On a chunk, within the size, not the kept `have`: the sender restarts.
    let other = Resumable {
        key: key(60),
        from: C,
        size: record.size,
    };
    assert_eq!(
        resent.check(PEER, Some(&record), &other, b"f.bin"),
        Take::Restart
    );
    assert!(resent.marked(PEER, key(60)));
    // The caller removes the record; the resend from zero is taken once.
    later(&side);
    side.store.forget(PEER, key(60)).unwrap();
    let zero = Resumable { from: 0, ..other };
    assert_eq!(resent.check(PEER, None, &zero, b"f.bin"), Take::Resend);
    assert_eq!(resent.check(PEER, None, &zero, b"f.bin"), Take::Refuse);
}

#[tokio::test]
async fn a_kept_record_is_rewritten_only_past_its_bytes_and_time_or_at_a_drop() {
    let side = side("keep-cadence", Limits::default());
    let partial = "big.part";
    let file = side.root.create(OsStr::new(partial)).unwrap();
    file.set_len(C).unwrap();
    let rewrite = Limits::default().rewrite_bytes;
    let record = Receiving {
        peer: PEER,
        key: key(61),
        same: Vec::new(),
        size: C + rewrite + 4 * C,
        at: PartialAt::of(&side.root, &side.root, &file, OsStr::new(partial)).unwrap(),
        have: C,
        prefix: hash(&[1]),
        created: 10,
        last: 10,
    };
    seams::DIR_SYNCS.set(0);
    side.store.receiving(&record, &file).unwrap();
    assert_eq!(seams::DIR_SYNCS.get(), 1, "a new record's name is synced");
    let held = |have: u64| Held {
        key: key(61),
        have,
        prefix: hash(&have.to_le_bytes()),
    };
    let keep = |have: u64, now: u64, at_drop: bool| {
        side.store
            .keep(PEER, &held(have), now, &file, at_drop)
            .unwrap()
    };
    let now = || {
        let r = side.store.received(PEER, key(61)).unwrap();
        (r.have, r.last)
    };
    // Under the bytes, long after: not written.
    assert!(!keep(2 * C, 100, false));
    assert_eq!(now(), (C, 10));
    // Past the bytes, within the time: not written.
    assert!(!keep(C + rewrite, 10, false));
    assert_eq!(now(), (C, 10));
    // Both: written.
    assert!(keep(C + rewrite, 11, false));
    assert_eq!(now(), (C + rewrite, 11));
    assert_eq!(
        side.store.received(PEER, key(61)).unwrap().prefix,
        held(C + rewrite).prefix
    );
    // At a drop, at once.
    assert!(keep(2 * C + rewrite, 11, true));
    assert_eq!(now(), (2 * C + rewrite, 11));
    assert_eq!(seams::DIR_SYNCS.get(), 1, "a rewrite makes no new name");
}

#[tokio::test]
async fn another_store_on_the_same_folder_leaves_this_ones_records_and_partials() {
    let side = side("two-stores", Limits::default());
    kept_zeros(&side, PEER, key(62), "good", 1, 0);
    kept_zeros(&side, PEER, key(63), "bad", 1, 0);
    source(&side, key(64), b"sent");
    // One partial no longer matches its prefix: held_from would remove it.
    let mut bad = std::fs::OpenOptions::new()
        .write(true)
        .open(side.place.join("bad.part"))
        .unwrap();
    bad.write_all(b"x").unwrap();
    drop(bad);
    let other = Store::open(&side.base.join("records"), Limits::default()).unwrap();
    let day = 24 * 60 * 60;
    let expired = other.expire(30 * day, Duration::from_secs(1), &[]).unwrap();
    assert_eq!(expired.removed, 0);
    // Held by the first store: left, and said to be in use.
    let busy = Some(ResumeError::Busy);
    assert_eq!(other.forget(PEER, key(62)).err(), busy);
    assert_eq!(other.forget(PEER, key(64)).err(), busy);
    assert_eq!(other.remove_all(None).unwrap(), 0);
    let listed = other.held_from(PEER).await.unwrap();
    assert_eq!(listed.iter().map(|h| h.key).collect::<Vec<_>>(), [key(62)]);
    for (n, name) in [(62, "good.part"), (63, "bad.part")] {
        assert!(side.store.received(PEER, key(n)).is_some(), "{name}");
        assert!(side.place.join(name).exists(), "{name}");
    }
    assert!(side.store.sent(PEER, key(64)).is_some());
    // Its own store still removes them.
    drop(other);
    assert_eq!(side.store.remove_all(None).unwrap(), 3);
    assert!(!side.place.join("good.part").exists());
    assert!(!side.place.join("bad.part").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_source_replaced_by_a_pipe_is_refused_without_waiting() {
    let side = side("source-fifo", Limits::default());
    let record = source(&side, key(65), b"sent");
    std::fs::remove_file(&record.path).unwrap();
    let c = std::ffi::CString::new(record.path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: `c` is a valid C string.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let (tx, rx) = std::sync::mpsc::channel();
    let opening = record.clone();
    std::thread::spawn(move || {
        let _ = tx.send(Source::open(&opening).is_none());
    });
    let refused = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("opening a pipe must not wait for a writer");
    assert!(refused);
}

// A save that keeps its partial, the store behind it, and the cleanup.

/// The receiver's hooks around a save: the journal hears of the partial
/// when it is created, and the store of each whole chunk.
struct Keeping<'a> {
    side: &'a Side,
    journal: &'a Journal,
    key: TransferKey,
    name: &'a str,
    size: u64,
    partial: std::sync::Mutex<Option<std::ffi::OsString>>,
}

fn failed<E>(_: E) -> std::io::Error {
    std::io::Error::other("refused")
}

impl safecopy_chunks::Hooks for Keeping<'_> {
    fn created(&self, file: &File, partial: &OsStr) -> std::io::Result<()> {
        self.journal
            .started(0, &self.side.root, file, partial, self.name, self.size)
            .map_err(failed)?;
        *self.partial.lock().unwrap() = Some(partial.to_owned());
        Ok(())
    }

    fn whole(&self, file: &File, have: u64, prefix: Digest) -> std::io::Result<()> {
        let partial = self.partial.lock().unwrap().clone().expect("created");
        let (store, root) = (&self.side.store, &self.side.root);
        if store.received(PEER, self.key).is_none() {
            let record = Receiving {
                peer: PEER,
                key: self.key,
                same: self.name.as_bytes().to_vec(),
                size: self.size,
                at: PartialAt::of(root, root, file, &partial).map_err(failed)?,
                have,
                prefix,
                created: 10,
                last: 10,
            };
            return store.receiving(&record, file).map_err(failed);
        }
        let held = Held {
            key: self.key,
            have,
            prefix,
        };
        store
            .keep(PEER, &held, 10, file, false)
            .map(|_| ())
            .map_err(failed)
    }
}

fn cut_short(error: &safecopy_chunks::ChunkError) -> bool {
    matches!(error, safecopy_chunks::ChunkError::CutShort { .. })
}

fn files_in(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn a_cut_save_keeps_its_partial_and_a_later_one_continues_and_publishes() {
    use safecopy_chunks::{SaveOptions, Start, Target, save_resumable};
    use safecopy_folder::{Meta, MetaRules, PublishRules};
    let receiver = side("save-receiver", Limits::default());
    let sender = side("save-sender", Limits::default());
    let bytes = data(3 * C + 1000);
    let size = bytes.len() as u64;
    let sending = source(&sender, key(100), &bytes);

    // The first stream is cut halfway through its third chunk.
    let stream = send(key(100), &bytes, 0).await;
    let mut reader = &stream[..stream.len() - 33 - (C / 2 + 1000) as usize];
    let prefix = Resumable::read_prefix(&mut reader).await.unwrap();
    let mut resent = Resent::new();
    // The file is this save's from before the check's answer is acted on.
    let saving = receiver.store.saving(PEER, key(100)).unwrap();
    assert_eq!(resent.check(PEER, None, &prefix, b"f.bin"), Take::New);
    let first = journal(&receiver);
    let hooks = Keeping {
        side: &receiver,
        journal: &first,
        key: key(100),
        name: "f.bin",
        size,
        partial: Default::default(),
    };
    let stopped = save_resumable(
        &mut reader,
        Target {
            dir: &receiver.root,
            name: "f.bin",
            size,
        },
        Start::New,
        &cut_short,
        &SaveOptions::default(),
        &hooks,
        std::future::pending(),
    )
    .await
    .unwrap_err();
    let kept = stopped.kept.expect("a cut stream is kept");
    assert_eq!(kept.have(), 2 * C);
    // The record had the first chunk from the hook; the cut brings it up to date.
    assert_eq!(receiver.store.received(PEER, key(100)).unwrap().have, C);
    let held = Held {
        key: key(100),
        have: kept.have(),
        prefix: kept.prefix(),
    };
    // Written at once and let go, by the save's guard.
    saving.kept(&held, 20, &kept.file).unwrap();
    assert_eq!(receiver.store.received(PEER, key(100)).unwrap().have, 2 * C);
    // The session ends: nothing is called on the journal for a kept partial.
    drop(kept);
    drop(first);
    assert_eq!(receiver.store.release_peer(PEER), 0, "let go already");
    assert_eq!(files_in(&receiver.journals).len(), 1);
    // A cleanup at the next start leaves the file, and the journal that lists it.
    for _ in 0..2 {
        let done = receiver
            .store
            .clean_up(&receiver.journals, &Default::default())
            .unwrap();
        assert_eq!((done.removed, done.left, done.kept), (0, 1, 1));
        assert_eq!(files_in(&receiver.journals).len(), 1);
    }
    let on_disk = std::fs::read(receiver.place.join("f.bin.part")).unwrap();
    assert!(on_disk == bytes[..(2 * C) as usize], "cut to whole chunks");

    // Later. What is kept is listed with no byte of it read; each side
    // then reads its prefix once, and goes on from that hasher.
    seams::PREFIX_BYTES.set(0);
    let mut listed = receiver.store.kept_by(PEER).unwrap();
    assert_eq!(seams::PREFIX_BYTES.get(), 0);
    let held = listed.remove(0);
    assert!(listed.is_empty());
    assert_eq!((held.key, held.have), (key(100), 2 * C));
    let fresh = hash(&bytes[..(2 * C) as usize]);
    assert_eq!(held.prefix, fresh);
    let opened = Source::open(&sending).unwrap();
    let (next, sent) = sender_checks(
        Some(&sending),
        Some(&opened),
        &held,
        None,
        &Default::default(),
    )
    .await
    .unwrap();
    assert_eq!(next, Next::Continue { from: 2 * C });
    let mut sent = sent.expect("the hasher the check built");
    assert_eq!(seams::PREFIX_BYTES.get(), 2 * C, "once by the sender");
    assert_eq!(sent.position(), 2 * C);
    assert_eq!(sent.clone().finish(), fresh);
    let mut stream = Vec::new();
    let from = held.have;
    Resumable {
        key: key(100),
        from,
        size,
    }
    .write_prefix(&mut stream)
    .await
    .unwrap();
    let mut rest = &bytes[from as usize..];
    copy_exact(&mut rest, &mut stream, size, from, &mut sent, &(), &())
        .await
        .unwrap();
    write_trailer(&mut stream, &sent.finish()).await.unwrap();

    let mut reader = &stream[..];
    let prefix = Resumable::read_prefix(&mut reader).await.unwrap();
    let saving = receiver.store.saving(PEER, key(100)).unwrap();
    let record = receiver.store.received(PEER, key(100)).unwrap();
    assert_eq!(
        resent.check(PEER, Some(&record), &prefix, b"f.bin"),
        Take::Append
    );
    // The prefix is checked through the handle the save appends through.
    let (dir, kept) = receiver.store.reopen_hashed(PEER, key(100)).await.unwrap();
    assert_eq!(seams::PREFIX_BYTES.get(), 4 * C, "and once by the receiver");
    assert_eq!(kept.have(), 2 * C);
    assert_eq!(kept.prefix(), fresh);
    let second = journal(&receiver);
    // The name the record keeps: the one the first save gave its partial.
    let partial = kept.partial.clone();
    assert_eq!(partial, record.at.partial.as_str());
    // The new run's journal lists the partial it continues.
    second
        .started(0, &dir, &kept.file, &partial, "f.bin", size)
        .unwrap();
    let hooks = Keeping {
        side: &receiver,
        journal: &second,
        key: key(100),
        name: "f.bin",
        size,
        partial: std::sync::Mutex::new(Some(partial.clone())),
    };
    let rules = PublishRules {
        mark: true,
        ..PublishRules::default()
    };
    let at = Time {
        secs: 1_000_000_000,
        nanos: 0,
    };
    let options = SaveOptions {
        publish: &rules,
        meta: Meta {
            modified: Some(at),
            executable: true,
        },
        meta_rules: MetaRules {
            times: true,
            executable: true,
        },
        ..SaveOptions::default()
    };
    let start = Start::Resume(Box::new(kept));
    let saved = save_resumable(
        &mut reader,
        Target {
            dir: &dir,
            name: "f.bin",
            size,
        },
        start,
        &cut_short,
        &options,
        &hooks,
        std::future::pending(),
    )
    .await
    .unwrap();
    assert_eq!(seams::PREFIX_BYTES.get(), 4 * C, "no prefix read again");
    assert_eq!(saved.digest, hash(&bytes), "the hash of the whole file");
    // The folder was opened by identity, which on Windows gives it no path.
    assert_eq!(saved.path.file_name().unwrap(), "f.bin");
    let saved_at = receiver.place.join("f.bin");
    assert!(std::fs::read(&saved_at).unwrap() == bytes);
    #[cfg(windows)]
    assert_eq!(saved.marked, safecopy_folder::Marked::Marked);
    let modified = std::fs::metadata(&saved_at).unwrap().modified().unwrap();
    assert_eq!(Time::from_system(modified), Some(at));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&saved_at).unwrap().permissions().mode();
        assert_ne!(mode & 0o100, 0, "the execute bit");
    }
    // Published: settled in its journal, and its record forgotten.
    second.done(0).unwrap();
    // Only the save's own guard forgets a file being saved.
    assert_eq!(
        receiver.store.forget(PEER, key(100)),
        Err(ResumeError::Busy)
    );
    saving.forget().unwrap();
    assert!(receiver.store.received(PEER, key(100)).is_none());
    drop(second);
    assert!(saved_at.exists(), "forgetting removes no published file");
    // The first run's journal has nothing left to wait for.
    let done = receiver
        .store
        .clean_up(&receiver.journals, &Default::default())
        .unwrap();
    assert_eq!((done.removed, done.kept), (0, 0));
    assert!(files_in(&receiver.journals).is_empty());
    assert_eq!(files_in(&receiver.place), ["f.bin"]);
}

#[tokio::test]
async fn a_kept_partial_whose_bytes_changed_is_found_out_and_not_appended_to() {
    let side = side("prefix-changed", Limits::default());
    let bytes = data(2 * C + 5);
    let journal = journal(&side);
    received(&side, &journal, 0, key(101), "good.bin", &bytes, 2 * C, 10).unwrap();
    received(&side, &journal, 1, key(102), "bad.bin", &bytes, 2 * C, 10).unwrap();
    drop(journal);
    later(&side);
    // Unchanged: its hasher stands at `have`, as a fresh hash of the prefix.
    seams::PREFIX_BYTES.set(0);
    let (_, kept) = side.store.reopen_hashed(PEER, key(101)).await.unwrap();
    assert_eq!(seams::PREFIX_BYTES.get(), 2 * C, "read once");
    assert_eq!(kept.have(), 2 * C);
    assert_eq!(kept.prefix(), hash(&bytes[..(2 * C) as usize]));
    assert_eq!(kept.partial, "good.bin.part");
    assert_eq!(kept.file.metadata().unwrap().len(), 2 * C);
    drop(kept);
    // One byte within `have` changed on the disk.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(side.place.join("bad.bin.part"))
        .unwrap();
    file.seek(SeekFrom::Start(C + 7)).unwrap();
    file.write_all(&[bytes[(C + 7) as usize] ^ 1]).unwrap();
    drop(file);
    let changed = side.store.reopen_hashed(PEER, key(102)).await;
    assert_eq!(changed.err(), Some(ResumeError::PrefixChanged));
    assert!(side.store.received(PEER, key(102)).is_none());
    assert!(!side.place.join("bad.bin.part").exists(), "from zero");
    assert_eq!(
        side.store.reopen_hashed(PEER, key(102)).await.err(),
        Some(ResumeError::NotHeld)
    );
    assert!(side.place.join("good.bin.part").exists());
}

#[tokio::test]
async fn a_kept_partial_whose_record_is_torn_leaves_no_orphan_after_cleanup() {
    let side = side("torn-kept", Limits::default());
    let bytes = data(2 * C);
    let journal = journal(&side);
    received(&side, &journal, 0, key(103), "f.bin", &bytes, C, 10).unwrap();
    // The cut: the partial is kept and the journal left as it is.
    drop(journal);
    let clean = || {
        side.store
            .clean_up(&side.journals, &Default::default())
            .unwrap()
    };
    // A good record: cleanup keeps the file and the journal, each time,
    // whatever fewer journals or shorter time the caller's settings give.
    let none = safecopy_journal::Settings {
        keep_most: 0,
        keep_for: Duration::ZERO,
        ..Default::default()
    };
    let done = side.store.clean_up(&side.journals, &none).unwrap();
    assert_eq!((done.removed, done.left, done.kept), (0, 1, 1));
    for _ in 0..2 {
        let done = clean();
        assert_eq!((done.removed, done.left, done.kept), (0, 1, 1));
        assert!(side.place.join("f.bin.part").exists());
        assert_eq!(files_in(&side.journals).len(), 1);
    }
    // The record is torn in both slots.
    assert!(side.store.release(PEER, key(103)));
    let path = std::fs::read_dir(side.base.join("records"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut raw = std::fs::read(&path).unwrap();
    for at in [20, record::SLOT + 20] {
        raw[at] ^= 0xFF;
    }
    raw.push(0);
    std::fs::write(&path, &raw).unwrap();
    assert!(side.store.received(PEER, key(103)).is_none());
    // The journal still knows the file: the partial goes, then the journal.
    let done = clean();
    assert_eq!((done.removed, done.left, done.kept), (1, 0, 0));
    assert!(!side.place.join("f.bin.part").exists(), "no orphan");
    assert!(files_in(&side.journals).is_empty());
    let expired = side.store.expire(20, Duration::from_secs(3600), &[]);
    assert_eq!(expired.unwrap().removed, 1);
    assert!(!path.exists());
}

#[tokio::test]
async fn two_stores_on_one_folder_read_each_others_records_and_write_only_their_own() {
    let side = side("two-writers", Limits::default());
    let records = side.base.join("records");
    let (a, b) = (
        &side.store,
        Store::open(&records, Limits::default()).unwrap(),
    );
    let of_a = kept_in(a, &side, PEER, key(110), "a", 1, 5);
    let of_b = kept_in(&b, &side, PEER, key(111), "b", 1, 6);
    // Each reads the other's, and counts its partial as kept.
    assert_eq!(b.received(PEER, key(110)), Some(of_a.clone()));
    assert_eq!(a.received(PEER, key(111)), Some(of_b.clone()));
    assert!(b.keeps(&of_a.at) && a.keeps(&of_b.at));
    let listed = |held: Vec<Held>| held.iter().map(|h| h.key).collect::<Vec<_>>();
    assert_eq!(listed(a.kept_by(PEER).unwrap()), [key(110), key(111)]);
    assert_eq!(listed(b.kept_by(PEER).unwrap()), [key(110), key(111)]);
    // A check that reads them passes by the one its own store has in use.
    assert_eq!(listed(a.held_from(PEER).await.unwrap()), [key(111)]);
    assert_eq!(listed(b.held_from(PEER).await.unwrap()), [key(110)]);
    // A journal's cleanup run by either leaves both partials.
    let journal = journal(&side);
    for (place, name) in [(0, "a.part"), (1, "b.part")] {
        let file = side.root.reopen(OsStr::new(name)).unwrap();
        journal
            .started(place, &side.root, &file, OsStr::new(name), name, C)
            .unwrap();
    }
    drop(journal);
    for store in [a, &b] {
        let done = store.clean_up(&side.journals, &Default::default()).unwrap();
        assert_eq!((done.removed, done.left), (0, 2));
    }
    // Neither writes the other's record.
    let file = side.root.reopen(OsStr::new("a.part")).unwrap();
    let held = Held {
        key: key(110),
        have: C,
        prefix: Digest([3; 32]),
    };
    assert_eq!(b.keep(PEER, &held, 9, &file, true), Err(ResumeError::Busy));
    assert_eq!(b.receiving(&of_a, &file), Err(ResumeError::Busy));
    let theirs = Held {
        key: key(111),
        ..held
    };
    assert_eq!(
        a.keep(PEER, &theirs, 9, &file, true),
        Err(ResumeError::Busy)
    );
    // Nor reopens its partial, which the other may be appending to.
    file.set_len(C + 9).unwrap();
    assert_eq!(b.reopen(PEER, key(110)).err(), Some(ResumeError::Busy));
    assert_eq!(
        b.reopen_hashed(PEER, key(110)).await.err(),
        Some(ResumeError::Busy)
    );
    assert_eq!(file.metadata().unwrap().len(), C + 9, "not cut under it");
    assert_eq!(a.received(PEER, key(110)), Some(of_a.clone()), "as it was");
    assert_eq!(b.received(PEER, key(111)), Some(of_b.clone()), "as it was");
    drop(file);
    // Nor removes it, or its partial, by any way.
    let day = 24 * 60 * 60;
    for (store, other) in [(a, key(111)), (&b, key(110))] {
        assert_eq!(store.forget(PEER, other).err(), Some(ResumeError::Busy));
    }
    for store in [a, &b] {
        let expired = store.expire(30 * day, Duration::from_secs(1), &[(PEER, key(0))]);
        assert_eq!(expired.unwrap().removed, 1, "its own, past its time");
    }
    assert!(!side.place.join("a.part").exists() && !side.place.join("b.part").exists());
    let of_a = kept_in(a, &side, PEER, key(112), "c", 1, 5);
    assert_eq!(b.remove_all(None).unwrap(), 0);
    assert_eq!(
        b.expire(30 * day, Duration::from_secs(1), &[])
            .unwrap()
            .removed,
        0
    );
    assert!(side.place.join("c.part").exists());
    assert_eq!(a.received(PEER, key(112)), Some(of_a));
}

#[tokio::test]
async fn a_released_record_is_another_stores_to_take_over() {
    let side = side("release", Limits::default());
    let records = side.base.join("records");
    let (a, b) = (
        &side.store,
        Store::open(&records, Limits::default()).unwrap(),
    );
    kept_in(a, &side, PEER, key(120), "f", 2, 5);
    kept_in(a, &side, OTHER, key(121), "g", 1, 5);
    let file = side.root.reopen(OsStr::new("f.part")).unwrap();
    let held = |have: u64| Held {
        key: key(120),
        have,
        prefix: Digest([have as u8 / 3; 32]),
    };
    assert_eq!(
        b.keep(PEER, &held(C), 9, &file, true),
        Err(ResumeError::Busy)
    );
    // Released, not removed.
    assert!(a.release(PEER, key(120)));
    assert!(!a.release(PEER, key(120)), "held no more");
    assert_eq!(a.received(PEER, key(120)).unwrap().have, 2 * C);
    assert!(side.place.join("f.part").exists());
    // Now the other store writes it, and this one reads what it wrote.
    assert_eq!(b.keep(PEER, &held(C), 9, &file, true), Ok(true));
    assert_eq!(a.received(PEER, key(120)).unwrap().have, C);
    assert_eq!(
        a.keep(PEER, &held(2 * C), 9, &file, true),
        Err(ResumeError::Busy)
    );
    assert_eq!(b.keep(PEER, &held(2 * C), 9, &file, true), Ok(true));
    assert_eq!(a.received(PEER, key(120)).unwrap().have, 2 * C);
    // A peer's records are let go together, and only that peer's.
    assert_eq!(b.release_peer(OTHER), 0);
    assert_eq!(b.release_peer(PEER), 1);
    assert_eq!(a.keep(PEER, &held(C), 9, &file, true), Ok(true));
    drop(file);
    assert_eq!(
        b.remove_all(None).unwrap(),
        0,
        "both held by the first again"
    );
    assert_eq!(a.release_peer(OTHER), 1);
    assert_eq!(b.remove_all(Some(OTHER)).unwrap(), 1);
    assert!(!side.place.join("g.part").exists());
    assert!(side.place.join("f.part").exists());
}

#[tokio::test]
async fn a_lookup_among_many_records_opens_only_the_files_that_changed() {
    let side = side("index", Limits::default());
    let records = side.base.join("records");
    let a = &side.store;
    let mut sent: Vec<Sending> = (0..30).map(|n| source(&side, key(n), b"sent")).collect();
    let look = |store: &Store| {
        for n in 0..30 {
            assert!(store.sent(PEER, key(n)).is_some(), "{n}");
        }
        assert!(store.sent(PEER, key(200)).is_none());
        assert!(store.received(PEER, key(0)).is_none());
        assert_eq!(store.kept_bytes(), 0);
    };
    // Its own records are never opened again.
    seams::OPENS.set(0);
    look(a);
    assert_eq!(seams::OPENS.get(), 0);
    // Another store reads each once, when it opens.
    let b = Store::open(&records, Limits::default()).unwrap();
    assert_eq!(seams::OPENS.get(), 30);
    look(&b);
    look(&b);
    assert_eq!(seams::OPENS.get(), 30, "no file opened for a lookup");
    // What the first then writes, new or again, is all the other reads.
    source(&side, key(30), b"sent");
    sent[7].last = 99;
    a.sending(&sent[7]).unwrap();
    assert_eq!(b.sent(PEER, key(7)).unwrap().last, 99);
    assert!(b.sent(PEER, key(30)).is_some());
    look(&b);
    assert_eq!(seams::OPENS.get(), 32);
    // One removed is forgotten without a file opened.
    a.forget(PEER, key(3)).unwrap();
    assert!(b.sent(PEER, key(3)).is_none());
    assert_eq!(seams::OPENS.get(), 32);
    // An index held to four files still finds every record, by reading the rest.
    let small = Limits {
        indexed: 4,
        ..Limits::default()
    };
    let c = Store::open(&records, small).unwrap();
    seams::OPENS.set(0);
    for n in [0, 9, 17, 30] {
        assert!(c.sent(PEER, key(n)).is_some(), "{n}");
    }
    assert_eq!(seams::OPENS.get(), 4 * (30 - 4));
}

#[tokio::test]
async fn expire_takes_live_files_by_peer_and_key_and_caps_sending_records() {
    let limits = Limits {
        sending: 2,
        ..Limits::default()
    };
    let side = side("expire-peer-key", limits);
    let day = 24 * 60 * 60;
    // One key value from two peers: only one of them is live.
    kept_zeros(&side, PEER, key(130), "live", 1, 0);
    kept_zeros(&side, OTHER, key(130), "theirs", 1, 0);
    let live = [(PEER, key(130))];
    let expired = side
        .store
        .expire(30 * day, Duration::from_secs(day), &live)
        .unwrap();
    assert_eq!(expired.removed, 1);
    assert!(side.store.received(PEER, key(130)).is_some());
    assert!(side.store.received(OTHER, key(130)).is_none());
    assert!(side.place.join("live.part").exists());
    assert!(!side.place.join("theirs.part").exists());
    // Five sending records, two kept: the oldest go, never a live one.
    for (n, last) in [(131, 40), (132, 10), (133, 30), (134, 20), (135, 5)] {
        let mut record = source(&side, key(n), b"sent");
        record.last = last;
        side.store.sending(&record).unwrap();
    }
    let live = [(PEER, key(130)), (PEER, key(135)), (OTHER, key(132))];
    let hour = Duration::from_secs(3600);
    let expired = side.store.expire(50, hour, &live).unwrap();
    assert_eq!(expired.removed, 2);
    let left: Vec<u8> = (131..=135)
        .filter(|n| side.store.sent(PEER, key(*n)).is_some())
        .collect();
    assert_eq!(left, [131, 133, 135], "the two newest, and the live one");
}

#[tokio::test]
async fn a_record_write_that_fails_leaves_no_file_behind() {
    let side = side("write-fails", Limits::default());
    let records = side.base.join("records");
    let partial = side.root.create(OsStr::new("f.part")).unwrap();
    partial.set_len(C).unwrap();
    let record = Receiving {
        peer: PEER,
        key: key(140),
        same: Vec::new(),
        size: C,
        at: PartialAt::of(&side.root, &side.root, &partial, OsStr::new("f.part")).unwrap(),
        have: C,
        prefix: Digest([1; 32]),
        created: 1,
        last: 1,
    };
    seams::FULL.set(true);
    let full = side.store.receiving(&record, &partial);
    assert_eq!(full, Err(ResumeError::Io(std::io::ErrorKind::StorageFull)));
    assert!(files_in(&records).is_empty(), "{:?}", files_in(&records));
    assert!(side.store.received(PEER, key(140)).is_none());
    // The next write is the record's first, in one file.
    side.store.receiving(&record, &partial).unwrap();
    assert_eq!(files_in(&records).len(), 1);
    // A rewrite that fails leaves the record as it was.
    seams::FULL.set(true);
    let held = Held {
        key: key(140),
        have: C,
        prefix: Digest([2; 32]),
    };
    assert!(side.store.keep(PEER, &held, 5, &partial, true).is_err());
    assert_eq!(side.store.received(PEER, key(140)), Some(record));
    assert_eq!(files_in(&records).len(), 1);
}

/// The listing is the index's only sign of a change: a record judged by an
/// older reading is read again before it is removed.
#[tokio::test]
async fn a_record_is_not_removed_by_an_older_reading_of_it() {
    let side = side("stale", Limits::default());
    let records = side.base.join("records");
    let a = &side.store;
    let mut record = source(&side, key(150), b"sent");
    let path = records.join(&files_in(&records)[0]);
    let before = std::fs::metadata(&path).unwrap();
    let b = Store::open(&records, Limits::default()).unwrap();
    assert_eq!(b.sent(PEER, key(150)).unwrap().last, 1);
    // Rewritten until its length comes round again, the last time just now.
    for _ in 0..256 {
        record.last = 1_000;
        a.sending(&record).unwrap();
    }
    assert!(a.release(PEER, key(150)));
    // And its time put back: the listing shows nothing new.
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_modified(before.modified().unwrap()).unwrap();
    drop(file);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), before.len());
    assert_eq!(b.sent(PEER, key(150)).unwrap().last, 1, "an older reading");
    // By that reading it is long past its time; as it is, it is not.
    let keep_for = Duration::from_secs(100);
    assert_eq!(b.expire(1_010, keep_for, &[]).unwrap().removed, 0);
    assert!(path.exists());
    assert_eq!(b.sent(PEER, key(150)).unwrap().last, 1_000, "read again");
    b.forget(PEER, key(150)).unwrap();
    assert!(!path.exists());
}

// What a review of the kept save found.

fn keys(held: &[Held]) -> Vec<TransferKey> {
    held.iter().map(|h| h.key).collect()
}

#[tokio::test]
async fn what_is_kept_is_listed_without_a_byte_of_it_read() {
    let few = Limits {
        per_peer: 3,
        ..Limits::default()
    };
    let side = side("kept-by", few);
    let records = side.base.join("records");
    kept_zeros(&side, PEER, key(160), "newer", 2, 5);
    kept_zeros(&side, PEER, key(161), "older", 1, 3);
    kept_zeros(&side, OTHER, key(162), "theirs", 1, 1);
    kept_zeros(&side, PEER, key(163), "replaced", 1, 1);
    kept_zeros(&side, PEER, key(164), "short", 1, 1);
    kept_zeros(&side, PEER, key(165), "live", 1, 9);
    kept_zeros(&side, PEER, key(166), "gone", 1, 1);
    kept_zeros(&side, PEER, key(167), "newest", 1, 20);
    // Another file under the name, as long as the one recorded.
    std::fs::rename(side.place.join("replaced.part"), side.place.join("moved")).unwrap();
    std::fs::write(side.place.join("replaced.part"), vec![0; C as usize]).unwrap();
    let short = side.root.reopen(OsStr::new("short.part")).unwrap();
    short.set_len(C - 1).unwrap();
    drop(short);
    std::fs::remove_file(side.place.join("gone.part")).unwrap();
    // Open as a save has it: on Windows, with no delete sharing.
    let live = side.root.reopen(OsStr::new("live.part")).unwrap();

    // Another process, which holds none of the records, and the one that
    // wrote them: the oldest three that are still there, in use or not.
    let other = Store::open(&records, few).unwrap();
    seams::PREFIX_BYTES.set(0);
    seams::OPENS.set(0);
    for store in [&other, &side.store] {
        let kept = store.kept_by(PEER).unwrap();
        assert_eq!(keys(&kept), [key(161), key(160), key(165)]);
        assert_eq!(kept[1].have, 2 * C);
        assert_eq!(kept[1].prefix, hash(&vec![0; (2 * C) as usize]));
        assert_eq!(keys(&store.kept_by(OTHER).unwrap()), [key(162)]);
    }
    assert_eq!(seams::PREFIX_BYTES.get(), 0, "no byte of a partial");
    assert_eq!(seams::OPENS.get(), 0, "no record file read again");
    // The ones passed by are removed by nothing here.
    for (n, name) in [(163, "replaced"), (164, "short"), (166, "gone")] {
        assert!(other.received(PEER, key(n)).is_some(), "{name}");
    }
    assert!(side.place.join("short.part").exists());
    assert_eq!(files_in(&records).len(), 8);
    // The live save is not disturbed by it.
    live.set_len(C).unwrap();
    drop(live);
}

/// The other side connects again while a file of its own is still coming,
/// or was just cut: a check that hashes what is kept leaves that file.
#[tokio::test]
async fn a_check_of_what_is_kept_leaves_a_partial_in_use_and_its_record() {
    let side = side("in-use", Limits::default());
    let records = side.base.join("records");
    let record = kept_zeros_in_use(&side);
    // Open as a save, or a `Kept` after a cut, has it.
    let file = side.root.reopen(OsStr::new("f.part")).unwrap();
    let still_there = |when: &str| {
        assert_eq!(
            side.store.received(PEER, key(170)),
            Some(record.clone()),
            "{when}"
        );
        assert!(side.place.join("f.part").exists(), "{when}");
    };
    // The store that holds the record passes it by.
    assert!(side.store.held_hashed(PEER).await.unwrap().is_empty());
    assert!(side.store.held_from(PEER).await.unwrap().is_empty());
    still_there("held by the store that checks");
    // And still holds it: the save's next chunk is recorded.
    let held = Held {
        key: key(170),
        have: C,
        prefix: record.prefix,
    };
    assert_eq!(side.store.keep(PEER, &held, 9, &file, true), Ok(true));
    let record = side.store.received(PEER, key(170)).unwrap();
    let still_there = |when: &str| {
        assert_eq!(
            side.store.received(PEER, key(170)),
            Some(record.clone()),
            "{when}"
        );
        assert!(side.place.join("f.part").exists(), "{when}");
    };
    // Another process checks it too, and leaves it.
    let other = Store::open(&records, Limits::default()).unwrap();
    let listed = other.held_hashed(PEER).await.unwrap();
    // Windows does not open a file a save has; Unix does, and it reads right.
    assert_eq!(listed.len(), usize::from(cfg!(unix)));
    still_there("checked by another store");
    // Let go by its store, with the file still open: a partial that
    // cannot be opened is not a partial that is gone.
    assert!(side.store.release(PEER, key(170)));
    for store in [&side.store, &other] {
        let listed = store.held_hashed(PEER).await.unwrap();
        assert_eq!(listed.len(), usize::from(cfg!(unix)));
        still_there("let go, still open");
    }
    #[cfg(windows)]
    {
        let busy = other.reopen_hashed(PEER, key(170)).await.err();
        assert_eq!(busy, Some(ResumeError::Busy));
        assert_eq!(other.reopen(PEER, key(170)).err(), Some(ResumeError::Busy));
        still_there("not reopened while open elsewhere");
    }
    // Closed, it is listed with its hasher, and continued.
    drop(file);
    let mut listed = side.store.held_hashed(PEER).await.unwrap();
    let (held, hasher) = listed.remove(0);
    assert_eq!((held.key, held.have), (key(170), C));
    assert_eq!(hasher.position(), C);
    assert_eq!(hasher.finish(), record.prefix);
    // A failed reopening left the record with no holder.
    let (_, kept) = other.reopen_hashed(PEER, key(170)).await.unwrap();
    assert_eq!(kept.have(), C);
}

/// One whole chunk of zeros kept under `key(170)` as `f.part`, its record
/// held by the side's store.
fn kept_zeros_in_use(side: &Side) -> Receiving {
    kept_in(&side.store, side, PEER, key(170), "f", 1, 5)
}

/// One key on two connections: the second must not cut the partial the
/// first is appending to.
#[tokio::test]
async fn a_partial_being_continued_is_not_reopened_a_second_time() {
    let side = side("reopen-twice", Limits::default());
    kept_zeros(&side, PEER, key(171), "f", 2, 5);
    // A new save's record, written and not let go, is in use already.
    assert_eq!(
        side.store.reopen(PEER, key(171)).err(),
        Some(ResumeError::Busy)
    );
    later(&side);
    let (_, kept) = side.store.reopen_hashed(PEER, key(171)).await.unwrap();
    // The save appends.
    kept.file.set_len(2 * C + 100).unwrap();
    assert_eq!(
        side.store.reopen(PEER, key(171)).err(),
        Some(ResumeError::Busy)
    );
    let again = side.store.reopen_hashed(PEER, key(171)).await;
    assert_eq!(again.err(), Some(ResumeError::Busy));
    assert_eq!(
        kept.file.metadata().unwrap().len(),
        2 * C + 100,
        "not cut under the save"
    );
    assert!(side.store.received(PEER, key(171)).is_some());
    // The save ended and its record is let go: reopened again, and cut.
    drop(kept);
    assert!(side.store.release(PEER, key(171)));
    let (_, file) = side.store.reopen(PEER, key(171)).unwrap();
    assert_eq!(file.metadata().unwrap().len(), 2 * C);
    drop(file);
    // In use, it is not forgotten under its save; let go, it is.
    assert_eq!(side.store.forget(PEER, key(171)), Err(ResumeError::Busy));
    assert!(side.store.received(PEER, key(171)).is_some());
    assert!(side.place.join("f.part").exists());
    assert!(side.store.release(PEER, key(171)));
    side.store.forget(PEER, key(171)).unwrap();
    assert!(!side.place.join("f.part").exists());
    assert_eq!(
        side.store.reopen(PEER, key(171)).err(),
        Some(ResumeError::NotHeld)
    );
}

#[tokio::test]
async fn a_reopening_that_fails_leaves_no_record_held() {
    let side = side("reopen-fails", Limits::default());
    let records = side.base.join("records");
    for hashed in [true, false] {
        kept_zeros(&side, PEER, key(172), "gone", 1, 5);
        kept_zeros(&side, PEER, key(173), "short", 2, 5);
        later(&side);
        std::fs::remove_file(side.place.join("gone.part")).unwrap();
        let short = side.root.reopen(OsStr::new("short.part")).unwrap();
        short.set_len(2 * C - 1).unwrap();
        drop(short);
        for (n, error) in [(172, ResumeError::Gone), (173, ResumeError::WrongSize)] {
            let failed = if hashed {
                side.store.reopen_hashed(PEER, key(n)).await.err()
            } else {
                side.store.reopen(PEER, key(n)).err()
            };
            assert_eq!(failed, Some(error), "{n}");
            // Removed as a check of what is kept removes it, not left held.
            assert!(side.store.received(PEER, key(n)).is_none(), "{n}");
        }
        assert!(files_in(&records).is_empty(), "{:?}", files_in(&records));
        assert!(!side.place.join("short.part").exists(), "from zero");
    }
}

/// One key on two sessions, in two processes: one record file, one writer.
#[tokio::test]
async fn two_stores_that_first_write_one_record_at_once_make_one_file() {
    use std::rc::Rc;
    let side = side("one-name", Limits::default());
    let records = side.base.join("records");
    let first = Rc::new(Store::open(&records, Limits::default()).unwrap());
    let partial = side.root.create(OsStr::new("f.part")).unwrap();
    partial.set_len(C).unwrap();
    let partial = Rc::new(partial);
    let record = Receiving {
        peer: PEER,
        key: key(180),
        same: Vec::new(),
        size: 2 * C,
        at: PartialAt::of(&side.root, &side.root, &partial, OsStr::new("f.part")).unwrap(),
        have: C,
        prefix: hash(&vec![0; C as usize]),
        created: 1,
        last: 1,
    };
    // The first store writes it in the moment after the second looked.
    let racing = (first.clone(), record.clone(), partial.clone());
    seams::LOOKED.set(Some(Box::new(move || {
        let (first, record, partial) = racing;
        first.receiving(&record, &partial).unwrap();
    })));
    assert_eq!(
        side.store.receiving(&record, &partial),
        Err(ResumeError::Busy)
    );
    let names = files_in(&records);
    assert_eq!(names.len(), 1, "{names:?}");
    assert_eq!(names[0].len(), 64 + ".resume".len(), "a hash, in hex");
    assert_eq!(keys(&side.store.kept_by(PEER).unwrap()), [key(180)]);
    // The first goes on with it; the second reads what it writes.
    let held = Held {
        key: key(180),
        have: C,
        prefix: record.prefix,
    };
    assert_eq!(first.keep(PEER, &held, 7, &partial, true), Ok(true));
    assert_eq!(side.store.received(PEER, key(180)).unwrap().last, 7);
    // Let go, it is the second's, under the same name.
    assert!(first.release(PEER, key(180)));
    assert_eq!(side.store.keep(PEER, &held, 8, &partial, true), Ok(true));
    assert_eq!(files_in(&records), names);
    // A sending record of the same peer and key is another file.
    let mut sending = source(&side, key(180), b"sent");
    assert_eq!(files_in(&records).len(), 2);
    // A record under a random name, as written before, is read, rewritten
    // in its place and removed as ever.
    assert!(side.store.release(PEER, key(180)));
    let old = "00112233445566778899aabbccddeeff.resume";
    let sent_as = files_in(&records)
        .into_iter()
        .find(|name| *name != names[0]);
    std::fs::rename(records.join(sent_as.unwrap()), records.join(old)).unwrap();
    let later = Store::open(&records, Limits::default()).unwrap();
    assert_eq!(later.sent(PEER, key(180)).unwrap().last, 1);
    sending.last = 2;
    later.sending(&sending).unwrap();
    assert_eq!(first.sent(PEER, key(180)).unwrap().last, 2);
    let mut expected = vec![old.to_string(), names[0].clone()];
    expected.sort();
    assert_eq!(files_in(&records), expected, "no second file for it");
    later.forget(PEER, key(180)).unwrap();
    assert!(files_in(&records).is_empty());
}

/// The reading of a long prefix is cancelled with its connection: the
/// file is not left in use by a reopening that never came about.
#[tokio::test]
async fn a_reopening_dropped_while_it_reads_leaves_the_record_not_held() {
    use std::task::Poll;
    let side = side("reopen-dropped", Limits::default());
    kept_zeros(&side, PEER, key(190), "f", 2, 5);
    later(&side);
    seams::STALL.set(true);
    let mut reopening = Box::pin(side.store.reopen_hashed(PEER, key(190)));
    let waits = std::future::poll_fn(|cx| Poll::Ready(reopening.as_mut().poll(cx).is_pending()));
    assert!(waits.await, "reading");
    // While it reads, the file is in use.
    assert_eq!(
        side.store.reopen(PEER, key(190)).err(),
        Some(ResumeError::Busy)
    );
    drop(reopening);
    seams::STALL.set(false);
    let (_, kept) = side.store.reopen_hashed(PEER, key(190)).await.unwrap();
    assert_eq!(kept.have(), 2 * C);
    assert!(side.place.join("f.part").exists());
}

/// Two first writes of one record through one store, the second begun
/// before the first and ended after it.
#[tokio::test]
async fn a_record_written_twice_at_once_by_one_store_keeps_the_later_write() {
    use std::rc::Rc;
    let side = side("written-twice", Limits::default());
    let records = side.base.join("records");
    let store = Rc::new(Store::open(&records, Limits::default()).unwrap());
    let partial = side.root.create(OsStr::new("f.part")).unwrap();
    partial.set_len(2 * C).unwrap();
    let partial = Rc::new(partial);
    let mut record = Receiving {
        peer: PEER,
        key: key(191),
        same: Vec::new(),
        size: 3 * C,
        at: PartialAt::of(&side.root, &side.root, &partial, OsStr::new("f.part")).unwrap(),
        have: C,
        prefix: hash(&vec![0; C as usize]),
        created: 1,
        last: 1,
    };
    // In the moment after this write looked, the record is written twice.
    let racing = (store.clone(), record.clone(), partial.clone());
    seams::LOOKED.set(Some(Box::new(move || {
        let (store, record, partial) = racing;
        store.receiving(&record, &partial).unwrap();
        let held = Held {
            key: key(191),
            have: C,
            prefix: record.prefix,
        };
        assert_eq!(store.keep(PEER, &held, 2, &partial, true), Ok(true));
    })));
    record.last = 99;
    store.receiving(&record, &partial).unwrap();
    assert_eq!(files_in(&records).len(), 1);
    assert_eq!(store.received(PEER, key(191)).unwrap().last, 99);
    // Another store reads the same from the file.
    let other = Store::open(&records, Limits::default()).unwrap();
    assert_eq!(other.received(PEER, key(191)).unwrap().last, 99);
}

/// [`Keeping`], counting the bytes recorded at each whole chunk.
struct Telling<'a>(Keeping<'a>, std::sync::atomic::AtomicU64);

impl Telling<'_> {
    /// Returns once the chunk ending at `have` is recorded. The save runs
    /// on the same task, so yielding is what lets it get there.
    async fn recorded(&self, have: u64) {
        while self.1.load(std::sync::atomic::Ordering::SeqCst) < have {
            tokio::task::yield_now().await;
        }
    }
}

impl safecopy_chunks::Hooks for Telling<'_> {
    fn created(&self, file: &File, partial: &OsStr) -> std::io::Result<()> {
        self.0.created(file, partial)
    }

    fn whole(&self, file: &File, have: u64, prefix: Digest) -> std::io::Result<()> {
        self.0.whole(file, have, prefix)?;
        self.1.store(have, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// The save itself, not a handle standing in for it: its next chunk is
/// recorded after the check, and at the cut its partial is kept.
#[tokio::test]
async fn a_peer_that_connects_again_while_its_file_comes_does_not_stop_the_save() {
    use safecopy_chunks::{ChunkError, SaveOptions, Start, Target, save_resumable};
    use tokio::io::AsyncWriteExt;
    let side = side("checked-live", Limits::default());
    let bytes = data(3 * C);
    let c = C as usize;
    let journal = journal(&side);
    let keeping = Keeping {
        side: &side,
        journal: &journal,
        key: key(192),
        name: "f.bin",
        size: 3 * C,
        partial: Default::default(),
    };
    let hooks = Telling(keeping, Default::default());
    let (mut sending, mut receiving) = tokio::io::duplex(1 << 16);
    let target = Target {
        dir: &side.root,
        name: "f.bin",
        size: 3 * C,
    };
    let options = SaveOptions::default();
    let pending = std::future::pending();
    let saving = save_resumable(
        &mut receiving,
        target,
        Start::New,
        &cut_short,
        &options,
        &hooks,
        pending,
    );
    let store = &side.store;
    let record_stays = || {
        assert!(store.received(PEER, key(192)).is_some());
        assert!(side.place.join("f.bin.part").exists());
    };
    let peer = async {
        sending.write_all(&bytes[..c + 10]).await.unwrap();
        hooks.recorded(C).await;
        // The same peer, on another connection, while the file still comes.
        assert!(store.held_hashed(PEER).await.unwrap().is_empty());
        assert!(store.held_from(PEER).await.unwrap().is_empty());
        assert_eq!(keys(&store.kept_by(PEER).unwrap()), [key(192)]);
        let again = store.reopen_hashed(PEER, key(192)).await;
        assert_eq!(again.err(), Some(ResumeError::Busy));
        // Whatever a check answered for that connection's stream, the
        // file of the save under way is not forgotten.
        assert_eq!(store.forget(PEER, key(192)), Err(ResumeError::Busy));
        assert_eq!(store.saving(PEER, key(192)).err(), Some(ResumeError::Busy));
        record_stays();
        // The save goes on, and its next chunk is recorded.
        sending.write_all(&bytes[c + 10..2 * c + 10]).await.unwrap();
        hooks.recorded(2 * C).await;
        // Then the stream is cut.
        drop(sending);
    };
    let (stopped, ()) = tokio::join!(saving, peer);
    let stopped = stopped.unwrap_err();
    assert!(
        matches!(stopped.error, ChunkError::CutShort { .. }),
        "{:?}",
        stopped.error
    );
    let kept = stopped.kept.expect("kept at the cut");
    assert_eq!(kept.have(), 2 * C);
    // Just cut, the `Kept` still in hand: the peer connects once more.
    assert!(store.held_hashed(PEER).await.unwrap().is_empty());
    record_stays();
    let held = Held {
        key: key(192),
        have: kept.have(),
        prefix: kept.prefix(),
    };
    assert_eq!(store.keep(PEER, &held, 20, &kept.file, true), Ok(true));
    // The save is over: let go, the file is listed by the check and continued.
    drop(kept);
    assert!(store.release(PEER, key(192)));
    assert_eq!(
        keys(&store.held_from(PEER).await.unwrap()),
        [key(192)],
        "listed once it is let go"
    );
    let (_, kept) = store.reopen_hashed(PEER, key(192)).await.unwrap();
    assert_eq!(kept.have(), 2 * C);
    assert_eq!(kept.prefix(), hash(&bytes[..2 * c]));
}

/// Makes `link` a junction to the folder `target`, with no other program
/// run: a junction needs no privilege.
#[cfg(windows)]
fn junction(target: &std::path::Path, link: &std::path::Path) {
    use ::windows::Win32::Foundation::HANDLE;
    use ::windows::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    use ::windows::Win32::System::IO::DeviceIoControl;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    const FSCTL_SET_REPARSE_POINT: u32 = 0x0009_00A4;
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
    std::fs::create_dir(link).unwrap();
    let folder = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(link)
        .unwrap();
    // The target as the system names it, then an empty name to show.
    let name: Vec<u16> = std::ffi::OsStr::new(r"\??\")
        .encode_wide()
        .chain(target.as_os_str().encode_wide())
        .collect();
    let len = (name.len() * 2) as u16;
    let mut buffer = Vec::new();
    buffer.extend(IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    for field in [len + 12, 0, 0, len, len + 2, 0] {
        buffer.extend(field.to_le_bytes());
    }
    buffer.extend(name.iter().flat_map(|unit| unit.to_le_bytes()));
    buffer.extend([0; 4]);
    // SAFETY: the handle is open, and the buffer is whole for the length given.
    unsafe {
        DeviceIoControl(
            HANDLE(folder.as_raw_handle()),
            FSCTL_SET_REPARSE_POINT,
            Some(buffer.as_ptr().cast()),
            buffer.len() as u32,
            None,
            0,
            None,
            None,
        )
    }
    .unwrap();
}

/// A record of one whole chunk of zeros for `key`, and its partial, not
/// yet written to any store.
fn unwritten(side: &Side, key: TransferKey, name: &str) -> (Receiving, File) {
    let partial = format!("{name}.part");
    let file = side.root.create(OsStr::new(&partial)).unwrap();
    file.set_len(C).unwrap();
    let record = Receiving {
        peer: PEER,
        key,
        same: Vec::new(),
        size: C,
        at: PartialAt::of(&side.root, &side.root, &file, OsStr::new(&partial)).unwrap(),
        have: C,
        prefix: hash(&vec![0; C as usize]),
        created: 5,
        last: 5,
    };
    (record, file)
}

/// A record file's name can be worked out from its peer and key: a file
/// put under it by another program is not written, whatever it is.
#[tokio::test]
async fn a_file_under_a_records_name_that_is_not_its_record_is_not_written() {
    let side = side("not-its-record", Limits::default());
    let records = side.base.join("records");
    let store = &side.store;
    let hour = Duration::from_secs(3600);

    // Bytes that are no record.
    let (record, partial) = unwritten(&side, key(200), "plain");
    let name = records.join(crate::store::file_name(true, PEER, key(200)));
    let theirs = b"someone's notes, longer than nothing".to_vec();
    std::fs::write(&name, &theirs).unwrap();
    for _ in 0..2 {
        assert_eq!(store.receiving(&record, &partial), Err(ResumeError::Busy));
        assert!(std::fs::read(&name).unwrap() == theirs, "not a byte of it");
    }
    assert!(store.received(PEER, key(200)).is_none());
    // The key is free again once an expiry has removed the file.
    assert_eq!(store.expire(10, hour, &[]).unwrap().removed, 1);
    assert!(!name.exists());
    store.receiving(&record, &partial).unwrap();
    assert_eq!(store.received(PEER, key(200)).unwrap().have, C);

    // Another file of the person's, given a second name there.
    let (record, partial) = unwritten(&side, key(201), "linked");
    let name = records.join(crate::store::file_name(true, PEER, key(201)));
    let other = side.place.join("thesis.docx");
    std::fs::write(&other, &theirs).unwrap();
    std::fs::hard_link(&other, &name).unwrap();
    assert_eq!(store.receiving(&record, &partial), Err(ResumeError::Busy));
    assert!(std::fs::read(&other).unwrap() == theirs, "not a byte of it");
    assert_eq!(store.expire(10, hour, &[]).unwrap().removed, 1);
    assert!(!name.exists(), "the name alone goes");
    assert!(std::fs::read(&other).unwrap() == theirs);
    store.receiving(&record, &partial).unwrap();
    assert_eq!(store.received(PEER, key(201)).unwrap().have, C);

    // A file that reads as the very record, under a second name: only
    // its count of names says it is not the store's to write.
    let (mut record, partial) = unwritten(&side, key(202), "copied");
    let name = records.join(crate::store::file_name(true, PEER, key(202)));
    store.receiving(&record, &partial).unwrap();
    assert!(store.release(PEER, key(202)));
    let copy = side.place.join("copy.bin");
    std::fs::copy(&name, &copy).unwrap();
    std::fs::remove_file(&name).unwrap();
    std::fs::hard_link(&copy, &name).unwrap();
    let was = std::fs::read(&copy).unwrap();
    record.last = 9;
    assert_eq!(store.receiving(&record, &partial), Err(ResumeError::Busy));
    let held = Held {
        key: key(202),
        have: C,
        prefix: record.prefix,
    };
    assert_eq!(
        store.keep(PEER, &held, 9, &partial, true),
        Err(ResumeError::Busy)
    );
    assert_eq!(store.reopen(PEER, key(202)).err(), Some(ResumeError::Busy));
    assert!(std::fs::read(&copy).unwrap() == was, "not a byte of it");

    // An empty file with the one name is a record not yet written.
    let (record, partial) = unwritten(&side, key(203), "empty");
    let name = records.join(crate::store::file_name(true, PEER, key(203)));
    std::fs::write(&name, b"").unwrap();
    store.receiving(&record, &partial).unwrap();
    assert_eq!(store.received(PEER, key(203)).unwrap().have, C);
}

/// A record whose first write was torn by a crash: its file reads as
/// nothing, and is written by no later run until an expiry removed it.
#[tokio::test]
async fn a_first_write_torn_by_a_crash_waits_for_the_next_expiry() {
    let mut side = side("torn-first", Limits::default());
    let records = side.base.join("records");
    let (record, partial) = unwritten(&side, key(204), "f");
    seams::TEAR.set(true);
    side.store.receiving(&record, &partial).unwrap();
    assert!(side.store.received(PEER, key(204)).is_none());
    // The process is gone; the next one finds the file.
    side.store = Store::open(&records, Limits::default()).unwrap();
    let torn = std::fs::read(records.join(&files_in(&records)[0])).unwrap();
    assert_eq!(
        side.store.receiving(&record, &partial),
        Err(ResumeError::Busy)
    );
    assert!(std::fs::read(records.join(&files_in(&records)[0])).unwrap() == torn);
    let expired = side.store.expire(10, Duration::from_secs(3600), &[]);
    assert_eq!(expired.unwrap().removed, 1);
    side.store.receiving(&record, &partial).unwrap();
    assert_eq!(side.store.received(PEER, key(204)).unwrap().have, C);
}

/// A connection lost takes its task with it, and the save's future is
/// dropped where it stood: the file is not left in use for good.
#[tokio::test]
async fn a_save_dropped_or_panicked_leaves_its_file_to_be_continued() {
    use safecopy_chunks::{SaveOptions, Start, Target, save_resumable};
    use tokio::io::AsyncWriteExt;
    let side = side("save-dropped", Limits::default());
    let bytes = data(3 * C);
    let c = C as usize;
    let journal = journal(&side);
    let keeping = Keeping {
        side: &side,
        journal: &journal,
        key: key(193),
        name: "f.bin",
        size: 3 * C,
        partial: Default::default(),
    };
    let hooks = Telling(keeping, Default::default());
    let (mut sending, mut receiving) = tokio::io::duplex(1 << 16);
    let store = &side.store;
    let busy = Some(ResumeError::Busy);
    let guard = store.saving(PEER, key(193)).unwrap();
    // The same key on a second connection, before any record of it.
    assert_eq!(store.saving(PEER, key(193)).err(), busy);
    assert_eq!(store.forget(PEER, key(193)).err(), busy);
    assert!(store.saving(OTHER, key(193)).is_ok(), "another peer's file");
    {
        let target = Target {
            dir: &side.root,
            name: "f.bin",
            size: 3 * C,
        };
        let options = SaveOptions::default();
        let pending = std::future::pending();
        let (start, keep) = (Start::New, &cut_short);
        let save = save_resumable(
            &mut receiving,
            target,
            start,
            keep,
            &options,
            &hooks,
            pending,
        );
        tokio::pin!(save);
        // One whole chunk and no byte more, so no write is under way.
        let peer = async {
            sending.write_all(&bytes[..c]).await.unwrap();
            hooks.recorded(C).await;
        };
        tokio::select! {
            biased;
            _ = &mut save => panic!("its stream is still open"),
            () = peer => {}
        }
        assert_eq!(store.forget(PEER, key(193)).err(), busy);
        assert_eq!(store.reopen(PEER, key(193)).err(), busy);
        // The save's future is dropped here, with its partial open.
    }
    assert_eq!(store.reopen(PEER, key(193)).err(), busy, "still held");
    drop(guard);
    assert!(!store.release(PEER, key(193)), "let go by the guard");
    assert!(side.place.join("f.bin.part").exists());
    assert_eq!(keys(&store.kept_by(PEER).unwrap()), [key(193)]);

    // A panic while the next save has it lets it go as well.
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _guard = store.saving(PEER, key(193)).unwrap();
        drop(store.reopen(PEER, key(193)).unwrap());
        std::panic::resume_unwind(Box::new("a save that panics"));
    }));
    assert!(panicked.is_err());

    // Continued later, from what the record last said.
    let guard = store.saving(PEER, key(193)).unwrap();
    let (_, kept) = store.reopen_hashed(PEER, key(193)).await.unwrap();
    assert_eq!(kept.have(), C);
    assert_eq!(kept.prefix(), hash(&bytes[..c]));
    // A save that ends with nothing kept: the record and the partial go.
    drop(kept);
    guard.forget().unwrap();
    assert!(store.received(PEER, key(193)).is_none());
    assert!(!side.place.join("f.bin.part").exists());
    assert!(files_in(&side.base.join("records")).is_empty());
    // And the key is free for a new save.
    assert!(store.saving(PEER, key(193)).is_ok());
}

/// A sending record has no save to cut: it is forgotten while held.
#[tokio::test]
async fn a_sending_record_is_forgotten_while_its_store_holds_it() {
    let side = side("forget-sending", Limits::default());
    source(&side, key(194), b"sent");
    assert!(side.store.sent(PEER, key(194)).is_some());
    side.store.forget(PEER, key(194)).unwrap();
    assert!(side.store.sent(PEER, key(194)).is_none());
    assert!(files_in(&side.base.join("records")).is_empty());
}

/// What is under a kept partial's name is listed only when it is the
/// recorded file itself, never through a link to it.
#[tokio::test]
async fn a_link_under_a_kept_partials_name_is_not_listed() {
    let side = side("kept-link", Limits::default());
    kept_zeros(&side, PEER, key(205), "f", 1, 5);
    later(&side);
    let (partial, moved) = (side.place.join("f.part"), side.place.join("moved"));
    assert_eq!(keys(&side.store.kept_by(PEER).unwrap()), [key(205)]);
    std::fs::rename(&partial, &moved).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&moved, &partial).unwrap();
        assert!(side.store.kept_by(PEER).unwrap().is_empty(), "a link");
        std::fs::remove_file(&partial).unwrap();
    }
    #[cfg(windows)]
    {
        junction(&side.place, &partial);
        assert!(partial.join("moved").exists(), "it leads there");
        assert!(side.store.kept_by(PEER).unwrap().is_empty(), "a junction");
        std::fs::remove_dir(&partial).unwrap();
        // A file symbolic link needs Developer Mode or an administrator.
        if std::os::windows::fs::symlink_file(&moved, &partial).is_ok() {
            assert!(side.store.kept_by(PEER).unwrap().is_empty(), "a link");
            std::fs::remove_file(&partial).unwrap();
        } else {
            let required = std::env::var_os("SAFECOPY_REQUIRE_SYMLINKS");
            assert!(
                required.is_none_or(|v| v.is_empty()),
                "SAFECOPY_REQUIRE_SYMLINKS is set, but no file symbolic link can be made"
            );
            eprintln!("skipped: no symbolic link can be made here");
        }
    }
    // Under its name again, it is listed, and its record was not removed.
    std::fs::rename(&moved, &partial).unwrap();
    assert_eq!(keys(&side.store.kept_by(PEER).unwrap()), [key(205)]);
}

/// A program spawns these on its runtime: none holds the store's lock,
/// nor anything else tied to its thread, across a wait.
#[tokio::test]
async fn the_stores_futures_can_run_on_another_thread() {
    fn sendable<T: Send>(_: &T) {}
    let side = side("send", Limits::default());
    sendable(&side.store.reopen_hashed(PEER, key(206)));
    sendable(&side.store.held_hashed(PEER));
    sendable(&side.store.held_from(PEER));
    sendable(&side.store.saving(PEER, key(206)).unwrap());
}

/// A record's file replaced under its name between this store's last
/// reading of it and its next write: what is there now is not written.
#[tokio::test]
async fn a_record_file_that_no_longer_reads_as_its_record_is_not_written() {
    let side = side("replaced-record", Limits::default());
    let records = side.base.join("records");
    let (mut record, partial) = unwritten(&side, key(207), "f");
    let name = records.join(crate::store::file_name(true, PEER, key(207)));
    side.store.receiving(&record, &partial).unwrap();
    assert!(side.store.release(PEER, key(207)));
    let theirs = b"someone's notes".to_vec();
    let (path, bytes) = (name.clone(), theirs.clone());
    seams::LOOKED.set(Some(Box::new(move || {
        std::fs::write(&path, &bytes).unwrap();
    })));
    record.last += 1;
    assert_eq!(
        side.store.receiving(&record, &partial),
        Err(ResumeError::Busy)
    );
    assert!(std::fs::read(&name).unwrap() == theirs, "not a byte of it");
}

/// A guard that ends lets go of its own file's record, and no other's.
#[tokio::test]
async fn a_guard_dropped_lets_go_of_its_own_record_only() {
    let side = side("guard-own", Limits::default());
    let store = &side.store;
    let (one, other) = (
        store.saving(PEER, key(208)).unwrap(),
        store.saving(PEER, key(209)).unwrap(),
    );
    kept_zeros(&side, PEER, key(208), "one", 1, 5);
    kept_zeros(&side, PEER, key(209), "other", 1, 5);
    source(&side, key(208), b"sent under the same key");
    drop(one);
    assert_eq!(store.reopen(PEER, key(209)).err(), Some(ResumeError::Busy));
    assert!(store.reopen(PEER, key(208)).is_ok(), "let go");
    // The sending record of the same key is still this store's to write.
    let later = Store::open(&side.base.join("records"), Limits::default()).unwrap();
    let mut sending = later.sent(PEER, key(208)).unwrap();
    sending.last = 2;
    assert_eq!(later.sending(&sending), Err(ResumeError::Busy));
    drop(other);
    assert!(store.reopen(PEER, key(209)).is_ok());
}

/// A symbolic link under a record's name leads to some other file: it is
/// not written through, and an expiry does not remove through it.
#[tokio::test]
async fn a_link_under_a_records_name_is_not_written_through() {
    let side = side("record-link", Limits::default());
    let records = side.base.join("records");
    let (record, partial) = unwritten(&side, key(210), "f");
    let name = records.join(crate::store::file_name(true, PEER, key(210)));
    let other = side.place.join("thesis.docx");
    std::fs::write(&other, b"theirs").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&other, &name).unwrap();
    // A file symbolic link needs Developer Mode or an administrator.
    #[cfg(windows)]
    if std::os::windows::fs::symlink_file(&other, &name).is_err() {
        let required = std::env::var_os("SAFECOPY_REQUIRE_SYMLINKS");
        assert!(
            required.is_none_or(|v| v.is_empty()),
            "SAFECOPY_REQUIRE_SYMLINKS is set, but no file symbolic link can be made"
        );
        eprintln!("skipped: no symbolic link can be made here");
        return;
    }
    assert_eq!(
        side.store.receiving(&record, &partial),
        Err(ResumeError::Busy)
    );
    let expired = side.store.expire(10, Duration::from_secs(3600), &[]);
    assert_eq!(expired.unwrap().removed, 0);
    assert!(std::fs::symlink_metadata(&name).is_ok(), "left as it is");
    assert_eq!(std::fs::read(&other).unwrap(), b"theirs");
}

/// An ordinary file of the person's in the root, opened as a save would
/// hold a partial.
fn ordinary(side: &Side, name: &str, bytes: &[u8]) -> File {
    let path = side.place.join(name);
    std::fs::write(&path, bytes).unwrap();
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap()
}

/// A record file someone else put into the store's folder: it reads as
/// `PEER`'s record of `key`, as a store writes one first, and says the
/// file `file`, recorded under `name`, is a partial holding one whole
/// chunk of `bytes`. Its path.
fn forged(side: &Side, key: TransferKey, file: &File, name: &str, bytes: &[u8]) -> PathBuf {
    planted(
        side,
        Receiving {
            peer: PEER,
            key,
            same: Vec::new(),
            size: bytes.len() as u64,
            at: PartialAt::of(&side.root, &side.root, file, OsStr::new(name)).unwrap(),
            have: C,
            prefix: hash(&bytes[..C as usize]),
            created: 5,
            last: 5,
        },
    )
}

/// The record file of `record` as a store writes one first, put into the
/// store's folder by someone else. Its path.
fn planted(side: &Side, record: Receiving) -> PathBuf {
    let key = record.key;
    let payload = crate::record::Record::Receiving(record).encode();
    let mut written = crate::record::slot(1, &payload).unwrap();
    written.resize(2 * crate::record::SLOT + 1, 0);
    let path = side
        .base
        .join("records")
        .join(crate::store::file_name(true, PEER, key));
    std::fs::write(&path, written).unwrap();
    path
}

/// A record names its partial by folder, name and file ID, and a record
/// file can be put into the store's folder by anyone: one naming another
/// file of the person's makes the store open, cut and remove nothing.
#[tokio::test]
async fn a_forged_record_naming_another_file_cuts_and_opens_nothing() {
    let side = side("forged", Limits::default());
    let store = &side.store;
    let bytes = data(2 * C);
    let thesis = side.place.join("thesis.docx");
    let file = ordinary(&side, "thesis.docx", &bytes);
    let forge = || forged(&side, key(211), &file, "thesis.docx", &bytes);
    let untouched = || {
        assert!(
            std::fs::read(&thesis).unwrap() == bytes,
            "not a byte of it, nor its length"
        );
    };
    let gone = Some(ResumeError::Gone);

    let record = forge();
    assert_eq!(
        store.received(PEER, key(211)).unwrap().have,
        C,
        "it reads as the peer's record"
    );
    assert!(store.kept_by(PEER).unwrap().is_empty(), "not listed");
    assert!(record.exists());
    assert_eq!(store.reopen(PEER, key(211)).err(), gone);
    untouched();
    assert!(!record.exists(), "the record goes, and nothing else");

    // The prefix it claims is the file's own: the hash would match.
    forge();
    assert_eq!(store.reopen_hashed(PEER, key(211)).await.err(), gone);
    untouched();
    assert!(!record.exists());

    forge();
    assert!(store.held_hashed(PEER).await.unwrap().is_empty());
    untouched();
    forge();
    assert!(store.held_from(PEER).await.unwrap().is_empty());
    untouched();

    // Listed by nothing, it goes at the next expiry, whatever its age:
    // its name alone, and the file it names stays.
    forge();
    assert!(store.kept_by(PEER).unwrap().is_empty());
    assert!(record.exists());
    let hour = Duration::from_secs(3600);
    assert_eq!(store.expire(6, hour, &[]).unwrap().removed, 1);
    assert!(!record.exists());
    untouched();

    // So do a forget and a removal of everything.
    forge();
    store.forget(PEER, key(211)).unwrap();
    assert!(!record.exists());
    untouched();
    forge();
    assert_eq!(store.remove_all(Some(PEER)).unwrap(), 1);
    assert!(!record.exists());
    untouched();
}

/// A name that would open a stream of a file, one with a separator, and
/// the ending alone are no partial's either: refused before any open.
#[tokio::test]
async fn a_forged_record_naming_a_stream_a_path_or_the_ending_alone_opens_nothing() {
    let side = side("forged-names", Limits::default());
    let store = &side.store;
    let bytes = data(2 * C);
    std::fs::create_dir(side.place.join("sub")).unwrap();
    let below = std::path::Path::new("sub").join("f.part");
    let files = [
        ("thesis.docx", "thesis.docx:x.part"),
        (below.to_str().unwrap(), "sub/f.part"),
        (below.to_str().unwrap(), r"sub\f.part"),
        (".part", ".part"),
    ];
    for (at, name) in files {
        let file = ordinary(&side, at, &bytes);
        let record = forged(&side, key(212), &file, name, &bytes);
        assert!(store.received(PEER, key(212)).is_some(), "{name}");
        assert!(store.kept_by(PEER).unwrap().is_empty(), "{name}");
        // `Gone`, and not what the folder's own refusal of the name gives.
        assert_eq!(
            store.reopen(PEER, key(212)).err(),
            Some(ResumeError::Gone),
            "{name}"
        );
        assert!(!record.exists(), "{name}");
        forged(&side, key(212), &file, name, &bytes);
        assert_eq!(
            store.reopen_hashed(PEER, key(212)).await.err(),
            Some(ResumeError::Gone),
            "{name}"
        );
        assert!(
            std::fs::read(side.place.join(at)).unwrap() == bytes,
            "{name}"
        );
    }
}

/// A record is written only for a file named as a partial: no caller's
/// place, and no record found in the folder, makes the store keep one
/// that names another file.
#[tokio::test]
async fn a_record_of_a_file_not_named_as_a_partial_is_not_written() {
    let side = side("not-partial", Limits::default());
    let store = &side.store;
    let records = side.base.join("records");
    let bytes = data(2 * C);
    let file = ordinary(&side, "thesis.docx", &bytes);
    let refused = Err(ResumeError::NotPartial);
    let mut record = Receiving {
        peer: PEER,
        key: key(213),
        same: Vec::new(),
        size: 2 * C,
        at: PartialAt::of(&side.root, &side.root, &file, OsStr::new("thesis.docx")).unwrap(),
        have: C,
        prefix: hash(&bytes[..C as usize]),
        created: 5,
        last: 5,
    };
    let names = [
        "thesis.docx",
        "thesis.docx:x.part",
        "sub/f.part",
        r"sub\f.part",
        ".part",
    ];
    for name in names {
        record.at.partial = name.to_string();
        assert_eq!(store.receiving(&record, &file), refused, "{name}");
        assert!(files_in(&records).is_empty(), "{name}: no record");
    }
    assert!(store.received(PEER, key(213)).is_none());

    // One found there is not rewritten for the save that asks.
    let path = forged(&side, key(213), &file, "thesis.docx", &bytes);
    let was = std::fs::read(&path).unwrap();
    let held = Held {
        key: key(213),
        have: 2 * C,
        prefix: hash(&bytes),
    };
    assert_eq!(
        store.keep(PEER, &held, 9, &file, true),
        Err(ResumeError::NotPartial)
    );
    let guard = store.saving(PEER, key(213)).unwrap();
    assert_eq!(guard.kept(&held, 9, &file), Err(ResumeError::NotPartial));
    assert!(std::fs::read(&path).unwrap() == was, "not a byte of it");
    assert_eq!(store.received(PEER, key(213)).unwrap().have, C);
}

/// The ending of a partial's name is the journal settings': a store
/// given another one keeps files of that ending, and no `.part`.
#[tokio::test]
async fn a_store_keeps_partials_of_its_own_ending_only() {
    let side = side("other-ending", Limits::default());
    let settings = safecopy_journal::Settings {
        partial_ending: ".tmp".to_string(),
        ..Default::default()
    };
    let store = Store::open(&side.base.join("other"), Limits::default())
        .unwrap()
        .with_journal_settings(settings);
    let bytes = data(2 * C);
    let record = |key: TransferKey, name: &str| {
        let file = ordinary(&side, name, &bytes);
        let record = Receiving {
            peer: PEER,
            key,
            same: Vec::new(),
            size: 2 * C,
            at: PartialAt::of(&side.root, &side.root, &file, OsStr::new(name)).unwrap(),
            have: C,
            prefix: hash(&bytes[..C as usize]),
            created: 5,
            last: 5,
        };
        (record, file)
    };
    let (theirs, file) = record(key(214), "f.bin.part");
    assert_eq!(
        store.receiving(&theirs, &file),
        Err(ResumeError::NotPartial)
    );
    drop(file);
    assert!(files_in(&side.base.join("other")).is_empty());
    let (own, file) = record(key(215), "f.bin.tmp");
    store.receiving(&own, &file).unwrap();
    drop(file);
    assert_eq!(keys(&store.kept_by(PEER).unwrap()), [key(215)]);
    assert!(store.release(PEER, key(215)));
    let (_, kept) = store.reopen_hashed(PEER, key(215)).await.unwrap();
    assert_eq!(kept.have(), C);
    drop(kept);
    assert_eq!(
        std::fs::metadata(side.place.join("f.bin.tmp"))
            .unwrap()
            .len(),
        C,
        "its own partial, cut to what it has"
    );
    // The default store takes `.part`, and not the other's ending.
    side.store
        .receiving(&theirs, &ordinary(&side, "g.bin", b""))
        .unwrap();
    assert_eq!(
        side.store.receiving(&own, &ordinary(&side, "h.bin", b"")),
        Err(ResumeError::NotPartial)
    );
}

/// More bytes than fit a number, in whole chunks.
const HUGE: u64 = u64::MAX / C * C;

/// A record put into the store's folder that names a file which is no
/// partial, and claims to keep more than any disk holds: it has no place
/// among the kept, so no kept file is removed to make room for it, and
/// it goes at the next expiry, in `live` or not.
#[tokio::test]
async fn a_planted_record_of_no_partial_pushes_nothing_out_and_goes_at_an_expiry() {
    let side = side("planted-huge", Limits::default());
    let store = &side.store;
    kept_zeros(&side, PEER, key(220), "real", 1, 5);
    later(&side);
    let bytes = data(C);
    let file = ordinary(&side, "thesis.docx", &bytes);
    let record = planted(
        &side,
        Receiving {
            peer: PEER,
            key: key(221),
            same: Vec::new(),
            size: HUGE,
            at: PartialAt::of(&side.root, &side.root, &file, OsStr::new("thesis.docx")).unwrap(),
            have: HUGE,
            prefix: hash(b""),
            created: 6,
            // Newer than the real one: the oldest goes first.
            last: 6,
        },
    );
    assert_eq!(store.received(PEER, key(221)).unwrap().have, HUGE);
    assert_eq!(store.kept_bytes(), C, "its bytes are not counted");
    seams::FREE.set(Some(100 * C));
    let hour = Duration::from_secs(3600);
    let expired = store.expire(7, hour, &[(PEER, key(221))]);
    seams::FREE.set(None);
    assert_eq!(
        expired,
        Ok(Expired {
            removed: 1,
            kept_bytes: C
        })
    );
    assert!(!record.exists(), "the planted record went");
    assert!(std::fs::read(side.place.join("thesis.docx")).unwrap() == bytes);
    assert_eq!(keys(&store.kept_by(PEER).unwrap()), [key(220)]);
    assert!(side.place.join("real.part").exists());
    // An expiry counts the bytes it may remove: a live file's are not in
    // its sum, and are in the store's.
    let live = store.expire(7, hour, &[(PEER, key(220))]).unwrap();
    assert_eq!((live.removed, live.kept_bytes), (0, 0));
    assert_eq!(store.kept_bytes(), C);
}

/// No store writes a record that keeps more bytes than its file has: one
/// that says so does not read as a record, and goes as any file that
/// reads as nothing.
#[tokio::test]
async fn a_record_keeping_more_than_its_size_is_not_read_as_a_record() {
    let side = side("past-size", Limits::default());
    let file = side.root.create(OsStr::new("f.part")).unwrap();
    let record = Receiving {
        peer: PEER,
        key: key(222),
        same: Vec::new(),
        size: C,
        at: PartialAt::of(&side.root, &side.root, &file, OsStr::new("f.part")).unwrap(),
        have: 2 * C,
        prefix: hash(b""),
        created: 5,
        last: 5,
    };
    let payload = crate::record::Record::Receiving(record.clone()).encode();
    assert_eq!(
        crate::record::Record::decode(&payload),
        Err(ResumeError::Damaged)
    );
    let whole = Receiving {
        have: C,
        ..record.clone()
    };
    let payload = crate::record::Record::Receiving(whole.clone()).encode();
    assert_eq!(
        crate::record::Record::decode(&payload),
        Ok(crate::record::Record::Receiving(whole)),
        "all of it kept is a record"
    );
    let path = planted(&side, record);
    assert!(side.store.received(PEER, key(222)).is_none());
    assert_eq!(side.store.kept_bytes(), 0);
    let hour = Duration::from_secs(3600);
    assert_eq!(side.store.expire(6, hour, &[]).unwrap().removed, 1);
    assert!(!path.exists());
    assert!(side.place.join("f.part").exists(), "by its name alone");
}

/// Records that read, and say more is kept than a number holds: the sums
/// stop at the largest number, and nothing wraps or panics.
#[tokio::test]
async fn kept_bytes_past_what_a_number_holds_do_not_overflow() {
    // A share nothing passes: only the sums are looked at.
    let limits = Limits {
        disk_share: (1, 1),
        ..Limits::default()
    };
    let side = side("kept-overflow", limits);
    for (n, name) in [(223, "a.part"), (224, "b.part")] {
        let file = side.root.create(OsStr::new(name)).unwrap();
        planted(
            &side,
            Receiving {
                peer: PEER,
                key: key(n),
                same: Vec::new(),
                size: HUGE,
                at: PartialAt::of(&side.root, &side.root, &file, OsStr::new(name)).unwrap(),
                have: HUGE,
                prefix: hash(b""),
                created: 5,
                last: 5,
            },
        );
    }
    assert_eq!(side.store.kept_bytes(), u64::MAX);
    seams::FREE.set(Some(u64::MAX));
    let expired = side.store.expire(6, Duration::from_secs(3600), &[]);
    seams::FREE.set(None);
    assert_eq!(
        expired,
        Ok(Expired {
            removed: 0,
            kept_bytes: u64::MAX
        })
    );
    // A share they are past: counted, they go, and the count comes down.
    let records = side.base.join("records");
    let strict = Store::open(&records, Limits::default()).unwrap();
    seams::FREE.set(Some(u64::MAX));
    let expired = strict.expire(6, Duration::from_secs(3600), &[]);
    seams::FREE.set(None);
    assert_eq!(
        expired,
        Ok(Expired {
            removed: 2,
            kept_bytes: 0
        })
    );
}

/// A `forget` claims the file as a save does while it removes, and ends
/// that claim without letting go of a record it never took: one a
/// reopening took meanwhile is still that reopening's.
#[tokio::test]
async fn a_forget_lets_go_of_no_record_a_reopening_took_meanwhile() {
    use std::sync::{Arc, Mutex};
    let side = side("forget-hold", Limits::default());
    let records = side.base.join("records");
    let store = Arc::new(Store::open(&records, Limits::default()).unwrap());
    kept_in(&store, &side, PEER, key(225), "f", 1, 5);
    assert!(store.release(PEER, key(225)));
    let (reopening, taken) = (store.clone(), Arc::new(Mutex::new(None)));
    let slot = taken.clone();
    seams::FORGETTING.set(Some(Box::new(move || {
        *slot.lock().unwrap() = Some(reopening.reopen(PEER, key(225)).unwrap());
    })));
    // Taken since the forget looked: the record stays, with its partial,
    // and the forget says so.
    let busy = Some(ResumeError::Busy);
    assert_eq!(store.forget(PEER, key(225)).err(), busy);
    assert!(taken.lock().unwrap().is_some(), "the reopening ran");
    assert!(store.received(PEER, key(225)).is_some());
    assert!(side.place.join("f.part").exists());
    assert_eq!(
        store.reopen(PEER, key(225)).err(),
        busy,
        "not reopened under the first"
    );
    assert_eq!(store.forget(PEER, key(225)).err(), busy);
    // And the forget's claim is gone: the file is a save's to take.
    assert!(store.release(PEER, key(225)));
    assert!(store.saving(PEER, key(225)).is_ok());
}

/// The other side knows the key of a file sent to it, and can hold a
/// stream open under that key: the sender's record is forgotten all the
/// same, and the file coming under the key is left to its save.
#[tokio::test]
async fn a_sending_record_is_forgotten_while_a_file_comes_under_its_key() {
    let side = side("forget-sent-key", Limits::default());
    let store = &side.store;
    source(&side, key(226), b"sent");
    let guard = store.saving(PEER, key(226)).unwrap();
    store.forget(PEER, key(226)).unwrap();
    assert!(store.sent(PEER, key(226)).is_none());
    // The same once that stream has a record, held by its save.
    source(&side, key(226), b"sent");
    kept_zeros(&side, PEER, key(226), "coming", 1, 5);
    store.forget(PEER, key(226)).unwrap();
    assert!(store.sent(PEER, key(226)).is_none());
    assert!(store.received(PEER, key(226)).is_some(), "the save's own");
    assert!(side.place.join("coming.part").exists());
    // With no sending record, the file being received is what is asked
    // for, and is in use.
    assert_eq!(store.forget(PEER, key(226)).err(), Some(ResumeError::Busy));
    assert!(store.received(PEER, key(226)).is_some());
    guard.forget().unwrap();
    assert!(store.received(PEER, key(226)).is_none());
    assert!(!side.place.join("coming.part").exists());
}

/// An empty file is taken for a record not yet written only with the one
/// name: one that some other name shows too is another file.
#[tokio::test]
async fn an_empty_file_with_a_second_name_under_a_records_name_is_not_written() {
    let side = side("empty-linked", Limits::default());
    let (record, partial) = unwritten(&side, key(227), "f");
    let name = side
        .base
        .join("records")
        .join(crate::store::file_name(true, PEER, key(227)));
    let other = side.place.join("notes.txt");
    std::fs::write(&other, b"").unwrap();
    std::fs::hard_link(&other, &name).unwrap();
    for _ in 0..2 {
        assert_eq!(
            side.store.receiving(&record, &partial),
            Err(ResumeError::Busy)
        );
        assert_eq!(std::fs::read(&other).unwrap(), b"", "not a byte in it");
    }
    assert!(side.store.received(PEER, key(227)).is_none());
}

/// A receiving record's bytes laid out field by field: `kind` is its kind
/// byte and `second` what follows its partial's place, nothing in a
/// record of before.
fn receiving_by_hand(kind: u8, second: &[u8]) -> Vec<u8> {
    // The record format, and the record's kind.
    let mut bytes = vec![1, kind];
    bytes.extend_from_slice(&PEER.0);
    bytes.extend_from_slice(&key(230).0);
    // Nothing that must stay the same; 9 bytes declared.
    bytes.extend_from_slice(&[0, 9]);
    // A Unix root: its path, its device and its inode.
    bytes.extend_from_slice(&[1, 3]);
    bytes.extend_from_slice(b"/in");
    bytes.push(7);
    bytes.extend_from_slice(&9u128.to_le_bytes());
    // No level below it; the partial's name and its inode.
    bytes.extend_from_slice(&[0, 6]);
    bytes.extend_from_slice(b"f.part");
    bytes.extend_from_slice(&11u128.to_le_bytes());
    bytes.extend_from_slice(second);
    // 4 bytes kept, their hash, made at 5 and last changed at 6.
    bytes.push(4);
    bytes.extend_from_slice(&hash(b"half").0);
    bytes.extend_from_slice(&[5, 6]);
    bytes
}

fn receiving_with(second: safecopy_journal::Second) -> Receiving {
    use safecopy_journal::{Location, Root};
    Receiving {
        peer: PEER,
        key: key(230),
        same: Vec::new(),
        size: 9,
        at: PartialAt {
            location: Location {
                root: Root::Unix {
                    path: b"/in".to_vec(),
                    device: 7,
                    inode: 9,
                },
                levels: Vec::new(),
            },
            partial: "f.part".into(),
            file: 11,
            second,
        },
        have: 4,
        prefix: hash(b"half"),
        created: 5,
        last: 6,
    }
}

/// Both directions of the record's change, with bytes built by hand: a
/// record of before reads as before and is still written so, and one with
/// a second identity is a kind of its own, which a version that knows two
/// kinds reads as damaged, as this one reads a kind it does not know.
#[test]
fn a_receiving_record_holds_a_second_identity_as_a_kind_of_its_own() {
    use crate::record::Record;
    use safecopy_journal::Second;
    let before = receiving_by_hand(0, &[]);
    let none = receiving_with(Second::None);
    assert_eq!(Record::decode(&before), Ok(Record::Receiving(none.clone())));
    assert_eq!(
        Record::Receiving(none).encode(),
        before,
        "not a byte changed"
    );

    // 3 seconds and 5 nanoseconds before the epoch, zigzagged (-4 is 7);
    // a generation of 300; both of them.
    let cases: [(Second, &[u8]); 3] = [
        (
            Second::Both {
                secs: -4,
                nanos: 999_999_995,
                generation: 300,
            },
            &[3, 7, 0xFB, 0x93, 0xEB, 0xDC, 0x03, 0xAC, 0x02],
        ),
        (
            Second::Born {
                secs: -4,
                nanos: 999_999_995,
            },
            &[1, 7, 0xFB, 0x93, 0xEB, 0xDC, 0x03],
        ),
        (Second::Generation(300), &[2, 0xAC, 0x02]),
    ];
    for (second, tail) in cases {
        let bytes = receiving_by_hand(2, tail);
        let record = Record::Receiving(receiving_with(second));
        assert_eq!(Record::decode(&bytes).as_ref(), Ok(&record), "{second:?}");
        assert_eq!(record.encode(), bytes, "{second:?}");
        // The kind byte is all a version of before reads of it.
        assert_eq!(bytes[1], 2);
        // Read by the rules of the kind of before, it does not end where
        // a record ends.
        let mut as_before = bytes.clone();
        as_before[1] = 0;
        assert_eq!(Record::decode(&as_before), Err(ResumeError::Damaged));
    }
    // A kind no version has; in the new kind, no second identity, one of
    // no kind, a second of a billion nanoseconds, a generation past 32 bits.
    assert_eq!(
        Record::decode(&receiving_by_hand(3, &[])),
        Err(ResumeError::Damaged)
    );
    let bad: [&[u8]; 5] = [
        &[],
        &[0],
        &[4, 1],
        &[1, 2, 0x80, 0x94, 0xEB, 0xDC, 0x03],
        &[2, 0x80, 0x80, 0x80, 0x80, 0x10],
    ];
    for tail in bad {
        assert_eq!(
            Record::decode(&receiving_by_hand(2, tail)),
            Err(ResumeError::Damaged),
            "{tail:?}"
        );
    }
}

/// A journal's entry for a partial that is gone and a record of a new
/// partial with its name and inode are not one partial: the record does
/// not keep what the entry lists.
#[test]
fn two_places_with_second_identities_that_differ_are_not_one_partial() {
    use safecopy_journal::Second;
    let born = |secs| Second::Born { secs, nanos: 0 };
    let at = |second| receiving_with(second).at;
    let same = crate::store::same_partial;
    assert!(same(&at(born(1)), &at(born(1))));
    assert!(!same(&at(born(1)), &at(born(2))));
    assert!(!same(
        &at(Second::Generation(1)),
        &at(Second::Generation(2))
    ));
    // One of before, or of another kind, says nothing against it.
    assert!(same(&at(born(1)), &at(Second::None)));
    assert!(same(&at(Second::None), &at(Second::Generation(2))));
    assert!(same(&at(born(1)), &at(Second::Generation(2))));
}

/// The store finds a kept partial through its record's place: a file under
/// the name with the recorded inode and another second identity, as a new
/// file given a removed partial's inode has, is not listed, not reopened
/// to be cut, and not removed with its record.
#[cfg(unix)]
#[test]
fn the_store_leaves_a_file_with_the_inode_and_another_second_identity() {
    use safecopy_journal::Second;
    let side = side("second-other", Limits::default());
    let real = kept_in(&side.store, &side, PEER, key(231), "real", 1, 5);
    // One generation on, or born a second before: the record of a file
    // that is gone. Before, since a file born later than its record is
    // another on every system, and one born earlier is not on all.
    let other = match real.at.second {
        Second::None => {
            eprintln!("skipped: this file system gives a file no second identity");
            return;
        }
        Second::Born { secs, nanos } => Second::Born {
            secs: secs - 1,
            nanos,
        },
        Second::Generation(n) => Second::Generation(n ^ 1),
        Second::Both {
            secs,
            nanos,
            generation,
        } => Second::Both {
            secs,
            nanos,
            generation: generation ^ 1,
        },
    };
    assert_eq!(keys(&side.store.kept_by(PEER).unwrap()), [key(231)]);
    // Let go of, so the record below is read from the disk again.
    side.store.release(PEER, key(231));
    let stale = Receiving {
        at: PartialAt {
            second: other,
            ..real.at.clone()
        },
        ..real.clone()
    };
    let path = planted(&side, stale.clone());
    assert_eq!(side.store.received(PEER, key(231)), Some(stale.clone()));
    assert!(side.store.kept_by(PEER).unwrap().is_empty());
    assert!(!side.store.keeps(&real.at), "another partial than the file");
    assert_eq!(
        side.store.reopen(PEER, key(231)).map(|_| ()),
        Err(ResumeError::Gone)
    );
    let hour = Duration::from_secs(3600);
    side.store.expire(5 + 2 * 3600, hour, &[]).unwrap();
    assert!(!path.exists(), "the record went");
    let file = side.place.join("real.part");
    assert_eq!(
        std::fs::metadata(&file).unwrap().len(),
        C,
        "not cut, not removed"
    );
}
