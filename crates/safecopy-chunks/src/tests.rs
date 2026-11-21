use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use safecopy_folder::{
    BitApplied, Directory, Marked, Meta, MetaRules, PublishRules, Time, TimeApplied,
};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::Semaphore;

use super::*;

/// A new, empty folder for one test, under the build's target folder.
fn temp(name: &str) -> PathBuf {
    // The test binary is in target/<profile>/deps.
    let exe = std::env::current_exe().unwrap();
    let target = exe.ancestors().nth(3).unwrap();
    let dir = target
        .join("test-tmp")
        .join(format!("chunks-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn sha(bytes: &[u8]) -> Digest {
    let mut d = [0; 32];
    d.copy_from_slice(ring::digest::digest(&SHA256, bytes).as_ref());
    Digest(d)
}

/// `len` bytes that differ from chunk to chunk.
fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 + i / 997) as u8).collect()
}

fn small(chunk: u64) -> NonZeroU64 {
    NonZeroU64::new(chunk).unwrap()
}

/// A stream as a sender writes it: the bytes, then the trailer.
async fn framed(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut hasher = Settings::default().sending();
    copy_exact(
        &mut &bytes[..],
        &mut out,
        bytes.len() as u64,
        0,
        &mut hasher,
        &(),
        &(),
    )
    .await
    .unwrap();
    write_trailer(&mut out, &hasher.finish()).await.unwrap();
    out
}

#[test]
fn the_framing_constants_and_defaults_are_the_designed_ones() {
    assert_eq!(CHUNK, 4 * 1024 * 1024);
    assert_eq!(SLICE, 64 * 1024);
    assert_eq!(FRAMING, 1);
    let settings = Settings::default();
    assert_eq!(settings.receiver_keep, 1);
    assert_eq!(settings.sender_keep, 4);
    assert_eq!(settings.meta_bytes, 4096);
    let options = SaveOptions::default();
    assert_eq!(*options.publish, PublishRules::default());
    assert_eq!(options.meta_rules, MetaRules::default());
    assert!(options.sync);
}

// The pace hook.

/// A writer that counts what reached it.
struct Counting(Arc<AtomicU64>);

impl AsyncWrite for Counting {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0.fetch_add(buf.len() as u64, Ordering::SeqCst);
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// A pace that lets one slice through per permit, and notes each call
/// with what was written when it came.
struct Gate {
    permits: Semaphore,
    written: Arc<AtomicU64>,
    calls: Mutex<Vec<(usize, u64)>>,
}

impl Pace for Gate {
    fn before_write(&self, len: usize) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let written = self.written.load(Ordering::SeqCst);
        self.calls.lock().unwrap().push((len, written));
        Box::pin(async move {
            self.permits.acquire().await.unwrap().forget();
        })
    }
}

