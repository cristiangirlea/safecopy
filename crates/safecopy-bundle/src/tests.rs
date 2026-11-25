use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use safecopy_chunks::CandidateHash;
use safecopy_folder::{Directory, Meta};
use tokio::io::{AsyncRead, AsyncWriteExt, DuplexStream, ReadBuf};

use super::*;

/// A new, empty folder for one test, under the build's target folder.
pub(crate) fn temp(name: &str) -> PathBuf {
    // The test binary is in target/<profile>/deps.
    let exe = std::env::current_exe().unwrap();
    let target = exe.ancestors().nth(3).unwrap();
    let dir = target
        .join("test-tmp")
        .join(format!("bundle-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub(crate) fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn sha(bytes: &[u8]) -> Digest {
    let mut d = [0; 32];
    d.copy_from_slice(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref());
    Digest(d)
}

use safecopy_chunks::Digest;

/// `len` bytes that differ by `seed`.
pub(crate) fn data(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

pub(crate) fn key(n: u8) -> TransferKey {
    TransferKey([n; 16])
}

pub(crate) fn entries(files: &[&[u8]]) -> Vec<TableEntry> {
    files
        .iter()
        .enumerate()
        .map(|(i, bytes)| TableEntry {
            key: key(i as u8 + 1),
            meta: format!("meta {i}").into_bytes(),
            size: bytes.len() as u64,
        })
        .collect()
}

/// The bundle a sender writes of `files`, each whole.
async fn bundle(files: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut writer = BundleWriter::new(
        &mut out,
        &entries(files),
        &BundleLimits::default(),
        WriteOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(writer.entries(), files.len());
    for file in files {
        assert_eq!(
            writer.write(&mut &file[..]).await.unwrap(),
            Written::Whole(sha(file))
        );
    }
    writer.finish().await.unwrap();
    out
}

/// Where each file's bytes start in a bundle of `files` with the test's
/// metadata blocks.
fn offsets(files: &[&[u8]]) -> Vec<usize> {
    let table: usize = 4 + entries(files)
        .iter()
        .map(|e| 28 + e.meta.len())
        .sum::<usize>();
    let mut at = 1 + 4 + table;
    files
        .iter()
        .map(|f| {
            let start = at;
            at += f.len() + 33;
            start
        })
        .collect()
}

/// An in-memory stream carrying `bytes`, then its end.
pub(crate) fn stream(bytes: Vec<u8>) -> DuplexStream {
    let (ours, mut theirs) = tokio::io::duplex(64 << 10);
    tokio::spawn(async move {
        // The reader may stop early and drop its end.
        let _ = theirs.write_all(&bytes).await;
        let _ = theirs.shutdown().await;
    });
    ours
}

pub(crate) fn save<'a>(dir: &'a Directory, name: &str) -> Take<'a> {
    Take::Save(Target {
        dir,
        name: name.to_string(),
        meta: Meta::default(),
    })
}

fn names(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("f{i}.bin")).collect()
}

fn quick() -> ReadOptions<'static> {
    ReadOptions {
        sync: false,
        ..ReadOptions::default()
    }
}

async fn read_all<'a>(
    bytes: Vec<u8>,
    decisions: impl FnOnce() -> Vec<Take<'a>>,
    hooks: &dyn BundleHooks,
) -> Report {
    let table = BundleReader::open(stream(bytes), &BundleLimits::default())
        .await
        .unwrap();
    let decisions = decisions();
    table
        .read(decisions, &quick(), hooks, std::future::pending())
        .await
}

fn saved_path(outcome: &Outcome) -> &Path {
    match outcome {
        Outcome::Saved(saved) => &saved.path,
        other => panic!("not saved: {other:?}"),
    }
}

/// Every call the reader made of its hooks.
#[derive(Default)]
struct Calls {
    before_create: Mutex<Vec<Vec<usize>>>,
    admitted: Mutex<Vec<Vec<(usize, String)>>>,
    published: Mutex<Vec<usize>>,
    ended: Mutex<Vec<Report>>,
    /// The bytes read off the stream, shared with a [`Counted`] reader.
    read: Arc<AtomicU64>,
    /// [`Self::read`] when the partials were admitted.
    read_at_admission: AtomicU64,
    /// Every admitted partial was on the disk when admitted.
    on_disk: Mutex<Option<bool>>,
}

impl BundleHooks for Calls {
    fn before_create(&self, taken: &[usize]) -> io::Result<()> {
        self.before_create.lock().unwrap().push(taken.to_vec());
        Ok(())
    }
    fn admitted(&self, partials: &[Partial<'_>]) -> io::Result<()> {
        self.read_at_admission
            .store(self.read.load(Ordering::SeqCst), Ordering::SeqCst);
        let all = partials.iter().all(|p| p.dir.path().join(p.name).is_file());
        *self.on_disk.lock().unwrap() = Some(all);
        self.admitted.lock().unwrap().push(
            partials
                .iter()
                .map(|p| (p.entry, p.name.to_string_lossy().into_owned()))
                .collect(),
        );
        Ok(())
    }
    fn before_publish(&self, entry: usize, _: &File, _: &OsStr) -> io::Result<()> {
        self.published.lock().unwrap().push(entry);
        Ok(())
    }
    fn ended(&self, report: &Report) {
        self.ended.lock().unwrap().push(report.clone());
    }
}

/// A reader that counts the bytes it gives.
struct Counted<R> {
    inner: R,
    read: Arc<AtomicU64>,
}

impl<R: AsyncRead + Unpin> AsyncRead for Counted<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
        let n = buf.filled().len() - before;
        self.read.fetch_add(n as u64, Ordering::SeqCst);
        polled
    }
}

/// A reader that sets `flag` once it has given `at` bytes, never giving
/// more than that in the read that reaches it.
struct Tripwire<R> {
    inner: R,
    given: u64,
    at: u64,
    flag: Arc<AtomicBool>,
}

impl<R: AsyncRead + Unpin> AsyncRead for Tripwire<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let room = if self.given < self.at {
            usize::try_from(self.at - self.given)
                .map_or(buf.remaining(), |left| left.min(buf.remaining()))
        } else {
            buf.remaining()
        };
        let mut bytes = vec![0; room];
        let mut part = ReadBuf::new(&mut bytes);
        let polled = Pin::new(&mut self.inner).poll_read(cx, &mut part);
        let n = part.filled().len();
        buf.put_slice(part.filled());
        self.given += n as u64;
        if self.given >= self.at {
            self.flag.store(true, Ordering::SeqCst);
        }
        polled
    }
}

