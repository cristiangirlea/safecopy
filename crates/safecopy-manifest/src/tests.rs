use super::*;
use crate::listing::{add_size, taken};
use crate::seams::{
    ATTRIBUTES, FINAL_PATH_FAILS, GONE_BEFORE_INSPECT, GONE_BEFORE_OPEN, INSPECT_FAILS, NO_VOLUME,
};
use crate::sources::{Kept, OFFLINE, RECALL_ON_DATA_ACCESS, RECALL_ON_OPEN, online_only};
use safecopy_folder::{Directory, Meta, Time, is_link};
use safecopy_names::{PathRules, Siblings, check_path, safe_name};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::Path;

const MAX_ENTRIES: usize = 10_000;
const MAX_DEPTH: usize = 32;
const MAX_PATH_CHARS: usize = 1024;
const MAX_NAME_UNITS: usize = 200;
const TIB: u64 = 1 << 40;

fn limits() -> Limits {
    Limits::default()
}

fn file(path: &str, size: u64) -> Entry {
    Entry {
        path: path.into(),
        kind: Kind::File,
        size,
    }
}

fn folder(path: &str) -> Entry {
    Entry {
        path: path.into(),
        kind: Kind::Folder,
        size: 0,
    }
}

/// The limits of a caller that only writes where a name is counted in
/// UTF-16 units: the tests of wide paths need names over 200 bytes.
fn wide_limits() -> Limits {
    Limits {
        names: safecopy_names::NameRules::default().with_max_bytes(usize::MAX),
        ..limits()
    }
}

fn new(entries: Vec<Entry>) -> Result<Manifest, ManifestError> {
    Manifest::new(1, entries, &limits())
}

fn why<T: std::fmt::Debug>(result: Result<T, ManifestError>) -> ManifestError {
    result.expect_err("refused")
}

/// Folders `a/a/...` `depth` deep, then a file in the last.
fn nested(depth: usize) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut path = String::new();
    for _ in 0..depth - 1 {
        path.push_str(if path.is_empty() { "a" } else { "/a" });
        entries.push(folder(&path));
    }
    entries.push(file(&format!("{path}/f"), 1));
    entries
}

/// Folders named by all of `parts` but the last, then a file named by all.
fn along(parts: &[String]) -> Vec<Entry> {
    let mut entries: Vec<Entry> = (1..parts.len())
        .map(|n| folder(&parts[..n].join("/")))
        .collect();
    entries.push(file(&parts.join("/"), 1));
    entries
}

/// Ten folders of a hundred four-byte characters each, then `last`.
fn wide_parts(last: &str) -> Vec<String> {
    let mut parts = vec!["😀".repeat(100); 10];
    parts.push(last.into());
    parts
}

/// A caller's framing that carries at most 4,050 bytes of path, a folder's
/// with a `/` after it.
fn fits_4050(path: &str, kind: Kind) -> bool {
    path.len() + usize::from(kind == Kind::Folder) <= 4050
}

// The limits.

#[test]
fn the_default_limits_are_the_values_the_list_was_made_with() {
    let limits = Limits::default();
    assert_eq!(limits.entries, MAX_ENTRIES);
    assert_eq!(limits.depth, MAX_DEPTH);
    assert_eq!(limits.path_chars, MAX_PATH_CHARS);
    assert_eq!(limits.bytes, TIB);
    assert_eq!(limits.encoded, 16 * 1024 * 1024);
    assert_eq!(limits.source_folders, 256);
    assert_eq!(limits.kept_entries, 10_000);
    assert_eq!(limits.names, safecopy_names::NameRules::default());
    assert!(limits.fits.is_none());
}

#[test]
fn a_list_of_ten_thousand_entries_is_taken_and_one_more_is_not() {
    let mut entries = vec![folder("r")];
    entries.extend((1..MAX_ENTRIES).map(|i| file(&format!("r/{i}"), 1)));
    let manifest = new(entries.clone()).unwrap();
    assert_eq!(manifest.entries().len(), MAX_ENTRIES);
    assert_eq!(manifest.files() as usize, MAX_ENTRIES - 1);
    entries.push(file("r/one more", 1));
    assert_eq!(why(new(entries)), ManifestError::TooMany { most: 10_000 });
}

#[test]
fn paths_may_be_thirty_two_parts_deep_and_no_deeper() {
    assert!(new(nested(MAX_DEPTH)).is_ok());
    assert_eq!(
        why(new(nested(MAX_DEPTH + 1))),
        ManifestError::Path(PathError::TooDeep { most: 32 })
    );
}

#[test]
fn the_path_length_in_characters_is_bounded_too() {
    let parts = |last: usize| {
        let mut parts = vec!["r".to_string()];
        parts.extend(vec!["x".repeat(200); 5]);
        parts.push("y".repeat(last));
        parts
    };
    let at = parts(17);
    assert_eq!(at.join("/").chars().count(), MAX_PATH_CHARS);
    assert!(new(along(&at)).is_ok());
    assert_eq!(
        why(new(along(&parts(18)))),
        ManifestError::Path(PathError::TooLong { most: 1024 })
    );
}

#[test]
fn the_sizes_add_up_to_the_byte_limit_and_never_wrap() {
    let half = TIB / 2;
    let at = vec![folder("r"), file("r/a", half), file("r/b", half)];
    assert_eq!(new(at).unwrap().bytes(), TIB);
    let over = vec![folder("r"), file("r/a", half), file("r/b", half + 1)];
    assert_eq!(why(new(over)), ManifestError::TooLarge { most: TIB });
    let wrapping = vec![folder("r"), file("r/a", u64::MAX), file("r/b", 2)];
    assert_eq!(why(new(wrapping)), ManifestError::TooLarge { most: TIB });
}

/// Every limit is a setting: a list at a narrowed limit is taken, one past
/// it refused.
#[test]
fn each_limit_takes_the_limit_itself_and_refuses_one_past_it() {
    let at = |limits: &Limits, entries: Vec<Entry>| Manifest::new(1, entries, limits);
    let entries = Limits {
        entries: 3,
        ..limits()
    };
    let three = vec![folder("r"), file("r/a", 1), file("r/b", 1)];
    assert!(at(&entries, three.clone()).is_ok());
    let mut four = three.clone();
    four.push(file("r/c", 1));
    assert_eq!(why(at(&entries, four)), ManifestError::TooMany { most: 3 });

    let depth = Limits {
        depth: 3,
        ..limits()
    };
    assert!(at(&depth, nested(3)).is_ok());
    assert_eq!(
        why(at(&depth, nested(4))),
        ManifestError::Path(PathError::TooDeep { most: 3 })
    );

    let chars = Limits {
        path_chars: 5,
        ..limits()
    };
    assert!(at(&chars, vec![folder("r"), file("r/abc", 1)]).is_ok());
    assert_eq!(
        why(at(&chars, vec![folder("r"), file("r/abcd", 1)])),
        ManifestError::Path(PathError::TooLong { most: 5 })
    );

    let bytes = Limits {
        bytes: 10,
        ..limits()
    };
    assert_eq!(
        at(&bytes, vec![folder("r"), file("r/a", 4), file("r/b", 6)])
            .unwrap()
            .bytes(),
        10
    );
    assert_eq!(
        why(at(
            &bytes,
            vec![folder("r"), file("r/a", 4), file("r/b", 7)]
        )),
        ManifestError::TooLarge { most: 10 }
    );

    let names = Limits {
        names: safecopy_names::NameRules::default().with_max_units(3),
        ..limits()
    };
    assert!(at(&names, vec![folder("r"), file("r/abc", 1)]).is_ok());
    assert_eq!(
        why(at(&names, vec![folder("r"), file("r/abcd", 1)])),
        ManifestError::Name
    );

    // The encoded length: the list's own bytes are the limit.
    let list = new(three).unwrap();
    let len = list.encode(&limits()).unwrap().len();
    let encoded = Limits {
        encoded: len,
        ..limits()
    };
    let bytes = list.encode(&encoded).unwrap();
    assert_eq!(Manifest::decode(&bytes, &encoded).unwrap(), list);
    let under = Limits {
        encoded: len - 1,
        ..limits()
    };
    assert_eq!(
        why(list.encode(&under)),
        ManifestError::TooLong { most: len - 1 }
    );
    assert_eq!(
        why(Manifest::decode(&bytes, &under)),
        ManifestError::TooLong { most: len - 1 }
    );
}

#[test]
fn a_path_the_callers_check_refuses_is_refused_never_cut() {
    let fits = Limits {
        fits: Some(fits_4050),
        ..wide_limits()
    };
    // 10 × 400 bytes and 10 separators, then 40: 4,050 bytes in 1,020 characters.
    let at = wide_parts(&"😀".repeat(10));
    assert_eq!(at.join("/").len(), 4050);
    let manifest = Manifest::new(1, along(&at), &fits).unwrap();
    assert_eq!(manifest.entries()[10].path, at.join("/"));
    let over = wide_parts(&format!("{}a", "😀".repeat(10)));
    assert_eq!(
        why(Manifest::new(1, along(&over), &fits)),
        ManifestError::DoesNotFit
    );
    // A folder's path carries one byte more than the file's.
    let mut as_folder = along(&at);
    as_folder[10] = folder(&at.join("/"));
    assert_eq!(
        why(Manifest::new(1, as_folder, &fits)),
        ManifestError::DoesNotFit
    );
}

/// Differs from the original, which always held a path to its own
/// framing's header: here that check is the caller's, and none is set by
/// default.
#[test]
fn without_a_check_of_the_callers_a_path_within_the_characters_is_taken() {
    let wide = wide_parts(&"😀".repeat(14));
    assert_eq!(wide.join("/").chars().count(), MAX_PATH_CHARS);
    assert!(Manifest::new(1, along(&wide), &wide_limits()).is_ok());
    // Under the default rules a part of 400 bytes is no safe name.
    assert_eq!(why(new(along(&wide))), ManifestError::Name);
    let fits = Limits {
        fits: Some(fits_4050),
        ..wide_limits()
    };
    assert_eq!(
        why(Manifest::new(1, along(&wide), &fits)),
        ManifestError::DoesNotFit
    );
}

// What a list may hold.

/// A name cut as versions before a cut kept the extension cut it: at its
/// end, between two characters, to 200 units and 200 bytes, then without
/// the dots and spaces at its end.
fn cut_at_its_end(name: &str) -> &str {
    let (mut units, mut end) = (0, 0);
    for c in name.chars() {
        units += c.len_utf16();
        if units > MAX_NAME_UNITS || end + c.len_utf8() > 200 {
            break;
        }
        end += c.len_utf8();
    }
    name[..end].trim_end_matches(['.', ' '])
}

