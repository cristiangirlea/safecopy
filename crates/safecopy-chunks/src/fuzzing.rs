//! What the fuzz target `framing` runs, kept in the crate so its tests run
//! every saved input through the same code. Built only for the tests and
//! under `cfg(fuzzing)`, which a fuzzer's build sets: an ordinary build has
//! none of it.

use std::future::Future;
use std::num::NonZeroU64;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use crate::{
    ChunkError, Hasher, Settings, copy_exact, read_end, read_meta, read_trailer, read_version,
    write_meta, write_trailer, write_version,
};

/// The longest metadata block the target's settings take.
const META: usize = 64;

/// Runs a future over in-memory streams, which never wait.
fn run<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(out) => out,
        Poll::Pending => panic!("an in-memory stream waited"),
    }
}

/// Small limits, so the fuzzer reaches them.
fn settings() -> Settings {
    Settings {
        meta_bytes: META as u32,
        ..Settings::default()
    }
}

/// A hasher keeping `keep` boundaries of a small chunk, so the fuzzer
/// crosses them.
fn hasher(keep: usize) -> Hasher {
    Hasher::with_chunk(keep, NonZeroU64::new(256).unwrap())
}

/// Bytes read as one stream of a file `size` bytes long.
fn read(stream: &[u8], size: u64) -> Result<(Vec<u8>, Vec<u8>), ChunkError> {
    let settings = settings();
    let mut reader = stream;
    run(async {
        read_version(&mut reader).await?;
        let meta = read_meta(&mut reader, &settings).await?;
        assert!(meta.len() <= settings.meta_bytes as usize);
        let mut file = Vec::new();
        let mut hasher = hasher(settings.receiver_keep);
        let copied = copy_exact(&mut reader, &mut file, size, 0, &mut hasher, &(), &()).await;
        // Nothing past the size, whatever came.
        assert!(file.len() as u64 <= size);
        copied?;
        assert_eq!(file.len() as u64, size);
        let trailer = read_trailer(&mut reader).await?;
        read_end(&mut reader).await?;
        if hasher.finish() != trailer {
            return Err(ChunkError::Damaged);
        }
        Ok((meta, file))
    })
}

fn write(meta: &[u8], file: &[u8]) -> Vec<u8> {
    let settings = settings();
    let mut out = Vec::new();
    run(async {
        write_version(&mut out).await?;
        write_meta(&mut out, meta, &settings).await?;
        let mut hasher = hasher(settings.sender_keep);
        let mut source = file;
        let size = file.len() as u64;
        copy_exact(&mut source, &mut out, size, 0, &mut hasher, &(), &()).await?;
        write_trailer(&mut out, &hasher.finish()).await
    })
    .expect("an in-memory stream takes every write");
    out
}

/// What an input is: a stream to read as a file of a size, or a metadata
/// block and a file to write and read back.
enum Shape<'a> {
    Stream { size: u64, stream: &'a [u8] },
    Parts { meta: &'a [u8], file: &'a [u8] },
}

/// The first byte chooses: even, the next two are the size and the rest the
/// stream; odd, the next is the block's length, then the block, then the
/// file. `None` for an input too short for what it chose.
fn input(data: &[u8]) -> Option<Shape<'_>> {
    let (&mode, rest) = data.split_first()?;
    if mode % 2 == 0 {
        let (size, stream) = rest.split_first_chunk::<2>()?;
        let size = u64::from(u16::from_le_bytes(*size));
        Some(Shape::Stream { size, stream })
    } else {
        let (&len, rest) = rest.split_first()?;
        let (meta, file) = rest.split_at_checked(usize::from(len) % (META + 1))?;
        Some(Shape::Parts { meta, file })
    }
}

