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
    side.store.receiving(&record, &file).unwrap();
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
    let next = sender_checks(Some(&sending), Some(&opened), &held, None)
        .await
        .unwrap();
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
        let next = sender_checks(Some(&sending), Some(&opened), &held, kept)
            .await
            .unwrap();
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
        let next = sender_checks(Some(&sending), opened.as_ref(), &held, None)
            .await
            .unwrap();
        assert_eq!(next, Next::Changed, "{case}");
        assert_eq!(Reply::of(next), Reply::Stop(Verdict::Dropped));
    }
    // No record at all: dropped.
    let side = side("changed-none", Limits::default());
    let sending = source(&side, key(5), &bytes);
    let opened = Source::open(&sending).unwrap();
    let next = sender_checks(None, Some(&opened), &held, None)
        .await
        .unwrap();
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
        let next = sender_checks(Some(&sending), Some(&opened), &held, None)
            .await
            .unwrap();
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
    let next = sender_checks(record.as_ref(), opened.as_ref(), &held, None)
        .await
        .unwrap();
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
    let live = [key(20), key(21), key(23)];
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
    let held = receiver.store.held_from(PEER).await.unwrap();
    assert_eq!(held.len(), 2);
    let limits = Limits::default();
    // A small buffer, so each side waits on the other.
    let (mut a, mut b) = tokio::io::duplex(64);
    let answering = handshake::answer(&mut a, &held, &limits);
    let offering = handshake::offer(&mut b, &limits, async |held: &Held| {
        let record = sender.store.sent(PEER, held.key);
        let opened = record.as_ref().and_then(Source::open);
        sender_checks(record.as_ref(), opened.as_ref(), held, None)
            .await
            .unwrap()
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
    other.forget(PEER, key(62)).unwrap();
    other.forget(PEER, key(64)).unwrap();
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