/// Two programs on different versions list one long name differently. A
/// receiver never cuts a listed name again: it takes a name that is safe
/// and within the limits as it is, and both cuts are, so a list goes
/// either way.
#[test]
fn a_list_cut_by_an_older_or_a_newer_sender_is_taken_by_the_other() {
    let x = |n: usize| "x".repeat(n);
    // The name on disk, then twice in one folder as an older version
    // listed it and as this one does.
    let cases = [
        (
            format!("{}.txt", x(250)),
            [x(200), format!("{} (2)", x(196))],
            [format!("{}.txt", x(196)), format!("{} (2).txt", x(192))],
        ),
        (
            format!("{}.pdf", "文".repeat(90)),
            ["文".repeat(66), format!("{} (2)", "文".repeat(65))],
            [
                format!("{}.pdf", "文".repeat(65)),
                format!("{} (2).pdf", "文".repeat(64)),
            ],
        ),
        (
            format!("{}.png", "😀".repeat(100)),
            ["😀".repeat(50), format!("{} (2)", "😀".repeat(49))],
            [
                format!("{}.png", "😀".repeat(49)),
                format!("{} (2).png", "😀".repeat(48)),
            ],
        ),
        (
            format!("NUL{}x.txt", " ".repeat(197)),
            ["_NUL".to_string(), "_NUL (2)".to_string()],
            ["_NUL.txt".to_string(), "_NUL (2).txt".to_string()],
        ),
    ];
    // Rules that keep no extension cut a plain name as an older version
    // did; a numbered one here has no extension left to differ by.
    let older = Limits {
        names: safecopy_names::NameRules::default().with_max_extension(0),
        ..limits()
    };
    let listed = |names: &[String; 2]| {
        vec![
            folder("r"),
            file(&format!("r/{}", names[0]), 1),
            file(&format!("r/{}", names[1]), 1),
            file(&names[0], 1),
        ]
    };
    for (name, before, now) in cases {
        // What each version makes of the name, the older one by hand.
        let mut siblings = Siblings::default();
        let mut older_siblings = Siblings::new(older.names.clone());
        for n in 0..2 {
            assert_eq!(siblings.take(OsStr::new(&name)).unwrap(), now[n]);
            assert_eq!(older_siblings.take(OsStr::new(&name)).unwrap(), before[n]);
        }
        let first = cut_at_its_end(&name);
        assert_eq!(before[0].trim_start_matches('_'), first, "{name}");
        assert_ne!(before, now, "{name}");

        // An older sender's list at this receiver.
        let sent = Manifest::new(1, listed(&before), &older).unwrap();
        let bytes = sent.encode(&older).unwrap();
        assert_eq!(Manifest::decode(&bytes, &limits()).unwrap(), sent);
        // This sender's list at an older receiver, which took a name it
        // would not cut: within both limits, nothing at its end to drop.
        let sent = new(listed(&now)).unwrap();
        let bytes = sent.encode(&limits()).unwrap();
        assert_eq!(Manifest::decode(&bytes, &older).unwrap(), sent);
        for name in &now {
            assert_eq!(cut_at_its_end(name), name);
        }
        // Either list's paths are created as they are listed.
        for name in before.iter().chain(&now) {
            let path = format!("r/{name}");
            for names in [&older.names, &limits().names] {
                let rules = PathRules::default().with_names(names.clone());
                let taken = check_path(&path, &rules).expect(&path);
                assert_eq!(taken.parts.join("/"), path);
                assert_eq!(safe_name(name, names).as_deref(), Some(name.as_str()));
            }
        }
    }
}

#[test]
fn every_listed_path_is_one_the_receiver_takes_as_it_is() {
    let mut top = Siblings::default();
    let mut inner = Siblings::default();
    let mut entries = Vec::new();
    let names = [
        "CON",
        "report.pdf",
        "notes.",
        "a:b",
        "cafe\u{301}",
        "café",
        "😀",
        "photo\u{202e}gpj.exe",
    ];
    let root = top.take(OsStr::new("Photos")).unwrap();
    entries.push(folder(&root));
    for name in names {
        let safe = inner.take(OsStr::new(name)).unwrap();
        entries.push(file(&format!("{root}/{safe}"), 1));
    }
    let long = inner.take(OsStr::new(&"z".repeat(300))).unwrap();
    entries.push(folder(&format!("{root}/{long}")));
    for name in names {
        entries.push(file(&top.take(OsStr::new(name)).unwrap(), 1));
    }
    let manifest = new(entries).unwrap();
    let names = safecopy_names::NameRules::default();
    for entry in manifest.entries() {
        let path = &entry.path;
        match (entry.kind, path.contains('/')) {
            // A file on its own goes as a single file does.
            (Kind::File, false) => {
                assert_eq!(safe_name(path, &names).as_deref(), Some(path.as_str()));
            }
            (kind, _) => {
                let sent = match kind {
                    Kind::Folder => format!("{path}/"),
                    Kind::File => path.clone(),
                };
                let taken = check_path(&sent, &PathRules::default()).expect(path);
                assert_eq!(&taken.parts.join("/"), path);
                assert_eq!(taken.folder, kind == Kind::Folder);
            }
        }
    }
}

#[test]
fn paths_that_leave_the_list_are_refused() {
    for (path, kind) in [
        ("..", PathError::LeavesFolder),
        ("r/..", PathError::LeavesFolder),
        ("r/../x", PathError::LeavesFolder),
        ("r/ .. ", PathError::LeavesFolder),
        (".", PathError::LeavesFolder),
        ("/etc", PathError::AtTop),
        ("\\Windows", PathError::AtTop),
        ("\\\\server\\share", PathError::AtTop),
        ("//server/share", PathError::AtTop),
        ("C:", PathError::DriveOrStream),
        ("C:/Windows", PathError::DriveOrStream),
        ("r/C:x", PathError::DriveOrStream),
        ("r/a.txt:secret", PathError::DriveOrStream),
        ("r/a.txt::$DATA", PathError::DriveOrStream),
        ("r//a", PathError::EmptyPart),
        ("r/", PathError::EmptyPart),
    ] {
        let entries = vec![folder("r"), file(path, 1)];
        assert_eq!(why(new(entries)), ManifestError::Path(kind), "{path}");
    }
    // A backslash would start a folder of its own on Windows.
    let entries = vec![folder("r"), file("r/a\\b", 1)];
    assert_eq!(why(new(entries)), ManifestError::Name);
}

#[test]
fn names_cleaning_would_change_are_refused_in_a_list() {
    for name in [
        "a.",
        "a ",
        " a",
        "CON",
        "nul.txt",
        "COM1",
        "a?b",
        "a*b",
        "a\"b",
        "a<b>",
        "a|b",
        "a\u{7}b",
        "photo\u{202e}gpj.exe",
        "cafe\u{301}",
        &"x".repeat(MAX_NAME_UNITS + 1),
    ] {
        let entries = vec![folder("r"), file(&format!("r/{name}"), 1)];
        assert!(
            matches!(
                why(new(entries)),
                ManifestError::Name | ManifestError::Path(PathError::Reserved)
            ),
            "{name:?}"
        );
    }
    let at = "x".repeat(MAX_NAME_UNITS);
    assert!(new(vec![folder("r"), file(&format!("r/{at}"), 1)]).is_ok());
}

/// A name within the units that a system counting bytes would refuse is
/// no safe name: 67 three-byte characters are 201 bytes.
#[test]
fn names_over_the_byte_limit_are_refused_in_a_list() {
    let list = |name: &str| vec![folder("r"), file(&format!("r/{name}"), 1)];
    for name in ["文".repeat(67), "😀".repeat(51), "я".repeat(101)] {
        assert_eq!(why(new(list(&name))), ManifestError::Name, "{name}");
        assert_eq!(
            why(new(vec![folder(&name), file(&format!("{name}/a"), 1)])),
            ManifestError::Name,
            "{name}"
        );
        assert!(Manifest::new(1, list(&name), &wide_limits()).is_ok());
    }
    for name in ["文".repeat(66), "😀".repeat(50), "я".repeat(100)] {
        assert!(new(list(&name)).is_ok(), "{name}");
    }
    // What a listing gives for such names is taken, numbered ones too.
    let long = format!("{}.pdf", "文".repeat(90));
    let mut siblings = Siblings::default();
    let one = siblings.take(OsStr::new(&long)).unwrap();
    let two = siblings.take(OsStr::new(&long)).unwrap();
    assert!(one.len() <= 200 && two.len() <= 200 && one != two);
    let entries = vec![
        folder("r"),
        file(&format!("r/{one}"), 1),
        file(&format!("r/{two}"), 1),
    ];
    assert!(new(entries).is_ok());
}

#[test]
fn names_that_differ_only_in_case_or_accents_are_refused_in_a_list() {
    for (one, other) in [
        ("Report.txt", "report.txt"),
        ("ÉTÉ", "été"),
        ("Ǆ", "ǆ"),
        ("r", "R"),
    ] {
        let entries = vec![
            folder("r"),
            file(&format!("r/{one}"), 1),
            file(&format!("r/{other}"), 1),
        ];
        assert_eq!(why(new(entries)), ManifestError::SameName);
    }
    let twice = vec![folder("r"), file("r/a", 1), file("r/a", 1)];
    assert_eq!(why(new(twice)), ManifestError::SameName);
    // The same name in two folders is two files.
    let apart = vec![
        folder("r"),
        folder("r/x"),
        folder("r/y"),
        file("r/x/a", 1),
        file("r/y/A", 1),
    ];
    assert!(new(apart).is_ok());
}

#[test]
fn names_cut_to_the_same_length_are_numbered_within_it_and_listed() {
    let long = "x".repeat(250);
    let mut siblings = Siblings::default();
    let one = siblings.take(OsStr::new(&format!("{long}1.txt"))).unwrap();
    let two = siblings.take(OsStr::new(&format!("{long}2.txt"))).unwrap();
    let entries = vec![
        folder("r"),
        file(&format!("r/{one}"), 1),
        file(&format!("r/{two}"), 1),
    ];
    assert!(new(entries).is_ok());
}

#[test]
fn a_list_s_folders_must_hold_together() {
    let as_folder = vec![folder("r"), file("r/a", 1), file("r/a/b", 1)];
    assert_eq!(why(new(as_folder)), ManifestError::FileAsFolder);
    let orphan = vec![folder("r"), file("r/x/b", 1)];
    assert_eq!(why(new(orphan)), ManifestError::Orphan);
    let late = vec![file("r/b", 1), folder("r")];
    assert_eq!(why(new(late)), ManifestError::Orphan);
    let sized = vec![Entry {
        size: 1,
        ..folder("r")
    }];
    assert_eq!(why(new(sized)), ManifestError::FolderSize);
    assert_eq!(why(new(vec![])), ManifestError::Empty);
}

#[test]
fn a_list_of_empty_folders_alone_is_a_list() {
    let manifest = new(vec![folder("r"), folder("r/a"), folder("r/a/b")]).unwrap();
    assert_eq!((manifest.files(), manifest.bytes()), (0, 0));
}

// The encoding.

fn varint(bytes: &mut Vec<u8>, mut n: u64) {
    loop {
        let low = (n & 0x7F) as u8;
        n >>= 7;
        if n == 0 {
            return bytes.push(low);
        }
        bytes.push(low | 0x80);
    }
}

/// The start of a list's bytes: its id and its count.
fn head(id: u64, count: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    varint(&mut bytes, id);
    varint(&mut bytes, u64::from(count));
    bytes
}

#[test]
fn a_list_comes_back_from_the_wire_as_it_went() {
    let any = Limits {
        bytes: u64::MAX,
        ..limits()
    };
    let manifest = Manifest::new(
        7,
        vec![
            folder("Photos"),
            folder("Photos/empty"),
            file("Photos/a.jpg", 3),
            file("Photos/b.jpg", u64::MAX - 3),
        ],
        &any,
    )
    .unwrap();
    let bytes = manifest.encode(&any).unwrap();
    let back = Manifest::decode(&bytes, &any).unwrap();
    assert_eq!(back, manifest);
    assert_eq!(back.id(), 7);
    assert_eq!(manifest.entry(2).unwrap().path, "Photos/a.jpg");
    assert!(manifest.entry(4).is_none());
}