/// A file's stream as the other side sends it: the framing version, the
/// metadata block, the file's bytes and the trailer. Never a panic, never
/// more than the limits given, and a stream the writer wrote reads back
/// equal.
pub fn framing(data: &[u8]) {
    match input(data) {
        None => {}
        Some(Shape::Stream { size, stream }) => {
            if let Ok((meta, file)) = read(stream, size) {
                // Taken only when exactly the framing came.
                assert_eq!(stream.len(), 1 + 4 + meta.len() + file.len() + 32);
                assert_eq!(write(&meta, &file), stream);
            }
        }
        Some(Shape::Parts { meta, file }) => {
            // What the writer writes, the reader reads back equal.
            let stream = write(meta, file);
            let (read_meta, read_file) = read(&stream, file.len() as u64).expect("written reads");
            assert_eq!(read_meta, meta);
            assert_eq!(read_file, file);
            // One byte cut or one more is refused.
            if let Some((_, cut)) = stream.split_last() {
                assert!(read(cut, file.len() as u64).is_err());
            }
            let mut longer = stream.clone();
            longer.push(0);
            assert_eq!(read(&longer, file.len() as u64), Err(ChunkError::TooLong));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    /// A seed or a saved input: its file's name and its bytes.
    type Input = (String, Vec<u8>);

    /// One fuzz target: what it runs, whether bytes are a whole input of
    /// the format as it is now, the seeds the format writes, and the seed
    /// a test cuts short.
    struct Target {
        name: &'static str,
        run: fn(&[u8]),
        whole: fn(&[u8]) -> bool,
        seeds: fn() -> Vec<Input>,
        cut: &'static str,
    }

    const TARGETS: &[Target] = &[Target {
        name: "framing",
        run: framing,
        whole,
        seeds,
        cut: "stream-small",
    }];

    /// A stream that reads whole as a file of its size, or parts to write.
    fn whole(bytes: &[u8]) -> bool {
        match input(bytes) {
            None => false,
            Some(Shape::Stream { size, stream }) => read(stream, size).is_ok(),
            Some(Shape::Parts { .. }) => true,
        }
    }

    /// `len` bytes, each differing from the one before.
    fn bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31)).collect()
    }

    /// Streams as the writer writes them, and parts for it to write: no
    /// metadata and the longest block, an empty file, and files that end
    /// before, on and after the small chunk's boundary.
    fn seeds() -> Vec<Input> {
        let parts: Vec<(&str, Vec<u8>, Vec<u8>)> = vec![
            ("empty", Vec::new(), Vec::new()),
            ("small", b"meta".to_vec(), b"ten bytes!".to_vec()),
            ("longest-meta", bytes(META), bytes(255)),
            ("one-chunk", bytes(7), bytes(256)),
            ("past-a-chunk", Vec::new(), bytes(300)),
            ("two-chunks", bytes(1), bytes(512)),
        ];
        let mut seeds = Vec::new();
        for (name, meta, file) in parts {
            let mut stream = vec![0];
            stream.extend_from_slice(&(file.len() as u16).to_le_bytes());
            stream.extend_from_slice(&write(&meta, &file));
            seeds.push((format!("stream-{name}"), stream));
            let mut parts = vec![1, meta.len() as u8];
            parts.extend_from_slice(&meta);
            parts.extend_from_slice(&file);
            seeds.push((format!("parts-{name}"), parts));
        }
        seeds
    }

    /// `fuzz/<kind>/<target>` in the repository.
    fn folder(kind: &str, target: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fuzz")
            .join(kind)
            .join(target)
    }

    /// The inputs a folder holds, by name. A file whose name starts with a
    /// dot is none: the one that keeps an empty folder, or a system's own.
    fn saved(dir: &Path) -> Vec<Input> {
        let mut all: Vec<Input> = std::fs::read_dir(dir)
            .expect("the folder")
            .map(|entry| entry.expect("an entry"))
            .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                (name, std::fs::read(entry.path()).expect("an input"))
            })
            .collect();
        all.sort();
        all
    }

    /// The first of `inputs` that is not a whole input now.
    fn stale(inputs: &[Input], whole: fn(&[u8]) -> bool) -> Option<&str> {
        let found = inputs.iter().find(|(_, bytes)| !whole(bytes));
        found.map(|(name, _)| name.as_str())
    }

    const WRITE: &str = "write them again: cargo test --workspace write_fuzz_seeds -- --ignored";

    #[test]
    fn the_seeds_are_what_the_format_writes_now() {
        for target in TARGETS {
            let saved = saved(&folder("seeds", target.name));
            assert!(!saved.is_empty(), "{}: no seeds", target.name);
            assert_eq!(
                stale(&saved, target.whole),
                None,
                "{}: {WRITE}",
                target.name
            );
            let mut made = (target.seeds)();
            made.sort();
            assert!(saved == made, "{}: {WRITE}", target.name);
        }
    }

    #[test]
    fn a_seed_cut_short_is_found() {
        for target in TARGETS {
            let mut copy = saved(&folder("seeds", target.name));
            let seed = copy.iter_mut().find(|(name, _)| name == target.cut);
            seed.expect("the seed").1.pop();
            assert_eq!(stale(&copy, target.whole), Some(target.cut));
        }
    }

    #[test]
    fn every_saved_input_runs_as_the_fuzz_target_runs_it() {
        for target in TARGETS {
            let mut inputs = saved(&folder("seeds", target.name));
            inputs.extend(saved(&folder("regressions", target.name)));
            for (name, bytes) in &inputs {
                let ran = std::panic::catch_unwind(|| (target.run)(bytes));
                assert!(ran.is_ok(), "{}: the input {name}", target.name);
            }
        }
    }

    #[test]
    #[ignore = "writes fuzz/seeds: run when a format changes"]
    fn write_fuzz_seeds() {
        for target in TARGETS {
            let dir = folder("seeds", target.name);
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("the folder");
            for (name, bytes) in (target.seeds)() {
                std::fs::write(dir.join(name), bytes).expect("a seed");
            }
        }
    }
}