/// A cancel that is ready once `flag` is set. Nothing wakes it: the reader
/// sees it at its next check, so no timing is involved.
fn on_flag(flag: &AtomicBool) -> impl std::future::Future<Output = ()> + '_ {
    std::future::poll_fn(move |_| {
        if flag.load(Ordering::SeqCst) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
}

/// Sets `flag` before entry `publish` is published; records the entries
/// that reached publishing and counts the ends.
struct Trip {
    publish: Option<usize>,
    flag: Arc<AtomicBool>,
    published: Mutex<Vec<usize>>,
    ended: AtomicUsize,
}

impl Trip {
    fn new(publish: Option<usize>) -> Self {
        Self {
            publish,
            flag: Arc::default(),
            published: Mutex::default(),
            ended: AtomicUsize::new(0),
        }
    }
}

impl BundleHooks for Trip {
    fn before_publish(&self, entry: usize, _: &File, _: &OsStr) -> io::Result<()> {
        self.published.lock().unwrap().push(entry);
        if Some(entry) == self.publish {
            self.flag.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
    fn ended(&self, _: &Report) {
        self.ended.fetch_add(1, Ordering::SeqCst);
    }
}

// SaveUnlessSame.

/// A folder with `a.txt` holding `there`, and the hash of its candidate
/// for `incoming`'s size, when it has one.
async fn with_candidate(name: &str, there: &[u8], incoming: usize) -> (PathBuf, Option<Digest>) {
    let path = temp(name);
    std::fs::write(path.join("a.txt"), there).unwrap();
    let dir = Directory::open(&path).unwrap();
    let candidate = dir.candidate(OsStr::new("a.txt"), incoming as u64);
    let digest = match candidate {
        Some(mut candidate) => Some(candidate.hash(&()).await.unwrap()),
        None => None,
    };
    (path, digest)
}

#[tokio::test]
async fn a_bundled_file_equal_to_its_candidate_is_already_there_and_its_partial_removed() {
    let same = data(5000, 1);
    let other = data(300, 2);
    let (path, digest) = with_candidate("already-there", &same, same.len()).await;
    let digest = digest.expect("the same name and size is a candidate");
    let dir = Directory::open(&path).unwrap();
    let calls = Calls::default();
    let report = read_all(
        bundle(&[&same, &other]).await,
        || {
            vec![
                Take::SaveUnlessSame(
                    Target {
                        dir: &dir,
                        name: "a.txt".into(),
                        meta: Meta::default(),
                    },
                    digest,
                ),
                save(&dir, "b.txt"),
            ]
        },
        &calls,
    )
    .await;
    assert_eq!(report.error, None);
    assert_eq!(report.outcomes[0], Outcome::AlreadyThere);
    assert_eq!(saved_path(&report.outcomes[1]), path.join("b.txt"));
    // Its partial was made at admission, then removed; the file there is
    // untouched and never numbered.
    assert_eq!(
        calls.admitted.lock().unwrap()[0],
        vec![(0, "a.txt.part".to_string()), (1, "b.txt.part".to_string())]
    );
    assert_eq!(*calls.published.lock().unwrap(), vec![1]);
    assert_eq!(files_in(&path), ["a.txt", "b.txt"]);
    assert_eq!(std::fs::read(path.join("a.txt")).unwrap(), same);
}

#[tokio::test]
async fn a_bundled_file_unlike_its_candidate_is_saved_under_a_numbered_name() {
    let there = data(5000, 1);
    let incoming = data(5000, 9);
    assert_ne!(there, incoming);
    let (path, digest) = with_candidate("unlike", &there, incoming.len()).await;
    let digest = digest.expect("the same name and size is a candidate");
    let dir = Directory::open(&path).unwrap();
    let report = read_all(
        bundle(&[&incoming]).await,
        || {
            vec![Take::SaveUnlessSame(
                Target {
                    dir: &dir,
                    name: "a.txt".into(),
                    meta: Meta::default(),
                },
                digest,
            )]
        },
        &(),
    )
    .await;
    assert_eq!(report.error, None);
    assert_eq!(saved_path(&report.outcomes[0]), path.join("a (2).txt"));
    assert_eq!(files_in(&path), ["a (2).txt", "a.txt"]);
    assert_eq!(std::fs::read(path.join("a.txt")).unwrap(), there);
    assert_eq!(std::fs::read(path.join("a (2).txt")).unwrap(), incoming);
}

// plan.

#[test]
fn small_files_are_bundled_and_large_ones_are_not() {
    const MIB: u64 = 1 << 20;
    let limits = BundleLimits::default();
    assert_eq!(limits.small, MIB);
    assert_eq!(limits.files, 64);
    assert_eq!(limits.bytes, 8 * MIB);
    assert_eq!(limits.table_bytes, 128 << 10);
    assert_eq!(limits.meta_bytes, 4 << 10);
    assert_eq!(limits.at_once, 2);
    assert_eq!(limits.alone_over, 8 * MIB);

    // 1 MiB is small, a byte more overlaps, 8 MiB still overlaps, a byte
    // more goes alone; the small ones around them share a bundle.
    let sizes = [0, MIB, MIB + 1, 10, 8 * MIB, 8 * MIB + 1, 20];
    assert_eq!(
        plan(&sizes, &limits),
        vec![
            Group::Bundle(vec![0, 1, 3, 6]),
            Group::Overlapped(2),
            Group::Overlapped(4),
            Group::Alone(5),
        ]
    );
    // 64 files to a bundle at most.
    let many = vec![1; 130];
    let groups = plan(&many, &limits);
    assert_eq!(
        groups,
        vec![
            Group::Bundle((0..64).collect()),
            Group::Bundle((64..128).collect()),
            Group::Bundle(vec![128, 129]),
        ]
    );
    // 8 MiB to a bundle at most: eight files of 1 MiB fill one.
    let full = vec![MIB; 9];
    assert_eq!(
        plan(&full, &limits),
        vec![Group::Bundle((0..8).collect()), Group::Bundle(vec![8])]
    );
    // The limits are settings.
    let tight = BundleLimits {
        small: 10,
        files: 2,
        alone_over: 100,
        ..limits
    };
    assert_eq!(
        plan(&[10, 11, 5, 5, 101], &tight),
        vec![
            Group::Bundle(vec![0, 2]),
            Group::Overlapped(1),
            Group::Bundle(vec![3]),
            Group::Alone(4),
        ]
    );
}

// One bad file never spoils the others.

#[tokio::test]
async fn one_bad_hash_in_a_bundle_spoils_no_other_file() {
    let path = temp("bad-hash");
    let dir = Directory::open(&path).unwrap();
    let files = [data(100, 1), data(70_000, 2), data(5, 3)];
    let refs: Vec<&[u8]> = files.iter().map(|f| &f[..]).collect();
    let mut bytes = bundle(&refs).await;
    // One byte of the middle file's bytes is changed on the way.
    bytes[offsets(&refs)[1] + 40_000] ^= 0xff;
    let calls = Calls::default();
    let names = names(3);
    let report = read_all(
        bytes,
        || names.iter().map(|n| save(&dir, n)).collect(),
        &calls,
    )
    .await;
    assert_eq!(report.error, None);
    assert_eq!(report.outcomes[1], Outcome::Damaged);
    assert_eq!(saved_path(&report.outcomes[0]), path.join("f0.bin"));
    assert_eq!(saved_path(&report.outcomes[2]), path.join("f2.bin"));
    assert_eq!(files_in(&path), ["f0.bin", "f2.bin"]);
    assert_eq!(std::fs::read(path.join("f0.bin")).unwrap(), files[0]);
    assert_eq!(std::fs::read(path.join("f2.bin")).unwrap(), files[2]);
    assert_eq!(*calls.published.lock().unwrap(), vec![0, 2]);
}

/// A source that gives `len` bytes and then fails.
struct Failing(usize);

impl AsyncRead for Failing {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.0 == 0 {
            return Poll::Ready(Err(io::ErrorKind::PermissionDenied.into()));
        }
        let n = self.0.min(buf.remaining());
        buf.put_slice(&vec![7; n]);
        self.0 -= n;
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn a_file_given_up_in_a_bundle_is_padded_and_thrown_away() {
    let path = temp("given-up");
    let dir = Directory::open(&path).unwrap();
    let files = [
        data(10, 1),
        data(3000, 2),
        data(2000, 3),
        data(500, 4),
        data(9, 5),
    ];
    let refs: Vec<&[u8]> = files.iter().map(|f| &f[..]).collect();
    let mut bytes = Vec::new();
    let mut writer = BundleWriter::new(
        &mut bytes,
        &entries(&refs),
        &BundleLimits::default(),
        WriteOptions::default(),
    )
    .await
    .unwrap();
    writer.write(&mut &files[0][..]).await.unwrap();
    // Given up outright; a source shorter than its size; one that fails;
    // one that changed while it was read.
    writer.give_up().await.unwrap();
    assert_eq!(
        writer.write(&mut &files[2][..1000]).await.unwrap(),
        Written::GivenUp
    );
    assert_eq!(
        writer.write(&mut Failing(100)).await.unwrap(),
        Written::GivenUp
    );
    assert_eq!(
        writer
            .write_checked(&mut &files[4][..], || false)
            .await
            .unwrap(),
        Written::GivenUp
    );
    assert_eq!(
        writer.write(&mut &files[0][..]).await,
        Err(BundleError::NoEntryLeft)
    );
    writer.finish().await.unwrap();
    // Each given-up file keeps its place in the framing: its size in
    // bytes, the mark 1 and a zero hash.
    let at = offsets(&refs);
    assert_eq!(bytes.len(), at[4] + 9 + 33);
    assert!(bytes[at[1]..at[1] + 3000].iter().all(|b| *b == 0));
    assert_eq!(bytes[at[1] + 3000], GIVEN_UP);
    assert!(bytes[at[1] + 3001..at[2]].iter().all(|b| *b == 0));
    assert_eq!(bytes[at[2] + 2000], GIVEN_UP);
    assert!(bytes[at[2] + 1000..at[2] + 2000].iter().all(|b| *b == 0));
    assert_eq!(bytes[at[3] + 500], GIVEN_UP);
    assert_eq!(bytes[at[4] + 9], GIVEN_UP);
    assert_eq!(bytes[at[0] + 10], WHOLE);

    let calls = Calls::default();
    let names = names(5);
    let report = read_all(
        bytes,
        || names.iter().map(|n| save(&dir, n)).collect(),
        &calls,
    )
    .await;
    assert_eq!(report.error, None);
    assert!(matches!(report.outcomes[0], Outcome::Saved(_)));
    for outcome in &report.outcomes[1..] {
        assert_eq!(*outcome, Outcome::GivenUp);
    }
    assert_eq!(files_in(&path), ["f0.bin"]);
    assert_eq!(*calls.published.lock().unwrap(), vec![0]);
}

#[tokio::test]
async fn a_skipped_item_is_read_past() {
    let path = temp("skipped");
    let dir = Directory::open(&path).unwrap();
    let files = [data(100, 1), data(1 << 20, 2), data(5, 3)];
    let refs: Vec<&[u8]> = files.iter().map(|f| &f[..]).collect();
    let calls = Calls::default();
    let report = read_all(
        bundle(&refs).await,
        || vec![save(&dir, "a"), Take::Skip, save(&dir, "c")],
        &calls,
    )
    .await;
    assert_eq!(report.error, None);
    assert_eq!(report.outcomes[1], Outcome::Refused);
    assert_eq!(saved_path(&report.outcomes[2]), path.join("c"));
    assert_eq!(std::fs::read(path.join("c")).unwrap(), files[2]);
    // No partial for the skipped one, ever.
    assert_eq!(*calls.before_create.lock().unwrap(), vec![vec![0, 2]]);
    assert_eq!(
        calls.admitted.lock().unwrap()[0],
        vec![(0, "a.part".to_string()), (2, "c.part".to_string())]
    );
    assert_eq!(files_in(&path), ["a", "c"]);
}

#[tokio::test]
async fn a_cut_bundle_keeps_the_saved_files_and_names_the_rest_not_received() {
    let path = temp("cut");
    let dir = Directory::open(&path).unwrap();
    let files = [
        data(100, 1),
        data(200, 2),
        data(300_000, 3),
        data(400, 4),
        data(5, 5),
    ];
    let refs: Vec<&[u8]> = files.iter().map(|f| &f[..]).collect();
    let mut bytes = bundle(&refs).await;
    // Cut in the middle of the third file.
    bytes.truncate(offsets(&refs)[2] + 150_000);
    let calls = Calls::default();
    let names = names(5);
    let report = read_all(
        bytes,
        || {
            let mut takes: Vec<Take<'_>> = names.iter().map(|n| save(&dir, n)).collect();
            takes[4] = Take::Skip;
            takes
        },
        &calls,
    )
    .await;
    assert_eq!(report.error, Some(BundleError::CutShort { entry: 2 }));
    assert!(matches!(report.outcomes[0], Outcome::Saved(_)));
    assert!(matches!(report.outcomes[1], Outcome::Saved(_)));
    assert_eq!(report.outcomes[2], Outcome::NotReceived);
    assert_eq!(report.outcomes[3], Outcome::NotReceived);
    assert_eq!(report.outcomes[4], Outcome::Refused);
    // The saved files stay; the partial being written and those not
    // reached are gone.
    assert_eq!(files_in(&path), ["f0.bin", "f1.bin"]);
    assert_eq!(*calls.ended.lock().unwrap(), vec![report]);
}

// The table's bounds.

/// The bytes of a bundle whose table holds `entries` and says `count`.
fn raw_table(count: u32, entries: &[(u64, usize)]) -> Vec<u8> {
    let mut table = count.to_le_bytes().to_vec();
    for (i, (size, meta)) in entries.iter().enumerate() {
        table.extend_from_slice(&size.to_le_bytes());
        table.extend_from_slice(&[i as u8; 16]);
        table.extend_from_slice(&(*meta as u32).to_le_bytes());
        table.extend(std::iter::repeat_n(b'm', *meta));
    }
    let mut out = vec![safecopy_chunks::FRAMING];
    out.extend_from_slice(&(table.len() as u32).to_le_bytes());
    out.extend_from_slice(&table);
    out
}

async fn open_raw(bytes: Vec<u8>) -> Result<Vec<TableEntry>, BundleError> {
    BundleReader::open(&bytes[..], &BundleLimits::default())
        .await
        .map(|table| table.entries().to_vec())
}

#[tokio::test]
async fn an_entry_over_small_or_more_than_64_entries_or_past_8_mib_is_refused() {
    const MIB: u64 = 1 << 20;
    // The limits themselves are taken: 1 MiB, 64 entries, 8 MiB in all.
    assert_eq!(open_raw(raw_table(1, &[(MIB, 0)])).await.unwrap().len(), 1);
    assert_eq!(
        open_raw(raw_table(64, &[(1, 0); 64])).await.unwrap().len(),
        64
    );
    assert_eq!(
        open_raw(raw_table(8, &[(MIB, 0); 8])).await.unwrap().len(),
        8
    );
    // One past each is refused.
    assert_eq!(
        open_raw(raw_table(2, &[(1, 0), (MIB + 1, 0)])).await,
        Err(BundleError::TooLarge {
            entry: 1,
            size: MIB + 1,
            most: MIB
        })
    );
    assert_eq!(
        open_raw(raw_table(65, &[(1, 0); 65])).await,
        Err(BundleError::TooMany {
            count: 65,
            most: 64
        })
    );
    let mut past = vec![(MIB, 0); 8];
    past.push((1, 0));
    assert_eq!(
        open_raw(raw_table(9, &past)).await,
        Err(BundleError::TooManyBytes {
            bytes: 8 * MIB + 1,
            most: 8 * MIB
        })
    );
    // A metadata block past 4 KiB too.
    assert_eq!(
        open_raw(raw_table(1, &[(1, 4097)])).await,
        Err(BundleError::MetaTooLong {
            entry: 0,
            len: 4097,
            most: 4096
        })
    );
    // A huge declared size never overflows the sum.
    let limits = BundleLimits {
        small: u64::MAX,
        bytes: u64::MAX,
        ..BundleLimits::default()
    };
    let huge = raw_table(2, &[(u64::MAX, 0), (1, 0)]);
    assert!(matches!(
        BundleReader::open(&huge[..], &limits).await,
        Err(BundleError::TooManyBytes { .. })
    ));

    // The writer never writes such a table: it refuses an entry over
    // `small`, and ends the bundle at 64 entries or 8 MiB.
    let big = TableEntry {
        key: key(1),
        meta: Vec::new(),
        size: MIB + 1,
    };
    let refused = BundleWriter::new(
        Vec::new(),
        &[big],
        &BundleLimits::default(),
        WriteOptions::default(),
    )
    .await;
    assert!(matches!(refused, Err(BundleError::TooLarge { .. })));
    let one = |size| TableEntry {
        key: key(1),
        meta: Vec::new(),
        size,
    };
    for (list, taken) in [(vec![one(1); 70], 64), (vec![one(MIB); 9], 8)] {
        let writer = BundleWriter::new(
            Vec::new(),
            &list,
            &BundleLimits::default(),
            WriteOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(writer.entries(), taken);
    }
}

#[tokio::test]
async fn a_table_over_128_kib_or_whose_count_does_not_match_is_refused_before_any_byte() {
    const TABLE: usize = 128 << 10;
    let file_bytes = vec![9u8; 1000];
    // Exactly 128 KiB of table is taken: 33 entries whose metadata blocks
    // fill it.
    let mut fill = vec![(1, 3944); 32];
    fill.push((1, 3936));
    let exact = raw_table(33, &fill);
    assert_eq!(exact.len(), 1 + 4 + TABLE);
    assert_eq!(open_raw(exact).await.unwrap().len(), 33);

    // One byte more is refused from the length alone: the reader takes the
    // version and the length, nothing else.
    fill[32].1 += 1;
    let mut over = raw_table(33, &fill);
    over.extend_from_slice(&file_bytes);
    let mut rest = &over[..];
    assert_eq!(
        BundleReader::open(&mut rest, &BundleLimits::default())
            .await
            .unwrap_err(),
        BundleError::TableTooLong {
            len: TABLE as u64 + 1,
            most: TABLE as u64
        }
    );
    assert_eq!(rest.len(), over.len() - 5);

    // A count that does not match the entries: fewer held than counted, or
    // bytes left after them. The table is read, no byte after it.
    for (count, held) in [(3, 2), (1, 2), (0, 1)] {
        let mut bytes = raw_table(count, &vec![(10, 4); held]);
        let table_end = bytes.len();
        bytes.extend_from_slice(&file_bytes);
        let mut rest = &bytes[..];
        assert_eq!(
            BundleReader::open(&mut rest, &BundleLimits::default())
                .await
                .unwrap_err(),
            BundleError::CountMismatch
        );
        assert_eq!(rest.len(), bytes.len() - table_end);
    }

    // A stream that ends inside the table.
    let whole = raw_table(2, &[(10, 4); 2]);
    assert_eq!(
        open_raw(whole[..whole.len() - 1].to_vec())
            .await
            .unwrap_err(),
        BundleError::TableCutShort
    );
    // Another framing version.
    let mut other = whole.clone();
    other[0] = 2;
    assert_eq!(
        open_raw(other).await.unwrap_err(),
        BundleError::Version { got: 2 }
    );

    // The writer ends a bundle early rather than pass 128 KiB of table,
    // and refuses an entry that alone would.
    let entry = |meta| TableEntry {
        key: key(1),
        meta: vec![1; meta],
        size: 1,
    };
    let writer = BundleWriter::new(
        Vec::new(),
        &vec![entry(4096); 40],
        &BundleLimits::default(),
        WriteOptions::default(),
    )
    .await
    .unwrap();
    // 4 + 31 * (28 + 4096) fits; one more does not.
    assert_eq!(writer.entries(), 31);
    let tight = BundleLimits {
        table_bytes: 100,
        ..BundleLimits::default()
    };
    assert!(matches!(
        BundleWriter::new(Vec::new(), &[entry(100)], &tight, WriteOptions::default()).await,
        Err(BundleError::TableTooLong { .. })
    ));
}

#[tokio::test]
async fn bytes_that_disagree_with_the_table_refuse_the_rest_and_keep_what_was_published() {
    let files = [data(100, 1), data(200, 2), data(300, 3)];
    let refs: Vec<&[u8]> = files.iter().map(|f| &f[..]).collect();
    let whole = bundle(&refs).await;
    let at = offsets(&refs);
    let names = names(3);

    // The stream ending early, inside the last file's hash.
    let mut early = whole.clone();
    early.pop();
    // A byte past the last hash.
    let mut long = whole.clone();
    long.push(0);
    // A mark neither 0 nor 1 after the second file.
    let mut bad_mark = whole.clone();
    bad_mark[at[1] + 200] = 2;

    let cases = [
        ("early", early, BundleError::CutShort { entry: 2 }, 2),
        ("long", long, BundleError::TooLong, 2),
        (
            "mark",
            bad_mark,
            BundleError::BadMark { entry: 1, mark: 2 },
            1,
        ),
    ];
    for (name, bytes, error, kept) in cases {
        let path = temp(&format!("disagree-{name}"));
        let dir = Directory::open(&path).unwrap();
        let report = read_all(bytes, || names.iter().map(|n| save(&dir, n)).collect(), &()).await;
        assert_eq!(report.error, Some(error), "{name}");
        for (i, outcome) in report.outcomes.iter().enumerate() {
            if i < kept {
                assert!(matches!(outcome, Outcome::Saved(_)), "{name} {i}");
            } else {
                assert_eq!(*outcome, Outcome::NotReceived, "{name} {i}");
            }
        }
        // What was published stays; nothing else, no partial.
        assert_eq!(files_in(&path), names[..kept], "{name}");
    }

    // An empty bundle with a byte after its table is too long too.
    let mut empty = raw_table(0, &[]);
    empty.push(0);
    let report = read_all(empty, Vec::new, &()).await;
    assert_eq!(report.error, Some(BundleError::TooLong));
}

#[tokio::test]
async fn the_whole_table_is_read_before_the_first_file_so_admission_is_one_batch() {
    let path = temp("admission");
    let dir = Directory::open(&path).unwrap();
    let files: Vec<Vec<u8>> = (0..64).map(|i| data(100 + i, i as u8)).collect();
    let refs: Vec<&[u8]> = files.iter().map(|f| &f[..]).collect();
    let bytes = bundle(&refs).await;
    let header = offsets(&refs)[0] as u64;
    let calls = Calls::default();
    let counted = Counted {
        inner: stream(bytes),
        read: Arc::clone(&calls.read),
    };
    let table = BundleReader::open(counted, &BundleLimits::default())
        .await
        .unwrap();
    // The whole table, and not one byte of a file.
    assert_eq!(calls.read.load(Ordering::SeqCst), header);
    assert_eq!(table.entries().len(), 64);
    let names = names(64);
    let decisions: Vec<Take<'_>> = names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            if i % 3 == 1 {
                Take::Skip
            } else {
                save(&dir, n)
            }
        })
        .collect();
    let report = table
        .read(decisions, &quick(), &calls, std::future::pending())
        .await;
    assert_eq!(report.error, None);
    // Every entry decided at once, every taken partial created and admitted
    // in one call, all before any file's byte was read.
    let taken: Vec<usize> = (0..64).filter(|i| i % 3 != 1).collect();
    assert_eq!(*calls.before_create.lock().unwrap(), vec![taken.clone()]);
    let admitted = calls.admitted.lock().unwrap();
    assert_eq!(admitted.len(), 1);
    assert_eq!(
        admitted[0].iter().map(|(e, _)| *e).collect::<Vec<_>>(),
        taken
    );
    assert_eq!(calls.read_at_admission.load(Ordering::SeqCst), header);
    assert_eq!(*calls.on_disk.lock().unwrap(), Some(true));
    // And the bundle then ended once, every file settled.
    assert_eq!(calls.ended.lock().unwrap().len(), 1);
    assert_eq!(files_in(&path).len(), taken.len());
}

#[tokio::test]
async fn each_bundled_file_carries_its_transfer_key() {
    let path = temp("keys");
    let dir = Directory::open(&path).unwrap();
    let files = [data(10, 1), data(20, 2), data(30, 3)];
    let sent: Vec<TableEntry> = files
        .iter()
        .enumerate()
        .map(|(i, f)| TableEntry {
            key: TransferKey::random().unwrap(),
            meta: vec![i as u8; i * 7],
            size: f.len() as u64,
        })
        .collect();
    let (ours, theirs) = tokio::io::duplex(1 << 10);
    let sending = async {
        let mut writer = BundleWriter::new(
            theirs,
            &sent,
            &BundleLimits::default(),
            WriteOptions::default(),
        )
        .await
        .unwrap();
        for file in &files {
            writer.write(&mut &file[..]).await.unwrap();
        }
        writer.finish().await.unwrap();
    };
    let receiving = async {
        let table = BundleReader::open(ours, &BundleLimits::default())
            .await
            .unwrap();
        assert_eq!(table.entries(), &sent[..]);
        let decisions = (0..3).map(|i| save(&dir, &format!("k{i}"))).collect();
        table
            .read(decisions, &quick(), &(), std::future::pending())
            .await
    };
    let ((), report) = tokio::join!(sending, receiving);
    assert_eq!(report.error, None);
    assert_eq!(files_in(&path), ["k0", "k1", "k2"]);
    // Each key is in the table's bytes, after its entry's size.
    let mut out = Vec::new();
    let _ = BundleWriter::new(
        &mut out,
        &sent,
        &BundleLimits::default(),
        WriteOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(&out[1 + 4 + 4 + 8..][..16], &sent[0].key.0);
}

// Beyond the named tests.

#[tokio::test]
async fn a_cancel_removes_every_partial_not_published() {
    let path = temp("cancel");
    let dir = Directory::open(&path).unwrap();
    let files = [data(10, 1), data(200_000, 2), data(30, 3)];
    let refs: Vec<&[u8]> = files.iter().map(|f| &f[..]).collect();
    let mut bytes = bundle(&refs).await;
    bytes.truncate(offsets(&refs)[1] + 1000);
    // The sender stalls in the second file and never ends the stream.
    let (ours, mut theirs) = tokio::io::duplex(1 << 20);
    theirs.write_all(&bytes).await.unwrap();
    // The cancel comes once the second file's first byte arrived.
    let flag = Arc::new(AtomicBool::new(false));
    let reader = Tripwire {
        inner: ours,
        given: 0,
        at: offsets(&refs)[1] as u64 + 1,
        flag: flag.clone(),
    };
    let table = BundleReader::open(reader, &BundleLimits::default())
        .await
        .unwrap();
    let names = names(3);
    let decisions = names.iter().map(|n| save(&dir, n)).collect();
    let report = table.read(decisions, &quick(), &(), on_flag(&flag)).await;
    assert_eq!(report.error, Some(BundleError::Cancelled));
    assert!(matches!(report.outcomes[0], Outcome::Saved(_)));
    assert_eq!(report.outcomes[1..], vec![Outcome::NotReceived; 2]);
    assert_eq!(files_in(&path), ["f0.bin"]);
    drop(theirs);
}

/// Three files read with `hooks` and `options`, cancelled by their flag
/// (set by the hooks, or once entry `hashed`'s hash has come whole): only
/// the first is published, nothing else is left, and the end is reported once.
async fn cancelled_after_the_first(
    name: &str,
    options: &ReadOptions<'_>,
    hooks: &Trip,
    hashed: Option<usize>,
) {
    let path = temp(name);
    let dir = Directory::open(&path).unwrap();
    let files = [data(10, 1), data(20_000, 2), data(30, 3)];
    let refs: Vec<&[u8]> = files.iter().map(|f| &f[..]).collect();
    let reader = Tripwire {
        inner: stream(bundle(&refs).await),
        given: 0,
        at: hashed.map_or(u64::MAX, |n| {
            (offsets(&refs)[n] + refs[n].len() + 33) as u64
        }),
        flag: hooks.flag.clone(),
    };
    let table = BundleReader::open(reader, &BundleLimits::default())
        .await
        .unwrap();
    let names = names(3);
    let decisions = names.iter().map(|n| save(&dir, n)).collect();
    let report = table
        .read(decisions, options, hooks, on_flag(&hooks.flag))
        .await;
    assert_eq!(report.error, Some(BundleError::Cancelled));
    assert!(
        matches!(report.outcomes[0], Outcome::Saved(_)),
        "{report:?}"
    );
    assert_eq!(report.outcomes[1..], vec![Outcome::NotReceived; 2]);
    assert_eq!(files_in(&path), ["f0.bin"]);
    assert_eq!(hooks.ended.load(Ordering::SeqCst), 1);
    drop(dir);
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn a_cancel_while_publishing_ends_the_bundle_with_the_file_unnamed() {
    let hooks = Trip::new(Some(1));
    cancelled_after_the_first("cancel-publish", &quick(), &hooks, None).await;
    assert_eq!(*hooks.published.lock().unwrap(), [0, 1]);
}

#[tokio::test]
async fn a_cancel_while_flushing_ends_the_bundle_with_the_file_unnamed() {
    let options = ReadOptions {
        sync: true,
        ..ReadOptions::default()
    };
    let hooks = Trip::new(None);
    // Set once the second file's hash has come whole: the flush is next.
    cancelled_after_the_first("cancel-flush", &options, &hooks, Some(1)).await;
    // Cancelled in the flush: the second file never reached the hook.
    assert_eq!(*hooks.published.lock().unwrap(), [0]);
}

#[tokio::test]
async fn a_hook_that_refuses_admission_leaves_nothing_on_the_disk() {
    struct NoRoom;
    impl BundleHooks for NoRoom {
        fn admitted(&self, _: &[Partial<'_>]) -> io::Result<()> {
            Err(io::ErrorKind::StorageFull.into())
        }
    }
    let path = temp("no-room");
    let dir = Directory::open(&path).unwrap();
    let files = [data(10, 1), data(20, 2)];
    let refs: Vec<&[u8]> = files.iter().map(|f| &f[..]).collect();
    let report = read_all(
        bundle(&refs).await,
        || vec![save(&dir, "a"), save(&dir, "b")],
        &NoRoom,
    )
    .await;
    assert_eq!(
        report.error,
        Some(BundleError::Hook(io::ErrorKind::StorageFull))
    );
    assert_eq!(report.outcomes, vec![Outcome::NotReceived; 2]);
    assert!(files_in(&path).is_empty());

    // As many decisions as entries, or none taken at all.
    let report = read_all(bundle(&refs).await, || vec![Take::Skip], &()).await;
    assert_eq!(
        report.error,
        Some(BundleError::Decisions { got: 1, entries: 2 })
    );
    assert!(files_in(&path).is_empty());
}

#[tokio::test]
async fn the_writer_and_the_reader_pace_every_slice_and_count_it() {
    struct Count(AtomicUsize, AtomicU64);
    impl safecopy_chunks::Pace for Count {
        fn before_write(
            &self,
            len: usize,
        ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            self.1.fetch_add(len as u64, Ordering::SeqCst);
            Box::pin(std::future::ready(()))
        }
    }
    let path = temp("pace");
    let dir = Directory::open(&path).unwrap();
    let file = data(200_000, 1);
    let sender = Count(AtomicUsize::new(0), AtomicU64::new(0));
    let mut out = Vec::new();
    let mut writer = BundleWriter::new(
        &mut out,
        &entries(&[&file, &file]),
        &BundleLimits::default(),
        WriteOptions {
            progress: &(),
            pace: &sender,
        },
    )
    .await
    .unwrap();
    writer.write(&mut &file[..]).await.unwrap();
    writer.give_up().await.unwrap();
    writer.finish().await.unwrap();
    // Slices of at most 64 KiB, padding included.
    assert_eq!(sender.0.load(Ordering::SeqCst), 8);
    assert_eq!(sender.1.load(Ordering::SeqCst), 400_000);

    let receiver = Count(AtomicUsize::new(0), AtomicU64::new(0));
    let table = BundleReader::open(stream(out), &BundleLimits::default())
        .await
        .unwrap();
    let options = ReadOptions {
        pace: &receiver,
        ..quick()
    };
    let report = table
        .read(
            vec![save(&dir, "a"), save(&dir, "b")],
            &options,
            &(),
            std::future::pending(),
        )
        .await;
    assert_eq!(report.outcomes[1], Outcome::GivenUp);
    assert!(receiver.0.load(Ordering::SeqCst) >= 4);
    assert_eq!(receiver.1.load(Ordering::SeqCst), 400_000);
}

#[tokio::test]
async fn a_third_bundle_waits_until_one_of_two_ends() {
    let gate = Gate::new(&BundleLimits::default());
    let first = gate.enter().await;
    let _second = gate.enter().await;
    assert!(gate.try_enter().is_none());
    let waiting = tokio::spawn({
        let gate = gate.clone();
        async move { gate.enter().await }
    });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    drop(first);
    let _third = waiting.await.unwrap();
    assert!(gate.try_enter().is_none());
}

#[tokio::test]
async fn a_hook_refusing_one_publish_or_one_partial_that_cannot_be_made_fails_only_that_file() {
    struct NotTheSecond(Mutex<Vec<usize>>);
    impl BundleHooks for NotTheSecond {
        fn admitted(&self, partials: &[Partial<'_>]) -> io::Result<()> {
            *self.0.lock().unwrap() = partials.iter().map(|p| p.entry).collect();
            Ok(())
        }
        fn before_publish(&self, entry: usize, _: &File, _: &OsStr) -> io::Result<()> {
            if entry == 1 {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            Ok(())
        }
    }
    let path = temp("one-fails");
    let dir = Directory::open(&path).unwrap();
    let files = [data(10, 1), data(20, 2), data(30, 3), data(40, 4)];
    let refs: Vec<&[u8]> = files.iter().map(|f| &f[..]).collect();
    let hooks = NotTheSecond(Mutex::new(Vec::new()));
    let report = read_all(
        bundle(&refs).await,
        // A name with a separator in it never gets a partial file.
        || {
            vec![
                save(&dir, "a"),
                save(&dir, "b"),
                save(&dir, "c/d"),
                save(&dir, "e"),
            ]
        },
        &hooks,
    )
    .await;
    assert_eq!(report.error, None);
    assert!(matches!(report.outcomes[0], Outcome::Saved(_)));
    assert_eq!(
        report.outcomes[1],
        Outcome::Failed(io::ErrorKind::PermissionDenied)
    );
    assert_eq!(
        report.outcomes[2],
        Outcome::Folder(Refusal::LeavesFolder),
        "the folder crate's refusal, as the refusal it was"
    );
    assert!(matches!(report.outcomes[3], Outcome::Saved(_)));
    assert_eq!(*hooks.0.lock().unwrap(), vec![0, 1, 3]);
    assert_eq!(files_in(&path), ["a", "e"]);

    // An entry whose partial could not be made stays `Failed` when the
    // bundle is cut before it.
    let mut cut = bundle(&refs).await;
    cut.truncate(offsets(&refs)[1] + 5);
    let path = temp("one-fails-cut");
    let dir = Directory::open(&path).unwrap();
    let report = read_all(
        cut,
        || {
            vec![
                save(&dir, "a"),
                save(&dir, "b"),
                save(&dir, "c/d"),
                save(&dir, "e"),
            ]
        },
        &(),
    )
    .await;
    assert_eq!(report.error, Some(BundleError::CutShort { entry: 1 }));
    assert_eq!(report.outcomes[1], Outcome::NotReceived);
    assert_eq!(report.outcomes[2], Outcome::Folder(Refusal::LeavesFolder));
    assert_eq!(report.outcomes[3], Outcome::NotReceived);
    assert_eq!(files_in(&path), ["a"]);
}

/// One entry refused by the folder crate when its partial is created and
/// one when it is published: each says its refusal, and the rest are saved.
#[tokio::test]
async fn an_entry_the_folder_crate_refuses_says_the_refusal_and_the_others_are_saved() {
    let path = temp("refused");
    let dir = Directory::open(&path).unwrap();
    let files = [data(10, 1), data(20, 2), data(30, 3), data(40, 4)];
    let refs: Vec<&[u8]> = files.iter().map(|f| &f[..]).collect();
    crate::seams::PUBLISH_REFUSAL.set(Some((2, Refusal::Unproven)));
    let report = read_all(
        bundle(&refs).await,
        || {
            vec![
                save(&dir, "a"),
                save(&dir, "no/b"),
                save(&dir, "c"),
                save(&dir, "d"),
            ]
        },
        &(),
    )
    .await;
    crate::seams::PUBLISH_REFUSAL.set(None);
    assert_eq!(report.error, None);
    assert_eq!(saved_path(&report.outcomes[0]), path.join("a"));
    assert_eq!(report.outcomes[1], Outcome::Folder(Refusal::LeavesFolder));
    assert_eq!(report.outcomes[2], Outcome::Folder(Refusal::Unproven));
    assert_eq!(saved_path(&report.outcomes[3]), path.join("d"));
    // The refused entry's partial is removed, and the bundle read on.
    assert_eq!(files_in(&path), ["a", "d"]);
    assert_eq!(std::fs::read(path.join("d")).unwrap(), files[3]);
}

#[test]
fn an_error_of_the_system_keeps_its_kind_and_a_refusal_its_own() {
    let denied = || io::Error::from(io::ErrorKind::PermissionDenied);
    assert_eq!(
        Outcome::failed(&denied()),
        Outcome::Failed(io::ErrorKind::PermissionDenied)
    );
    assert_eq!(
        BundleError::from(denied()),
        BundleError::Io(io::ErrorKind::PermissionDenied)
    );
    // `Other` with no refusal in it stays the system's.
    assert_eq!(
        Outcome::failed(&io::Error::other("not a refusal")),
        Outcome::Failed(io::ErrorKind::Other)
    );
    for why in [
        Refusal::Link,
        Refusal::NotFileOrFolder,
        Refusal::NotFolder,
        Refusal::LeavesFolder,
        Refusal::Stream,
        Refusal::Nul,
        Refusal::TooLong,
        Refusal::BadListing,
        Refusal::NoVolume,
        Refusal::Replaced,
        Refusal::NoNameLeft { tried: 3 },
        Refusal::Unproven,
        Refusal::OtherFolder,
    ] {
        let refused = || io::Error::other(why);
        assert_eq!(Outcome::failed(&refused()), Outcome::Folder(why));
        assert_eq!(BundleError::from(refused()), BundleError::Folder(why));
        assert_eq!(
            crate::writer::chunk_error(safecopy_chunks::ChunkError::from(refused())),
            BundleError::Folder(why)
        );
    }
}

#[tokio::test]
async fn a_hook_refusing_before_create_leaves_nothing_and_admits_nothing() {
    struct Full(AtomicUsize);
    impl BundleHooks for Full {
        fn before_create(&self, _: &[usize]) -> io::Result<()> {
            Err(io::ErrorKind::StorageFull.into())
        }
        fn admitted(&self, _: &[Partial<'_>]) -> io::Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    let path = temp("before-create");
    let dir = Directory::open(&path).unwrap();
    let files = [data(10, 1), data(20, 2)];
    let refs: Vec<&[u8]> = files.iter().map(|f| &f[..]).collect();
    let hooks = Full(AtomicUsize::new(0));
    let report = read_all(
        bundle(&refs).await,
        || vec![save(&dir, "a"), Take::Skip],
        &hooks,
    )
    .await;
    assert_eq!(
        report.error,
        Some(BundleError::Hook(io::ErrorKind::StorageFull))
    );
    assert_eq!(
        report.outcomes,
        vec![Outcome::NotReceived, Outcome::Refused]
    );
    assert_eq!(hooks.0.load(Ordering::SeqCst), 0);
    assert!(files_in(&path).is_empty());
}

#[tokio::test]
async fn save_unless_same_with_damaged_bytes_is_damaged_not_already_there() {
    let file = data(500, 1);
    let path = temp("unless-damaged");
    let dir = Directory::open(&path).unwrap();
    let mut bytes = bundle(&[&file]).await;
    // The bytes change on the way; the trailer is still the hash of the
    // file, which is the candidate's.
    bytes[offsets(&[&file])[0] + 10] ^= 1;
    let report = read_all(
        bytes,
        || {
            vec![Take::SaveUnlessSame(
                Target {
                    dir: &dir,
                    name: "a".into(),
                    meta: Meta::default(),
                },
                sha(&file),
            )]
        },
        &(),
    )
    .await;
    assert_eq!(report.outcomes, vec![Outcome::Damaged]);
    assert!(files_in(&path).is_empty());
}

/// A pace that never lets a slice through: a pause that never lifts.
struct Paused;

impl safecopy_chunks::Pace for Paused {
    fn before_write(&self, _: usize) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn a_cancel_ends_a_paused_bundle_and_dropping_the_read_removes_its_partials() {
    let files = [data(10, 1), data(20, 2)];
    let refs: Vec<&[u8]> = files.iter().map(|f| &f[..]).collect();
    let options = ReadOptions {
        pace: &Paused,
        ..quick()
    };

    // Cancelled while the pace holds the first write.
    let path = temp("paused-cancel");
    let dir = Directory::open(&path).unwrap();
    let calls = Calls::default();
    let table = BundleReader::open(stream(bundle(&refs).await), &BundleLimits::default())
        .await
        .unwrap();
    let report = table
        .read(
            vec![save(&dir, "a"), save(&dir, "b")],
            &options,
            &calls,
            tokio::time::sleep(std::time::Duration::from_millis(100)),
        )
        .await;
    assert_eq!(report.error, Some(BundleError::Cancelled));
    assert_eq!(report.outcomes, vec![Outcome::NotReceived; 2]);
    assert!(files_in(&path).is_empty());
    assert_eq!(calls.ended.lock().unwrap().len(), 1);

    // The read dropped by the caller instead: its partials go all the same.
    let path = temp("paused-dropped");
    let dir = Directory::open(&path).unwrap();
    let table = BundleReader::open(stream(bundle(&refs).await), &BundleLimits::default())
        .await
        .unwrap();
    let reading = table.read(
        vec![save(&dir, "a"), save(&dir, "b")],
        &options,
        &(),
        std::future::pending(),
    );
    let timed_out = tokio::time::timeout(std::time::Duration::from_millis(100), reading).await;
    assert!(timed_out.is_err());
    assert!(files_in(&path).is_empty());
}

/// A stream that takes `room` bytes and then fails.
struct Breaks(usize);

impl tokio::io::AsyncWrite for Breaks {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.0 == 0 {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        let n = self.0.min(buf.len());
        self.0 -= n;
        Poll::Ready(Ok(n))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn a_writer_torn_in_an_entry_writes_nothing_more() {
    let file = data(100_000, 1);
    let list = entries(&[&file, &file]);
    let table = offsets(&[&file, &file])[0];
    let mut writer = BundleWriter::new(
        Breaks(table + 70_000),
        &list,
        &BundleLimits::default(),
        WriteOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        writer.write(&mut &file[..]).await,
        Err(BundleError::Io(io::ErrorKind::BrokenPipe))
    );
    assert_eq!(writer.give_up().await, Err(BundleError::Torn));
    assert_eq!(writer.write(&mut &file[..]).await, Err(BundleError::Torn));
    assert!(matches!(writer.finish().await, Err(BundleError::Torn)));
}

#[tokio::test]
async fn loose_limits_never_overflow_or_allocate_past_the_table() {
    let loose = BundleLimits {
        small: u64::MAX,
        files: u32::MAX,
        bytes: u64::MAX,
        ..BundleLimits::default()
    };
    // plan sums sizes without overflow: a sum past `u64::MAX` starts a
    // new bundle.
    assert_eq!(
        plan(&[u64::MAX, 1, u64::MAX], &loose),
        vec![
            Group::Bundle(vec![0]),
            Group::Bundle(vec![1]),
            Group::Bundle(vec![2]),
        ]
    );
    // A four-byte table that claims four billion entries is refused, not
    // allocated for.
    let huge = raw_table(u32::MAX, &[]);
    assert_eq!(
        BundleReader::open(&huge[..], &loose).await.unwrap_err(),
        BundleError::CountMismatch
    );
}

#[tokio::test]
async fn a_byte_past_the_end_after_a_skipped_last_entry_is_too_long() {
    let path = temp("skip-last-long");
    let dir = Directory::open(&path).unwrap();
    let files = [data(10, 1), data(20, 2)];
    let refs: Vec<&[u8]> = files.iter().map(|f| &f[..]).collect();
    let mut bytes = bundle(&refs).await;
    bytes.push(7);
    let report = read_all(bytes, || vec![save(&dir, "a"), Take::Skip], &()).await;
    assert_eq!(report.error, Some(BundleError::TooLong));
    assert!(matches!(report.outcomes[0], Outcome::Saved(_)));
    assert_eq!(report.outcomes[1], Outcome::Refused);
}

// A write still on the blocking pool when a read ends.

/// Lets a held write go when the test ends, however it ends.
struct Release(Arc<Mutex<Option<std::sync::mpsc::Sender<()>>>>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.lock().unwrap().take();
    }
}

/// Hears every step of this thread's writes, and holds each write at
/// `held` until the test lets go, or until its reader reaches `until`.
fn hold_writes(
    held: crate::seams::Step,
    until: Option<crate::seams::Step>,
) -> (std::sync::mpsc::Receiver<crate::seams::Step>, Release) {
    use std::sync::mpsc;
    let (heard, steps) = mpsc::channel();
    let (release, go) = mpsc::channel::<()>();
    let go = Mutex::new(go);
    let release = Arc::new(Mutex::new(Some(release)));
    let guard = Release(release.clone());
    crate::seams::WRITES.set(Some(Arc::new(move |step| {
        let _ = heard.send(step);
        if step == held {
            let _ = go.lock().unwrap().recv();
        } else if Some(step) == until {
            drop(release.lock().unwrap().take());
        }
    })));
    (steps, guard)
}

/// A cut comes while the last write is still on the blocking pool, where
/// it holds the partial file open. The read waits for it: when it
/// returns, nothing of it is still to be written and the partial is gone.
#[tokio::test]
async fn a_cut_bundle_returns_with_its_last_write_done_and_no_partial_left() {
    use crate::seams::Step;
    let path = temp("cut-writing");
    let dir = Directory::open(&path).unwrap();
    let file = data(1000, 1);
    let mut bytes = bundle(&[&file[..]]).await;
    bytes.truncate(offsets(&[&file[..]])[0] + 500);
    // Read from memory: the half file comes in one read, so in one write.
    let table = BundleReader::open(&bytes[..], &BundleLimits::default())
        .await
        .unwrap();
    let (steps, _release) = hold_writes(Step::Writing, Some(Step::Settling));
    let report = table
        .read(
            vec![save(&dir, "a.bin")],
            &quick(),
            &(),
            std::future::pending(),
        )
        .await;
    // Read at once, with no wait for the pool.
    assert_eq!(files_in(&path), [""; 0]);
    let heard: Vec<Step> = steps.try_iter().collect();
    assert_eq!(report.error, Some(BundleError::CutShort { entry: 0 }));
    assert_eq!(report.outcomes, [Outcome::NotReceived]);
    let at = |step| {
        let found = heard.iter().position(|s| *s == step);
        found.unwrap_or_else(|| panic!("no {step:?} in {heard:?}"))
    };
    // The write was held until the read waited for it, and ended first.
    assert!(at(Step::Writing) < at(Step::Ended), "{heard:?}");
    assert!(at(Step::Settling) < at(Step::Ended), "{heard:?}");
    assert_eq!(heard.last(), Some(&Step::Closing), "{heard:?}");
}

/// A read of `a.bin` whose stream stays open after half the file, polled
/// once: that half is handed to the blocking pool, and the read waits
/// for more.
macro_rules! half_a_file_in {
    ($path:expr, $dir:ident, $theirs:ident, $reading:ident) => {
        let $dir = Directory::open($path).unwrap();
        let file = data(1000, 1);
        let bytes = bundle(&[&file[..]]).await;
        let (ours, mut $theirs) = tokio::io::duplex(64 << 10);
        $theirs
            .write_all(&bytes[..offsets(&[&file[..]])[0] + 500])
            .await
            .unwrap();
        let table = BundleReader::open(ours, &BundleLimits::default())
            .await
            .unwrap();
        let options = quick();
        let mut $reading = Box::pin(table.read(
            vec![save(&$dir, "a.bin")],
            &options,
            &(),
            std::future::pending(),
        ));
        let polled = std::future::poll_fn(|cx| Poll::Ready($reading.as_mut().poll(cx))).await;
        assert!(polled.is_pending(), "its stream is still open");
    };
}

/// The same when the read is dropped instead: the drop waits for the
/// write under way.
#[tokio::test]
async fn a_read_dropped_with_a_write_under_way_leaves_no_partial_behind() {
    use crate::seams::Step;
    let path = temp("dropped-writing");
    let (steps, _release) = hold_writes(Step::Writing, Some(Step::Closing));
    half_a_file_in!(&path, dir, theirs, reading);
    assert_eq!(steps.recv().unwrap(), Step::Queued);
    assert_eq!(steps.recv().unwrap(), Step::Writing);
    assert_eq!(files_in(&path), ["a.bin.part"]);
    drop(reading);
    // Read at once, with no wait for the pool: the write had ended.
    let heard: Vec<Step> = steps.try_iter().collect();
    assert_eq!(heard, [Step::Closing, Step::Ended]);
    assert_eq!(files_in(&path), [""; 0]);
    drop(theirs);
}

/// A write that had not started when its read was dropped writes
/// nothing, however late it runs, and holds no file open meanwhile.
#[tokio::test]
async fn a_write_still_waiting_when_its_read_is_dropped_writes_nothing() {
    use crate::seams::Step;
    let path = temp("dropped-waiting");
    let (steps, release) = hold_writes(Step::Queued, None);
    half_a_file_in!(&path, dir, theirs, reading);
    assert_eq!(steps.recv().unwrap(), Step::Queued);
    drop(reading);
    assert_eq!(steps.recv().unwrap(), Step::Closing);
    // The write is still held on the pool, and the partial is gone.
    assert_eq!(files_in(&path), [""; 0]);
    drop(release);
    // No `Writing` came before it: the gate was shut.
    assert_eq!(steps.recv().unwrap(), Step::Ended, "it never wrote");
    assert!(steps.try_recv().is_err());
    drop(theirs);
}

/// A write that fails on the blocking pool is heard of with the next
/// write, or with the end of the file's bytes: the file is `Failed`, its
/// partial removed, and the bundle reads on.
#[tokio::test]
async fn a_write_that_fails_on_the_pool_fails_only_its_file() {
    const SLICE: usize = safecopy_chunks::SLICE;
    // In the second slice of three, and in the last.
    for (case, fail_at) in [SLICE as u64 + 1, 2 * SLICE as u64 + 1]
        .into_iter()
        .enumerate()
    {
        let path = temp(&format!("failed-write-{case}"));
        let dir = Directory::open(&path).unwrap();
        let files = [data(2 * SLICE + 5, 1), data(40, 2)];
        let refs: Vec<&[u8]> = files.iter().map(|f| &f[..]).collect();
        let bytes = bundle(&refs).await;
        let table = BundleReader::open(&bytes[..], &BundleLimits::default())
            .await
            .unwrap();
        crate::seams::WRITE_ERROR_AT.set(Some(fail_at));
        let report = table
            .read(
                vec![save(&dir, "a"), save(&dir, "b")],
                &quick(),
                &(),
                std::future::pending(),
            )
            .await;
        assert_eq!(report.error, None);
        assert_eq!(
            report.outcomes[0],
            Outcome::Failed(io::ErrorKind::StorageFull)
        );
        assert!(matches!(report.outcomes[1], Outcome::Saved(_)));
        assert_eq!(files_in(&path), ["b"]);
    }
}

/// The handle a hook is given shares its position with every clone of
/// it: the writer goes by its own offsets, not by that position.
#[tokio::test]
async fn a_hook_that_moves_a_partial_files_position_moves_no_byte() {
    use std::io::{Seek, SeekFrom};
    struct Moves;
    impl BundleHooks for Moves {
        fn admitted(&self, partials: &[Partial<'_>]) -> io::Result<()> {
            for partial in partials {
                let mut file = partial.file;
                file.seek(SeekFrom::Start(7))?;
            }
            Ok(())
        }
    }
    let path = temp("moved-position");
    let dir = Directory::open(&path).unwrap();
    let files = [data(100, 1), data(3 * safecopy_chunks::SLICE + 5, 2)];
    let refs: Vec<&[u8]> = files.iter().map(|f| &f[..]).collect();
    let report = read_all(
        bundle(&refs).await,
        || vec![save(&dir, "a"), save(&dir, "b")],
        &Moves,
    )
    .await;
    assert_eq!(report.error, None);
    assert!(std::fs::read(path.join("a")).unwrap() == files[0]);
    assert!(std::fs::read(path.join("b")).unwrap() == files[1]);
    assert_eq!(files_in(&path), ["a", "b"]);
}

// The search for a free partial name.

/// A bundle of two small files read into `dir` as `first` and `second`.
async fn read_two(
    dir: &Directory,
    [first, second]: [&str; 2],
    options: &ReadOptions<'_>,
    hooks: &dyn BundleHooks,
    cancel: impl std::future::Future<Output = ()>,
) -> Report {
    let files = [data(10, 1), data(20, 2)];
    let refs: Vec<&[u8]> = files.iter().map(|f| &f[..]).collect();
    let table = BundleReader::open(stream(bundle(&refs).await), &BundleLimits::default())
        .await
        .unwrap();
    let decisions = vec![save(dir, first), save(dir, second)];
    table.read(decisions, options, hooks, cancel).await
}

#[test]
fn a_partial_name_is_tried_as_far_as_a_name_is_numbered_by_default() {
    assert_eq!(ReadOptions::default().partial_tries, 10_001);
    assert_eq!(
        ReadOptions::default().partial_tries,
        safecopy_folder::NAME_TRIES
    );
}

/// An entry whose partial names are all taken up to the limit gets none
/// and says how many were tried; the others are saved. With a higher
/// limit it takes the next name.
#[tokio::test]
async fn an_entry_tries_only_as_many_partial_names_as_the_options_say() {
    let path = temp("partial-limit");
    std::fs::write(path.join("a.part"), b"another's").unwrap();
    std::fs::write(path.join("a.2.part"), b"another's").unwrap();
    let dir = Directory::open(&path).unwrap();
    for tries in [0, 1, 2] {
        let options = ReadOptions {
            partial_tries: tries,
            ..quick()
        };
        let hooks = Calls::default();
        let report = read_two(&dir, ["a", "b"], &options, &hooks, std::future::pending()).await;
        assert_eq!(report.error, None);
        assert_eq!(report.outcomes[0], Outcome::NoPartialName { tried: tries });
        if tries == 0 {
            // Not one name is tried, for the entry whose name is free either.
            assert_eq!(report.outcomes[1], Outcome::NoPartialName { tried: 0 });
            assert_eq!(*hooks.admitted.lock().unwrap(), [vec![]]);
            assert_eq!(files_in(&path), ["a.2.part", "a.part"]);
        } else {
            assert!(matches!(report.outcomes[1], Outcome::Saved(_)));
            assert_eq!(
                *hooks.admitted.lock().unwrap(),
                [vec![(1, "b.part".to_string())]]
            );
            assert_eq!(files_in(&path), ["a.2.part", "a.part", "b"]);
            std::fs::remove_file(path.join("b")).unwrap();
        }
        assert_eq!(hooks.ended.lock().unwrap().len(), 1);
    }
    let options = ReadOptions {
        partial_tries: 3,
        ..quick()
    };
    let hooks = Calls::default();
    let report = read_two(&dir, ["a", "b"], &options, &hooks, std::future::pending()).await;
    assert_eq!(report.error, None);
    assert!(matches!(report.outcomes[0], Outcome::Saved(_)));
    assert_eq!(
        *hooks.admitted.lock().unwrap(),
        [vec![(0, "a.3.part".to_string()), (1, "b.part".to_string())]]
    );
    assert_eq!(files_in(&path), ["a", "a.2.part", "a.part", "b"]);
    drop(dir);
    std::fs::remove_dir_all(path).unwrap();
}

/// A cancel given while an entry's partial names are taken ends the
/// bundle before the next name is tried and before admission: the partial
/// made for an earlier entry is removed, and none is made after.
#[tokio::test]
async fn a_cancel_between_partial_names_ends_the_bundle_with_nothing_created() {
    let path = temp("cancel-names");
    // The second name of `a` is free: only the cancel keeps the read from it.
    std::fs::write(path.join("a.part"), b"another's").unwrap();
    let dir = Directory::open(&path).unwrap();
    let flag = Arc::new(AtomicBool::new(false));
    // Given right after the first name of `a` was found taken, with the
    // first entry's partial on the disk.
    crate::seams::NAME_TAKEN.set(Some(Box::new({
        let (flag, path) = (flag.clone(), path.clone());
        move |taken| {
            assert_eq!(taken, 1);
            assert_eq!(files_in(&path), ["a.part", "b.part"]);
            flag.store(true, Ordering::SeqCst);
        }
    })));
    let hooks = Calls::default();
    let report = read_two(&dir, ["b", "a"], &quick(), &hooks, on_flag(&flag)).await;
    crate::seams::NAME_TAKEN.set(None);
    assert!(flag.load(Ordering::SeqCst));
    assert_eq!(report.error, Some(BundleError::Cancelled));
    assert_eq!(report.outcomes, vec![Outcome::NotReceived; 2]);
    assert_eq!(files_in(&path), ["a.part"]);
    assert_eq!(hooks.before_create.lock().unwrap().len(), 1);
    assert!(hooks.admitted.lock().unwrap().is_empty());
    assert_eq!(*hooks.ended.lock().unwrap(), [report]);
    drop(dir);
    std::fs::remove_dir_all(path).unwrap();
}

/// A read dropped while an entry's partial names are taken leaves no
/// partial of an earlier entry behind, and has admitted nothing.
#[tokio::test]
async fn a_read_dropped_between_partial_names_removes_the_partials_it_made() {
    let path = temp("drop-names");
    std::fs::write(path.join("a.part"), b"another's").unwrap();
    let dir = Directory::open(&path).unwrap();
    let flag = Arc::new(AtomicBool::new(false));
    crate::seams::NAME_TAKEN.set(Some(Box::new({
        let (flag, path) = (flag.clone(), path.clone());
        move |_| {
            assert_eq!(files_in(&path), ["a.part", "b.part"]);
            flag.store(true, Ordering::SeqCst);
        }
    })));
    let hooks = Calls::default();
    let options = quick();
    // The read is polled until it waits between names, then dropped.
    tokio::select! {
        biased;
        () = on_flag(&flag) => {}
        report = read_two(&dir, ["b", "a"], &options, &hooks, std::future::pending()) => {
            panic!("{report:?}")
        }
    }
    crate::seams::NAME_TAKEN.set(None);
    assert_eq!(files_in(&path), ["a.part"]);
    assert!(hooks.admitted.lock().unwrap().is_empty());
    assert!(hooks.ended.lock().unwrap().is_empty());
    drop(dir);
    std::fs::remove_dir_all(path).unwrap();
}

/// The read lets the runtime's other tasks run between taken names: a
/// cancel one of them gives on the read's own thread is seen there.
#[tokio::test(flavor = "current_thread")]
async fn a_cancel_given_by_another_task_is_seen_between_partial_names() {
    let path = temp("cancel-names-task");
    std::fs::write(path.join("a.part"), b"another's").unwrap();
    let dir = Directory::open(&path).unwrap();
    let files = [data(10, 1)];
    let refs: Vec<&[u8]> = files.iter().map(|f| &f[..]).collect();
    // The whole bundle is at hand: no task of the test's is needed to read it.
    let bytes = bundle(&refs).await;
    let table = BundleReader::open(&bytes[..], &BundleLimits::default())
        .await
        .unwrap();
    let flag = Arc::new(AtomicBool::new(false));
    // Runs only once the read lets go of the thread.
    let cancelling = tokio::spawn({
        let flag = flag.clone();
        async move { flag.store(true, Ordering::SeqCst) }
    });
    let hooks = Calls::default();
    let report = table
        .read(vec![save(&dir, "a")], &quick(), &hooks, on_flag(&flag))
        .await;
    assert_eq!(report.error, Some(BundleError::Cancelled));
    assert_eq!(report.outcomes, [Outcome::NotReceived]);
    assert_eq!(files_in(&path), ["a.part"]);
    // Seen in the search itself: no second name was made and admitted.
    assert!(hooks.admitted.lock().unwrap().is_empty());
    cancelling.await.unwrap();
    drop(dir);
    std::fs::remove_dir_all(path).unwrap();
}

/// A hook's "already exists" is the hook's refusal, never a taken name:
/// the bundle ends at once with it and no second partial name is tried.
#[tokio::test]
async fn a_hook_that_says_already_exists_ends_the_bundle_and_is_no_taken_name() {
    struct Exists(&'static str, Mutex<Vec<String>>);
    impl BundleHooks for Exists {
        fn before_create(&self, _: &[usize]) -> io::Result<()> {
            if self.0 == "before_create" {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            Ok(())
        }
        fn admitted(&self, partials: &[Partial<'_>]) -> io::Result<()> {
            let names = partials.iter().map(|p| p.name.to_string_lossy().into());
            self.1.lock().unwrap().extend(names);
            Err(io::ErrorKind::AlreadyExists.into())
        }
    }
    for (at, admitted) in [
        ("before_create", vec![]),
        ("admitted", vec!["a.part", "b.part"]),
    ] {
        let path = temp("hook-exists");
        let dir = Directory::open(&path).unwrap();
        let hooks = Exists(at, Mutex::default());
        let report = read_two(&dir, ["a", "b"], &quick(), &hooks, std::future::pending()).await;
        assert_eq!(
            report.error,
            Some(BundleError::Hook(io::ErrorKind::AlreadyExists)),
            "{at}"
        );
        assert_eq!(report.outcomes, vec![Outcome::NotReceived; 2], "{at}");
        assert_eq!(*hooks.1.lock().unwrap(), admitted, "{at}");
        assert!(files_in(&path).is_empty(), "{at}");
        drop(dir);
        std::fs::remove_dir_all(path).unwrap();
    }
}

/// A checked entry for which every name up to the publish's limit is taken
/// gets none and says how many were tried: its partial is removed, and
/// the bundle reads on.
#[tokio::test]
async fn an_entry_whose_names_are_all_taken_says_how_many_it_tried() {
    let path = temp("name-limit");
    std::fs::write(path.join("a"), b"theirs").unwrap();
    std::fs::write(path.join("a (2)"), b"theirs").unwrap();
    let dir = Directory::open(&path).unwrap();
    let rules = safecopy_folder::PublishRules {
        name_tries: 2,
        ..Default::default()
    };
    let options = ReadOptions {
        publish: &rules,
        ..quick()
    };
    let report = read_two(&dir, ["a", "b"], &options, &(), std::future::pending()).await;
    assert_eq!(report.error, None);
    assert_eq!(report.outcomes[0], Outcome::NoNameLeft { tried: 2 });
    assert!(matches!(report.outcomes[1], Outcome::Saved(_)));
    assert_eq!(files_in(&path), ["a", "a (2)", "b"]);
    drop(dir);
    std::fs::remove_dir_all(path).unwrap();
}