/// The bytes are fixed: the same as the encoding the list had before, so
/// both sides of a link read each other.
#[test]
fn the_encoding_has_fixed_bytes() {
    let manifest = Manifest::new(
        300,
        vec![folder("r"), file("r/é", 300), file("r/z", 0)],
        &limits(),
    )
    .unwrap();
    let bytes = manifest.encode(&limits()).unwrap();
    assert_eq!(
        bytes,
        [
            0xAC, 0x02, 3, // id 300, three entries
            1, b'r', 1, 0, // folder "r"
            4, b'r', b'/', 0xC3, 0xA9, 0, 0xAC, 0x02, // file "r/é", 300 bytes
            3, b'r', b'/', b'z', 0, 0, // file "r/z", empty
        ]
    );
    let mut most = head(u64::MAX, 1);
    assert_eq!(
        most,
        [
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01, 1
        ]
    );
    most.extend([1, b'r', 1, 0]);
    assert_eq!(Manifest::decode(&most, &limits()).unwrap().id(), u64::MAX);
}

#[test]
fn the_wire_list_is_bounded_in_bytes() {
    let manifest = Manifest::new(7, vec![folder("r"), file("r/a", 1)], &limits()).unwrap();
    let bytes = manifest.encode(&limits()).unwrap();
    let at = Limits {
        encoded: bytes.len(),
        ..limits()
    };
    assert!(Manifest::decode(&bytes, &at).is_ok());
    let under = Limits {
        encoded: bytes.len() - 1,
        ..limits()
    };
    assert!(matches!(
        why(Manifest::decode(&bytes, &under)),
        ManifestError::TooLong { .. }
    ));
    let huge = vec![0u8; 16 * 1024 * 1024 + 1];
    assert_eq!(
        why(Manifest::decode(&huge, &limits())),
        ManifestError::TooLong {
            most: 16 * 1024 * 1024
        }
    );
}

/// A list too large to send is refused at its encoding, at the default
/// 16 MiB: 10,000 paths of about a thousand three-byte characters take
/// about 30 MB.
#[test]
fn an_oversized_list_is_refused_when_encoded() {
    let part = "中".repeat(200);
    let mut entries = vec![folder("r")];
    let mut deepest = "r".to_string();
    for _ in 0..4 {
        deepest = format!("{deepest}/{part}");
        entries.push(folder(&deepest));
    }
    for i in 0..(MAX_ENTRIES - entries.len()) {
        entries.push(file(&format!("{deepest}/{i:05}{}", "中".repeat(195)), 1));
    }
    let manifest = Manifest::new(1, entries, &wide_limits()).unwrap();
    assert_eq!(manifest.entries().len(), MAX_ENTRIES);
    assert_eq!(
        why(manifest.encode(&wide_limits())),
        ManifestError::TooLong {
            most: 16 * 1024 * 1024
        }
    );
}

#[test]
fn a_damaged_or_boastful_wire_list_is_refused_before_it_is_kept() {
    // A count far past the limit is refused before room is set aside for it.
    let boast = head(7, u32::MAX);
    assert_eq!(
        why(Manifest::decode(&boast, &limits())),
        ManifestError::TooMany { most: MAX_ENTRIES }
    );
    let short = head(7, 2);
    assert_eq!(
        why(Manifest::decode(&short, &limits())),
        ManifestError::Damaged
    );
    let manifest = Manifest::new(7, vec![folder("r")], &limits()).unwrap();
    let mut extra = manifest.encode(&limits()).unwrap();
    extra.push(0);
    assert_eq!(
        why(Manifest::decode(&extra, &limits())),
        ManifestError::Damaged
    );
}

#[test]
fn a_wire_list_claiming_one_entry_too_many_is_refused_at_its_count() {
    let over = head(7, MAX_ENTRIES as u32 + 1);
    assert_eq!(
        why(Manifest::decode(&over, &limits())),
        ManifestError::TooMany { most: MAX_ENTRIES }
    );
    // At the limit the count is taken, and the missing entries found.
    let at = head(7, MAX_ENTRIES as u32);
    assert_eq!(
        why(Manifest::decode(&at, &limits())),
        ManifestError::Damaged
    );
}

/// Numbers past their width, text that is not UTF-8, an unknown kind, a
/// length past the end: each is damaged, never a panic.
#[test]
fn bytes_that_do_not_read_are_damaged() {
    let mut cases: Vec<Vec<u8>> = Vec::new();
    // An id of eleven bytes, and one whose tenth byte holds more than one bit.
    cases.push([[0xFF; 10].as_slice(), &[0x01, 1, 1, b'r', 1, 0]].concat());
    cases.push([[0xFF; 9].as_slice(), &[0x02, 1, 1, b'r', 1, 0]].concat());
    // A count whose fifth byte holds more than four bits.
    cases.push(vec![7, 0xFF, 0xFF, 0xFF, 0xFF, 0x10]);
    let mut not_text = head(7, 1);
    not_text.extend([1, 0xFF, 1, 0]);
    cases.push(not_text);
    let mut kind = head(7, 1);
    kind.extend([1, b'r', 2, 0]);
    cases.push(kind);
    let mut past = head(7, 1);
    past.extend([0xFF, 0xFF, 0xFF, 0xFF, 0x0F, b'r']);
    cases.push(past);
    let mut cut = head(7, 1);
    cut.extend([1, b'r', 0, 0x80]);
    cases.push(cut);
    for bytes in cases {
        assert_eq!(
            why(Manifest::decode(&bytes, &limits())),
            ManifestError::Damaged,
            "{bytes:02x?}"
        );
    }
    // A longer form of a number than needed is taken, as it was before.
    let mut long = vec![0x87, 0x00, 0x81, 0x00];
    long.extend([1, b'r', 0x81, 0x00, 0]);
    assert_eq!(Manifest::decode(&long, &limits()).unwrap().id(), 7);
}

// On disk.

