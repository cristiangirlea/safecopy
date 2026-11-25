use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use safecopy_folder::{
    BitApplied, Directory, Marked, Meta, MetaRules, PublishRules, Refusal, Time, TimeApplied,
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
    assert_eq!(options.publish, Publish::default());
    assert_eq!(options.meta_rules, MetaRules::default());
    assert!(options.sync);
    assert_eq!(options.partial_tries, 10_001);
    assert_eq!(options.partial_tries, safecopy_folder::NAME_TRIES);
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
    let rules = Publish {
        mark: true,
        ..Publish::default()
    };
    let sent = Time {
        secs: 1_000_000_000,
        nanos: 0,
    };
    let options = SaveOptions {
        publish: rules,
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
    let rules = Publish {
        mark: true,
        ..Publish::default()
    };
    let options = SaveOptions {
        publish: rules,
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
        [
            "before_create",
            "created a.exe.part",
            "ended Err(Damaged) Removed"
        ]
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
            "ended Err(Cancelled) Removed"
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
            "ended Err(Cancelled) Removed"
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

    fn ended(&self, outcome: Result<&Saved, &ChunkError>, partial: PartialFate) {
        let note = match outcome {
            Ok(saved) => format!(
                "ended Ok({}) {partial:?}",
                saved.path.file_name().unwrap().to_string_lossy()
            ),
            Err(e) => format!("ended Err({e:?}) {partial:?}"),
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
    let rules = Publish {
        mark: true,
        ..Publish::default()
    };
    let options = SaveOptions {
        publish: rules,
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
            "ended Ok(a.exe) Published"
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
            vec![
                "before_create",
                "ended Err(Hook(PermissionDenied)) NotCreated",
            ],
        ),
        (
            "created",
            vec![
                "before_create",
                "created a.exe.part",
                "ended Err(Hook(PermissionDenied)) Removed",
            ],
        ),
        (
            "before_publish",
            vec![
                "before_create",
                "created a.exe.part",
                "before_publish a.exe.part",
                "ended Err(Hook(PermissionDenied)) Removed",
            ],
        ),
    ] {
        let (saved, hooks, files) = run(Recorder::failing(step), "hooks-fail").await;
        assert_eq!(saved.err(), hook, "{step}");
        assert_eq!(hooks.steps(), steps, "{step}");
        assert!(files.is_empty(), "{step}: {files:?}");
    }
}

/// Hooks that note what became of the partial file.
#[derive(Default)]
struct Fates(Mutex<Vec<PartialFate>>);

impl Hooks for Fates {
    fn ended(&self, _: Result<&Saved, &ChunkError>, partial: PartialFate) {
        self.0.lock().unwrap().push(partial);
    }
}

/// A partial file that could not be created was never there to remove.
#[tokio::test]
async fn a_save_that_cannot_create_its_partial_says_none_was_created() {
    let to = temp("not-created");
    let dir = Directory::open(&to).unwrap();
    let stream = framed(b"x").await;
    let hooks = Fates::default();
    // Not one name in the folder.
    let saved = save_verified(
        &mut &stream[..],
        &dir,
        "a/b",
        1,
        &SaveOptions::default(),
        &hooks,
        std::future::pending(),
    )
    .await;
    // The folder crate's refusal, as the refusal it was.
    assert_eq!(saved, Err(ChunkError::Folder(Refusal::LeavesFolder)));
    assert_eq!(*hooks.0.lock().unwrap(), [PartialFate::NotCreated]);
    assert!(files_in(&to).is_empty());

    // A save that may keep its partial is refused the same way.
    let hooks = Fates::default();
    let stopped = save_resumable(
        &mut &stream[..],
        Target {
            dir: &dir,
            name: "c/d",
            size: 1,
        },
        Start::New,
        &|_| true,
        &SaveOptions::default(),
        &hooks,
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert_eq!(stopped.error, ChunkError::Folder(Refusal::LeavesFolder));
    assert!(stopped.kept.is_none());
    assert_eq!(*hooks.0.lock().unwrap(), [PartialFate::NotCreated]);
    assert!(files_in(&to).is_empty());

    // The system's own refusal of a name is still its kind.
    let hooks = Fates::default();
    let long = "x".repeat(5000);
    let saved = save_verified(
        &mut &stream[..],
        &dir,
        &long,
        1,
        &SaveOptions::default(),
        &hooks,
        std::future::pending(),
    )
    .await;
    assert!(matches!(saved, Err(ChunkError::Io(_))), "{saved:?}");
    assert_eq!(*hooks.0.lock().unwrap(), [PartialFate::NotCreated]);
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

#[test]
fn an_error_of_the_system_keeps_its_kind_and_a_refusal_its_own() {
    let denied = io::Error::from(io::ErrorKind::PermissionDenied);
    assert_eq!(
        ChunkError::from(denied),
        ChunkError::Io(io::ErrorKind::PermissionDenied)
    );
    // An error that is `Other` with no refusal in it stays the system's.
    assert_eq!(
        ChunkError::from(io::Error::other("not a refusal")),
        ChunkError::Io(io::ErrorKind::Other)
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
        assert_eq!(
            ChunkError::from(io::Error::other(why)),
            ChunkError::Folder(why)
        );
    }
}

/// A checked file whose publish the folder crate refuses, and whose
/// removal it refuses then: both reach the caller as what they were.
#[tokio::test]
async fn a_refused_publish_and_a_refused_removal_say_the_refusal() {
    let to = temp("refused-publish");
    let dir = Directory::open(&to).unwrap();
    let bytes = data(1000);
    let stream = framed(&bytes).await;
    let left = |refused| PartialFate::Left {
        kind: io::ErrorKind::Other,
        raw: None,
        refused: Some(refused),
    };

    let hooks = Fates::default();
    crate::seams::PUBLISH_REFUSAL.set(Some(Refusal::Unproven));
    crate::seams::DISCARD_REFUSAL.set(Some(Refusal::TooLong));
    let saved = save_verified(
        &mut &stream[..],
        &dir,
        "a.bin",
        1000,
        &SaveOptions::default(),
        &hooks,
        std::future::pending(),
    )
    .await;
    assert_eq!(crate::seams::PUBLISH_REFUSAL.get(), None, "the publish ran");
    assert_eq!(crate::seams::DISCARD_REFUSAL.get(), None, "the removal ran");
    assert_eq!(saved, Err(ChunkError::Folder(Refusal::Unproven)));
    assert_eq!(*hooks.0.lock().unwrap(), [left(Refusal::TooLong)]);
    assert_eq!(files_in(&to), ["a.bin.part"]);

    // Another file under the name is still its own fate.
    let hooks = Fates::default();
    crate::seams::PUBLISH_REFUSAL.set(Some(Refusal::Link));
    crate::seams::DISCARD_REFUSAL.set(Some(Refusal::Replaced));
    let saved = save_verified(
        &mut &stream[..],
        &dir,
        "b.bin",
        1000,
        &SaveOptions::default(),
        &hooks,
        std::future::pending(),
    )
    .await;
    assert_eq!(saved, Err(ChunkError::Folder(Refusal::Link)));
    assert_eq!(*hooks.0.lock().unwrap(), [PartialFate::Replaced]);

    // With no seam: a kept partial handed in under a name that is not one
    // part of a path. The folder crate refuses to give the file its name,
    // and then to remove it under that one.
    let file = dir.create(OsStr::new("c.bin.part")).unwrap();
    let kept = Start::Resume(Box::new(Kept {
        file,
        partial: "no/c.bin.part".into(),
        hasher: Settings::default().receiving(),
    }));
    let hooks = Fates::default();
    let stopped = save_resumable(
        &mut &stream[..],
        Target {
            dir: &dir,
            name: "c.bin",
            size: 1000,
        },
        kept,
        &|_| true,
        &SaveOptions::default(),
        &hooks,
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert_eq!(stopped.error, ChunkError::Folder(Refusal::LeavesFolder));
    assert!(stopped.kept.is_none(), "a checked file is never kept");
    assert_eq!(*hooks.0.lock().unwrap(), [left(Refusal::LeavesFolder)]);
    assert_eq!(std::fs::read(to.join("c.bin.part")).unwrap(), bytes);
    assert!(!to.join("c.bin").exists());

    // Refused at its publish through the seam too, and removed then.
    let file = dir.create(OsStr::new("d.bin.part")).unwrap();
    let kept = Start::Resume(Box::new(Kept {
        file,
        partial: "d.bin.part".into(),
        hasher: Settings::default().receiving(),
    }));
    let hooks = Fates::default();
    crate::seams::PUBLISH_REFUSAL.set(Some(Refusal::NoVolume));
    let stopped = save_resumable(
        &mut &stream[..],
        Target {
            dir: &dir,
            name: "d.bin",
            size: 1000,
        },
        kept,
        &|_| true,
        &SaveOptions::default(),
        &hooks,
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert_eq!(stopped.error, ChunkError::Folder(Refusal::NoVolume));
    assert_eq!(*hooks.0.lock().unwrap(), [PartialFate::Removed]);
    assert!(!to.join("d.bin.part").exists());
}

// The search for a free partial name.

/// Takes the first `count` partial names of `name` in `to`.
fn take_partials(to: &Path, name: &str, count: usize) {
    for n in 1..=count {
        let partial = if n == 1 {
            format!("{name}.part")
        } else {
            format!("{name}.{n}.part")
        };
        std::fs::write(to.join(partial), b"another save's").unwrap();
    }
}

/// One new save of `stream` as `a.txt`, through `save_verified` or
/// through `save_resumable` with a rule that keeps everything.
async fn save_new(
    resumable: bool,
    stream: &[u8],
    dir: &Directory,
    options: &SaveOptions<'_>,
    hooks: &dyn Hooks,
    cancel: impl Future<Output = ()>,
) -> Result<Saved, ChunkError> {
    let mut stream = stream;
    let size = stream.len() as u64 - DIGEST_LEN as u64;
    if !resumable {
        return save_verified(&mut stream, dir, "a.txt", size, options, hooks, cancel).await;
    }
    let target = Target {
        dir,
        name: "a.txt",
        size,
    };
    let keep = |_: &ChunkError| true;
    save_resumable(
        &mut stream,
        target,
        Start::New,
        &keep,
        options,
        hooks,
        cancel,
    )
    .await
    .map_err(|stopped| {
        assert!(stopped.kept.is_none());
        stopped.error
    })
}

/// A folder where every partial name up to the limit is taken: the save
/// fails with how many it tried and creates nothing. With a higher limit
/// it takes the next name.
#[tokio::test]
async fn a_save_tries_only_as_many_partial_names_as_its_options_say() {
    let stream = framed(b"ours").await;
    for resumable in [false, true] {
        let to = temp("partial-limit");
        take_partials(&to, "a.txt", 3);
        let before = files_in(&to);
        let dir = Directory::open(&to).unwrap();
        for tries in [0, 1, 3] {
            let options = SaveOptions {
                partial_tries: tries,
                ..SaveOptions::default()
            };
            let hooks = Recorder::default();
            let cancel = std::future::pending();
            let saved = save_new(resumable, &stream, &dir, &options, &hooks, cancel).await;
            assert_eq!(saved, Err(ChunkError::NoPartialName { tried: tries }));
            assert_eq!(files_in(&to), before);
            assert_eq!(
                hooks.steps(),
                [
                    "before_create".to_string(),
                    format!("ended Err(NoPartialName {{ tried: {tries} }}) NotCreated"),
                ]
            );
        }
        let options = SaveOptions {
            partial_tries: 4,
            ..SaveOptions::default()
        };
        let hooks = Recorder::default();
        let cancel = std::future::pending();
        let saved = save_new(resumable, &stream, &dir, &options, &hooks, cancel).await;
        assert_eq!(saved.unwrap().path, to.join("a.txt"));
        assert_eq!(
            hooks.steps(),
            [
                "before_create",
                "created a.txt.4.part",
                "before_publish a.txt.4.part",
                "ended Ok(a.txt) Published"
            ]
        );
        drop(dir);
        std::fs::remove_dir_all(&to).unwrap();
    }
}

/// A cancel that is ready once `flag` is set. Nothing wakes it: the save
/// sees it at its next look, so no timing is involved.
fn on_flag(flag: &std::sync::atomic::AtomicBool) -> impl Future<Output = ()> + '_ {
    std::future::poll_fn(move |_| {
        if flag.load(Ordering::SeqCst) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
}

/// A cancel given while partial names are taken ends the save before the
/// next name is tried: nothing is created, and `created` never comes.
#[tokio::test]
async fn a_cancel_between_partial_names_ends_the_save_with_nothing_created() {
    let stream = framed(b"ours").await;
    for resumable in [false, true] {
        let to = temp("cancel-names");
        // The second name is free: only the cancel keeps the save from it.
        take_partials(&to, "a.txt", 1);
        let dir = Directory::open(&to).unwrap();
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Given right after the first name was found taken.
        crate::seams::NAME_TAKEN.set(Some(Box::new({
            let flag = flag.clone();
            move |taken| {
                assert_eq!(taken, 1);
                flag.store(true, Ordering::SeqCst);
            }
        })));
        let hooks = Recorder::default();
        let options = SaveOptions::default();
        let saved = save_new(resumable, &stream, &dir, &options, &hooks, on_flag(&flag)).await;
        crate::seams::NAME_TAKEN.set(None);
        assert!(flag.load(Ordering::SeqCst));
        assert_eq!(saved, Err(ChunkError::Cancelled));
        assert_eq!(files_in(&to), ["a.txt.part"]);
        assert_eq!(
            hooks.steps(),
            ["before_create", "ended Err(Cancelled) NotCreated"]
        );
        drop(dir);
        std::fs::remove_dir_all(&to).unwrap();
    }
}

/// A save dropped while partial names are taken has created nothing.
#[tokio::test]
async fn a_save_dropped_between_partial_names_has_created_nothing() {
    let stream = framed(b"ours").await;
    let to = temp("drop-names");
    take_partials(&to, "a.txt", 1);
    let dir = Directory::open(&to).unwrap();
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    crate::seams::NAME_TAKEN.set(Some(Box::new({
        let flag = flag.clone();
        move |_| flag.store(true, Ordering::SeqCst)
    })));
    let hooks = Recorder::default();
    let options = SaveOptions::default();
    // The save is polled until it waits between names, then dropped.
    tokio::select! {
        biased;
        () = on_flag(&flag) => {}
        saved = save_new(false, &stream, &dir, &options, &hooks, std::future::pending()) => {
            panic!("{saved:?}")
        }
    }
    crate::seams::NAME_TAKEN.set(None);
    assert_eq!(files_in(&to), ["a.txt.part"]);
    assert_eq!(hooks.steps(), ["before_create"]);
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

/// The save lets the runtime's other tasks run between taken names: a
/// cancel one of them gives on the save's own thread is seen there.
#[tokio::test(flavor = "current_thread")]
async fn a_cancel_given_by_another_task_is_seen_between_partial_names() {
    let stream = framed(b"ours").await;
    let to = temp("cancel-names-task");
    take_partials(&to, "a.txt", 1);
    let dir = Directory::open(&to).unwrap();
    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Runs only once the save lets go of the thread.
    let cancelling = tokio::spawn({
        let flag = flag.clone();
        async move { flag.store(true, Ordering::SeqCst) }
    });
    let hooks = Recorder::default();
    let options = SaveOptions::default();
    let saved = save_new(false, &stream, &dir, &options, &hooks, on_flag(&flag)).await;
    assert_eq!(saved, Err(ChunkError::Cancelled));
    assert_eq!(files_in(&to), ["a.txt.part"]);
    assert_eq!(
        hooks.steps(),
        ["before_create", "ended Err(Cancelled) NotCreated"]
    );
    cancelling.await.unwrap();
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

/// Hooks that note each step and refuse at `at` with "already exists".
struct Exists {
    at: &'static str,
    steps: Mutex<Vec<String>>,
}

impl Exists {
    fn step(&self, step: &'static str, note: String) -> io::Result<()> {
        self.steps.lock().unwrap().push(note);
        if self.at == step {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        Ok(())
    }
}

impl Hooks for Exists {
    fn before_create(&self) -> io::Result<()> {
        self.step("before_create", "before_create".into())
    }

    fn created(&self, _: &File, partial: &OsStr) -> io::Result<()> {
        self.step("created", format!("created {}", partial.to_string_lossy()))
    }

    fn ended(&self, outcome: Result<&Saved, &ChunkError>, partial: PartialFate) {
        let note = format!("ended {:?} {partial:?}", outcome.err());
        self.steps.lock().unwrap().push(note);
    }
}

/// A hook's "already exists" is the hook's refusal, never a taken name:
/// the save fails at once with it, tries no second partial name, and
/// removes the partial it had made.
#[tokio::test]
async fn a_hook_that_says_already_exists_fails_the_save_and_is_no_taken_name() {
    let stream = framed(b"ours").await;
    let error = ChunkError::Hook(io::ErrorKind::AlreadyExists);
    for resumable in [false, true] {
        for (at, steps) in [
            (
                "before_create",
                vec![
                    "before_create",
                    "ended Some(Hook(AlreadyExists)) NotCreated",
                ],
            ),
            (
                "created",
                vec![
                    "before_create",
                    "created a.txt.part",
                    "ended Some(Hook(AlreadyExists)) Removed",
                ],
            ),
        ] {
            let to = temp("hook-exists");
            let dir = Directory::open(&to).unwrap();
            let hooks = Exists {
                at,
                steps: Mutex::default(),
            };
            let options = SaveOptions::default();
            let cancel = std::future::pending();
            let saved = save_new(resumable, &stream, &dir, &options, &hooks, cancel).await;
            assert_eq!(saved, Err(error), "{at}");
            assert_eq!(*hooks.steps.lock().unwrap(), steps, "{at}");
            assert!(files_in(&to).is_empty(), "{at}");
            drop(dir);
            std::fs::remove_dir_all(&to).unwrap();
        }
    }
}

/// A checked file for which every name up to the publish's limit is taken
/// gets none: the save says how many were tried and removes its partial.
#[tokio::test]
async fn a_save_whose_names_are_all_taken_says_how_many_it_tried() {
    let stream = framed(b"ours").await;
    for resumable in [false, true] {
        let to = temp("name-limit");
        std::fs::write(to.join("a.txt"), b"theirs").unwrap();
        std::fs::write(to.join("a (2).txt"), b"theirs").unwrap();
        let dir = Directory::open(&to).unwrap();
        let rules = Publish {
            name_tries: 2,
            ..Publish::default()
        };
        let options = SaveOptions {
            publish: rules,
            ..SaveOptions::default()
        };
        let hooks = Recorder::default();
        let cancel = std::future::pending();
        let saved = save_new(resumable, &stream, &dir, &options, &hooks, cancel).await;
        assert_eq!(saved, Err(ChunkError::NoNameLeft { tried: 2 }));
        assert_eq!(files_in(&to), ["a (2).txt", "a.txt"]);
        assert_eq!(
            hooks.steps().last().unwrap(),
            "ended Err(NoNameLeft { tried: 2 }) Removed"
        );
        drop(dir);
        std::fs::remove_dir_all(&to).unwrap();
    }
}

/// Where a partial is removed by its name, another file put under that
/// name before the save failed is not removed, and `ended` says so.
#[cfg(unix)]
#[tokio::test]
async fn a_partial_whose_name_another_file_took_is_reported_replaced() {
    struct Swap(PathBuf, Fates);
    impl Hooks for Swap {
        fn created(&self, _: &File, partial: &OsStr) -> io::Result<()> {
            std::fs::remove_file(self.0.join(partial))?;
            std::fs::write(self.0.join(partial), b"another program's")?;
            Err(io::ErrorKind::PermissionDenied.into())
        }
        fn ended(&self, outcome: Result<&Saved, &ChunkError>, partial: PartialFate) {
            self.1.ended(outcome, partial);
        }
    }
    let to = temp("replaced");
    let dir = Directory::open(&to).unwrap();
    let stream = framed(b"x").await;
    let hooks = Swap(to.clone(), Fates::default());
    let saved = save_verified(
        &mut &stream[..],
        &dir,
        "a.bin",
        1,
        &SaveOptions::default(),
        &hooks,
        std::future::pending(),
    )
    .await;
    assert_eq!(
        saved,
        Err(ChunkError::Hook(io::ErrorKind::PermissionDenied))
    );
    assert_eq!(*hooks.1.0.lock().unwrap(), [PartialFate::Replaced]);
    assert_eq!(
        std::fs::read(to.join("a.bin.part")).unwrap(),
        b"another program's"
    );
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

/// A partial file the system refuses to remove is left where it is, and
/// `ended` is told so with the system's code; the save's own error stands.
#[tokio::test]
async fn a_partial_that_cannot_be_removed_is_reported_left_and_is_still_there() {
    // Access denied, as each system numbers it.
    let denied = if cfg!(windows) { 5 } else { 13 };
    let to = temp("left");
    let dir = Directory::open(&to).unwrap();
    let bytes = data(1000);
    let mut stream = framed(&bytes).await;
    stream[500] ^= 1;
    let hooks = Fates::default();
    crate::seams::DISCARD_ERROR.set(Some(denied));
    let saved = save_verified(
        &mut &stream[..],
        &dir,
        "a.bin",
        1000,
        &SaveOptions::default(),
        &hooks,
        std::future::pending(),
    )
    .await;
    assert_eq!(crate::seams::DISCARD_ERROR.get(), None, "the removal ran");
    assert_eq!(saved, Err(ChunkError::Damaged));
    assert_eq!(
        *hooks.0.lock().unwrap(),
        [PartialFate::Left {
            kind: io::ErrorKind::PermissionDenied,
            raw: Some(denied),
            refused: None,
        }]
    );
    assert_eq!(files_in(&to), ["a.bin.part"]);

    // The same save with nothing in the way: removed, and said so.
    let hooks = Fates::default();
    let saved = save_verified(
        &mut &stream[..],
        &dir,
        "b.bin",
        1000,
        &SaveOptions::default(),
        &hooks,
        std::future::pending(),
    )
    .await;
    assert_eq!(saved, Err(ChunkError::Damaged));
    assert_eq!(*hooks.0.lock().unwrap(), [PartialFate::Removed]);
    assert_eq!(files_in(&to), ["a.bin.part"]);
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
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
    let rules = Publish {
        mark: true,
        ..Publish::default()
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
        publish: rules,
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

// A save that keeps its partial at a cut, and one that continues it.

/// Hooks that note each whole chunk and how the save ended, and fail at
/// the chunk named.
#[derive(Default)]
struct Wholes {
    seen: Mutex<Vec<(u64, Digest, u64)>>,
    fates: Fates,
    fail_at: Option<u64>,
    cancel: Option<Arc<tokio::sync::Notify>>,
}

impl Wholes {
    fn seen(&self) -> Vec<(u64, Digest)> {
        let seen = self.seen.lock().unwrap();
        seen.iter()
            .map(|(have, prefix, _)| (*have, *prefix))
            .collect()
    }
}

impl Hooks for Wholes {
    fn whole(&self, file: &File, have: u64, prefix: Digest) -> io::Result<()> {
        let len = file.metadata()?.len();
        self.seen.lock().unwrap().push((have, prefix, len));
        if let Some(cancel) = &self.cancel {
            cancel.notify_one();
        }
        if self.fail_at == Some(have) {
            return Err(io::ErrorKind::StorageFull.into());
        }
        Ok(())
    }

    fn ended(&self, outcome: Result<&Saved, &ChunkError>, partial: PartialFate) {
        self.fates.ended(outcome, partial);
    }
}

fn cut_short(error: &ChunkError) -> bool {
    matches!(error, ChunkError::CutShort { .. })
}

/// What a sender writes to continue `bytes` from `from`: the rest, then
/// the hash of the whole file.
fn rest(bytes: &[u8], from: u64) -> Vec<u8> {
    let mut out = bytes[from as usize..].to_vec();
    out.extend_from_slice(&sha(bytes).0);
    out
}

/// A kept partial made by hand, with chunks of `chunk` bytes: `on_disk` in
/// `name`, and a hasher over the first `have` bytes of `bytes`.
fn kept_by_hand(
    dir: &Directory,
    name: &str,
    bytes: &[u8],
    on_disk: &[u8],
    have: usize,
    chunk: u64,
) -> Start {
    let mut file = dir.create(OsStr::new(name)).unwrap();
    file.write_all(on_disk).unwrap();
    let mut hasher = Hasher::with_chunk(1, small(chunk));
    hasher.update(&bytes[..have]);
    Start::Resume(Box::new(Kept {
        file,
        partial: name.into(),
        hasher,
    }))
}

#[tokio::test]
async fn a_cut_stream_keeps_whole_chunks_and_a_later_save_appends_and_publishes() {
    let to = temp("keep-resume");
    let dir = Directory::open(&to).unwrap();
    let c = CHUNK as usize;
    let bytes = data(2 * c + c / 2 + 7);
    let size = bytes.len() as u64;
    let stream = framed(&bytes).await;
    // The stream is cut halfway through the second chunk.
    let cut = c + c / 2;
    let hooks = Wholes::default();
    let stopped = save_resumable(
        &mut &stream[..cut],
        Target {
            dir: &dir,
            name: "report.pdf",
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
    assert_eq!(
        stopped.error,
        ChunkError::CutShort {
            got: cut as u64,
            size
        }
    );
    let kept = stopped.kept.expect("the rule keeps a cut stream");
    assert_eq!(kept.have(), CHUNK);
    assert_eq!(kept.prefix(), sha(&bytes[..c]));
    assert_eq!(kept.partial, "report.pdf.part");
    assert_eq!(kept.file.metadata().unwrap().len(), CHUNK, "cut back");
    assert_eq!(files_in(&to), ["report.pdf.part"]);
    assert_eq!(
        std::fs::read(to.join("report.pdf.part")).unwrap(),
        bytes[..c]
    );
    assert_eq!(hooks.seen(), [(CHUNK, sha(&bytes[..c]))]);
    assert_eq!(
        *hooks.fates.0.lock().unwrap(),
        [PartialFate::Kept { have: CHUNK }]
    );

    // Later: the sender continues from what is kept.
    let rules = Publish {
        mark: true,
        ..Publish::default()
    };
    let sent = Time {
        secs: 1_000_000_000,
        nanos: 0,
    };
    let options = SaveOptions {
        publish: rules,
        meta: Meta {
            modified: Some(sent),
            executable: true,
        },
        meta_rules: MetaRules {
            times: true,
            executable: true,
        },
        ..SaveOptions::default()
    };
    let hooks = Wholes::default();
    let more = rest(&bytes, kept.have());
    let saved = save_resumable(
        &mut &more[..],
        Target {
            dir: &dir,
            name: "report.pdf",
            size,
        },
        Start::Resume(kept),
        &cut_short,
        &options,
        &hooks,
        std::future::pending(),
    )
    .await
    .unwrap();
    assert_eq!(saved.digest, sha(&bytes), "the hash of the whole file");
    assert_eq!(saved.path, to.join("report.pdf"));
    assert_eq!(saved.applied.time, TimeApplied::Set);
    #[cfg(unix)]
    assert_eq!(saved.applied.executable, BitApplied::Set);
    #[cfg(windows)]
    assert_eq!(saved.applied.executable, BitApplied::NotHere);
    #[cfg(windows)]
    assert_eq!(saved.marked, Marked::Marked);
    #[cfg(unix)]
    assert_eq!(saved.marked, Marked::NotOnThisSystem);
    // Only the chunk that came now is told.
    assert_eq!(hooks.seen(), [(2 * CHUNK, sha(&bytes[..2 * c]))]);
    assert_eq!(*hooks.fates.0.lock().unwrap(), [PartialFate::Published]);
    drop(dir);
    assert_eq!(files_in(&to), ["report.pdf"]);
    assert!(std::fs::read(to.join("report.pdf")).unwrap() == bytes);
    let modified = std::fs::metadata(to.join("report.pdf"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(Time::from_system(modified), Some(sent));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(to.join("report.pdf"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o100, 0, "the execute bit");
    }
    std::fs::remove_dir_all(&to).unwrap();
}

#[tokio::test]
async fn a_cancel_and_a_damaged_hash_keep_nothing_whatever_the_rule_says() {
    let to = temp("never-kept");
    let dir = Directory::open(&to).unwrap();
    let c = CHUNK as usize;
    let bytes = data(c + c / 2);
    let size = bytes.len() as u64;
    let always = |_: &ChunkError| true;

    // A bit flipped in the first chunk: every byte came, the hash is wrong.
    let mut damaged = framed(&bytes).await;
    damaged[100] ^= 1;
    let hooks = Wholes::default();
    let stopped = save_resumable(
        &mut &damaged[..],
        Target {
            dir: &dir,
            name: "a.bin",
            size,
        },
        Start::New,
        &always,
        &SaveOptions::default(),
        &hooks,
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert_eq!(stopped.error, ChunkError::Damaged);
    assert!(stopped.kept.is_none());
    assert_eq!(hooks.seen().len(), 1, "a whole chunk was there to keep");
    assert_eq!(*hooks.fates.0.lock().unwrap(), [PartialFate::Removed]);
    assert!(files_in(&to).is_empty());

    // A cancel that comes once the first whole chunk is written.
    let stream = framed(&bytes).await;
    let cancel = Arc::new(tokio::sync::Notify::new());
    let hooks = Wholes {
        cancel: Some(cancel.clone()),
        ..Wholes::default()
    };
    // The sender never ends its stream: only the cancel ends the save.
    let (mut sending, mut receiving) = tokio::io::duplex(1 << 16);
    let sender = async {
        sending.write_all(&stream[..c + 10]).await.unwrap();
        std::future::pending::<()>().await;
    };
    let options = SaveOptions::default();
    let saving = save_resumable(
        &mut receiving,
        Target {
            dir: &dir,
            name: "b.bin",
            size,
        },
        Start::New,
        &always,
        &options,
        &hooks,
        cancel.notified(),
    );
    let stopped = tokio::select! {
        stopped = saving => stopped.unwrap_err(),
        () = sender => unreachable!(),
    };
    assert_eq!(stopped.error, ChunkError::Cancelled);
    assert!(stopped.kept.is_none());
    assert_eq!(hooks.seen().len(), 1);
    assert_eq!(*hooks.fates.0.lock().unwrap(), [PartialFate::Removed]);
    assert!(files_in(&to).is_empty());

    // The same cut with no cancel is kept: the rule was the one asked.
    let hooks = Wholes::default();
    let stopped = save_resumable(
        &mut &stream[..c + 10],
        Target {
            dir: &dir,
            name: "c.bin",
            size,
        },
        Start::New,
        &always,
        &SaveOptions::default(),
        &hooks,
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert_eq!(stopped.kept.unwrap().have(), CHUNK);
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

#[tokio::test]
async fn a_cut_before_the_first_whole_chunk_keeps_nothing() {
    let to = temp("under-a-chunk");
    let dir = Directory::open(&to).unwrap();
    let c = CHUNK as usize;
    let bytes = data(c + 5);
    let stream = framed(&bytes).await;
    let hooks = Wholes::default();
    let stopped = save_resumable(
        &mut &stream[..c - 1],
        Target {
            dir: &dir,
            name: "a.bin",
            size: bytes.len() as u64,
        },
        Start::New,
        &|_: &ChunkError| true,
        &SaveOptions::default(),
        &hooks,
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert!(matches!(stopped.error, ChunkError::CutShort { .. }));
    assert!(stopped.kept.is_none());
    assert!(hooks.seen().is_empty());
    assert_eq!(*hooks.fates.0.lock().unwrap(), [PartialFate::Removed]);
    assert!(files_in(&to).is_empty());
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

#[tokio::test]
async fn whole_fires_once_per_whole_chunk_and_its_error_fails_the_save() {
    let to = temp("whole-hook");
    let dir = Directory::open(&to).unwrap();
    let c = CHUNK as usize;
    // Two chunks and a half; then a file that ends on a chunk.
    for (len, wholes) in [(2 * c + c / 2, 2), (2 * c, 2), (c - 1, 0)] {
        let bytes = data(len);
        let stream = framed(&bytes).await;
        let hooks = Wholes::default();
        save_verified(
            &mut &stream[..],
            &dir,
            "a.bin",
            len as u64,
            &SaveOptions::default(),
            &hooks,
            std::future::pending(),
        )
        .await
        .unwrap();
        let expected: Vec<(u64, Digest)> = (1..=wholes)
            .map(|n| (n as u64 * CHUNK, sha(&bytes[..n * c])))
            .collect();
        assert_eq!(hooks.seen(), expected, "{len}");
        // Each chunk was written when its call came.
        for (have, _, written) in hooks.seen.lock().unwrap().iter() {
            assert!(written >= have, "{len}: {written} of {have}");
        }
        std::fs::remove_file(to.join("a.bin")).unwrap();
    }
    // An error at the second chunk ends the save, and nothing is left.
    let bytes = data(3 * c);
    let stream = framed(&bytes).await;
    let hooks = Wholes {
        fail_at: Some(2 * CHUNK),
        ..Wholes::default()
    };
    let saved = save_verified(
        &mut &stream[..],
        &dir,
        "a.bin",
        bytes.len() as u64,
        &SaveOptions::default(),
        &hooks,
        std::future::pending(),
    )
    .await;
    assert_eq!(saved, Err(ChunkError::Hook(io::ErrorKind::StorageFull)));
    assert_eq!(hooks.seen().len(), 2, "no chunk after the one refused");
    assert_eq!(*hooks.fates.0.lock().unwrap(), [PartialFate::Removed]);
    assert!(files_in(&to).is_empty());
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

#[tokio::test]
async fn a_resumed_save_checks_its_stream_and_its_partial_as_a_new_one_does() {
    let to = temp("resume-checks");
    let dir = Directory::open(&to).unwrap();
    let chunk = 1024;
    let bytes = data(3 * 1024 + 100);
    let size = bytes.len() as u64;
    let have = 2 * 1024;
    let resume = |start: Start, stream: Vec<u8>, size: u64| {
        let dir = &dir;
        async move {
            let hooks = Fates::default();
            let saved = save_resumable(
                &mut &stream[..],
                Target {
                    dir,
                    name: "a.bin",
                    size,
                },
                start,
                &|_: &ChunkError| false,
                &SaveOptions::default(),
                &hooks,
                std::future::pending(),
            )
            .await
            .map_err(|stopped| stopped.error);
            let fate = hooks.0.lock().unwrap()[0];
            (saved, fate)
        }
    };
    let good = rest(&bytes, have as u64);
    // Fewer bytes than the rest, counted from the file's first byte.
    let start = kept_by_hand(&dir, "a.bin.part", &bytes, &bytes[..have], have, chunk);
    let (saved, fate) = resume(start, good[..500].to_vec(), size).await;
    let got = have as u64 + 500;
    assert_eq!(saved, Err(ChunkError::CutShort { got, size }));
    assert_eq!(fate, PartialFate::Removed);
    assert!(files_in(&to).is_empty());
    // More than the rest and its trailer.
    let start = kept_by_hand(&dir, "a.bin.part", &bytes, &bytes[..have], have, chunk);
    let mut longer = good.clone();
    longer.push(0);
    assert_eq!(
        resume(start, longer, size).await.0,
        Err(ChunkError::TooLong)
    );
    // A trailer that is not the whole file's hash.
    let start = kept_by_hand(&dir, "a.bin.part", &bytes, &bytes[..have], have, chunk);
    let mut wrong = good.clone();
    *wrong.last_mut().unwrap() ^= 1;
    assert_eq!(resume(start, wrong, size).await.0, Err(ChunkError::Damaged));
    // A partial shorter than its hasher says.
    let start = kept_by_hand(&dir, "a.bin.part", &bytes, &bytes[..have - 1], have, chunk);
    let short = ChunkError::PartialShort {
        have: have as u64,
        len: have as u64 - 1,
    };
    assert_eq!(resume(start, good.clone(), size).await.0, Err(short));
    // A hasher off a chunk, and one past the size.
    let start = kept_by_hand(&dir, "a.bin.part", &bytes, &bytes[..have], have - 1, chunk);
    assert!(matches!(
        resume(start, good.clone(), size).await.0,
        Err(ChunkError::Offset { .. })
    ));
    let start = kept_by_hand(&dir, "a.bin.part", &bytes, &bytes[..have], have, chunk);
    assert_eq!(
        resume(start, good.clone(), 1024).await.0,
        Err(ChunkError::Offset {
            from: have as u64,
            size: 1024,
            hashed: have as u64
        })
    );
    assert!(files_in(&to).is_empty(), "{:?}", files_in(&to));
    // Longer on the disk than its hasher says: cut to it, then appended to.
    let start = kept_by_hand(&dir, "a.bin.part", &bytes, &bytes[..have + 50], have, chunk);
    let (saved, _) = resume(start, good.clone(), size).await;
    assert_eq!(saved.unwrap().digest, sha(&bytes));
    assert_eq!(std::fs::read(to.join("a.bin")).unwrap(), bytes);
    std::fs::remove_file(to.join("a.bin")).unwrap();
    // A file that is whole but for its trailer.
    let whole = data(2 * 1024);
    let start = kept_by_hand(&dir, "a.bin.part", &whole, &whole, 2 * 1024, chunk);
    let (saved, _) = resume(start, sha(&whole).0.to_vec(), 2 * 1024).await;
    assert_eq!(saved.unwrap().digest, sha(&whole));
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

/// What the rule may not keep, it may not keep of a resumed save either,
/// which has whole chunks from its first moment.
#[tokio::test]
async fn a_resumed_save_keeps_nothing_after_a_cancel_a_wrong_hash_or_a_matched_one() {
    let to = temp("resume-never-kept");
    let dir = Directory::open(&to).unwrap();
    let bytes = data(3 * 1024 + 100);
    let size = bytes.len() as u64;
    let always = |_: &ChunkError| true;
    let good = rest(&bytes, 1024);
    let start = || kept_by_hand(&dir, "a.bin.part", &bytes, &bytes[..1024], 1024, 1024);

    // The rule is asked, and keeps a cut.
    let hooks = Wholes::default();
    let options = SaveOptions::default();
    let stopped = save_resumable(
        &mut &good[..10],
        Target {
            dir: &dir,
            name: "a.bin",
            size,
        },
        start(),
        &always,
        &options,
        &hooks,
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert_eq!(stopped.kept.unwrap().have(), 1024);
    std::fs::remove_file(to.join("a.bin.part")).unwrap();

    // A wrong hash.
    let mut wrong = good.clone();
    wrong[5] ^= 1;
    let hooks = Wholes::default();
    let stopped = save_resumable(
        &mut &wrong[..],
        Target {
            dir: &dir,
            name: "a.bin",
            size,
        },
        start(),
        &always,
        &options,
        &hooks,
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert_eq!(stopped.error, ChunkError::Damaged);
    assert!(stopped.kept.is_none());
    assert_eq!(*hooks.fates.0.lock().unwrap(), [PartialFate::Removed]);
    assert!(files_in(&to).is_empty());

    // A cancel, once the next whole chunk is written.
    let cancel = Arc::new(tokio::sync::Notify::new());
    let hooks = Wholes {
        cancel: Some(cancel.clone()),
        ..Wholes::default()
    };
    let (mut sending, mut receiving) = tokio::io::duplex(1 << 16);
    let sender = async {
        sending.write_all(&good[..1024 + 10]).await.unwrap();
        std::future::pending::<()>().await;
    };
    let saving = save_resumable(
        &mut receiving,
        Target {
            dir: &dir,
            name: "a.bin",
            size,
        },
        start(),
        &always,
        &options,
        &hooks,
        cancel.notified(),
    );
    let stopped = tokio::select! {
        stopped = saving => stopped.unwrap_err(),
        () = sender => unreachable!(),
    };
    assert_eq!(stopped.error, ChunkError::Cancelled);
    assert!(stopped.kept.is_none());
    assert!(files_in(&to).is_empty());

    // A hook that refuses after the hash matched: whole and checked, not kept.
    let hooks = Recorder::failing("before_publish");
    let stopped = save_resumable(
        &mut &good[..],
        Target {
            dir: &dir,
            name: "a.bin",
            size,
        },
        start(),
        &always,
        &options,
        &hooks,
        std::future::pending(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        stopped.error,
        ChunkError::Hook(io::ErrorKind::PermissionDenied)
    );
    assert!(stopped.kept.is_none());
    assert_eq!(
        hooks.steps().last().unwrap(),
        "ended Err(Hook(PermissionDenied)) Removed"
    );
    assert!(files_in(&to).is_empty());
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

/// A kept partial cut again keeps what it had and the chunks that came.
#[tokio::test]
async fn a_resumed_save_that_is_cut_keeps_its_partial_again() {
    let to = temp("resume-again");
    let dir = Directory::open(&to).unwrap();
    let bytes = data(4 * 1024 + 9);
    let size = bytes.len() as u64;
    let more = rest(&bytes, 1024);
    let mut start = kept_by_hand(&dir, "a.bin.part", &bytes, &bytes[..1024], 1024, 1024);
    // Cut at once, then a chunk and a half later.
    for (cut, have) in [(0, 1024), (1024 + 512, 2 * 1024)] {
        let stopped = save_resumable(
            &mut &more[..cut],
            Target {
                dir: &dir,
                name: "a.bin",
                size,
            },
            start,
            &cut_short,
            &SaveOptions::default(),
            &(),
            std::future::pending(),
        )
        .await
        .unwrap_err();
        let kept = stopped.kept.expect("kept again");
        assert_eq!(kept.have(), have);
        assert_eq!(kept.prefix(), sha(&bytes[..have as usize]));
        assert_eq!(kept.file.metadata().unwrap().len(), have);
        start = Start::Resume(kept);
    }
    let more = rest(&bytes, 2 * 1024);
    let saved = save_resumable(
        &mut &more[..],
        Target {
            dir: &dir,
            name: "a.bin",
            size,
        },
        start,
        &cut_short,
        &SaveOptions::default(),
        &(),
        std::future::pending(),
    )
    .await
    .unwrap();
    assert_eq!(saved.digest, sha(&bytes));
    assert_eq!(std::fs::read(to.join("a.bin")).unwrap(), bytes);
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

/// Hooks that leave the file's position somewhere else each time.
struct Wandering;

impl Hooks for Wandering {
    fn created(&self, mut file: &File, _: &OsStr) -> io::Result<()> {
        io::Seek::seek(&mut file, io::SeekFrom::Start(77)).map(|_| ())
    }

    fn whole(&self, mut file: &File, _: u64, _: Digest) -> io::Result<()> {
        let mut first = [0; 16];
        io::Seek::seek(&mut file, io::SeekFrom::Start(3))?;
        io::Read::read_exact(&mut file, &mut first)
    }
}

/// The digest is of the stream, so only the file's bytes show a chunk
/// that landed in the wrong place.
#[tokio::test]
async fn a_hook_that_moves_the_files_position_moves_no_byte() {
    let to = temp("wandering");
    let dir = Directory::open(&to).unwrap();
    let c = CHUNK as usize;
    let bytes = data(2 * c + 9);
    let stream = framed(&bytes).await;
    let target = Target {
        dir: &dir,
        name: "a.bin",
        size: bytes.len() as u64,
    };
    let options = SaveOptions::default();
    let keep = |_: &ChunkError| false;
    let pending = std::future::pending();
    let saved = save_resumable(
        &mut &stream[..],
        target,
        Start::New,
        &keep,
        &options,
        &Wandering,
        pending,
    )
    .await
    .unwrap();
    assert_eq!(saved.digest, sha(&bytes));
    assert!(std::fs::read(to.join("a.bin")).unwrap() == bytes);
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

/// A clone of the file kept from `created`, and moved to the file's start
/// by another thread before every slice is written, while the bytes come.
#[derive(Default)]
struct Meddling(Mutex<Option<File>>);

impl Hooks for Meddling {
    fn created(&self, file: &File, _: &OsStr) -> io::Result<()> {
        *self.0.lock().unwrap() = Some(file.try_clone()?);
        Ok(())
    }
}

impl Pace for Meddling {
    fn before_write(&self, _: usize) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let kept = self.0.lock().unwrap().as_ref().map(File::try_clone);
        let mut kept = kept.expect("created").unwrap();
        // Joined, so the position is moved before the slice is written.
        let moved = std::thread::spawn(move || io::Seek::seek(&mut kept, io::SeekFrom::Start(0)));
        moved.join().unwrap().unwrap();
        Box::pin(std::future::ready(()))
    }
}

#[tokio::test]
async fn a_clone_of_the_file_moved_on_another_thread_moves_no_byte() {
    let to = temp("meddling");
    let dir = Directory::open(&to).unwrap();
    let c = CHUNK as usize;
    let bytes = data(c + 3 * SLICE + 9);
    let stream = framed(&bytes).await;
    let target = Target {
        dir: &dir,
        name: "a.bin",
        size: bytes.len() as u64,
    };
    let meddling = Meddling::default();
    let options = SaveOptions {
        pace: &meddling,
        ..SaveOptions::default()
    };
    let keep = |_: &ChunkError| false;
    let pending = std::future::pending();
    let saved = save_resumable(
        &mut &stream[..],
        target,
        Start::New,
        &keep,
        &options,
        &meddling,
        pending,
    )
    .await
    .unwrap();
    assert_eq!(saved.digest, sha(&bytes));
    assert!(std::fs::read(to.join("a.bin")).unwrap() == bytes);
    drop((meddling, dir));
    std::fs::remove_dir_all(&to).unwrap();
}

/// A write runs on the blocking pool after the call that asked for it
/// returned: its error is not lost, and comes with the flush.
#[tokio::test]
async fn a_placed_write_the_system_refuses_fails_the_flush() {
    let to = temp("placed-refused");
    std::fs::write(to.join("a.part"), b"kept").unwrap();
    // Open for reading only: every write through it is refused.
    let file = File::open(to.join("a.part")).unwrap();
    let mut writer = crate::save::Placed::new(&file, 0).unwrap();
    assert!(writer.write_all(b"more").await.is_ok(), "handed on");
    assert!(writer.flush().await.is_err(), "refused");
    assert!(writer.flush().await.is_ok(), "told once");
    drop((writer, file));
    assert_eq!(std::fs::read(to.join("a.part")).unwrap(), b"kept");
    std::fs::remove_dir_all(&to).unwrap();
}

/// A kept partial with no byte kept, its handle left past the start.
#[tokio::test]
async fn a_resumed_save_from_zero_writes_from_the_files_first_byte() {
    let to = temp("resume-zero");
    let dir = Directory::open(&to).unwrap();
    let bytes = data(2 * 1024 + 5);
    let start = kept_by_hand(&dir, "a.bin.part", &bytes, b"left from before", 0, 1024);
    let target = Target {
        dir: &dir,
        name: "a.bin",
        size: bytes.len() as u64,
    };
    let more = rest(&bytes, 0);
    let options = SaveOptions::default();
    let pending = std::future::pending();
    let saved = save_resumable(
        &mut &more[..],
        target,
        start,
        &cut_short,
        &options,
        &(),
        pending,
    )
    .await
    .unwrap();
    assert_eq!(saved.digest, sha(&bytes));
    assert_eq!(std::fs::read(to.join("a.bin")).unwrap(), bytes);
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

/// One resumed save of `stream` over a partial of one 1024-byte chunk of
/// `bytes`, under a rule that keeps every error.
async fn resumed_keeping_all(
    dir: &Directory,
    bytes: &[u8],
    start: Start,
    stream: &[u8],
    hooks: &Wholes,
) -> Stopped {
    let target = Target {
        dir,
        name: "a.bin",
        size: bytes.len() as u64,
    };
    let always = |_: &ChunkError| true;
    let options = SaveOptions::default();
    let pending = std::future::pending();
    save_resumable(
        &mut &stream[..],
        target,
        start,
        &always,
        &options,
        hooks,
        pending,
    )
    .await
    .unwrap_err()
}

#[tokio::test]
async fn a_write_that_fails_within_a_chunk_keeps_the_whole_chunks_before_it() {
    let to = temp("write-fails");
    let dir = Directory::open(&to).unwrap();
    let bytes = data(4 * 1024 + 100);
    let good = rest(&bytes, 1024);
    let start = kept_by_hand(&dir, "a.bin.part", &bytes, &bytes[..1024], 1024, 1024);
    // The disk fills halfway through the fourth chunk.
    crate::seams::WRITE_ERROR_AT.set(Some(3 * 1024 + 512));
    let hooks = Wholes::default();
    let stopped = resumed_keeping_all(&dir, &bytes, start, &good, &hooks).await;
    assert_eq!(crate::seams::WRITE_ERROR_AT.get(), None, "the write failed");
    assert_eq!(stopped.error, ChunkError::Io(io::ErrorKind::StorageFull));
    let kept = stopped.kept.expect("the rule keeps every error");
    assert_eq!(kept.have(), 3 * 1024);
    assert_eq!(kept.prefix(), sha(&bytes[..3 * 1024]));
    assert_eq!(kept.file.metadata().unwrap().len(), kept.have());
    assert_eq!(
        *hooks.fates.0.lock().unwrap(),
        [PartialFate::Kept { have: 3 * 1024 }]
    );
    assert_eq!(
        std::fs::read(to.join("a.bin.part")).unwrap(),
        bytes[..3 * 1024]
    );
    drop((kept, dir));
    std::fs::remove_dir_all(&to).unwrap();
}

#[tokio::test]
async fn a_partial_that_cannot_be_cut_back_is_removed() {
    let to = temp("cut-fails");
    let dir = Directory::open(&to).unwrap();
    let bytes = data(4 * 1024 + 100);
    let good = rest(&bytes, 1024);
    let start = kept_by_hand(&dir, "a.bin.part", &bytes, &bytes[..1024], 1024, 1024);
    crate::seams::CUT_ERROR.set(true);
    let hooks = Wholes::default();
    let stopped = resumed_keeping_all(&dir, &bytes, start, &good[..1024 + 512], &hooks).await;
    assert!(!crate::seams::CUT_ERROR.get(), "the cut was tried");
    assert!(matches!(stopped.error, ChunkError::CutShort { .. }));
    assert!(stopped.kept.is_none(), "half a chunk past its hash");
    assert_eq!(*hooks.fates.0.lock().unwrap(), [PartialFate::Removed]);
    assert!(files_in(&to).is_empty());
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

/// Whole and checked, it has no stream left to continue: the flush and
/// the publish are not the rule's to keep, as the hook before them is not.
#[tokio::test]
async fn a_flush_or_a_publish_that_fails_after_the_hash_matched_keeps_nothing() {
    let to = temp("after-match");
    let dir = Directory::open(&to).unwrap();
    let bytes = data(3 * 1024 + 100);
    let good = rest(&bytes, 1024);
    for (step, kind) in [
        ("flush", io::ErrorKind::StorageFull),
        ("publish", io::ErrorKind::PermissionDenied),
    ] {
        let start = kept_by_hand(&dir, "a.bin.part", &bytes, &bytes[..1024], 1024, 1024);
        let seam = if step == "flush" {
            &crate::seams::SYNC_ERROR
        } else {
            &crate::seams::PUBLISH_ERROR
        };
        seam.set(true);
        let hooks = Wholes::default();
        let stopped = resumed_keeping_all(&dir, &bytes, start, &good, &hooks).await;
        assert!(!seam.get(), "{step}: reached");
        assert_eq!(stopped.error, ChunkError::Io(kind), "{step}");
        assert!(stopped.kept.is_none(), "{step}");
        assert_eq!(hooks.seen().len(), 2, "{step}: whole chunks were there");
        assert_eq!(
            *hooks.fates.0.lock().unwrap(),
            [PartialFate::Removed],
            "{step}"
        );
        assert!(files_in(&to).is_empty(), "{step}");
    }
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

/// Nothing shows which of its bytes are the file's.
#[tokio::test]
async fn a_short_partial_and_a_wrong_offset_are_removed_whatever_the_rule_says() {
    let to = temp("unfit-kept");
    let dir = Directory::open(&to).unwrap();
    let bytes = data(3 * 1024 + 100);
    let good = rest(&bytes, 2 * 1024);
    // Shorter than its hasher says; a hasher off a chunk.
    for (on_disk, have) in [(2 * 1024 - 1, 2 * 1024), (2 * 1024, 2 * 1024 - 1)] {
        let start = kept_by_hand(&dir, "a.bin.part", &bytes, &bytes[..on_disk], have, 1024);
        let hooks = Wholes::default();
        let stopped = resumed_keeping_all(&dir, &bytes, start, &good, &hooks).await;
        assert!(
            matches!(
                stopped.error,
                ChunkError::PartialShort { .. } | ChunkError::Offset { .. }
            ),
            "{:?}",
            stopped.error
        );
        assert!(stopped.kept.is_none(), "{have}");
        assert_eq!(*hooks.fates.0.lock().unwrap(), [PartialFate::Removed]);
        assert!(files_in(&to).is_empty(), "{have}");
    }
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

/// The chunk the hook refused is written and hashed: it is kept with the
/// ones before it.
#[tokio::test]
async fn an_error_from_whole_that_the_rule_keeps_keeps_the_chunk_it_was_told_of() {
    let to = temp("whole-kept");
    let dir = Directory::open(&to).unwrap();
    let bytes = data(4 * 1024 + 100);
    let good = rest(&bytes, 1024);
    let start = kept_by_hand(&dir, "a.bin.part", &bytes, &bytes[..1024], 1024, 1024);
    let hooks = Wholes {
        fail_at: Some(3 * 1024),
        ..Wholes::default()
    };
    let stopped = resumed_keeping_all(&dir, &bytes, start, &good, &hooks).await;
    assert_eq!(stopped.error, ChunkError::Hook(io::ErrorKind::StorageFull));
    let kept = stopped.kept.expect("the rule keeps a hook's error");
    assert_eq!(kept.have(), 3 * 1024);
    assert_eq!(kept.prefix(), sha(&bytes[..3 * 1024]));
    assert_eq!(kept.file.metadata().unwrap().len(), 3 * 1024);
    assert_eq!(hooks.seen().len(), 2, "no chunk after the one refused");
    assert_eq!(
        *hooks.fates.0.lock().unwrap(),
        [PartialFate::Kept { have: 3 * 1024 }]
    );
    drop((kept, dir));
    std::fs::remove_dir_all(&to).unwrap();
}

/// A program runs its saves on a runtime's workers.
#[test]
fn a_resumable_save_can_run_on_another_thread() {
    fn sendable<T: Send>(_: &T) {}
    let dir = Directory::open(&temp("send")).unwrap();
    let mut reader: &[u8] = &[];
    let options = SaveOptions::default();
    let saving = save_resumable(
        &mut reader,
        Target {
            dir: &dir,
            name: "a.bin",
            size: 0,
        },
        Start::New,
        &cut_short,
        &options,
        &(),
        std::future::pending(),
    );
    sendable(&saving);
}

/// One slice of a file on a stream that stays open, and a save of it
/// polled once: the slice is handed to the blocking pool, and the save
/// waits for the next.
macro_rules! one_slice_in {
    ($to:expr, $bytes:expr, $dir:ident, $save:ident) => {
        let $dir = Directory::open($to).unwrap();
        let (mut sending, mut receiving) = tokio::io::duplex(SLICE);
        sending.write_all(&$bytes[..SLICE]).await.unwrap();
        let target = Target {
            dir: &$dir,
            name: "a.bin",
            size: $bytes.len() as u64,
        };
        let options = SaveOptions::default();
        let keep = |_: &ChunkError| false;
        let pending = std::future::pending();
        let mut $save = Box::pin(save_resumable(
            &mut receiving,
            target,
            Start::New,
            &keep,
            &options,
            &(),
            pending,
        ));
        let polled = std::future::poll_fn(|cx| Poll::Ready($save.as_mut().poll(cx))).await;
        assert!(polled.is_pending(), "its stream is still open");
    };
}

/// A write on the blocking pool does not end with the future that asked
/// for it. One that had not started when its save was dropped writes
/// nothing, however late it runs: a save continuing the partial by then
/// has no byte of the dropped one written over its own.
#[tokio::test]
async fn a_write_still_waiting_when_its_save_is_dropped_writes_nothing() {
    use crate::seams::Step;
    use std::sync::mpsc;
    let to = temp("dropped-waiting");
    let bytes = data(3 * SLICE);
    let (heard, steps) = mpsc::channel();
    let (release, go) = mpsc::channel::<()>();
    let go = Mutex::new(go);
    crate::seams::WRITES.set(Some(Arc::new(move |step| {
        let _ = heard.send(step);
        // Held before it starts, as a busy pool holds it; let go when
        // the test drops `release`.
        if step == Step::Queued {
            let _ = go.lock().unwrap().recv();
        }
    })));
    one_slice_in!(&to, bytes, dir, save);
    assert_eq!(steps.recv().unwrap(), Step::Queued);
    drop(save);
    assert_eq!(steps.recv().unwrap(), Step::Closing);
    drop(release);
    assert_eq!(steps.recv().unwrap(), Step::Ended, "it never wrote");
    assert!(steps.try_recv().is_err());
    assert_eq!(std::fs::read(to.join("a.bin.part")).unwrap(), b"");
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

/// Lets a held write go when the test ends, however it ends.
struct Release(Arc<Mutex<Option<std::sync::mpsc::Sender<()>>>>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.lock().unwrap().take();
    }
}

/// A write that was under way when its save was dropped is whole when the
/// drop returns: nothing of it is still to land.
#[tokio::test]
async fn a_write_under_way_when_its_save_is_dropped_is_whole_when_the_drop_returns() {
    use crate::seams::Step;
    use std::sync::mpsc;
    let to = temp("dropped-writing");
    let bytes = data(3 * SLICE);
    let (heard, steps) = mpsc::channel();
    let (release, go) = mpsc::channel::<()>();
    let go = Mutex::new(go);
    let release = Arc::new(Mutex::new(Some(release)));
    let _release = Release(release.clone());
    crate::seams::WRITES.set(Some(Arc::new(move |step| {
        let _ = heard.send(step);
        match step {
            // Held with the gate, before its first byte.
            Step::Writing => {
                let _ = go.lock().unwrap().recv();
            }
            // Let go only once the drop is at the gate.
            Step::Closing => drop(release.lock().unwrap().take()),
            _ => {}
        }
    })));
    one_slice_in!(&to, bytes, dir, save);
    assert_eq!(steps.recv().unwrap(), Step::Queued);
    assert_eq!(steps.recv().unwrap(), Step::Writing);
    drop(save);
    // Read at once, with no wait for the pool.
    assert!(std::fs::read(to.join("a.bin.part")).unwrap() == bytes[..SLICE]);
    assert_eq!(steps.recv().unwrap(), Step::Closing);
    assert_eq!(steps.recv().unwrap(), Step::Ended);
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}

/// The handles this process has open of the file at `path`, where the
/// system lists them (Linux and Android): by the file itself, since a
/// handle opened under the partial name still says that name.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn handles_of(path: &Path) -> usize {
    use std::os::unix::fs::MetadataExt;
    let file = std::fs::metadata(path).unwrap();
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        // One of another test, closed since, is not this file's.
        .filter_map(|fd| std::fs::metadata(fd.ok()?.path()).ok())
        .filter(|open| open.dev() == file.dev() && open.ino() == file.ino())
        .count()
}

/// A save's rules are the folder crate's, each field passed on as it is,
/// and never ask for the published file to be held.
#[test]
fn a_save_s_publish_rules_are_the_folder_s_and_hold_nothing() {
    assert_eq!(Publish::default().rules(), PublishRules::default());
    let rules = Publish {
        mark: true,
        retries: 7,
        retry_wait: std::time::Duration::from_millis(3),
        name_tries: 2,
    };
    assert_eq!(
        rules.rules(),
        PublishRules {
            mark: true,
            retries: 7,
            retry_wait: std::time::Duration::from_millis(3),
            name_tries: 2,
            hold: false,
        }
    );
}

/// When a save returns, the published file is open through nothing of
/// the save: it and its folder can be renamed and removed at once.
#[tokio::test]
async fn a_saved_file_is_held_open_by_nothing_when_the_save_returns() {
    let to = temp("nothing-held");
    std::fs::create_dir(to.join("in")).unwrap();
    let dir = Directory::open(&to.join("in")).unwrap();
    let bytes = data(100_000);
    let stream = framed(&bytes).await;
    for (name, kind) in [("a.bin", 0), ("b.bin", 1), ("c.bin", 2)] {
        let target = Target {
            dir: &dir,
            name,
            size: bytes.len() as u64,
        };
        let options = SaveOptions {
            publish: Publish::default(),
            ..SaveOptions::default()
        };
        let saved = match kind {
            0 => save_verified(
                &mut &stream[..],
                &dir,
                name,
                target.size,
                &options,
                &(),
                std::future::pending(),
            )
            .await
            .unwrap(),
            // The second and third through the resumable save, new and resumed.
            1 => {
                let keep = |_: &ChunkError| true;
                save_resumable(
                    &mut &stream[..],
                    target,
                    Start::New,
                    &keep,
                    &options,
                    &(),
                    std::future::pending(),
                )
                .await
                .unwrap()
            }
            _ => {
                let partial = std::ffi::OsString::from("c.bin.part");
                let kept = Box::new(Kept {
                    file: dir.create(&partial).unwrap(),
                    partial,
                    hasher: Settings::default().receiving(),
                });
                let keep = |_: &ChunkError| true;
                save_resumable(
                    &mut &stream[..],
                    target,
                    Start::Resume(kept),
                    &keep,
                    &options,
                    &(),
                    std::future::pending(),
                )
                .await
                .unwrap()
            }
        };
        assert_eq!(saved.path, to.join("in").join(name));
        #[cfg(any(target_os = "linux", target_os = "android"))]
        assert_eq!(handles_of(&saved.path), 0);
    }
    // Windows renames no folder with a file open below it, and removes
    // no file open without delete sharing, as a save's partial is.
    std::fs::rename(to.join("in"), to.join("moved")).unwrap();
    assert_eq!(files_in(&to.join("moved")), ["a.bin", "b.bin", "c.bin"]);
    for name in ["a.bin", "b.bin", "c.bin"] {
        assert!(std::fs::read(to.join("moved").join(name)).unwrap() == bytes);
        std::fs::remove_file(to.join("moved").join(name)).unwrap();
    }
    assert!(files_in(&to.join("moved")).is_empty());
    drop(dir);
    std::fs::remove_dir_all(&to).unwrap();
}
