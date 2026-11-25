//! What the fuzz target `resume_handshake` runs, kept in the crate so its
//! tests run every saved input through the same code. Built only for the
//! tests and under `cfg(fuzzing)`, which a fuzzer's build sets: an ordinary
//! build has none of it.

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use safecopy_folder::FileId;
use safecopy_journal::{Location, PartialAt, Root, Second};
use tokio::io::AsyncWriteExt;

use crate::handshake::{self, Reply};
use crate::wire::Slice;
use crate::{
    CHUNK, Digest, Held, Limits, Next, PeerId, Receiving, Resumable, ResumeError, Sending, Source,
    Take, TransferKey, receiver_takes, sender_checks,
};

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime")
    })
}

/// The folder the target's file is made in: the system's temporary one in
/// the fuzzer; the build's in the tests, where their other files are.
fn scratch() -> PathBuf {
    #[cfg(not(test))]
    return std::env::temp_dir();
    #[cfg(test)]
    {
        // The test binary is in target/<profile>/deps.
        let exe = std::env::current_exe().expect("this program");
        let target = exe.ancestors().nth(3).expect("the build's folder");
        let dir = target.join("test-tmp");
        std::fs::create_dir_all(&dir).expect("a folder");
        dir
    }
}

/// A small real file the sender's checks open, the same for every run.
fn file() -> &'static PathBuf {
    static FILE: OnceLock<PathBuf> = OnceLock::new();
    FILE.get_or_init(|| {
        let path = scratch().join(format!("safecopy-fuzz-{}", std::process::id()));
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

/// What the receiver holds when it reads the sender's replies.
fn held() -> [Held; 2] {
    [
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
    ]
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
    other_file: bool,
    record_size_matches: bool,
}

/// An offset: a byte chooses a count of whole chunks in the next byte, or
/// any number in the next eight, so one near a chunk comes as often as any.
fn offset(input: &mut Slice<'_>) -> Result<u64, ResumeError> {
    Ok(if input.byte()? % 2 == 1 {
        u64::from(input.byte()? % 4) * CHUNK
    } else {
        u64::from_le_bytes(input.array()?)
    })
}

/// A few bytes: their count, at most eight, then themselves.
fn few(input: &mut Slice<'_>) -> Result<Vec<u8>, ResumeError> {
    let len = usize::from(input.byte()? % 9);
    Ok(input.take(len)?.to_vec())
}

/// The checks' values, in the order of the fields; refused when the input
/// ends before the last.
fn checks(input: &mut Slice<'_>) -> Result<Checks, ResumeError> {
    Ok(Checks {
        have: offset(input)?,
        prefix: input.array()?,
        record_have: offset(input)?,
        size: offset(input)?,
        from: offset(input)?,
        same: few(input)?,
        stream_same: few(input)?,
        other_file: input.byte()? % 2 == 1,
        record_size_matches: input.byte()? % 2 == 1,
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
            second: Second::None,
        },
        have: input.record_have,
        prefix: Digest(input.prefix),
        created: 0,
        last: 0,
    }
}

/// The sender reads what the receiver says it holds; whether it took it.
async fn offers(bytes: &[u8]) -> bool {
    let limits = limits();
    let (mut stream, _theirs) = duplex_with(bytes).await;
    let checked = std::cell::Cell::new(0usize);
    let offered = handshake::offer(&mut stream, &limits, async |held: &Held| {
        checked.set(checked.get() + 1);
        Next::Continue { from: held.have }
    })
    .await;
    let Ok(replies) = offered else {
        assert!(checked.get() <= limits.per_peer);
        return false;
    };
    assert!(replies.len() <= limits.per_peer);
    assert_eq!(replies.len(), checked.get());
    for (held, reply) in replies {
        if let Reply::Continue { from } = reply {
            assert_eq!(from, held.have);
        }
    }
    true
}

/// The receiver reads the sender's replies to its list; whether it took
/// them.
async fn answers(bytes: &[u8]) -> bool {
    let held = held();
    let (mut stream, _theirs) = duplex_with(bytes).await;
    let Ok(replies) = handshake::answer(&mut stream, &held, &limits()).await else {
        return false;
    };
    assert_eq!(replies.len(), held.len());
    for (one, reply) in replies {
        if let Reply::Continue { from } = reply {
            assert_eq!(from, one.have, "a continue only from what is held");
        }
    }
    true
}

/// Both sides' checks of a stream and of a small real file: what the
/// receiver does with the stream under its record, and the sender's answer.
fn checked(checks: &Checks) -> (Take, Option<Next>) {
    // The receiver's check of a stream.
    let record = receiving(checks);
    let stream = Resumable {
        key: record.key,
        from: checks.from,
        size: if checks.record_size_matches {
            record.size
        } else {
            checks.size ^ 1
        },
    };
    let mut under_record = Take::Refuse;
    for kept in [None, Some(&record)] {
        let take = receiver_takes(kept, &stream, &checks.stream_same);
        under_record = take;
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
        file: if checks.other_file {
            FileId { volume: 1, file: 1 }
        } else {
            opened.id
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
    let next = runtime().block_on(sender_checks(
        Some(&sending),
        Some(&opened),
        &held,
        None,
        &Default::default(),
    ));
    if let Ok((Next::Continue { from }, hasher)) = &next {
        assert!(*from <= opened.size && from.is_multiple_of(CHUNK));
        assert_eq!(*from, checks.have);
        // The hasher it hands back stands where the stream continues.
        assert_eq!(hasher.as_ref().map(|h| h.position()), Some(*from));
    } else if let Ok((_, hasher)) = &next {
        assert!(hasher.is_none());
    }
    (under_record, next.ok().map(|(next, _)| next))
}

/// Runs one input; whether it was whole: a list or replies that were
/// taken, or every value of the checks.
fn ran(data: &[u8]) -> bool {
    let Some((&mode, rest)) = data.split_first() else {
        return false;
    };
    match mode % 3 {
        0 => runtime().block_on(offers(rest)),
        1 => runtime().block_on(answers(rest)),
        _ => {
            let Ok(checks) = checks(&mut Slice(rest)) else {
                return false;
            };
            checked(&checks);
            true
        }
    }
}

/// The resume exchange as the other side sends it: `handshake::offer` and
/// `handshake::answer` fed the input's bytes over an in-memory stream, then
/// `receiver_takes` and `sender_checks` with records and lists made from
/// them against a small real file. Never a panic, never more than the
/// limits given; never a continue or an append from an offset off a chunk
/// or past the size, and never an append when `same` differs.
pub fn resume_handshake(data: &[u8]) {
    ran(data);
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::Verdict;

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
        name: "resume_handshake",
        run: resume_handshake,
        whole: ran,
        seeds,
        cut: "held-three",
    }];

    /// An offset of `chunks` whole chunks, as [`offset`] reads one.
    fn chunks(out: &mut Vec<u8>, chunks: u8) {
        out.extend_from_slice(&[1, chunks]);
    }

    /// Any offset, as [`offset`] reads one.
    fn any(out: &mut Vec<u8>, n: u64) {
        out.push(0);
        out.extend_from_slice(&n.to_le_bytes());
    }

    fn few(out: &mut Vec<u8>, bytes: &[u8]) {
        out.push(bytes.len() as u8);
        out.extend_from_slice(bytes);
    }

    /// Every part of the exchange as each side writes it: lists of what a
    /// receiver holds, none to the most; replies of every kind; and checks
    /// that append, continue, resend, restart and refuse.
    fn seeds() -> Vec<Input> {
        let mut seeds = Vec::new();
        for (name, count) in [("none", 0u8), ("one", 1), ("three", 3), ("most", 8)] {
            let mut list = vec![0, handshake::VERSION, count];
            for n in 0..count {
                let one = Held {
                    key: TransferKey([n; 16]),
                    have: u64::from(n) * CHUNK,
                    prefix: Digest([n.wrapping_mul(37); 32]),
                };
                list.extend_from_slice(&one.encode());
            }
            seeds.push((format!("held-{name}"), list));
        }
        let [first, second] = held();
        let go = |held: Held| Reply::Continue { from: held.have };
        let replies = [
            ("continue", go(first), go(second)),
            ("restart", go(first), Reply::Stop(Verdict::Restart)),
            (
                "dropped",
                Reply::Stop(Verdict::Dropped),
                Reply::Stop(Verdict::Restart),
            ),
        ];
        for (name, one, other) in replies {
            let mut bytes = vec![1];
            bytes.extend_from_slice(&one.encode());
            bytes.extend_from_slice(&other.encode());
            seeds.push((format!("replies-{name}"), bytes));
        }
        // The hash of no bytes, as a sender's hasher gives it: a receiver
        // holding nothing yet continues from zero.
        let nothing = safecopy_chunks::Settings::default().sending().finish().0;

        // The receiver appends at a chunk; the sender continues from zero.
        let mut append = vec![2];
        chunks(&mut append, 0);
        append.extend_from_slice(&nothing);
        chunks(&mut append, 1);
        chunks(&mut append, 2);
        chunks(&mut append, 1);
        few(&mut append, b"same");
        few(&mut append, b"same");
        append.extend_from_slice(&[0, 1]);
        seeds.push(("checks-append".to_owned(), append));

        // From zero under a kept key: the one resend.
        let mut resend = vec![2];
        chunks(&mut resend, 0);
        resend.extend_from_slice(&[9; 32]);
        chunks(&mut resend, 1);
        chunks(&mut resend, 3);
        chunks(&mut resend, 0);
        few(&mut resend, b"");
        few(&mut resend, b"");
        resend.extend_from_slice(&[0, 1]);
        seeds.push(("checks-resend".to_owned(), resend));

        // From another place than is held, and more held than the file has.
        let mut restart = vec![2];
        chunks(&mut restart, 1);
        restart.extend_from_slice(&[0; 32]);
        chunks(&mut restart, 2);
        chunks(&mut restart, 3);
        chunks(&mut restart, 1);
        few(&mut restart, b"12345678");
        few(&mut restart, b"12345678");
        restart.extend_from_slice(&[0, 1]);
        seeds.push(("checks-restart".to_owned(), restart));

        // Off a chunk, another `same`, another size and another file.
        let mut refuse = vec![2];
        any(&mut refuse, 300);
        refuse.extend_from_slice(&[0xFF; 32]);
        any(&mut refuse, CHUNK + 1);
        any(&mut refuse, u64::MAX);
        any(&mut refuse, 77);
        few(&mut refuse, b"one");
        few(&mut refuse, b"other");
        refuse.extend_from_slice(&[1, 0]);
        seeds.push(("checks-refuse".to_owned(), refuse));
        seeds
    }

    /// A seed of checks is kept for what both sides answer to it: a change
    /// of the checks that makes it another case fails here.
    #[test]
    fn each_seed_of_checks_comes_to_what_its_name_says() {
        let came_to = |name: &str| {
            let seeds = seeds();
            let (_, bytes) = seeds.iter().find(|(n, _)| n == name).expect("the seed");
            checked(&checks(&mut Slice(&bytes[1..])).expect("every value"))
        };
        let from_zero = Some(Next::Continue { from: 0 });
        assert_eq!(came_to("checks-append"), (Take::Append, from_zero));
        assert_eq!(came_to("checks-resend").0, Take::Resend);
        assert_eq!(
            came_to("checks-restart"),
            (Take::Restart, Some(Next::Restart))
        );
        assert_eq!(
            came_to("checks-refuse"),
            (Take::Refuse, Some(Next::Changed))
        );
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