/// A new, empty folder for one test, under the build's target folder.
fn temp(name: &str) -> PathBuf {
    // The test binary is in target/<profile>/deps.
    let exe = std::env::current_exe().unwrap();
    let target = exe.ancestors().nth(3).unwrap();
    let dir = target
        .join("test-tmp")
        .join(format!("manifest-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scratch(name: &str) -> (Scratch, PathBuf) {
    let dir = temp(name);
    (Scratch(dir.clone()), dir)
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn list<P: AsRef<Path>>(roots: &[P]) -> Result<(Manifest, Sources), ManifestError> {
    Sources::list(1, roots, &limits())
}

fn read(sources: &Sources, n: u32) -> Vec<u8> {
    let mut opened = sources.open(n).unwrap().expect("a file");
    let mut bytes = Vec::new();
    opened.file.read_to_end(&mut bytes).unwrap();
    opened.unchanged().unwrap();
    bytes
}

fn paths(manifest: &Manifest) -> Vec<(&str, Kind, u64)> {
    manifest
        .entries()
        .iter()
        .map(|e| (e.path.as_str(), e.kind, e.size))
        .collect()
}

/// Makes `link` a junction (Windows) or symbolic link to the folder
/// `target`; false where this computer cannot.
fn link_folder(target: &Path, link: &Path) -> bool {
    #[cfg(windows)]
    {
        // mklink takes backslashes only.
        let link: PathBuf = link.components().collect();
        let target: PathBuf = target.components().collect();
        let made = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output();
        matches!(made, Ok(output) if output.status.success() && link.exists())
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
}

/// Whether a link that cannot be made fails its test: CI sets
/// `SAFECOPY_REQUIRE_JUNCTIONS` where it must make them.
fn junctions_required() -> bool {
    std::env::var_os("SAFECOPY_REQUIRE_JUNCTIONS").is_some_and(|v| !v.is_empty())
}

fn no_link_here(part: &str) {
    assert!(
        !junctions_required(),
        "SAFECOPY_REQUIRE_JUNCTIONS is set, but no junction or link can be made for {part}"
    );
    eprintln!("skipped {part}: no junction or link can be made here");
}

fn remove_link(path: &Path) {
    #[cfg(windows)]
    std::fs::remove_dir(path).unwrap();
    #[cfg(unix)]
    std::fs::remove_file(path).unwrap();
}

fn is_link_error(error: &ManifestError) -> bool {
    matches!(error, ManifestError::Link { .. })
}

#[test]
fn a_list_puts_folders_before_what_they_hold_and_opens_by_number() {
    let (_scratch, dir) = scratch("listed");
    write(&dir.join("Photos/b.jpg"), b"bee");
    write(&dir.join("Photos/2024/a.jpg"), b"a");
    std::fs::create_dir_all(dir.join("Photos/empty")).unwrap();
    write(&dir.join("notes.txt"), b"notes");
    let roots = [dir.join("Photos"), dir.join("notes.txt")];
    let (manifest, sources) = Sources::list(9, &roots, &limits()).unwrap();
    assert_eq!(
        paths(&manifest),
        [
            ("Photos", Kind::Folder, 0),
            ("Photos/2024", Kind::Folder, 0),
            ("Photos/2024/a.jpg", Kind::File, 1),
            ("Photos/b.jpg", Kind::File, 3),
            ("Photos/empty", Kind::Folder, 0),
            ("notes.txt", Kind::File, 5),
        ]
    );
    assert_eq!((manifest.id(), sources.id()), (9, 9));
    assert_eq!((manifest.files(), manifest.bytes()), (3, 9));
    assert_eq!(read(&sources, 2), b"a");
    assert_eq!(read(&sources, 3), b"bee");
    assert_eq!(read(&sources, 5), b"notes");
    assert!(sources.open(4).unwrap().is_none());
    assert_eq!(why(sources.open(6)), ManifestError::NotListed);
    let encoded = manifest.encode(&limits()).unwrap();
    assert_eq!(Manifest::decode(&encoded, &limits()).unwrap(), manifest);
}

#[test]
fn an_empty_folder_is_listed_and_checked_when_opened() {
    let (_scratch, dir) = scratch("empty");
    std::fs::create_dir_all(dir.join("Empty/inner")).unwrap();
    let (manifest, sources) = list(&[dir.join("Empty")]).unwrap();
    assert_eq!(
        paths(&manifest),
        [("Empty", Kind::Folder, 0), ("Empty/inner", Kind::Folder, 0)]
    );
    assert!(sources.open(1).unwrap().is_none());
    // Made before the first goes, so its number differs.
    std::fs::create_dir(dir.join("other")).unwrap();
    std::fs::remove_dir(dir.join("Empty/inner")).unwrap();
    assert_eq!(why(sources.open(1)), ManifestError::Changed);
    // Another folder of the same name is not the one listed.
    std::fs::rename(dir.join("other"), dir.join("Empty/inner")).unwrap();
    assert_eq!(why(sources.open(1)), ManifestError::Changed);
}

#[test]
fn picked_files_of_the_same_name_from_two_folders_are_both_listed() {
    let (_scratch, dir) = scratch("two-roots");
    write(&dir.join("one/a.txt"), b"one");
    write(&dir.join("two/A.TXT"), b"two");
    let (manifest, sources) = list(&[dir.join("one/a.txt"), dir.join("two/A.TXT")]).unwrap();
    assert_eq!(
        paths(&manifest),
        [("a.txt", Kind::File, 3), ("A (2).TXT", Kind::File, 3)]
    );
    assert_eq!(read(&sources, 0), b"one");
    assert_eq!(read(&sources, 1), b"two");
}

/// NTFS keeps both forms of an accent apart, as Linux does; only Linux
/// keeps both cases. macOS keeps neither apart: one name, one file.
#[test]
fn files_on_disk_that_differ_only_in_case_or_accents_are_numbered() {
    let (_scratch, dir) = scratch("case");
    write(&dir.join("r/cafe\u{301}"), b"decomposed");
    write(&dir.join("r/café"), b"composed");
    if cfg!(target_os = "macos") {
        let (manifest, _) = list(&[dir.join("r")]).unwrap();
        assert_eq!(manifest.entries().len(), 2);
        return;
    }
    let case = cfg!(unix);
    let mut expected = vec![("r", Kind::Folder, 0)];
    if case {
        write(&dir.join("r/a.txt"), b"lower");
        write(&dir.join("r/A.txt"), b"upper");
        expected.extend([("r/A.txt", Kind::File, 5), ("r/a (2).txt", Kind::File, 5)]);
    }
    expected.extend([("r/café", Kind::File, 10), ("r/café (2)", Kind::File, 8)]);
    let (manifest, sources) = list(&[dir.join("r")]).unwrap();
    assert_eq!(paths(&manifest), expected);
    let last = manifest.entries().len() as u32 - 1;
    assert_eq!(read(&sources, last - 1), b"decomposed");
    assert_eq!(read(&sources, last), b"composed");
    if case {
        assert_eq!(read(&sources, 1), b"upper");
        assert_eq!(read(&sources, 2), b"lower");
    }
}

#[test]
fn a_list_over_the_entry_limit_is_refused_whole_and_one_at_it_is_not() {
    let (_scratch, dir) = scratch("count");
    for i in 0..4 {
        write(&dir.join(format!("r/{i}")), b"x");
    }
    let at = Limits {
        entries: 5,
        ..limits()
    };
    let (manifest, _) = Sources::list(1, &[dir.join("r")], &at).unwrap();
    assert_eq!(manifest.entries().len(), 5);
    let under = Limits { entries: 4, ..at };
    assert_eq!(
        why(Sources::list(1, &[dir.join("r")], &under)),
        ManifestError::TooMany { most: 4 }
    );
}

#[test]
fn a_list_over_the_size_limit_is_refused_and_one_at_it_is_not() {
    let (_scratch, dir) = scratch("total");
    write(&dir.join("r/a"), &[0; 600]);
    write(&dir.join("r/b"), &[0; 424]);
    let at = Limits {
        bytes: 1024,
        ..limits()
    };
    let (manifest, _) = Sources::list(1, &[dir.join("r")], &at).unwrap();
    assert_eq!(manifest.bytes(), 1024);
    let under = Limits { bytes: 1023, ..at };
    assert_eq!(
        why(Sources::list(1, &[dir.join("r")], &under)),
        ManifestError::TooLarge { most: 1023 }
    );
}

#[test]
fn a_list_nested_too_deep_is_refused_whole() {
    let (_scratch, dir) = scratch("deep");
    let mut path = dir.join("r");
    for _ in 1..MAX_DEPTH - 1 {
        path.push("d");
    }
    write(&path.join("f"), b"x");
    let (manifest, _) = list(&[dir.join("r")]).unwrap();
    assert_eq!(manifest.entries().len(), MAX_DEPTH);
    write(&path.join("d/f"), b"x");
    assert!(matches!(
        why(list(&[dir.join("r")])),
        ManifestError::TooDeep { most: 32, .. }
    ));
}

#[test]
// macOS cannot hold a whole path past 1,024 bytes, so the default limit is checked elsewhere.
#[cfg(not(target_os = "macos"))]
fn a_list_with_a_path_too_long_in_characters_is_refused_whole() {
    let (_scratch, dir) = scratch("long");
    let mut inner = dir.join("r");
    for _ in 0..5 {
        inner.push("x".repeat(200));
    }
    write(&inner.join("y".repeat(17)), b"x");
    let (manifest, _) = list(&[dir.join("r")]).unwrap();
    assert_eq!(manifest.entries()[6].path.chars().count(), MAX_PATH_CHARS);
    write(&inner.join("y".repeat(18)), b"x");
    assert!(matches!(
        why(list(&[dir.join("r")])),
        ManifestError::PathTooLong { most: 1024, .. }
    ));
}

/// The character limit is a setting: a path at it lists, one past it refuses
/// the list whole. Short enough for every system's own path limit.
#[test]
fn a_path_past_the_character_setting_refuses_the_list_whole() {
    let (_scratch, dir) = scratch("long-setting");
    let short = Limits {
        path_chars: 400,
        ..limits()
    };
    let mut inner = dir.join("r");
    for _ in 0..2 {
        inner.push("x".repeat(150));
    }
    // "r/" + two 150-character folders with their slashes is 304 characters.
    write(&inner.join("y".repeat(96)), b"x");
    let (manifest, _) = Sources::list(1, &[dir.join("r")], &short).unwrap();
    assert_eq!(manifest.entries()[3].path.chars().count(), 400);
    write(&inner.join("y".repeat(97)), b"x");
    assert!(matches!(
        why(Sources::list(1, &[dir.join("r")], &short)),
        ManifestError::PathTooLong { most: 400, .. }
    ));
}

/// A path within the characters that the caller's framing cannot carry
/// refuses the list whole. Linux and macOS keep a name to 255 bytes, so
/// this needs Windows' names of 255 characters.
#[cfg(windows)]
#[test]
fn a_listed_path_the_callers_check_refuses_is_refused_whole() {
    let (_scratch, dir) = scratch("wide");
    let fits = Limits {
        fits: Some(fits_4050),
        ..wide_limits()
    };
    let parts = wide_parts("");
    let root = dir.join(&parts[0]);
    let inner: PathBuf = std::iter::once(dir.clone())
        .chain(parts[..10].iter().map(PathBuf::from))
        .collect();
    let at = inner.join("😀".repeat(10));
    write(&at, b"x");
    let (manifest, _) = Sources::list(1, std::slice::from_ref(&root), &fits).unwrap();
    assert_eq!(manifest.entries()[10].path.len(), 4050);
    std::fs::remove_file(&at).unwrap();
    let over = inner.join(format!("{}a", "😀".repeat(10)));
    write(&over, b"x");
    assert_eq!(
        why(Sources::list(1, std::slice::from_ref(&root), &fits)),
        ManifestError::DoesNotFit
    );
}

#[test]
fn a_list_through_a_linked_folder_follows_the_link_above_it() {
    let (_scratch, dir) = scratch("link-above");
    write(&dir.join("real/a.txt"), b"a");
    write(&dir.join("real/r/b.txt"), b"bee");
    let link = dir.join("link");
    if !link_folder(&dir.join("real"), &link) {
        return no_link_here("a list through a linked folder");
    }
    let (manifest, sources) = list(&[link.join("a.txt"), link.join("r")]).unwrap();
    assert_eq!(
        paths(&manifest),
        [
            ("a.txt", Kind::File, 1),
            ("r", Kind::Folder, 0),
            ("r/b.txt", Kind::File, 3),
        ]
    );
    assert_eq!(read(&sources, 0), b"a");
    assert_eq!(read(&sources, 2), b"bee");
    // The link picked itself is still refused, as a folder opened as given is.
    assert!(is_link_error(&why(list(std::slice::from_ref(&link)))));
    assert!(is_link(&Directory::open(&link).unwrap_err()));
    drop(sources);
    remove_link(&link);
}

/// A kept-out folder never leaves: picked itself, a file in it, a folder
/// that holds it, or a file reached through a link to it, the list is
/// refused whole. A pick beside it is listed.
#[test]
fn a_list_that_reaches_a_kept_out_folder_is_refused_whole() {
    let (_scratch, dir) = scratch("kept-out");
    let kept = dir.join("Data/Kept");
    write(&kept.join("host.key"), b"key");
    write(&kept.join("sub/notes.txt"), b"n");
    write(&dir.join("Data/other.txt"), b"o");
    write(&dir.join("Work/a.txt"), b"a");
    let listed = |roots: &[PathBuf]| {
        Sources::list_keeping_out(1, roots, std::slice::from_ref(&kept), &limits())
    };
    for roots in [
        vec![kept.clone()],
        vec![kept.join("host.key")],
        vec![kept.join("sub/notes.txt")],
        vec![dir.join("Data")],
        vec![dir.join("Work/a.txt"), kept.join("host.key")],
    ] {
        assert_eq!(why(listed(&roots)), ManifestError::KeptOut, "{roots:?}");
    }
    let (manifest, _) = listed(&[dir.join("Work/a.txt"), dir.join("Data/other.txt")]).unwrap();
    assert_eq!(manifest.files(), 2);
    // With nothing kept out, the same pick is listed.
    assert!(list(&[kept.join("host.key")]).is_ok());
    if !link_folder(&kept, &dir.join("Work/link")) {
        return no_link_here("a link to a kept-out folder");
    }
    assert_eq!(
        why(listed(&[dir.join("Work/link/host.key")])),
        ManifestError::KeptOut
    );
    remove_link(&dir.join("Work/link"));
}

/// A pick from a folder with no volume of this computer's, as a network
/// share is, is refused whole; and through a real share where one can be
/// reached.
#[test]
fn a_list_from_a_network_folder_is_refused() {
    let (_scratch, dir) = scratch("network");
    write(&dir.join("Work/a.txt"), b"a");
    NO_VOLUME.set(true);
    let listed = list(&[dir.join("Work/a.txt")]);
    NO_VOLUME.set(false);
    assert_eq!(why(listed), ManifestError::Network);
    assert!(list(&[dir.join("Work/a.txt")]).is_ok());
    #[cfg(windows)]
    {
        let Some(shared) = admin_share(&dir.join("Work")) else {
            return;
        };
        assert_eq!(why(list(&[shared.join("a.txt")])), ManifestError::Network);
    }
}

/// `path` through this computer's own admin share, where it can be reached.
#[cfg(windows)]
fn admin_share(path: &Path) -> Option<PathBuf> {
    let local = std::fs::canonicalize(path).unwrap();
    let local = local
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_string();
    let (drive, rest) = local.split_once(':').unwrap();
    let shared = PathBuf::from(format!(r"\\localhost\{drive}${rest}"));
    if !shared.is_dir() {
        eprintln!("skipped a real share: this computer's admin share cannot be reached");
        return None;
    }
    Some(shared)
}

/// A hard link to a file in a kept-out folder, made outside it, is that
/// file: refused picked itself or in a folder, from either of two kept-out
/// folders.
#[test]
fn a_hard_link_into_a_kept_out_folder_and_a_second_kept_out_folder_are_kept_out() {
    let (_scratch, dir) = scratch("kept-linked");
    let (one, two) = (dir.join("Roaming/Kept"), dir.join("Local/Kept"));
    write(&one.join("host.key"), b"key");
    write(&two.join("device-key"), b"device");
    write(&dir.join("Work/a.txt"), b"a");
    std::fs::hard_link(one.join("host.key"), dir.join("Work/linked.key")).unwrap();
    write(&dir.join("Other/b.txt"), b"b");
    std::fs::hard_link(two.join("device-key"), dir.join("Other/device")).unwrap();
    let kept = [one.clone(), two.clone()];
    let listed = |roots: &[PathBuf]| Sources::list_keeping_out(1, roots, &kept, &limits());
    for roots in [
        vec![dir.join("Work/linked.key")],
        vec![dir.join("Work")],
        vec![two.join("device-key")],
        vec![dir.join("Local")],
        vec![dir.join("Other")],
    ] {
        assert_eq!(why(listed(&roots)), ManifestError::KeptOut, "{roots:?}");
    }
    assert!(listed(&[dir.join("Work/a.txt"), dir.join("Other/b.txt")]).is_ok());
}

/// A kept-out folder reached through this computer's own admin share is
/// the same folder, where the share passes the files' identities.
#[cfg(windows)]
#[test]
fn a_kept_out_folder_through_an_admin_share_is_kept_out() {
    let (_scratch, dir) = scratch("kept-share");
    let kept = dir.join("Data/Kept");
    write(&kept.join("host.key"), b"key");
    let kept = std::fs::canonicalize(&kept).unwrap();
    let Some(shared) = admin_share(&kept) else {
        return;
    };
    if std::fs::metadata(shared.join("host.key")).is_err() {
        return eprintln!("skipped the admin share: its files cannot be opened here");
    }
    let kept = [kept];
    assert_eq!(
        why(Sources::list_keeping_out(
            1,
            &[shared.join("host.key")],
            &kept,
            &limits()
        )),
        ManifestError::KeptOut
    );
    // The folder itself, from a share's folder above it: refused as a
    // network folder before anything in it is walked.
    assert_eq!(
        why(Sources::list_keeping_out(
            1,
            std::slice::from_ref(&shared),
            &kept,
            &limits()
        )),
        ManifestError::Network
    );
}

/// Where a folder really is cannot be read: it counts as kept out.
#[test]
fn a_kept_out_folder_whose_place_cannot_be_read_refuses_the_list() {
    let (_scratch, dir) = scratch("kept-unsure");
    let kept = dir.join("Data/Kept");
    write(&kept.join("host.key"), b"key");
    write(&dir.join("Work/a.txt"), b"a");
    let held = Kept::open(std::slice::from_ref(&kept), &limits()).unwrap();
    let work = Directory::followed(&dir.join("Work")).unwrap();
    assert!(!held.holds(&work));
    FINAL_PATH_FAILS.set(true);
    assert!(held.holds(&work), "unsure is held");
    let opened = Kept::open(std::slice::from_ref(&kept), &limits()).map(|_| ());
    let listed = Sources::list_keeping_out(
        1,
        &[dir.join("Work/a.txt")],
        std::slice::from_ref(&kept),
        &limits(),
    );
    FINAL_PATH_FAILS.set(false);
    assert_eq!(why(opened), ManifestError::KeptOut);
    assert_eq!(why(listed), ManifestError::KeptOut);
    // A folder that cannot be found is no more sure.
    assert_eq!(
        why(Sources::list_keeping_out(
            1,
            &[dir.join("Work/a.txt")],
            &[dir.join("missing")],
            &limits()
        )),
        ManifestError::KeptOut
    );
}

/// A file saved by renaming, or a folder taken away, while a kept-out
/// folder is walked does not refuse an unrelated list; any other error
/// reading it still does.
#[test]
fn an_entry_gone_from_a_kept_out_folder_mid_walk_is_skipped() {
    let (_scratch, dir) = scratch("kept-gone");
    let kept = dir.join("Data/Kept");
    write(&kept.join("host.key"), b"key");
    write(&kept.join("settings.tmp"), b"saving");
    write(&kept.join("journal/1.log"), b"j");
    write(&kept.join("old/2.log"), b"j");
    write(&dir.join("Work/a.txt"), b"a");
    let kept_out = std::slice::from_ref(&kept);
    let pick = [dir.join("Work/a.txt")];
    GONE_BEFORE_INSPECT.with_borrow_mut(|gone| {
        gone.insert("settings.tmp".into());
        gone.insert("journal".into());
    });
    GONE_BEFORE_OPEN.with_borrow_mut(|gone| gone.insert("old".into()));
    let listed = Sources::list_keeping_out(1, &pick, kept_out, &limits());
    let left = GONE_BEFORE_INSPECT.with_borrow(|gone| gone.len())
        + GONE_BEFORE_OPEN.with_borrow(|gone| gone.len());
    assert_eq!(left, 0, "every entry was taken away mid-walk");
    assert!(!kept.join("old").exists());
    let (manifest, _) = listed.unwrap();
    assert_eq!(manifest.entries()[0].path, "a.txt");
    // The key is still kept out.
    assert_eq!(
        why(Sources::list_keeping_out(
            1,
            &[kept.join("host.key")],
            kept_out,
            &limits()
        )),
        ManifestError::KeptOut
    );
    for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::Other] {
        INSPECT_FAILS.with_borrow_mut(|fails| fails.insert("host.key".into(), kind));
        let listed = Sources::list_keeping_out(1, &pick, kept_out, &limits());
        INSPECT_FAILS.with_borrow_mut(|fails| fails.clear());
        assert_eq!(why(listed), ManifestError::KeptOut, "{kind:?}");
    }
}

/// The kept-out walk is bounded: past `kept_entries` nothing is listed.
#[test]
fn a_kept_out_folder_holding_more_than_its_limit_refuses_every_list() {
    let (_scratch, dir) = scratch("kept-many");
    let kept = dir.join("Kept");
    for i in 0..3 {
        write(&kept.join(format!("{i}")), b"x");
    }
    write(&dir.join("Work/a.txt"), b"a");
    // The folder itself and its three files.
    let at = Limits {
        kept_entries: 4,
        ..limits()
    };
    let pick = [dir.join("Work/a.txt")];
    assert!(Sources::list_keeping_out(1, &pick, std::slice::from_ref(&kept), &at).is_ok());
    let under = Limits {
        kept_entries: 3,
        ..limits()
    };
    assert_eq!(
        why(Sources::list_keeping_out(
            1,
            &pick,
            std::slice::from_ref(&kept),
            &under
        )),
        ManifestError::KeptOut
    );
}

#[test]
fn a_link_or_junction_in_a_list_refuses_it_whole() {
    let (_scratch, dir) = scratch("link-in");
    write(&dir.join("outside/secret.txt"), b"secret");
    write(&dir.join("r/a.txt"), b"a");
    if !link_folder(&dir.join("outside"), &dir.join("r/link")) {
        return no_link_here("a link in a list");
    }
    assert_eq!(
        why(list(&[dir.join("r")])),
        ManifestError::Link {
            at: dir.join("r").join("link")
        }
    );
    // Picked itself, it is refused too, not followed.
    assert!(is_link_error(&why(list(&[dir.join("r/link")]))));
    remove_link(&dir.join("r/link"));
}

#[test]
fn a_folder_swapped_for_a_link_after_listing_is_changed() {
    let (_scratch, dir) = scratch("link-swap");
    write(&dir.join("r/inner/a.txt"), b"listed");
    write(&dir.join("outside/a.txt"), b"secret");
    let (manifest, sources) = list(&[dir.join("r")]).unwrap();
    assert_eq!(manifest.entries()[2].path, "r/inner/a.txt");
    std::fs::rename(dir.join("r/inner"), dir.join("moved")).unwrap();
    if !link_folder(&dir.join("outside"), &dir.join("r/inner")) {
        return no_link_here("a folder swapped for a link");
    }
    assert_eq!(why(sources.open(2)), ManifestError::Changed);
    assert_eq!(why(sources.open(1)), ManifestError::Changed);
    remove_link(&dir.join("r/inner"));
}

#[test]
fn a_replaced_folder_cannot_offer_the_same_file_through_a_hard_link() {
    let (_scratch, dir) = scratch("folder-hard-link");
    write(&dir.join("r/inner/a.txt"), b"listed");
    let (_, sources) = list(&[dir.join("r")]).unwrap();
    std::fs::rename(dir.join("r/inner"), dir.join("moved")).unwrap();
    std::fs::create_dir(dir.join("r/inner")).unwrap();
    std::fs::hard_link(dir.join("moved/a.txt"), dir.join("r/inner/a.txt")).unwrap();
    assert_eq!(why(sources.open(2)), ManifestError::Changed);
}

#[test]
fn a_picked_folder_swapped_for_a_link_after_listing_is_changed() {
    let (_scratch, dir) = scratch("root-swap");
    write(&dir.join("r/a.txt"), b"listed");
    write(&dir.join("outside/a.txt"), b"secret");
    let (_, sources) = list(&[dir.join("r")]).unwrap();
    std::fs::rename(dir.join("r"), dir.join("moved")).unwrap();
    if !link_folder(&dir.join("outside"), &dir.join("r")) {
        return no_link_here("a picked folder swapped for a link");
    }
    assert_eq!(why(sources.open(1)), ManifestError::Changed);
    remove_link(&dir.join("r"));
    // Nor does a real folder put in its place stand for it.
    std::fs::rename(dir.join("outside"), dir.join("r")).unwrap();
    assert_eq!(why(sources.open(1)), ManifestError::Changed);
}

/// A list made on Unix for Windows: names Unix allows and Windows does not
/// (a colon, a backslash, a device name, a trailing dot or space, two names
/// apart only in case) are listed as names Windows can hold, each still
/// opening its own file.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn a_unix_list_holds_only_names_windows_can_hold() {
    let (_scratch, dir) = scratch("unix-names");
    let originals = [
        "a:b.txt",
        r"x\y.txt",
        "Readme.txt",
        "README.txt",
        "trail.",
        "space ",
        "q?.txt",
        "pipe|.txt",
        "CON.txt",
        "aux",
    ];
    for name in originals {
        write(&dir.join("Picked").join(name), name.as_bytes());
    }
    let (manifest, sources) = list(&[dir.join("Picked")]).unwrap();
    let rules = safecopy_names::NameRules::default();
    let mut folded = std::collections::HashSet::new();
    let mut contents = Vec::new();
    for (n, (path, kind, _)) in paths(&manifest).into_iter().enumerate() {
        let name = path.rsplit('/').next().unwrap();
        assert_eq!(safe_name(name, &rules).as_deref(), Some(name), "{name:?}");
        assert!(!reserved(name), "{name:?}");
        assert!(!name.contains([':', '\\', '?', '|', '/']), "{name:?}");
        assert!(!name.ends_with(['.', ' ']), "{name:?}");
        if kind == Kind::File {
            assert!(folded.insert(fold(name)), "{name:?} twice on Windows");
            contents.push(String::from_utf8(read(&sources, n as u32)).unwrap());
        }
    }
    contents.sort();
    let mut wanted: Vec<String> = originals.iter().map(|n| n.to_string()).collect();
    wanted.sort();
    assert_eq!(contents, wanted, "each name opens its own file");
}

/// A name that is not Unicode cannot travel without loss: the list is
/// refused, not guessed.
#[test]
fn a_name_on_disk_that_is_not_unicode_refuses_the_list() {
    let (_scratch, dir) = scratch("not-unicode");
    #[cfg(unix)]
    let name = {
        use std::os::unix::ffi::OsStrExt;
        OsStr::from_bytes(b"caf\xe9.txt").to_os_string()
    };
    #[cfg(windows)]
    let name = {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&[0x61, 0xd800, 0x62])
    };
    let path = dir.join("r").join(&name);
    std::fs::create_dir_all(dir.join("r")).unwrap();
    if std::fs::write(&path, b"x").is_err() {
        // APFS keeps names to UTF-8.
        return eprintln!("skipped: this file system takes only Unicode names");
    }
    assert_eq!(
        why(list(&[dir.join("r")])),
        ManifestError::BadName {
            at: path,
            why: NameError::NotUnicode
        }
    );
}

#[cfg(unix)]
#[test]
fn a_file_swapped_for_a_link_after_listing_is_changed() {
    let (_scratch, dir) = scratch("file-swap");
    write(&dir.join("r/a.txt"), b"listed");
    write(&dir.join("outside.txt"), b"secret");
    let (_, sources) = list(&[dir.join("r")]).unwrap();
    std::fs::remove_file(dir.join("r/a.txt")).unwrap();
    std::os::unix::fs::symlink(dir.join("outside.txt"), dir.join("r/a.txt")).unwrap();
    assert_eq!(why(sources.open(1)), ManifestError::Changed);
}

#[cfg(windows)]
#[test]
fn a_file_swapped_for_a_junction_after_listing_is_changed() {
    let (_scratch, dir) = scratch("file-junction");
    write(&dir.join("r/a.txt"), b"listed");
    write(&dir.join("outside/a.txt"), b"secret");
    let (_, sources) = list(&[dir.join("r")]).unwrap();
    std::fs::remove_file(dir.join("r/a.txt")).unwrap();
    if !link_folder(&dir.join("outside"), &dir.join("r/a.txt")) {
        return no_link_here("a file swapped for a junction");
    }
    assert_eq!(why(sources.open(1)), ManifestError::Changed);
    remove_link(&dir.join("r/a.txt"));
}

#[test]
fn a_file_of_another_size_is_changed_though_its_time_is_put_back() {
    let (_scratch, dir) = scratch("size-only");
    let path = dir.join("a.txt");
    write(&path, b"listed");
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let (_, sources) = list(std::slice::from_ref(&path)).unwrap();
    let opened = sources.open(0).unwrap().unwrap();
    let grown = std::fs::File::options().append(true).open(&path).unwrap();
    (&grown).write_all(b" and more").unwrap();
    grown.set_modified(modified).unwrap();
    drop(grown);
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        modified
    );
    assert_eq!(why(opened.unchanged()), ManifestError::Changed);
    assert_eq!(why(sources.open(0)), ManifestError::Changed);
}

