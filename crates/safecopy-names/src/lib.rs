//! Safe file names.
//!
//! Checks a name that came from the other side before it touches the disk:
//! refuses unsafe and reserved names and paths that leave their folder,
//! makes numbered names (`report (2).pdf`) and finds names that collide
//! within one folder. No I/O: every function works on text alone.
//!
//! The rules are Windows' rules on every system, so a name saved anywhere
//! can be copied to Windows later: no `<>:"|?*`, no control characters, no
//! device names (`CON`, `nul.txt`, `COM¹`), no dots or spaces at the end,
//! and at most [`NameRules::max_units`] UTF-16 units. Characters that change
//! how a name reads without being seen ([`hidden`]) are left out.

mod path;
mod siblings;

pub use path::{EntryPath, PathError, PathRules, check_path};
pub use siblings::{NameError, Siblings};

use unicode_normalization::UnicodeNormalization;

/// Limits on a single name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameRules {
    /// The longest name kept, in UTF-16 units as Windows counts them. A
    /// name may have 255 there; the default 200 leaves room for " (99)"
    /// and ".part" after it.
    pub max_units: usize,
    /// The highest number [`Siblings::take`] tries before it gives up on a
    /// name ("name (10001)").
    pub max_number: usize,
}

impl Default for NameRules {
    fn default() -> Self {
        Self {
            max_units: 200,
            max_number: 10_001,
        }
    }
}

/// A file name that is safe to create in a folder: the last part of `name`
/// alone, nothing Windows refuses in a name, no [`hidden`] characters, no
/// device name (one gets a `_` in front), within [`NameRules::max_units`].
/// `None` when nothing usable is left.
pub fn safe_name(name: &str, rules: &NameRules) -> Option<String> {
    // Cut first: a cut can leave a device name ("NUL", then spaces). The
    // mark that makes it safe is cut to fit again.
    let fitted = fitted(&cleaned_name(name)?, rules.max_units)?;
    if reserved(&fitted) {
        self::fitted(&format!("_{fitted}"), rules.max_units)
    } else {
        Some(fitted)
    }
}

/// `name` as one entry of a list may carry it to another system: the rules
/// of [`safe_name`] in composed Unicode form (NFC), with `/` and `\` made
/// `_` instead of starting a path. The same name again when it already is
/// one. `None` when nothing usable is left.
pub fn entry_name(name: &str, rules: &NameRules) -> Option<String> {
    let mut name: String = name
        .nfc()
        .map(|c| if matches!(c, '/' | '\\') { '_' } else { c })
        .collect();
    // Cleaning can uncompose a character and composing can lengthen a name: settle both.
    for _ in 0..4 {
        let safe: String = safe_name(&name, rules)?.nfc().collect();
        if safe == name {
            return Some(safe);
        }
        name = safe;
    }
    None
}

/// Whether Windows keeps a name for a device ("CON", "nul.txt", "COM1",
/// "lpt¹ .log"), in any case, before any extension.
pub fn reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("");
    let stem = stem.trim_end_matches(' ').to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let mut chars = stem.chars();
    let port: String = chars.by_ref().take(3).collect();
    let number: Vec<char> = chars.collect();
    matches!(port.as_str(), "COM" | "LPT") && matches!(number[..], ['0'..='9' | '¹' | '²' | '³'])
}

/// Characters a name from the other side may not carry, because they change
/// how it reads:
///
/// - control characters, and the line and paragraph separators (U+2028,
///   U+2029);
/// - the bidirectional marks (U+200E, U+200F, U+061C), embeddings and
///   overrides (U+202A to U+202E) and isolates (U+2066 to U+2069): an
///   override shows "photo", U+202E, "gpj.exe" as "photoexe.jpg";
/// - the invisible operators (U+2061 to U+2064);
/// - the tag characters (U+E0001, U+E0020 to U+E007F), invisible copies of
///   ASCII that can carry hidden text.
pub fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{2028}'
                | '\u{2029}'
                | '\u{200E}'
                | '\u{200F}'
                | '\u{061C}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2061}'..='\u{2064}'
                | '\u{2066}'..='\u{2069}'
                | '\u{E0001}'
                | '\u{E0020}'..='\u{E007F}'
        )
}

