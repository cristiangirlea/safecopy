//! A file's stream as the other side sends it: the framing version, the
//! metadata block, the file's bytes and the trailer. Never a panic, never
//! more than the limits given, and a stream the writer wrote reads back
//! equal.
#![no_main]

use std::future::Future;
use std::num::NonZeroU64;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use libfuzzer_sys::arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use safecopy::chunks::{
    ChunkError, Hasher, Settings, copy_exact, read_end, read_meta, read_trailer, read_version,
    write_meta, write_trailer, write_version,
};

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
        meta_bytes: 64,
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

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    if mode % 2 == 0 {
        // Arbitrary bytes, as a stream of a file of the size the first two give.
        let Some((size, stream)) = rest.split_first_chunk::<2>() else {
            return;
        };
        let size = u64::from(u16::from_le_bytes(*size));
        if let Ok((meta, file)) = read(stream, size) {
            // Taken only when exactly the framing came.
            assert_eq!(stream.len(), 1 + 4 + meta.len() + file.len() + 32);
            assert_eq!(write(&meta, &file), stream);
        }
    } else {
        // What the writer writes, the reader reads back equal.
        let mut input = Unstructured::new(rest);
        let Ok(meta_len) = input.int_in_range(0..=64usize) else {
            return;
        };
        let Ok(meta) = input.bytes(meta_len) else {
            return;
        };
        let file = input.take_rest();
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
        assert_eq!(
            read(&longer, file.len() as u64),
            Err(ChunkError::TooLong)
        );
    }
});