/// The source check: another size, another modified time, another file in
/// its place (same size and time) or gone is changed; a folder in a file's
/// place is no longer a file.
#[test]
fn a_source_changed_in_size_time_or_identity_is_changed() {
    let (_scratch, dir) = scratch("changed");
    for name in ["size", "time", "other", "gone", "folder"] {
        write(&dir.join("r").join(name), b"listed");
    }
    let (manifest, sources) = list(&[dir.join("r")]).unwrap();
    let n = |name: &str| {
        manifest
            .entries()
            .iter()
            .position(|e| e.path == format!("r/{name}"))
            .unwrap() as u32
    };
    let modified = std::fs::metadata(dir.join("r/other"))
        .unwrap()
        .modified()
        .unwrap();
    std::fs::write(dir.join("r/size"), b"longer now").unwrap();
    let touched = std::fs::File::options()
        .write(true)
        .open(dir.join("r/time"))
        .unwrap();
    touched
        .set_modified(modified + std::time::Duration::from_secs(5))
        .unwrap();
    drop(touched);
    // Same size, same time, another file: made before the first goes, so its number differs.
    std::fs::write(dir.join("faked"), b"faked!").unwrap();
    let faked = std::fs::File::options()
        .write(true)
        .open(dir.join("faked"))
        .unwrap();
    faked.set_modified(modified).unwrap();
    drop(faked);
    std::fs::rename(dir.join("faked"), dir.join("r/other")).unwrap();
    std::fs::remove_file(dir.join("r/gone")).unwrap();
    std::fs::remove_file(dir.join("r/folder")).unwrap();
    std::fs::create_dir(dir.join("r/folder")).unwrap();
    for name in ["size", "time", "other", "gone"] {
        assert_eq!(why(sources.open(n(name))), ManifestError::Changed, "{name}");
    }
    assert_eq!(
        why(sources.open(n("folder"))),
        ManifestError::NotAFile {
            at: dir.join("r").join("folder")
        }
    );
}