/// `text` without its [`hidden`] characters.
pub fn without_hidden(text: &str) -> String {
    text.chars().filter(|c| !hidden(*c)).collect()
}

/// `name` cut to `max_units` UTF-16 units, without the dots and spaces
/// Windows drops from a name's end; `None` when nothing is left.
fn fitted(name: &str, max_units: usize) -> Option<String> {
    let mut units = 0;
    let cut: String = name
        .chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= max_units
        })
        .collect();
    let cut = cut.trim_end_matches(['.', ' ']);
    (!cut.is_empty()).then(|| cut.to_string())
}

/// The last part of `name`, without what Windows refuses in a name and
/// without [`hidden`] characters; control characters become `_`.
fn cleaned_name(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .filter(|&c| c.is_control() || !hidden(c))
        .map(|c| {
            if c.is_control() || "<>:\"|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
    if cleaned.is_empty() || cleaned.chars().all(|c| c == '.') {
        return None;
    }
    Some(cleaned)
}

/// "report" and ".pdf" for "report.pdf"; a leading dot is part of the stem.
pub fn stem_and_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// The names a file called `name` may be saved under, preferred first:
/// "name.ext", then "name (2).ext", "name (3).ext" and so on, without end.
/// Unlike [`numbered`] nothing is cut: `name` is a [`safe_name`] already,
/// whose limit leaves room for the number.
pub fn candidates(name: &str) -> impl Iterator<Item = String> + use<> {
    let (stem, ext) = stem_and_ext(name);
    let (stem, ext) = (stem.to_string(), ext.to_string());
    std::iter::once(name.to_string()).chain((2..).map(move |n| format!("{stem} ({n}){ext}")))
}

/// `name` numbered: "name (2).ext" for 2, its stem cut so it stays within
/// [`NameRules::max_units`]; `None` when no [`entry_name`] is left.
pub fn numbered(name: &str, n: usize, rules: &NameRules) -> Option<String> {
    let units = |text: &str| text.encode_utf16().count();
    let suffix = format!(" ({n})");
    let (mut stem, mut ext) = stem_and_ext(name);
    if units(&suffix) + units(ext) >= rules.max_units {
        (stem, ext) = (name, "");
    }
    let room = rules.max_units.checked_sub(units(&suffix) + units(ext))?;
    let mut used = 0;
    let cut: String = stem
        .chars()
        .take_while(|c| {
            used += c.len_utf16();
            used <= room
        })
        .collect();
    let cut = cut.trim_end_matches(['.', ' ']);
    if cut.is_empty() {
        return None;
    }
    let candidate = format!("{cut}{suffix}{ext}");
    (entry_name(&candidate, rules).as_deref() == Some(candidate.as_str())).then_some(candidate)
}

/// The form two names that are the same to a file system share: composed,
/// in one case.
pub fn fold(name: &str) -> String {
    name.nfc()
        .flat_map(char::to_uppercase)
        .flat_map(char::to_lowercase)
        .nfc()
        .collect()
}

/// Whether a file sent as `sent` may have been saved as `saved`: under its
/// [`safe_name`], or a numbered one when that was taken ([`candidates`]).
/// A `saved` name that kept [`hidden`] characters (as an older receiver
/// might have) still matches.
pub fn same_name(sent: &str, saved: &str, rules: &NameRules) -> bool {
    let Some(base) = safe_name(sent, rules) else {
        return false;
    };
    // Hidden characters kept the dots and spaces before them from the end.
    let saved = without_hidden(saved);
    let saved = saved.trim_end_matches(['.', ' ']);
    if saved == base {
        return true;
    }
    let (stem, ext) = stem_and_ext(&base);
    saved
        .strip_prefix(stem)
        .and_then(|rest| rest.strip_prefix(" ("))
        .and_then(|rest| rest.strip_suffix(ext))
        .and_then(|rest| rest.strip_suffix(')'))
        .and_then(|n| n.parse::<u32>().ok())
        .is_some_and(|n| n >= 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_NAME_UNITS: usize = 200;

    fn safe(name: &str) -> Option<String> {
        safe_name(name, &NameRules::default())
    }

    #[test]
    fn the_default_rules_are_two_hundred_units_and_ten_thousand_and_one_numbers() {
        let rules = NameRules::default();
        assert_eq!(rules.max_units, MAX_NAME_UNITS);
        assert_eq!(rules.max_number, 10_001);
    }

    #[test]
    fn names_are_made_safe() {
        assert_eq!(safe("report.pdf").as_deref(), Some("report.pdf"));
        assert_eq!(safe(r"..\..\Windows\evil.dll").as_deref(), Some("evil.dll"));
        assert_eq!(safe("../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(safe("a<b>:c?.txt").as_deref(), Some("a_b__c_.txt"));
        assert_eq!(safe("con.txt").as_deref(), Some("_con.txt"));
        assert_eq!(safe("COM1").as_deref(), Some("_COM1"));
        assert_eq!(safe("compass.txt").as_deref(), Some("compass.txt"));
        assert_eq!(safe("trailing. . ").as_deref(), Some("trailing"));
        assert_eq!(safe(".."), None);
        assert_eq!(safe("dir/"), None);
        assert_eq!(safe(&"x".repeat(300)).map(|n| n.len()), Some(200));
    }

    /// An override would show "photo\u{202E}gpj.exe" as "photoexe.jpg".
    #[test]
    fn names_on_disk_lose_what_would_disguise_them() {
        for (sent, saved) in [
            ("photo\u{202E}gpj.exe", Some("photogpj.exe")),
            ("a\u{202A}b\u{202C}c\u{2066}d\u{2069}.txt", Some("abcd.txt")),
            ("\u{200E}\u{200F}\u{061C}mark.txt", Some("mark.txt")),
            ("tag\u{E0001}\u{E0041}\u{E007F}.txt", Some("tag.txt")),
            ("line\u{2028}break.txt", Some("linebreak.txt")),
            // Control characters still become "_"; joiners stay.
            ("a\u{1}b.txt", Some("a_b.txt")),
            (
                "\u{0646}\u{200C}\u{0647}.txt",
                Some("\u{0646}\u{200C}\u{0647}.txt"),
            ),
            // What is left is checked as before: a device, a name with none.
            ("CON\u{202E}.txt", Some("_CON.txt")),
            ("\u{202E}\u{2066}", None),
            ("name.\u{202E}", Some("name")),
        ] {
            assert_eq!(safe(sent).as_deref(), saved, "{sent:?}");
        }
    }

    #[test]
    fn hidden_characters_are_the_ones_that_change_how_text_reads() {
        let text = "a\u{202E}b\u{2066}c\u{E0041}d\u{2028}e\u{7}";
        assert_eq!(without_hidden(text), "abcde");
        for c in ['a', ' ', '\u{200C}', '\u{200D}', 'é', '😀'] {
            assert!(!hidden(c), "{c:?}");
        }
    }

    /// Windows keeps COM and LPT with a superscript digit for devices too,
    /// in any case, with or without an extension, dots or spaces after.
    #[test]
    fn device_names_with_superscript_digits_are_reserved() {
        for name in [
            "COM¹",
            "com²",
            "Com³",
            "LPT¹",
            "lpt²",
            "LpT³",
            "COM¹.txt",
            "lpt³.tar.gz",
            "COM² .txt",
            "lpt¹. ",
            "COM³ ",
            "COM0",
            "LPT9.log",
            "con .txt",
            "Nul.",
            "PRN",
            "aux.c",
        ] {
            assert!(reserved(name), "{name:?}");
            let safe = safe(name).unwrap();
            assert!(safe.starts_with('_'), "{name:?} is saved as {safe:?}");
        }
        for name in [
            "COM10",
            "COM",
            "LPT¹0",
            "COMa",
            "COM⁴",
            "console",
            "com¹x.txt",
        ] {
            assert!(!reserved(name), "{name:?}");
        }
    }

    #[test]
    fn a_name_cut_down_to_a_device_name_is_made_safe() {
        // 201 units: the cut drops the "x", the trim the spaces: "NUL".
        let name = format!("NUL{}x", " ".repeat(197));
        assert_eq!(safe(&name).as_deref(), Some("_NUL"));
        assert_eq!(safe(&format!("{name}.txt")).as_deref(), Some("_NUL"));
    }

    #[test]
    fn a_marked_device_name_stays_within_the_limit() {
        let full = format!("NUL.{}", "b".repeat(196)); // 200 units already
        assert_eq!(safe(&full).unwrap(), format!("_NUL.{}", "b".repeat(195)));
        for name in [
            full.clone(),
            format!("CON.{}", "a".repeat(196)),
            format!("com¹.{}", "😀".repeat(120)),
            format!("NUL{}x", " ".repeat(197)),
            "😀".repeat(150),
            "x".repeat(300),
        ] {
            let safe = safe(&name).unwrap();
            assert!(safe.encode_utf16().count() <= MAX_NAME_UNITS, "{safe}");
        }
    }

    /// A name is cut by UTF-16 units, as Windows counts them, so that a
    /// number and ".part" still fit within its 255.
    #[test]
    fn a_long_name_is_cut_to_what_windows_holds_with_room_to_spare() {
        let wide = "😀".repeat(150); // 2 units each: 300 units
        let cut = safe(&wide).unwrap();
        assert_eq!(cut.encode_utf16().count(), MAX_NAME_UNITS);
        assert_eq!(cut.chars().count(), MAX_NAME_UNITS / 2);
        let numbered = format!("{cut} (99).part");
        assert!(numbered.encode_utf16().count() <= 255, "{numbered}");
        // A cut that ends in a space or a dot drops it, as Windows would.
        let spaced = format!("{} b", "a".repeat(MAX_NAME_UNITS - 1));
        assert_eq!(safe(&spaced).unwrap(), "a".repeat(MAX_NAME_UNITS - 1));
        assert_eq!(safe(&"x".repeat(300)).unwrap().len(), MAX_NAME_UNITS);
    }

    #[test]
    fn the_name_limit_is_a_setting() {
        let rules = NameRules {
            max_units: 10,
            ..NameRules::default()
        };
        assert_eq!(safe_name(&"x".repeat(20), &rules).unwrap(), "x".repeat(10));
        assert_eq!(safe_name("NUL.txt", &rules).as_deref(), Some("_NUL.txt"));
        assert_eq!(
            numbered("abcdefghij", 2, &rules).as_deref(),
            Some("abcdef (2)")
        );
    }

    /// What a file's name becomes when it holds a path: its last part.
    #[test]
    fn safe_name_keeps_only_the_last_part_of_a_hostile_path() {
        assert_eq!(safe("../x").as_deref(), Some("x"));
        assert_eq!(safe("a/../../x").as_deref(), Some("x"));
        assert_eq!(safe("C:/x").as_deref(), Some("x"));
        assert_eq!(safe("a:stream").as_deref(), Some("a_stream"));
        assert_eq!(safe("CON/x").as_deref(), Some("x"));
        assert_eq!(safe("x/CON").as_deref(), Some("_CON"));
        assert_eq!(safe(&("a/".repeat(500) + "x")).as_deref(), Some("x"));
        assert_eq!(safe("Photos/empty/"), None, "an empty folder is refused");
        assert_eq!(safe("Photos/"), None);
    }

    #[test]
    fn source_names_are_made_safe_without_changing_what_is_safe() {
        let rules = NameRules::default();
        for (name, safe) in [
            ("report.pdf", "report.pdf"),
            ("CON", "_CON"),
            ("nul.txt", "_nul.txt"),
            ("notes.", "notes"),
            ("a:b", "a_b"),
            ("why?", "why_"),
            ("a\\b", "a_b"),
            ("a/b", "a_b"),
            ("cafe\u{301}", "café"),
            ("photo\u{202e}gpj.exe", "photogpj.exe"),
        ] {
            assert_eq!(entry_name(name, &rules).as_deref(), Some(safe), "{name:?}");
            assert_eq!(entry_name(safe, &rules).as_deref(), Some(safe), "{safe:?}");
        }
        for name in ["", "..", ".", "  ", "\u{202e}"] {
            assert_eq!(entry_name(name, &rules), None, "{name:?}");
        }
    }

    #[test]
    fn a_taken_name_gets_a_number() {
        let first: Vec<String> = candidates("a.txt").take(3).collect();
        assert_eq!(first, ["a.txt", "a (2).txt", "a (3).txt"]);
        let hidden: Vec<String> = candidates(".hidden").take(2).collect();
        assert_eq!(hidden, [".hidden", ".hidden (2)"]);
        let folder: Vec<String> = candidates("v1.2").take(2).collect();
        assert_eq!(folder, ["v1.2", "v1 (2).2"]);
        assert_eq!(stem_and_ext("archive.tar.gz"), ("archive.tar", ".gz"));
        assert_eq!(stem_and_ext(".hidden"), (".hidden", ""));
        assert_eq!(stem_and_ext("plain"), ("plain", ""));
    }

    #[test]
    fn names_cut_to_the_same_length_are_numbered_within_it() {
        let rules = NameRules::default();
        let ext = "y".repeat(197);
        assert_eq!(
            numbered(&format!("a.{ext}"), 2, &rules),
            Some(format!("a.{} (2)", &ext[..194]))
        );
        let long = "x".repeat(MAX_NAME_UNITS);
        let two = numbered(&long, 2, &rules).unwrap();
        assert_eq!(two, format!("{} (2)", "x".repeat(MAX_NAME_UNITS - 4)));
        assert_eq!(two.encode_utf16().count(), MAX_NAME_UNITS);
        // A wide character is never split to make room.
        let wide = numbered(&"😀".repeat(100), 2, &rules).unwrap();
        assert_eq!(wide, format!("{} (2)", "😀".repeat(98)));
        // Nothing left of the stem, or a number that is unsafe, is none.
        assert_eq!(numbered("...", 2, &rules), None);
        let tight = NameRules {
            max_units: 3,
            ..NameRules::default()
        };
        assert_eq!(numbered("abc", 2, &tight), None);
    }

    #[test]
    fn names_that_differ_only_in_case_or_accents_fold_alike() {
        assert_eq!(fold("Report.TXT"), fold("report.txt"));
        assert_eq!(fold("cafe\u{301}"), fold("CAFÉ"));
        assert_eq!(fold("Straße"), fold("STRASSE"));
        assert_ne!(fold("a.txt"), fold("b.txt"));
    }

    #[test]
    fn a_saved_name_is_told_apart_from_another_files() {
        let saved_as = |sent: &str, saved: &str| same_name(sent, saved, &NameRules::default());
        assert!(saved_as("notes.txt", "notes.txt"));
        assert!(saved_as("notes.txt", "notes (2).txt"));
        assert!(saved_as("notes.txt", "notes (17).txt"));
        assert!(saved_as("a<b.txt", "a_b (2).txt"), "made safe first");
        assert!(saved_as(".hidden", ".hidden (2)"));
        assert!(
            !saved_as("notes.txt", "notes (1).txt"),
            "numbers start at 2"
        );
        assert!(!saved_as("notes.txt", "notes (x).txt"));
        assert!(!saved_as("notes.txt", "notes (2).md"));
        assert!(!saved_as("notes.txt", "other (2).txt"));
        assert!(!saved_as("big.iso", "notes (2).txt"));
        assert!(!saved_as("..", ".. (2)"), "no usable name");
        // Saved without the override here; an older receiver kept it.
        let disguised = "photo\u{202E}gpj.exe";
        assert!(saved_as(disguised, "photogpj.exe"));
        assert!(saved_as(disguised, "photogpj (2).exe"));
        assert!(saved_as(disguised, disguised));
        assert!(saved_as(disguised, "photo\u{202E}gpj (2).exe"));
        assert!(!saved_as(disguised, "photoexe.jpg"));
        assert!(saved_as("name.\u{202E}", "name.\u{202E}"));
        assert!(saved_as("name.\u{202E}", "name (2).\u{202E}"));
    }
}
