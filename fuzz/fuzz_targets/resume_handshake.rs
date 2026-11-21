//! The resume exchange as the other side sends it: `handshake::offer` and
//! `handshake::answer` fed arbitrary bytes over an in-memory stream, then
//! `receiver_takes` and `sender_checks` with arbitrary records and lists
//! against a small real file. Never a panic, never more than the limits
//! given; never a continue or an append from an offset off a chunk or past
//! the size, and never an append when `same` differs.
#![no_main]

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use libfuzzer_sys::arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use safecopy::chunks::{CHUNK, Digest};
use safecopy::folder::FileId;
use safecopy::journal::{Location, PartialAt, Root};
use safecopy::resume::handshake::{self, Reply};
use safecopy::resume::{
    Held, Limits, Next, PeerId, Receiving, Resumable, Sending, Source, Take, TransferKey,
    receiver_takes, sender_checks,
};
use tokio::io::AsyncWriteExt;

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime")
    })
}

/// A small real file the sender's checks open, the same for every run.
fn file() -> &'static PathBuf {
    static FILE: OnceLock<PathBuf> = OnceLock::new();
    FILE.get_or_init(|| {
        let path = std::env::temp_dir().join(format!("safecopy-fuzz-{}", std::process::id()));
        std::fs::File::create(&path)
            .and_then(|mut f| f.write_all(&[7; 300]))
            .expect("a temporary file");
        path
    })
}

fn limits() -> Limits {
    Limits {
        per_peer: 8,
        ..Limits::default()
    }
}

/// `bytes` from the other side, then the end of what it sends; its end
/// stays open, with room for every reply, until both are dropped.
async fn duplex_with(bytes: &[u8]) -> (tokio::io::DuplexStream, tokio::io::DuplexStream) {
    let (ours, mut theirs) = tokio::io::duplex(1 << 20);
    theirs.write_all(bytes).await.expect("room for it");
    theirs.shutdown().await.expect("an in-memory stream ends");
    (ours, theirs)
}

#[derive(Debug)]
struct Checks {
    have: u64,
    prefix: [u8; 32],
    record_have: u64,
    size: u64,
    from: u64,
    same: Vec<u8>,
    stream_same: Vec<u8>,
    chunks: u8,
    record_size_matches: bool,
}

fn checks(u: &mut Unstructured<'_>) -> libfuzzer_sys::arbitrary::Result<Checks> {
    // Offsets near whole chunks as often as anywhere.
    let offset = |u: &mut Unstructured<'_>| -> libfuzzer_sys::arbitrary::Result<u64> {
        Ok(if bool::arbitrary(u)? {
            u64::from(u8::arbitrary(u)? % 4) * CHUNK
        } else {
            u64::arbitrary(u)?
        })
    };
    Ok(Checks {
        have: offset(u)?,
        prefix: <[u8; 32]>::arbitrary(u)?,
        record_have: offset(u)?,
        size: offset(u)?,
        from: offset(u)?,
        same: Vec::arbitrary(u)?,
        stream_same: Vec::arbitrary(u)?,
        chunks: u8::arbitrary(u)?,
        record_size_matches: bool::arbitrary(u)?,
    })
}

fn receiving(input: &Checks) -> Receiving {
    Receiving {
        peer: PeerId([1; 32]),
        key: TransferKey([2; 16]),
        same: input.same.clone(),
        size: input.size,
        at: PartialAt {
            location: Location {
                root: Root::Unix {
                    path: b"/nowhere".to_vec(),
                    device: 0,
                    inode: 0,
                },
                levels: Vec::new(),
            },
            partial: "f.part".into(),
            file: 0,
        },
        have: input.record_have,
        prefix: Digest(input.prefix),
        created: 0,
        last: 0,
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&mode, rest)) = data.split_first() else {
        return;
    };
    let limits = limits();
    match mode % 3 {
        0 => runtime().block_on(async {
            // The sender reads what the receiver says it holds.
            let (mut stream, _theirs) = duplex_with(rest).await;
            let checked = std::cell::Cell::new(0usize);
            let offered = handshake::offer(&mut stream, &limits, async |held: &Held| {
                checked.set(checked.get() + 1);
                Next::Continue { from: held.have }
            })
            .await;
            if let Ok(replies) = offered {
                assert!(replies.len() <= limits.per_peer);
                assert_eq!(replies.len(), checked.get());
                for (held, reply) in replies {
                    if let Reply::Continue { from } = reply {
                        assert_eq!(from, held.have);
                    }
                }
            } else {
                assert!(checked.get() <= limits.per_peer);
            }
        }),
        1 => runtime().block_on(async {
            // The receiver reads the sender's replies to its list.
            let held = [
                Held {
                    key: TransferKey([3; 16]),
                    have: CHUNK,
                    prefix: Digest([4; 32]),
                },
                Held {
                    key: TransferKey([5; 16]),
                    have: 2 * CHUNK,
                    prefix: Digest([6; 32]),
                },
            ];
            let (mut stream, _theirs) = duplex_with(rest).await;
            if let Ok(replies) = handshake::answer(&mut stream, &held, &limits).await {
                assert_eq!(replies.len(), held.len());
                for (one, reply) in replies {
                    if let Reply::Continue { from } = reply {
                        assert_eq!(from, one.have, "a continue only from what is held");
                    }
                }
            }
        }),
        _ => {
            let mut input = Unstructured::new(rest);
            let Ok(checks) = checks(&mut input) else {
                return;
            };
            // The receiver's check of a stream.
            let record = receiving(&checks);
            let stream = Resumable {
                key: record.key,
                from: checks.from,
                size: if checks.record_size_matches {
                    record.size
                } else {
                    checks.size ^ 1
                },
            };
            for kept in [Some(&record), None] {
                let take = receiver_takes(kept, &stream, &checks.stream_same);
                if take == Take::Append {
                    assert!(stream.from <= stream.size);
                    assert!(stream.from.is_multiple_of(CHUNK) && stream.from > 0);
                    assert_eq!(
                        checks.same, checks.stream_same,
                        "never an append when same differs"
                    );
                    assert_eq!(Some(stream.from), kept.map(|r| r.have));
                }
            }
            // The sender's check of a small real file.
            let opened =
                Source::read(std::fs::File::open(file()).expect("the file")).expect("its metadata");
            let sending = Sending {
                peer: PeerId([1; 32]),
                key: TransferKey([2; 16]),
                path: file().clone(),
                file: if checks.chunks % 2 == 0 {
                    opened.id
                } else {
                    FileId { volume: 1, file: 1 }
                },
                size: opened.size,
                modified: opened.modified,
                created: 0,
                last: 0,
            };
            let held = Held {
                key: sending.key,
                have: checks.have,
                prefix: Digest(checks.prefix),
            };
            let next =
                runtime().block_on(sender_checks(Some(&sending), Some(&opened), &held, None));
            if let Ok(Next::Continue { from }) = next {
                assert!(from <= opened.size && from.is_multiple_of(CHUNK));
                assert_eq!(from, checks.have);
            }
        }
    }
});