/// A pipe under a listed file's name is no longer a file, and is not opened.
#[cfg(unix)]
#[test]
fn a_source_that_became_a_special_file_is_no_longer_a_file() {
    let (_scratch, dir) = scratch("source-fifo");
    write(&dir.join("r/a.txt"), b"listed");
    let (_, sources) = list(&[dir.join("r")]).unwrap();
    std::fs::remove_file(dir.join("r/a.txt")).unwrap();
    fifo(&dir.join("r/a.txt"));
    assert_eq!(
        why(sources.open(1)),
        ManifestError::NotAFile {
            at: dir.join("r").join("a.txt")
        }
    );
}

#[test]
fn a_file_written_while_it_is_read_is_refused_after() {
    let (_scratch, dir) = scratch("during");
    write(&dir.join("a.txt"), b"listed");
    let (_, sources) = list(&[dir.join("a.txt")]).unwrap();
    let opened = sources.open(0).unwrap().unwrap();
    assert_eq!(opened.size(), 6);
    let mut other = std::fs::File::options()
        .append(true)
        .open(dir.join("a.txt"))
        .unwrap();
    other.write_all(b" and more").unwrap();
    drop(other);
    assert_eq!(why(opened.unchanged()), ManifestError::Changed);
}

#[test]
fn an_online_only_file_is_listed_from_its_metadata_and_fetched_when_opened() {
    let (_scratch, dir) = scratch("cloud");
    write(&dir.join("r/online.docx"), b"in the cloud");
    write(&dir.join("r/recall.docx"), b"also");
    write(&dir.join("r/local.docx"), b"here");
    ATTRIBUTES.with_borrow_mut(|given| {
        given.insert("online.docx".into(), RECALL_ON_DATA_ACCESS | OFFLINE);
        given.insert("recall.docx".into(), RECALL_ON_OPEN);
    });
    let listed = list(&[dir.join("r")]);
    ATTRIBUTES.with_borrow_mut(HashMap::clear);
    let (manifest, sources) = listed.unwrap();
    assert_eq!(
        paths(&manifest)[1..],
        [
            ("r/local.docx", Kind::File, 4),
            ("r/online.docx", Kind::File, 12),
            ("r/recall.docx", Kind::File, 4),
        ]
    );
    assert!(!sources.online_only(1));
    assert!(sources.online_only(2));
    assert!(sources.online_only(3));
    assert!(!sources.online_only(4));
    assert_eq!(read(&sources, 2), b"in the cloud");
}

/// A known modified time for `path`, set through a handle of its own.
fn set_time(path: &Path, secs: u64) -> Time {
    let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(time).unwrap();
    Time::from_system(time).unwrap()
}

/// The modified time the disk gives for `path`, read by path.
fn time_of(path: &Path) -> Option<Time> {
    Time::from_system(std::fs::metadata(path).unwrap().modified().unwrap())
}

/// Holds `path` open so that nothing else can open it for its data
/// (Windows: shared with no one).
#[cfg(windows)]
fn held_alone(path: &Path) -> std::fs::File {
    use std::os::windows::fs::OpenOptionsExt;
    let held = std::fs::File::options()
        .read(true)
        .share_mode(0)
        .open(path)
        .unwrap();
    assert!(std::fs::File::open(path).is_err(), "not held alone");
    held
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Each entry's time and execute bit come with the list, a folder's time
/// too, and no file is opened for them: on Windows the file is held so
/// that no one else can open it for its data while it is listed.
#[test]
fn a_list_gives_each_entry_s_time_and_execute_bit_without_opening_it() {
    let (_scratch, dir) = scratch("meta");
    write(&dir.join("r/sub/doc.txt"), b"text");
    write(&dir.join("r/tool"), b"#!");
    let doc = set_time(&dir.join("r/sub/doc.txt"), 1_000_000_000);
    let tool = set_time(&dir.join("r/tool"), 1_500_000_000);
    #[cfg(unix)]
    set_mode(&dir.join("r/tool"), 0o744);
    #[cfg(windows)]
    let held = held_alone(&dir.join("r/sub/doc.txt"));
    let (manifest, sources) = list(&[dir.join("r")]).unwrap();
    assert_eq!(
        paths(&manifest),
        [
            ("r", Kind::Folder, 0),
            ("r/sub", Kind::Folder, 0),
            ("r/sub/doc.txt", Kind::File, 4),
            ("r/tool", Kind::File, 2),
        ]
    );
    let file = |modified, executable| {
        Some(Meta {
            modified: Some(modified),
            executable,
        })
    };
    assert_eq!(sources.meta(2), file(doc, false));
    // The owner's bit, where the system has one.
    assert_eq!(sources.meta(3), file(tool, cfg!(unix)));
    // A folder: its time, and never a program though it can be searched.
    let sub = sources.meta(1).unwrap();
    assert_eq!(sub.modified, time_of(&dir.join("r/sub")));
    assert!(sub.modified.is_some() && !sub.executable);
    assert!(!sources.meta(0).unwrap().executable);
    assert_eq!(sources.meta(4), None);
    #[cfg(windows)]
    {
        // Listed while no one could read it; opening it for its data still cannot.
        assert_eq!(why(sources.open(2)), ManifestError::Changed);
        drop(held);
    }
    assert_eq!(read(&sources, 2), b"text");
    assert_eq!(read(&sources, 3), b"#!");
}

/// An online-only file's time comes from the metadata it is listed from.
#[test]
fn an_online_only_file_s_time_is_given_without_fetching_it() {
    let (_scratch, dir) = scratch("cloud-meta");
    write(&dir.join("r/online.docx"), b"in the cloud");
    let time = set_time(&dir.join("r/online.docx"), 1_200_000_000);
    ATTRIBUTES.with_borrow_mut(|given| {
        given.insert("online.docx".into(), RECALL_ON_DATA_ACCESS);
    });
    // Fetching it means opening it for its data, which no one can here.
    #[cfg(windows)]
    let held = held_alone(&dir.join("r/online.docx"));
    let listed = list(&[dir.join("r")]);
    ATTRIBUTES.with_borrow_mut(HashMap::clear);
    let (_, sources) = listed.unwrap();
    assert!(sources.online_only(1));
    assert_eq!(
        sources.meta(1),
        Some(Meta {
            modified: Some(time),
            executable: false,
        })
    );
    #[cfg(windows)]
    drop(held);
    assert_eq!(read(&sources, 1), b"in the cloud");
}

/// Linux lists by a path handle: a file this user may not read still gives
/// its time, with nothing opened for its data.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn a_file_that_cannot_be_read_still_gives_its_time_with_the_list() {
    let (_scratch, dir) = scratch("no-read");
    let locked = dir.join("r/locked");
    write(&locked, b"listed");
    let modified = time_of(&locked);
    set_mode(&locked, 0o000);
    // An administrator reads any file: nothing to show then.
    if std::fs::File::open(&locked).is_err() {
        let (_, sources) = list(&[dir.join("r")]).unwrap();
        let meta = sources.meta(1).unwrap();
        assert_eq!((meta.modified, meta.executable), (modified, false));
    }
    set_mode(&locked, 0o644);
}