async fn settle() {
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn pace_is_awaited_before_every_slice_and_holds_the_copy_while_it_waits() {
    let bytes = data(3 * SLICE + 100);
    let written = Arc::new(AtomicU64::new(0));
    let gate = Arc::new(Gate {
        permits: Semaphore::new(0),
        written: written.clone(),
        calls: Mutex::new(Vec::new()),
    });
    let copy = tokio::spawn({
        let (bytes, gate, written) = (bytes.clone(), gate.clone(), written.clone());
        async move {
            let mut hasher = Hasher::new(1);
            let size = bytes.len() as u64;
            copy_exact(
                &mut &bytes[..],
                &mut Counting(written),
                size,
                0,
                &mut hasher,
                &(),
                &*gate,
            )
            .await
            .map(|()| hasher.finish())
        }
    });
    let slices = [SLICE, SLICE, SLICE, 100];
    let mut before = 0;
    for (i, len) in slices.into_iter().enumerate() {
        settle().await;
        // Asked before the slice, and held: nothing of it is written while it waits.
        assert_eq!(gate.calls.lock().unwrap().len(), i + 1);
        assert_eq!(gate.calls.lock().unwrap()[i], (len, before));
        assert_eq!(written.load(Ordering::SeqCst), before);
        settle().await;
        assert_eq!(written.load(Ordering::SeqCst), before, "held while waiting");
        gate.permits.add_permits(1);
        before += len as u64;
    }
    let digest = copy.await.unwrap().unwrap();
    assert_eq!(written.load(Ordering::SeqCst), bytes.len() as u64);
    assert_eq!(gate.calls.lock().unwrap().len(), slices.len());
    assert_eq!(digest, sha(&bytes));
}

// Saving a whole file.

#[tokio::test]
async fn a_whole_file_carries_its_hash_and_is_published() {
    let to = temp("whole");
    let dir = Directory::open(&to).unwrap();
    let bytes = data(CHUNK as usize + 12_345);
    let (mut sending, mut receiving) = tokio::io::duplex(64 << 10);
    let sender = async {
        let mut hasher = Settings::default().sending();
        let mut source = &bytes[..];
        copy_exact(
            &mut source,
            &mut sending,
            bytes.len() as u64,
            0,
            &mut hasher,
            &(),
            &(),
        )
        .await?;
        let digest = hasher.finish();
        write_trailer(&mut sending, &digest).await?;
        sending.shutdown().await?;
        Ok::<_, ChunkError>(digest)
    };
    let counted = AtomicU64::new(0);
    struct Count<'a>(&'a AtomicU64);
    impl Progress for Count<'_> {
        fn advanced(&self, bytes: u64) {
            self.0.fetch_add(bytes, Ordering::SeqCst);
        }
    }
    let progress = Count(&counted);
    let rules = PublishRules {
        mark: true,
        ..PublishRules::default()
    };
    let sent = Time {
        secs: 1_000_000_000,
        nanos: 0,
    };
    let options = SaveOptions {
        publish: &rules,
        meta: Meta {
            modified: Some(sent),
            executable: true,
        },
        meta_rules: MetaRules {
            times: true,
            executable: true,
        },
        progress: &progress,
        ..SaveOptions::default()
    };
    let receiver = save_verified(
        &mut receiving,
        &dir,
        "report.pdf",
        bytes.len() as u64,
        &options,
        &(),
        std::future::pending(),
    );
    let (sent_digest, saved) = tokio::join!(sender, receiver);
    let saved = saved.unwrap();
    assert_eq!(saved.digest, sent_digest.unwrap());
    assert_eq!(saved.digest, sha(&bytes));
    assert_eq!(saved.path, to.join("report.pdf"));
    assert_eq!(counted.load(Ordering::SeqCst), bytes.len() as u64);
    assert_eq!(saved.applied.time, TimeApplied::Set);
    #[cfg(unix)]
    assert_eq!(saved.applied.executable, BitApplied::Set);
    #[cfg(windows)]
    assert_eq!(saved.applied.executable, BitApplied::NotHere);
    #[cfg(windows)]
    assert_eq!(saved.marked, Marked::Marked);
    #[cfg(unix)]
    assert_eq!(saved.marked, Marked::NotOnThisSystem);
    drop(dir);
    assert_eq!(files_in(&to), ["report.pdf"]);
    assert_eq!(std::fs::read(to.join("report.pdf")).unwrap(), bytes);
    let modified = std::fs::metadata(to.join("report.pdf"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(Time::from_system(modified), Some(sent));
    std::fs::remove_dir_all(&to).unwrap();
}

/// A taken name is not replaced: the file arrives numbered, and so does
/// its partial when a partial of that name is there.
#[tokio::test]
async fn a_taken_name_and_a_taken_partial_name_are_left_alone() {
    let to = temp("taken");
    std::fs::write(to.join("a.txt"), b"theirs").unwrap();
    std::fs::write(to.join("a.txt.part"), b"another partial").unwrap();
    let dir = Directory::open(&to).unwrap();
    let stream = framed(b"ours").await;
    let saved = save_verified(
        &mut &stream[..],
        &dir,
        "a.txt",
        4,
        &SaveOptions::default(),
        &(),
        std::future::pending(),
    )
    .await
    .unwrap();
    assert_eq!(saved.path, to.join("a (2).txt"));
    drop(dir);
    assert_eq!(files_in(&to), ["a (2).txt", "a.txt", "a.txt.part"]);
    assert_eq!(std::fs::read(to.join("a.txt")).unwrap(), b"theirs");
    assert_eq!(
        std::fs::read(to.join("a.txt.part")).unwrap(),
        b"another partial"
    );
    std::fs::remove_dir_all(&to).unwrap();
}

#[tokio::test]
async fn a_wrong_hash_removes_the_file_before_it_gets_a_name() {
    let to = temp("damaged");
    let dir = Directory::open(&to).unwrap();
    let bytes = data(100_000);
    let mut stream = framed(&bytes).await;
    // One bit of the file flipped on the way.
    stream[50_000] ^= 1;
    let rules = PublishRules {
        mark: true,
        ..PublishRules::default()
    };
    let options = SaveOptions {
        publish: &rules,
        meta: Meta {
            modified: Some(Time { secs: 1, nanos: 0 }),
            executable: true,
        },
        ..SaveOptions::default()
    };
    let hooks = Recorder::default();
    let damaged = save_verified(
        &mut &stream[..],
        &dir,
        "a.exe",
        bytes.len() as u64,
        &options,
        &hooks,
        std::future::pending(),
    )
    .await;
    assert_eq!(damaged, Err(ChunkError::Damaged));
    assert!(files_in(&to).is_empty(), "{:?}", files_in(&to));
    // It never came near a name: no publish step ran.
    assert_eq!(
        hooks.steps(),
        ["before_create", "created a.exe.part", "ended Err(Damaged)"]
    );
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

#[tokio::test]
async fn a_cut_stream_is_cut_short_and_one_byte_more_is_too_long() {
    let to = temp("cut");
    let dir = Directory::open(&to).unwrap();
    let bytes = data(3 * SLICE + 7);
    let size = bytes.len() as u64;
    let stream = framed(&bytes).await;
    let save = |stream: Vec<u8>| {
        let dir = &dir;
        async move {
            save_verified(
                &mut &stream[..],
                dir,
                "a.bin",
                size,
                &SaveOptions::default(),
                &(),
                std::future::pending(),
            )
            .await
        }
    };
    // Cut inside the file's bytes: how many came.
    for cut in [0, 1, SLICE, 2 * SLICE + 3, bytes.len() - 1] {
        assert_eq!(
            save(stream[..cut].to_vec()).await,
            Err(ChunkError::CutShort {
                got: cut as u64,
                size
            })
        );
        assert!(files_in(&to).is_empty());
    }
    // Cut inside the trailer: how much of it came.
    for kept in [0, 1, 31] {
        assert_eq!(
            save(stream[..bytes.len() + kept].to_vec()).await,
            Err(ChunkError::TrailerCutShort { got: kept as u64 })
        );
        assert!(files_in(&to).is_empty());
    }
    // One byte more than the file and its trailer.
    let mut longer = stream.clone();
    longer.push(0);
    assert_eq!(save(longer).await, Err(ChunkError::TooLong));
    assert!(files_in(&to).is_empty());
    // Exactly the file and its trailer.
    assert!(save(stream).await.is_ok());
    assert_eq!(files_in(&to), ["a.bin"]);
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

/// A file as long as its trailer: a cut inside the file and one inside the
/// trailer, the same number of bytes in, are told apart.
#[tokio::test]
async fn a_cut_in_the_file_and_a_cut_in_the_trailer_are_different_errors() {
    let bytes = data(DIGEST_LEN);
    let stream = framed(&bytes).await;
    let read = |cut: usize| {
        let stream = stream[..cut].to_vec();
        async move {
            let mut reader = &stream[..];
            let mut hasher = Hasher::new(1);
            copy_exact(&mut reader, &mut Vec::new(), 32, 0, &mut hasher, &(), &()).await?;
            read_trailer(&mut reader).await
        }
    };
    assert_eq!(
        read(10).await,
        Err(ChunkError::CutShort { got: 10, size: 32 })
    );
    assert_eq!(
        read(32 + 10).await,
        Err(ChunkError::TrailerCutShort { got: 10 })
    );
    assert_eq!(read(64).await, Ok(sha(&bytes)));
}

#[tokio::test]
async fn copy_exact_reads_nothing_past_the_size_and_refuses_a_wrong_start() {
    let bytes = data(1000);
    let mut source = &bytes[..];
    let mut out = Vec::new();
    let mut hasher = Hasher::new(1);
    copy_exact(&mut source, &mut out, 600, 0, &mut hasher, &(), &())
        .await
        .unwrap();
    assert_eq!(out, bytes[..600]);
    assert_eq!(source.len(), 400, "the rest is left on the stream");
    // From 600 on, the hasher standing at 600: the whole file's hash.
    copy_exact(&mut source, &mut out, 1000, 600, &mut hasher, &(), &())
        .await
        .unwrap();
    assert_eq!(hasher.finish(), sha(&bytes));
    // A hasher not at `from`, or `from` past the size.
    let mut fresh = Hasher::new(1);
    assert_eq!(
        copy_exact(
            &mut &bytes[..],
            &mut Vec::new(),
            1000,
            600,
            &mut fresh,
            &(),
            &()
        )
        .await,
        Err(ChunkError::Offset {
            from: 600,
            size: 1000,
            hashed: 0
        })
    );
    assert!(matches!(
        copy_exact(
            &mut &bytes[..],
            &mut Vec::new(),
            10,
            20,
            &mut fresh,
            &(),
            &()
        )
        .await,
        Err(ChunkError::Offset { .. })
    ));
}

#[tokio::test]
async fn a_cancel_while_the_bytes_come_removes_the_partial() {
    let to = temp("cancel");
    let dir = Directory::open(&to).unwrap();
    // The sender writes a little and then waits forever.
    let (mut sending, mut receiving) = tokio::io::duplex(1024);
    sending.write_all(b"some").await.unwrap();
    let (cancel, cancelled) = tokio::sync::oneshot::channel::<()>();
    let hooks = Recorder::default();
    let options = SaveOptions::default();
    let saving = save_verified(
        &mut receiving,
        &dir,
        "a.bin",
        1 << 20,
        &options,
        &hooks,
        async {
            let _ = cancelled.await;
        },
    );
    let cancelling = async {
        settle().await;
        assert_eq!(files_in(&to), ["a.bin.part"]);
        cancel.send(()).unwrap();
    };
    let (saved, ()) = tokio::join!(saving, cancelling);
    assert_eq!(saved, Err(ChunkError::Cancelled));
    assert!(files_in(&to).is_empty());
    assert_eq!(
        hooks.steps(),
        [
            "before_create",
            "created a.bin.part",
            "ended Err(Cancelled)"
        ]
    );
    drop((dir, sending));
    std::fs::remove_dir_all(&to).unwrap();
}

/// A cancel while the caller's pace holds a slice ends the save at once,
/// before that slice is written, and removes the partial file.
#[tokio::test(flavor = "current_thread")]
async fn a_cancel_while_the_pace_holds_a_slice_removes_the_partial() {
    let to = temp("cancel-paced");
    let dir = Directory::open(&to).unwrap();
    let bytes = data(3 * SLICE);
    let stream = framed(&bytes).await;
    let written = Arc::new(AtomicU64::new(0));
    // One slice goes; the second is held.
    let gate = Gate {
        permits: Semaphore::new(1),
        written,
        calls: Mutex::new(Vec::new()),
    };
    let (cancel, cancelled) = tokio::sync::oneshot::channel::<()>();
    let hooks = Recorder::default();
    let options = SaveOptions {
        pace: &gate,
        ..SaveOptions::default()
    };
    let mut reader = &stream[..];
    let saving = save_verified(
        &mut reader,
        &dir,
        "a.bin",
        bytes.len() as u64,
        &options,
        &hooks,
        async {
            let _ = cancelled.await;
        },
    );
    let cancelling = async {
        settle().await;
        assert_eq!(gate.calls.lock().unwrap().len(), 2, "held at the second");
        assert_eq!(files_in(&to), ["a.bin.part"]);
        cancel.send(()).unwrap();
    };
    let (saved, ()) = tokio::join!(saving, cancelling);
    assert_eq!(saved, Err(ChunkError::Cancelled));
    assert!(files_in(&to).is_empty());
    assert_eq!(
        hooks.steps(),
        [
            "before_create",
            "created a.bin.part",
            "ended Err(Cancelled)"
        ]
    );
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

// The hooks.

/// Hooks that note each step, and fail at the one named.
#[derive(Default)]
struct Recorder {
    steps: Mutex<Vec<String>>,
    fail: Option<&'static str>,
    /// The partial file's time when `before_publish` came.
    at_publish: Mutex<Option<Option<Time>>>,
}

impl Recorder {
    fn failing(step: &'static str) -> Self {
        Self {
            fail: Some(step),
            ..Self::default()
        }
    }

    fn steps(&self) -> Vec<String> {
        self.steps.lock().unwrap().clone()
    }

    fn step(&self, step: &'static str, note: String) -> io::Result<()> {
        self.steps.lock().unwrap().push(note);
        if self.fail == Some(step) {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok(())
    }
}

impl Hooks for Recorder {
    fn before_create(&self) -> io::Result<()> {
        self.step("before_create", "before_create".into())
    }

    fn created(&self, _: &File, partial: &OsStr) -> io::Result<()> {
        self.step("created", format!("created {}", partial.to_string_lossy()))
    }

    fn before_publish(&self, file: &File, partial: &OsStr) -> io::Result<()> {
        *self.at_publish.lock().unwrap() = Some(Meta::read(file).unwrap().modified);
        self.step(
            "before_publish",
            format!("before_publish {}", partial.to_string_lossy()),
        )
    }

    fn ended(&self, outcome: Result<&Saved, &ChunkError>) {
        let note = match outcome {
            Ok(saved) => format!(
                "ended Ok({})",
                saved.path.file_name().unwrap().to_string_lossy()
            ),
            Err(e) => format!("ended Err({e:?})"),
        };
        self.steps.lock().unwrap().push(note);
    }
}

#[tokio::test]
async fn hooks_run_in_order_and_an_error_in_any_removes_the_partial() {
    let bytes = data(10_000);
    let stream = framed(&bytes).await;
    let sent = Time {
        secs: 1_000_000_000,
        nanos: 0,
    };
    let rules = PublishRules {
        mark: true,
        ..PublishRules::default()
    };
    let options = SaveOptions {
        publish: &rules,
        meta: Meta {
            modified: Some(sent),
            executable: false,
        },
        ..SaveOptions::default()
    };
    let run = |hooks: Recorder, name: &'static str| {
        let (stream, options) = (stream.clone(), options);
        async move {
            let to = temp(name);
            let dir = Directory::open(&to).unwrap();
            let saved = save_verified(
                &mut &stream[..],
                &dir,
                "a.exe",
                10_000,
                &options,
                &hooks,
                std::future::pending(),
            )
            .await;
            drop(dir);
            let files = files_in(&to);
            std::fs::remove_dir_all(&to).unwrap();
            (saved, hooks, files)
        }
    };

    let (saved, hooks, files) = run(Recorder::default(), "hooks-ok").await;
    assert_eq!(saved.unwrap().path.file_name().unwrap(), "a.exe");
    assert_eq!(
        hooks.steps(),
        [
            "before_create",
            "created a.exe.part",
            "before_publish a.exe.part",
            "ended Ok(a.exe)"
        ]
    );
    assert_eq!(files, ["a.exe"]);
    // At `before_publish` the hash matched, but the time and the mark wait for it.
    let time = hooks.at_publish.lock().unwrap().unwrap();
    assert_ne!(time, Some(sent), "the time comes after the hash");

    let hook = Some(ChunkError::Hook(io::ErrorKind::PermissionDenied));
    for (step, steps) in [
        (
            "before_create",
            vec!["before_create", "ended Err(Hook(PermissionDenied))"],
        ),
        (
            "created",
            vec![
                "before_create",
                "created a.exe.part",
                "ended Err(Hook(PermissionDenied))",
            ],
        ),
        (
            "before_publish",
            vec![
                "before_create",
                "created a.exe.part",
                "before_publish a.exe.part",
                "ended Err(Hook(PermissionDenied))",
            ],
        ),
    ] {
        let (saved, hooks, files) = run(Recorder::failing(step), "hooks-fail").await;
        assert_eq!(saved.err(), hook, "{step}");
        assert_eq!(hooks.steps(), steps, "{step}");
        assert!(files.is_empty(), "{step}: {files:?}");
    }
}

/// The mark comes after the hash matched, then the time, then the name.
#[cfg(windows)]
#[tokio::test]
async fn the_mark_comes_after_the_hash_and_before_the_time() {
    let to = temp("mark-order");
    let dir = Directory::open(&to).unwrap();
    let stream = framed(b"MZ").await;
    let sent = Time {
        secs: 1_000_000_000,
        nanos: 0,
    };
    let rules = PublishRules {
        mark: true,
        ..PublishRules::default()
    };
    struct Unmarked(PathBuf, Mutex<Option<bool>>);
    impl Hooks for Unmarked {
        fn before_publish(&self, _: &File, partial: &OsStr) -> io::Result<()> {
            let mut zone = self.0.join(partial).into_os_string();
            zone.push(":Zone.Identifier");
            *self.1.lock().unwrap() = Some(std::fs::metadata(zone).is_ok());
            Ok(())
        }
    }
    let hooks = Unmarked(to.clone(), Mutex::new(None));
    let options = SaveOptions {
        publish: &rules,
        meta: Meta {
            modified: Some(sent),
            executable: false,
        },
        ..SaveOptions::default()
    };
    let saved = save_verified(
        &mut &stream[..],
        &dir,
        "a.exe",
        2,
        &options,
        &hooks,
        std::future::pending(),
    )
    .await
    .unwrap();
    assert_eq!(
        *hooks.1.lock().unwrap(),
        Some(false),
        "not marked before the hash"
    );
    assert_eq!(saved.marked, Marked::Marked);
    drop(dir);
    // Marked, and the time is the sender's though the mark was written after the bytes.
    let modified = std::fs::metadata(&saved.path).unwrap().modified().unwrap();
    assert_eq!(Time::from_system(modified), Some(sent));
    std::fs::remove_dir_all(&to).unwrap();
}

// Hashes of a prefix.

#[tokio::test]
async fn the_prefix_hash_equals_the_hash_of_the_first_have_bytes() {
    let chunk = 1024;
    let bytes = data(3 * chunk + chunk / 2);
    let mut hasher = Hasher::with_chunk(4, small(chunk as u64));
    // In uneven pieces, across boundaries.
    for piece in bytes.chunks(700) {
        hasher.update(piece);
    }
    assert_eq!(hasher.position(), bytes.len() as u64);
    for have in [0, chunk, 2 * chunk, 3 * chunk] {
        let expected = sha(&bytes[..have]);
        assert_eq!(hasher.at(have as u64), Some(expected), "{have}");
        assert_eq!(
            hash_prefix(&mut &bytes[..], have as u64).await,
            Ok(expected),
            "{have}"
        );
    }
    // Off a boundary, or past what was hashed: not in memory.
    assert_eq!(hasher.at(chunk as u64 + 1), None);
    assert_eq!(hasher.at(4 * chunk as u64), None);
    // A prefix longer than the file is cut short.
    assert_eq!(
        hash_prefix(&mut &bytes[..], bytes.len() as u64 + 1).await,
        Err(ChunkError::CutShort {
            got: bytes.len() as u64,
            size: bytes.len() as u64 + 1
        })
    );
    assert_eq!(hasher.finish(), sha(&bytes));
    // A hasher keeping nothing still knows the empty prefix.
    let mut none = Hasher::with_chunk(0, small(chunk as u64));
    none.update(&bytes);
    assert_eq!(none.at(chunk as u64), None);
    assert_eq!(none.at(0), Some(sha(b"")));
}

/// After a cut mid-chunk, the receiver's last whole chunk is `have`. The
/// sender, several chunks ahead, finds the hash of that prefix among the
/// boundaries it keeps, or reads it from its disk when it is one boundary
/// past them; the receiver from its memory or its partial file. All four
/// equal the hash of the first `have` bytes, and each side goes on from
/// there to the whole file's hash.
#[tokio::test]
async fn the_prefix_hash_after_a_cut_mid_chunk_equals_the_first_have_bytes_on_both_sides() {
    let to = temp("cut-prefix");
    let chunk = 64 * 1024;
    // The framing's chunk, scaled down so the test stays small.
    let settings = Settings::default();
    let sending = || Hasher::with_chunk(settings.sender_keep, small(chunk as u64));
    let receiving = || Hasher::with_chunk(settings.receiver_keep, small(chunk as u64));
    let bytes = data(12 * chunk);
    let source_path = to.join("source");
    std::fs::write(&source_path, &bytes).unwrap();
    // The sender has sent 8.5 chunks; it keeps boundaries 5 to 8.
    let sent = 8 * chunk + chunk / 2;
    let mut sender = sending();
    let mut out = Vec::new();
    let source = tokio::fs::File::open(&source_path).await.unwrap();
    use tokio::io::AsyncReadExt;
    copy_exact(
        &mut source.take(sent as u64),
        &mut out,
        bytes.len() as u64,
        0,
        &mut sender,
        &(),
        &(),
    )
    .await
    .unwrap_err();
    assert_eq!(sender.position(), sent as u64);
    // Received 5.5 chunks (have 5, kept by the sender) or 4.5 (have 4, one past).
    for (received, kept_by_sender) in [(5 * chunk + chunk / 2, true), (4 * chunk + 100, false)] {
        let partial_path = to.join("partial");
        let mut partial = tokio::fs::File::create(&partial_path).await.unwrap();
        let mut receiver = receiving();
        let cut = copy_exact(
            &mut &out[..received],
            &mut partial,
            bytes.len() as u64,
            0,
            &mut receiver,
            &(),
            &(),
        )
        .await;
        assert_eq!(
            cut,
            Err(ChunkError::CutShort {
                got: received as u64,
                size: bytes.len() as u64
            })
        );
        drop(partial);
        let have = received as u64 / chunk as u64 * chunk as u64;
        let expected = sha(&bytes[..have as usize]);
        // The receiver: from memory, and from its partial file.
        assert_eq!(receiver.at(have), Some(expected));
        let mut on_disk = tokio::fs::File::open(&partial_path).await.unwrap();
        assert_eq!(hash_prefix(&mut on_disk, have).await, Ok(expected));
        // The sender: from memory while it keeps the boundary, else from its disk.
        let in_memory = sender.at(have);
        assert_eq!(in_memory.is_some(), kept_by_sender, "{have}");
        let mut source = tokio::fs::File::open(&source_path).await.unwrap();
        let from_disk = hash_prefix(&mut source, have).await.unwrap();
        assert_eq!(in_memory.unwrap_or(from_disk), expected);
        assert_eq!(from_disk, expected);

        // Both go on from `have` and reach the whole file's hash: the
        // receiver from its kept state, the sender from its own or, past
        // what it keeps, from its disk.
        let size = bytes.len() as u64;
        let rest = &bytes[have as usize..];
        let mut receiver = receiver.rewound(have).unwrap();
        assert_eq!(receiver.position(), have);
        let mut sending = match sender.rewound(have) {
            Some(rewound) => rewound,
            None => {
                let mut fresh = sending();
                let mut source = tokio::fs::File::open(&source_path).await.unwrap();
                fresh.read(&mut source, have).await.unwrap();
                fresh
            }
        };
        for hasher in [&mut receiver, &mut sending] {
            copy_exact(
                &mut &rest[..],
                &mut Vec::new(),
                size,
                have,
                hasher,
                &(),
                &(),
            )
            .await
            .unwrap();
        }
        assert_eq!(receiver.finish(), sha(&bytes));
        assert_eq!(sending.finish(), sha(&bytes));
        std::fs::remove_file(&partial_path).unwrap();
    }
    std::fs::remove_dir_all(&to).unwrap();
}

// The framing around the bytes.

#[tokio::test]
async fn the_framing_version_and_the_metadata_block_read_back() {
    let settings = Settings::default();
    let mut out = Vec::new();
    write_version(&mut out).await.unwrap();
    write_meta(&mut out, b"header", &settings).await.unwrap();
    assert_eq!(out, [1, 6, 0, 0, 0, b'h', b'e', b'a', b'd', b'e', b'r']);
    let mut reader = &out[..];
    read_version(&mut reader).await.unwrap();
    assert_eq!(read_meta(&mut reader, &settings).await.unwrap(), b"header");
    read_end(&mut reader).await.unwrap();

    assert_eq!(
        read_version(&mut &[2u8][..]).await,
        Err(ChunkError::Version { got: 2 })
    );
    assert_eq!(
        read_version(&mut &[][..]).await,
        Err(ChunkError::CutShort { got: 0, size: 1 })
    );
    // The limit itself is taken; one past it is refused, before its bytes are read.
    let limit = Settings {
        meta_bytes: 4,
        ..settings
    };
    let mut out = Vec::new();
    write_meta(&mut out, b"four", &limit).await.unwrap();
    assert_eq!(read_meta(&mut &out[..], &limit).await.unwrap(), b"four");
    assert_eq!(
        write_meta(&mut Vec::new(), b"five!", &limit).await,
        Err(ChunkError::MetaTooLong { len: 5, most: 4 })
    );
    let declared = [0xff, 0xff, 0xff, 0xff, 1, 2, 3];
    let mut reader = &declared[..];
    assert_eq!(
        read_meta(&mut reader, &limit).await,
        Err(ChunkError::MetaTooLong {
            len: u32::MAX.into(),
            most: 4
        })
    );
    assert_eq!(reader, [1, 2, 3], "nothing read past the length");
    assert_eq!(
        read_meta(&mut &[3, 0, 0, 0, 1][..], &limit).await,
        Err(ChunkError::CutShort { got: 1, size: 3 })
    );
}

// The candidate.

fn candidate_in(to: &Path, name: &str, bytes: &[u8]) -> (Directory, safecopy_folder::Candidate) {
    std::fs::write(to.join(name), bytes).unwrap();
    let dir = Directory::open(to).unwrap();
    let candidate = dir.candidate(OsStr::new(name), bytes.len() as u64).unwrap();
    (dir, candidate)
}

#[tokio::test]
async fn a_candidate_hashes_to_the_senders_hash_and_is_left_as_it_was() {
    let to = temp("candidate-match");
    let bytes = data(3 * SLICE + 5);
    let (dir, mut candidate) = candidate_in(&to, "a.bin", &bytes);
    let before = std::fs::metadata(to.join("a.bin"))
        .unwrap()
        .modified()
        .unwrap();
    let read = AtomicU64::new(0);
    struct Count<'a>(&'a AtomicU64);
    impl Progress for Count<'_> {
        fn advanced(&self, bytes: u64) {
            self.0.fetch_add(bytes, Ordering::SeqCst);
        }
    }
    assert_eq!(candidate.hash(&Count(&read)).await, Ok(sha(&bytes)));
    assert_eq!(read.load(Ordering::SeqCst), bytes.len() as u64);
    // Twice: each hash reads from byte 0.
    assert_eq!(candidate.hash(&()).await, Ok(sha(&bytes)));
    drop((candidate, dir));
    assert_eq!(std::fs::read(to.join("a.bin")).unwrap(), bytes);
    let after = std::fs::metadata(to.join("a.bin"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(before, after);
    std::fs::remove_dir_all(&to).unwrap();
}

/// Grows, gets another time, or another file takes its name while it is
/// hashed, or before: no match.
#[tokio::test]
async fn a_candidate_that_changes_while_it_is_hashed_is_no_match() {
    let to = temp("candidate-changing");
    let bytes = data(2 * SLICE + 1);
    let path = to.join("a.bin");

    /// Runs `change` at the first slice read.
    struct During<F: Fn() + Send + Sync>(F, AtomicU64);
    impl<F: Fn() + Send + Sync> Progress for During<F> {
        fn advanced(&self, _: u64) {
            if self.1.fetch_add(1, Ordering::SeqCst) == 0 {
                (self.0)();
            }
        }
    }
    let grow = || {
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"+")
            .unwrap();
    };
    let retime = || {
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000))
            .unwrap();
    };
    let replace = || {
        std::fs::write(to.join("other"), &bytes).unwrap();
        std::fs::rename(to.join("other"), &path).unwrap();
    };
    let changes: [(&str, &(dyn Fn() + Sync)); 3] = [
        ("grown", &grow),
        ("retimed", &retime),
        ("replaced", &replace),
    ];
    for (what, change) in changes {
        let (dir, mut candidate) = candidate_in(&to, "a.bin", &bytes);
        let during = During(change, AtomicU64::new(0));
        assert_eq!(
            candidate.hash(&during).await,
            Err(CandidateError::Changed),
            "{what} while hashed"
        );
        drop((candidate, dir));

        let (dir, mut candidate) = candidate_in(&to, "a.bin", &bytes);
        change();
        assert_eq!(
            candidate.hash(&()).await,
            Err(CandidateError::Changed),
            "{what} before"
        );
        drop((candidate, dir));
    }
    std::fs::remove_dir_all(&to).unwrap();
}

/// A file another program holds locked against reading is no match:
/// opened without sharing, it is no candidate; locked after it was found,
/// it cannot be read.
#[cfg(windows)]
#[tokio::test]
async fn a_candidate_locked_against_reading_is_no_match() {
    use std::os::windows::fs::OpenOptionsExt;
    let to = temp("candidate-locked");
    let bytes = data(1000);
    std::fs::write(to.join("a.bin"), &bytes).unwrap();
    let dir = Directory::open(&to).unwrap();
    let unshared = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(to.join("a.bin"))
        .unwrap();
    assert!(dir.candidate(OsStr::new("a.bin"), 1000).is_none());
    drop(unshared);

    let mut candidate = dir.candidate(OsStr::new("a.bin"), 1000).unwrap();
    let locker = File::options().write(true).open(to.join("a.bin")).unwrap();
    locker.lock().unwrap();
    assert_eq!(candidate.hash(&()).await, Err(CandidateError::Unreadable));
    locker.unlock().unwrap();
    assert_eq!(candidate.hash(&()).await, Ok(sha(&bytes)));
    drop((locker, candidate, dir));
    std::fs::remove_dir_all(&to).unwrap();
}