/// The execute bit alone, changed since the list, is a changed source: the
/// other side was told what the list gave. Put back, it opens.
#[cfg(unix)]
#[test]
fn a_source_whose_execute_bit_changed_is_changed() {
    let (_scratch, dir) = scratch("bit-changed");
    write(&dir.join("r/gained"), b"listed");
    write(&dir.join("r/lost"), b"listed");
    set_mode(&dir.join("r/lost"), 0o755);
    let (_, sources) = list(&[dir.join("r")]).unwrap();
    assert!(!sources.meta(1).unwrap().executable);
    assert!(sources.meta(2).unwrap().executable);
    let listing = Listing::list(&dir.join("r"), &limits()).unwrap();
    let opened = sources.open(1).unwrap().unwrap();
    opened.unchanged().unwrap();
    let times: Vec<_> = (1..3).map(|n| sources.meta(n).unwrap().modified).collect();
    set_mode(&dir.join("r/gained"), 0o744);
    set_mode(&dir.join("r/lost"), 0o644);
    // Size, time and identity are as listed: only the bit differs.
    assert_eq!(time_of(&dir.join("r/gained")), times[0]);
    assert_eq!(time_of(&dir.join("r/lost")), times[1]);
    assert_eq!(why(opened.unchanged()), ManifestError::Changed);
    for n in [1, 2] {
        assert_eq!(why(sources.open(n)), ManifestError::Changed, "{n}");
    }
    for entry in &listing.entries {
        assert_eq!(why(listing.open_entry(entry)), ManifestError::Changed);
    }
    // The group's and others' bits are not the owner's.
    set_mode(&dir.join("r/gained"), 0o655);
    set_mode(&dir.join("r/lost"), 0o700);
    opened.unchanged().unwrap();
    for n in [1, 2] {
        assert_eq!(read(&sources, n), b"listed");
    }
    for entry in &listing.entries {
        assert!(listing.open_entry(entry).unwrap().is_some());
    }
}

#[test]
fn a_file_a_storage_service_took_offline_refuses_the_list() {
    let (_scratch, dir) = scratch("offline");
    write(&dir.join("r/tape.bin"), b"gone");
    ATTRIBUTES.with_borrow_mut(|given| given.insert("tape.bin".into(), OFFLINE));
    let listed = list(&[dir.join("r")]);
    ATTRIBUTES.with_borrow_mut(HashMap::clear);
    assert_eq!(
        why(listed),
        ManifestError::Offline {
            at: dir.join("r").join("tape.bin")
        }
    );
    let at = Path::new("a");
    assert_eq!(online_only(0, at), Ok(false));
    assert_eq!(online_only(RECALL_ON_DATA_ACCESS, at), Ok(true));
}

#[test]
fn a_whole_drive_or_nothing_is_not_a_list() {
    #[cfg(windows)]
    let drive = PathBuf::from("C:\\");
    #[cfg(unix)]
    let drive = PathBuf::from("/");
    assert_eq!(why(list(&[drive])), ManifestError::WholeDrive);
    assert_eq!(why(list::<PathBuf>(&[])), ManifestError::Empty);
    let (_scratch, dir) = scratch("missing");
    assert!(matches!(
        why(list(&[dir.join("missing")])),
        ManifestError::Unreadable {
            kind: io::ErrorKind::NotFound,
            // ERROR_FILE_NOT_FOUND, ENOENT: the system's own code is kept.
            raw: Some(2),
            ..
        }
    ));
}

#[test]
fn a_list_from_more_than_the_source_folder_limit_is_refused() {
    let (_scratch, dir) = scratch("folders");
    let roots: Vec<PathBuf> = (0..=256)
        .map(|i| {
            let path = dir.join(format!("{i}/{i}.txt"));
            write(&path, b"x");
            path
        })
        .collect();
    let (manifest, sources) = list(&roots[..256]).unwrap();
    assert_eq!(manifest.files(), 256);
    assert_eq!(sources.folders.len(), 256);
    assert_eq!(
        why(list(&roots)),
        ManifestError::TooManyFolders { most: 256 }
    );
    // Files from one folder share it.
    let (_, sources) = list(&[dir.join("0/0.txt"), dir.join("0/0.txt")]).unwrap();
    assert_eq!(sources.folders.len(), 1);
    // The limit is a setting.
    let two = Limits {
        source_folders: 2,
        ..limits()
    };
    assert!(Sources::list(1, &roots[..2], &two).is_ok());
    assert_eq!(
        why(Sources::list(1, &roots[..3], &two)),
        ManifestError::TooManyFolders { most: 2 }
    );
}

#[test]
fn a_list_of_folders_deep_inside_counts_what_waits_above() {
    let (_scratch, dir) = scratch("pending");
    for i in 0..3 {
        write(&dir.join(format!("r/a/{i}")), b"x");
        write(&dir.join(format!("r/{i}")), b"x");
    }
    let at = Limits {
        entries: 8,
        ..limits()
    };
    assert_eq!(
        Sources::list(1, &[dir.join("r")], &at)
            .unwrap()
            .0
            .entries()
            .len(),
        8
    );
    // r/a's files are refused while r's own three still wait.
    let under = Limits { entries: 7, ..at };
    assert_eq!(
        why(Sources::list(1, &[dir.join("r")], &under)),
        ManifestError::TooMany { most: 7 }
    );
}

#[cfg(windows)]
#[test]
fn a_file_swapped_for_a_hard_link_after_listing_is_changed() {
    let (_scratch, dir) = scratch("hard-link");
    write(&dir.join("r/a.txt"), b"listed");
    write(&dir.join("outside.txt"), b"secret");
    let modified = std::fs::metadata(dir.join("r/a.txt"))
        .unwrap()
        .modified()
        .unwrap();
    std::fs::File::options()
        .write(true)
        .open(dir.join("outside.txt"))
        .unwrap()
        .set_modified(modified)
        .unwrap();
    let (_, sources) = list(&[dir.join("r")]).unwrap();
    std::fs::remove_file(dir.join("r/a.txt")).unwrap();
    std::fs::hard_link(dir.join("outside.txt"), dir.join("r/a.txt")).unwrap();
    assert_eq!(why(sources.open(1)), ManifestError::Changed);
}

#[cfg(windows)]
#[test]
fn a_picked_path_naming_a_stream_is_refused() {
    let (_scratch, dir) = scratch("stream");
    write(&dir.join("f.txt"), b"file");
    std::fs::write(dir.join("f.txt:hidden"), b"stream").unwrap();
    assert!(list(&[dir.join("f.txt:hidden")]).is_err());
    assert!(list(&[dir.join("f.txt")]).is_ok());
}

/// A compressed file is a reparse point Windows' own filter reads: it
/// lists from its metadata and opens unchanged.
#[cfg(windows)]
#[test]
fn a_compressed_file_lists_and_opens_through_its_filter() {
    let (_scratch, dir) = scratch("compressed");
    let path = dir.join("r/big.txt");
    write(&path, &b"compress me ".repeat(4096));
    let compacted = std::process::Command::new("compact")
        .args(["/c", "/exe:xpress4k"])
        .arg(&path)
        .output();
    if !matches!(compacted, Ok(output) if output.status.success()) {
        return eprintln!("skipped: this disk does not compress files");
    }
    let (manifest, sources) = list(&[dir.join("r")]).unwrap();
    assert_eq!(manifest.bytes(), 12 * 4096);
    assert!(!sources.online_only(1));
    assert_eq!(read(&sources, 1), b"compress me ".repeat(4096));
}

// A folder listed by handle.

fn names_of(listing: &Listing) -> Vec<&str> {
    listing.entries.iter().map(|e| e.name.as_str()).collect()
}

#[test]
fn a_folder_is_listed_depth_first_with_its_empty_folders() {
    let (_scratch, dir) = scratch("listing");
    let root = dir.join("Photos");
    write(&root.join("b.jpg"), b"bee");
    write(&root.join("2024/a.jpg"), b"a");
    std::fs::create_dir_all(root.join("empty")).unwrap();
    write(&root.join("a.txt"), b"x");
    let listing = Listing::list(&root, &limits()).unwrap();
    assert_eq!(listing.name, "Photos");
    assert_eq!(
        names_of(&listing),
        [
            "Photos/a.txt",
            "Photos/b.jpg",
            "Photos/2024/a.jpg",
            "Photos/empty/"
        ]
    );
    assert_eq!((listing.files, listing.bytes), (3, 5));
    assert!(listing.skipped.is_empty());
    let (mut file, size) = listing.open_entry(&listing.entries[1]).unwrap().unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!((bytes.as_slice(), size), (b"bee".as_slice(), 3));
    assert!(listing.open_entry(&listing.entries[3]).unwrap().is_none());
    assert_eq!(
        listing.entries[2].parts(),
        [OsString::from("2024"), OsString::from("a.jpg")]
    );
}

/// A folder's listing carries each file's time and execute bit, and an
/// empty folder's time, from what the walk read; an entry whose bit is no
/// longer the listed one does not open.
#[test]
fn a_folder_s_listing_gives_each_entry_s_time_and_execute_bit() {
    let (_scratch, dir) = scratch("listing-meta");
    let root = dir.join("Photos");
    write(&root.join("a.txt"), b"a");
    write(&root.join("tool"), b"#!");
    std::fs::create_dir_all(root.join("empty")).unwrap();
    let a = set_time(&root.join("a.txt"), 1_000_000_000);
    let tool = set_time(&root.join("tool"), 1_500_000_000);
    #[cfg(unix)]
    set_mode(&root.join("tool"), 0o700);
    let listing = Listing::list(&root, &limits()).unwrap();
    assert_eq!(
        names_of(&listing),
        ["Photos/a.txt", "Photos/tool", "Photos/empty/"]
    );
    let metas: Vec<Meta> = listing.entries.iter().map(|e| e.meta).collect();
    assert_eq!(
        metas,
        [
            Meta {
                modified: Some(a),
                executable: false,
            },
            Meta {
                modified: Some(tool),
                executable: cfg!(unix),
            },
            Meta {
                modified: time_of(&root.join("empty")),
                executable: false,
            },
        ]
    );
    assert!(metas[2].modified.is_some());
    for entry in &listing.entries {
        listing.open_entry(entry).unwrap();
    }
    // Told to the other side as the opposite of what the file is now.
    for entry in &listing.entries[..2] {
        let mut other = entry.clone();
        other.meta.executable = !other.meta.executable;
        assert_eq!(why(listing.open_entry(&other)), ManifestError::Changed);
    }
}

#[test]
fn a_folder_with_too_many_files_is_refused() {
    let (_scratch, dir) = scratch("too-many");
    let root = dir.join("Big");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    for name in ["a", "b", "sub/c"] {
        std::fs::write(root.join(name), name).unwrap();
    }
    // a, b, sub and sub/c.
    let three = Limits {
        entries: 3,
        ..limits()
    };
    assert_eq!(
        why(Listing::list(&root, &three)),
        ManifestError::TooMany { most: 3 }
    );
    let four = Limits {
        entries: 4,
        ..limits()
    };
    assert_eq!(Listing::list(&root, &four).unwrap().files, 3);
}

/// A folder is refused as soon as it is found too big, without reading
/// the rest of it: reading one entry past the first too many panics.
#[test]
fn a_folder_too_big_is_refused_without_reading_it_all() {
    let mut found = 0;
    let big = (0..100).map(|i| {
        assert!(i < 4, "read past the first entry too many");
        Ok::<u8, io::Error>(1)
    });
    assert_eq!(
        why(taken(big, &mut found, 3)),
        ManifestError::TooMany { most: 3 }
    );
    assert_eq!(found, 4, "it stopped at the first one too many");
}

/// An entry that cannot be read is counted and listed, not dropped.
#[test]
fn entries_that_cannot_be_read_are_listed_as_skipped() {
    let denied = || Err(io::Error::other("access denied"));
    let entries = vec![Ok(1u8), denied(), Ok(2), denied()];
    let mut found = 0;
    let (read, unreadable) = taken(entries.into_iter(), &mut found, 10).unwrap();
    assert_eq!((read, unreadable, found), (vec![1, 2], 2, 4));
}

/// Too deep, or a path not shorter than the limit (room for a folder's
/// `/`), is skipped and said so; at one less it is listed.
#[test]
fn a_folder_s_entries_too_deep_or_too_long_are_skipped() {
    let (_scratch, dir) = scratch("listing-deep");
    let root = dir.join("r");
    write(&root.join("d/d/f"), b"x");
    write(&root.join("abcd"), b"x");
    let at = Limits {
        depth: 3,
        path_chars: 7,
        ..limits()
    };
    // "r/abcd" is 6 characters, "r/d/d/f" 7 and 4 parts.
    let listing = Listing::list(&root, &at).unwrap();
    assert_eq!(names_of(&listing), ["r/abcd", "r/d/d/"]);
    assert_eq!(
        listing.skipped,
        [Skipped {
            path: "r/d/d/f".into(),
            why: Skip::TooDeep
        }]
    );
    let deeper = Limits {
        depth: 4,
        path_chars: 8,
        ..limits()
    };
    let listing = Listing::list(&root, &deeper).unwrap();
    assert_eq!(names_of(&listing), ["r/abcd", "r/d/d/f"]);
}

#[test]
fn a_link_inside_a_listed_folder_is_skipped_and_one_at_its_top_refused() {
    let (_scratch, dir) = scratch("listing-link");
    let root = dir.join("Photos");
    write(&root.join("a.txt"), b"a");
    write(&dir.join("outside/secret.txt"), b"secret");
    if !link_folder(&dir.join("outside"), &root.join("link")) {
        return no_link_here("a link inside a listed folder");
    }
    let listing = Listing::list(&root, &limits()).unwrap();
    assert_eq!(names_of(&listing), ["Photos/a.txt"]);
    assert_eq!(
        listing.skipped,
        [Skipped {
            path: "Photos/link".into(),
            why: Skip::Link
        }]
    );
    assert!(is_link_error(&why(Listing::list(
        &root.join("link"),
        &limits()
    ))));
    assert_eq!(
        why(Listing::list(Path::new("/"), &limits())),
        ManifestError::WholeDrive
    );
    remove_link(&root.join("link"));
}

/// The listed sizes are the most that is sent: a file that grew since is
/// refused, the rest of the folder still opens, and one that shrank opens
/// with its smaller size.
#[test]
fn a_folder_listed_by_handle_sends_nothing_that_grew() {
    let (_scratch, dir) = scratch("grew");
    let root = dir.join("Photos");
    write(&root.join("a.txt"), b"a");
    write(&root.join("b.txt"), b"b");
    write(&root.join("c.txt"), b"three");
    let listing = Listing::list(&root, &limits()).unwrap();
    std::fs::write(root.join("a.txt"), b"a file that grew").unwrap();
    std::fs::write(root.join("c.txt"), b"3").unwrap();
    assert_eq!(
        why(listing.open_entry(&listing.entries[0])),
        ManifestError::Changed
    );
    let (_, size) = listing.open_entry(&listing.entries[1]).unwrap().unwrap();
    assert_eq!(size, 1);
    let (_, size) = listing.open_entry(&listing.entries[2]).unwrap().unwrap();
    assert_eq!(size, 1);
    // A size of 0 when listed lets nothing through.
    let mut unknown = listing.entries[0].clone();
    unknown.size = 0;
    assert_eq!(why(listing.open_entry(&unknown)), ManifestError::Changed);
}

/// A listed file that became a folder is told apart from one that changed,
/// and an entry whose folder is gone names that folder, with the system's
/// code.
#[test]
fn a_listed_file_that_became_a_folder_is_no_longer_a_file() {
    let (_scratch, dir) = scratch("became-folder");
    let root = dir.join("Photos");
    write(&root.join("a.txt"), b"a");
    write(&root.join("sub/b.txt"), b"b");
    let listing = Listing::list(&root, &limits()).unwrap();
    assert_eq!(names_of(&listing), ["Photos/a.txt", "Photos/sub/b.txt"]);
    std::fs::remove_file(root.join("a.txt")).unwrap();
    std::fs::create_dir(root.join("a.txt")).unwrap();
    assert_eq!(
        why(listing.open_entry(&listing.entries[0])),
        ManifestError::NotAFile {
            at: root.join("a.txt")
        }
    );
    std::fs::remove_dir_all(root.join("sub")).unwrap();
    let gone = why(listing.open_entry(&listing.entries[1]));
    let ManifestError::Unreadable { at, kind, raw } = &gone else {
        panic!("{gone:?}");
    };
    assert_eq!(at, &root.join("sub"), "the part that failed");
    assert_eq!(*kind, io::ErrorKind::NotFound);
    assert!(raw.is_some());
}

#[cfg(unix)]
#[test]
fn a_listed_file_that_became_a_special_file_is_no_longer_a_file() {
    let (_scratch, dir) = scratch("became-fifo");
    let root = dir.join("Photos");
    write(&root.join("a.txt"), b"a");
    let listing = Listing::list(&root, &limits()).unwrap();
    std::fs::remove_file(root.join("a.txt")).unwrap();
    fifo(&root.join("a.txt"));
    assert_eq!(
        why(listing.open_entry(&listing.entries[0])),
        ManifestError::NotAFile {
            at: root.join("a.txt")
        }
    );
}

/// A folder inside replaced by a link after listing opens nothing through
/// it; the listed folder itself is held, so its replacement changes
/// nothing that is read.
#[test]
fn a_directory_replaced_by_a_link_after_listing_does_not_send_outside_content() {
    for replace_root in [false, true] {
        let (_scratch, dir) = scratch(if replace_root {
            "swap-root"
        } else {
            "swap-child"
        });
        let root = dir.join("Photos");
        write(&root.join("sub/secret.txt"), b"inside!");
        write(&dir.join("outside/sub/secret.txt"), b"outside");
        write(&dir.join("outside/secret.txt"), b"outside");
        let listing = Listing::list(&root, &limits()).unwrap();
        let replaced = if replace_root {
            root.clone()
        } else {
            root.join("sub")
        };
        std::fs::rename(&replaced, dir.join("original")).unwrap();
        if !link_folder(&dir.join("outside"), &replaced) {
            no_link_here("a folder replaced after listing");
            continue;
        }
        let opened = listing.open_entry(&listing.entries[0]);
        if replace_root {
            let (mut file, _) = opened.unwrap().unwrap();
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, b"inside!");
        } else {
            // The folder that is a link now, not the file below it.
            assert_eq!(
                why(opened),
                ManifestError::Link {
                    at: replaced.clone()
                }
            );
        }
        drop(listing);
        remove_link(&replaced);
    }
}

#[test]
fn a_file_replaced_by_a_link_after_listing_is_refused_with_a_reason() {
    let (_scratch, dir) = scratch("swap-file");
    let root = dir.join("Photos");
    let path = root.join("secret.txt");
    let secret = dir.join("outside/secret.txt");
    write(&path, b"inside!");
    write(&secret, b"outside");
    let listing = Listing::list(&root, &limits()).unwrap();
    std::fs::remove_file(&path).unwrap();
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(&secret, &path).is_ok();
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&secret, &path).is_ok();
    if !linked {
        // File symbolic links need a privilege on Windows that junctions do not.
        return eprintln!("skipped: this computer cannot create file symbolic links");
    }
    // The entry itself is the link.
    assert_eq!(
        why(listing.open_entry(&listing.entries[0])),
        ManifestError::Link { at: path }
    );
}

#[cfg(windows)]
#[test]
fn a_junction_replacing_a_listed_file_is_refused_without_elevation() {
    let (_scratch, dir) = scratch("file-junction-listed");
    let root = dir.join("Photos");
    let path = root.join("secret.txt");
    write(&path, b"inside");
    write(&dir.join("outside/secret.txt"), b"outside");
    let listing = Listing::list(&root, &limits()).unwrap();
    std::fs::remove_file(&path).unwrap();
    if !link_folder(&dir.join("outside"), &path) {
        return no_link_here("a junction replacing a listed file");
    }
    assert_eq!(
        why(listing.open_entry(&listing.entries[0])),
        ManifestError::Link { at: path.clone() }
    );
    let again = Listing::list(&root, &limits()).unwrap();
    assert_eq!(again.files, 0);
    assert_eq!(again.skipped[0].why, Skip::Link);
    drop(listing);
    remove_link(&path);
}

/// Differs from the original, whose total could overflow (a panic in a
/// debug build): the listing's byte count stops at the largest number.
#[test]
fn a_listing_s_byte_count_never_wraps() {
    assert_eq!(add_size(1, 2), 3);
    assert_eq!(add_size(u64::MAX - 1, 2), u64::MAX);
}

/// Makes a pipe at `path`, a file that is neither a file nor a folder.
#[cfg(unix)]
fn fifo(path: &Path) {
    let made = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .unwrap();
    assert!(made.success(), "mkfifo");
}

/// A pipe in a kept-out folder holds nothing: skipped, as before.
#[cfg(unix)]
#[test]
fn a_special_file_in_a_kept_out_folder_is_skipped() {
    let (_scratch, dir) = scratch("kept-fifo");
    let kept = dir.join("Kept");
    write(&kept.join("host.key"), b"key");
    fifo(&kept.join("pipe"));
    write(&dir.join("Work/a.txt"), b"a");
    let kept_out = std::slice::from_ref(&kept);
    let (manifest, _) =
        Sources::list_keeping_out(1, &[dir.join("Work/a.txt")], kept_out, &limits()).unwrap();
    assert_eq!(manifest.entries()[0].path, "a.txt");
    assert_eq!(
        why(Sources::list_keeping_out(
            1,
            &[kept.join("pipe")],
            kept_out,
            &limits()
        )),
        ManifestError::KeptOut
    );
}

#[cfg(unix)]
#[test]
fn a_special_file_in_a_listed_folder_is_skipped_as_one() {
    let (_scratch, dir) = scratch("listing-fifo");
    let root = dir.join("Photos");
    write(&root.join("a.txt"), b"a");
    fifo(&root.join("pipe"));
    let listing = Listing::list(&root, &limits()).unwrap();
    assert_eq!(names_of(&listing), ["Photos/a.txt"]);
    assert_eq!(
        listing.skipped,
        [Skipped {
            path: "Photos/pipe".into(),
            why: Skip::NotFileOrFolder
        }]
    );
}

#[cfg(unix)]
#[test]
fn a_picked_special_file_is_refused_as_one() {
    let (_scratch, dir) = scratch("picked-fifo");
    let pipe = dir.join("pipe");
    fifo(&pipe);
    assert_eq!(
        why(list(&[&pipe])),
        ManifestError::NotFileOrFolder { at: pipe.clone() }
    );
    // Inside a picked folder too.
    write(&dir.join("Work/a.txt"), b"a");
    fifo(&dir.join("Work/pipe"));
    assert_eq!(
        why(list(&[dir.join("Work")])),
        ManifestError::NotFileOrFolder {
            at: dir.join("Work/pipe")
        }
    );
}
