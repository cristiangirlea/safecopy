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
//!
//! A name is also at most [`NameRules::max_bytes`] bytes of UTF-8, which is
//! what Linux, Android and most Unix file systems count: the crate cannot
//! know which system a name is for, so both limits hold everywhere and a
//! safe name fits either kind.

mod path;
mod siblings;

pub use path::{EntryPath, PathError, PathRules, check_path};
pub use siblings::{NameError, Siblings};

use unicode_normalization::UnicodeNormalization;

/// Limits on a single name.
///
/// Both lengths hold: a name is cut to whichever it reaches first. Windows
/// counts a name in UTF-16 units and most other systems in bytes of UTF-8,
/// 255 of either, and one unit can be three bytes: 90 Chinese characters
/// are 90 units and 270 bytes. A caller that only ever writes to one kind
/// of system lifts the other limit with `usize::MAX`.
///
/// The defaults leave 55 of the 255 free, in units and in bytes alike,
/// for what is put after a safe name: " (10001)" and ".part" are 13 of
/// either. A caller that sets its own limits leaves its own room.
///
/// Built from the default, since more limits may come:
/// `NameRules::default().with_max_bytes(usize::MAX)`, or by setting a
/// field of one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NameRules {
    /// The longest name kept, in UTF-16 units as Windows counts them. A
    /// name may have 255 there; the default is 200.
    pub max_units: usize,
    /// The longest name kept, in bytes of UTF-8 as Linux, Android and most
    /// Unix file systems count them. A name may have 255 there; the
    /// default is 200, the same room as [`max_units`](Self::max_units)
    /// leaves.
    pub max_bytes: usize,
    /// The highest number [`Siblings::take`] tries before it gives up on a
    /// name ("name (10001)").
    pub max_number: usize,
}

impl Default for NameRules {
    fn default() -> Self {
        Self {
            max_units: 200,
            max_bytes: 200,
            max_number: 10_001,
        }
    }
}

impl NameRules {
    /// These rules with [`max_units`](Self::max_units) set.
    pub const fn with_max_units(mut self, max_units: usize) -> Self {
        self.max_units = max_units;
        self
    }

    /// These rules with [`max_bytes`](Self::max_bytes) set.
    pub const fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    /// These rules with [`max_number`](Self::max_number) set.
    pub const fn with_max_number(mut self, max_number: usize) -> Self {
        self.max_number = max_number;
        self
    }

    /// What both limits leave for the rest of a name that ends in `tail`;
    /// `None` when `tail` alone is over one of them.
    fn room(&self, tail: &str) -> Option<Length> {
        Some(Length {
            units: self.max_units.checked_sub(tail.encode_utf16().count())?,
            bytes: self.max_bytes.checked_sub(tail.len())?,
        })
    }
}

/// A length as each kind of system counts it.
#[derive(Clone, Copy)]
struct Length {
    units: usize,
    bytes: usize,
}

/// A file name that is safe to create in a folder: the last part of `name`
/// alone, nothing Windows refuses in a name, no [`hidden`] characters, no
/// device name (one gets a `_` in front), within [`NameRules::max_units`]
/// and [`NameRules::max_bytes`]. A longer name is cut at its end, its
/// extension with it, between two characters. `None` when nothing usable
/// is left.
pub fn safe_name(name: &str, rules: &NameRules) -> Option<String> {
    // Cut first: a cut can leave a device name ("NUL", then spaces). The
    // mark that makes it safe is cut to fit again.
    let fitted = fitted(&cleaned_name(name)?, "", rules)?;
    if reserved(&fitted) {
        self::fitted(&format!("_{fitted}"), "", rules)
    } else {
        Some(fitted)
    }
}

/// `name` as one entry of a list may carry it to another system: the rules
/// of [`safe_name`] in composed Unicode form (NFC), with `/` and `\` made
/// `_` instead of starting a path. The same name again when it already is
/// one. The limits hold for the name as it is returned. `None` when
/// nothing usable is left.
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

/// `stem` and `tail` as one name within both limits of `rules`: `stem` cut
/// at its end to make room, whole characters only, then without the dots
/// and spaces Windows drops from a name's end. `None` when no stem is left.
fn fitted(stem: &str, tail: &str, rules: &NameRules) -> Option<String> {
    let cut = head(stem, rules.room(tail)?).trim_end_matches(['.', ' ']);
    (!cut.is_empty()).then(|| format!("{cut}{tail}"))
}

/// The start of `text` that fits `room`: whole characters, so never half a
/// surrogate pair nor part of a character's bytes. A mark that combines
/// with the character before it is a character of its own here.
fn head(text: &str, room: Length) -> &str {
    let (mut units, mut end) = (0, 0);
    for c in text.chars() {
        units += c.len_utf16();
        if units > room.units || end + c.len_utf8() > room.bytes {
            break;
        }
        end += c.len_utf8();
    }
    &text[..end]
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
/// whose limits leave room for the number ([`NameRules`]).
pub fn candidates(name: &str) -> impl Iterator<Item = String> + use<> {
    let (stem, ext) = stem_and_ext(name);
    let (stem, ext) = (stem.to_string(), ext.to_string());
    std::iter::once(name.to_string()).chain((2..).map(move |n| format!("{stem} ({n}){ext}")))
}

/// `name` numbered: "name (2).ext" for 2, its stem cut so it stays within
/// [`NameRules::max_units`] and [`NameRules::max_bytes`] with its extension
/// kept; where the extension leaves no room for a stem it is cut as part
/// of the name ("name.e (2)"). `None` when no [`entry_name`] is left.
pub fn numbered(name: &str, n: usize, rules: &NameRules) -> Option<String> {
    let suffix = format!(" ({n})");
    let (stem, ext) = stem_and_ext(name);
    let candidate =
        fitted(stem, &format!("{suffix}{ext}"), rules).or_else(|| fitted(name, &suffix, rules))?;
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
    const MAX_NAME_BYTES: usize = 200;

    fn safe(name: &str) -> Option<String> {
        safe_name(name, &NameRules::default())
    }

    fn units(text: &str) -> usize {
        text.encode_utf16().count()
    }

    #[test]
    fn the_default_rules_are_two_hundred_units_and_ten_thousand_and_one_numbers() {
        let rules = NameRules::default();
        assert_eq!(rules.max_units, MAX_NAME_UNITS);
        assert_eq!(rules.max_number, 10_001);
    }

    /// 255 less the 55 the unit limit leaves of Windows' 255.
    #[test]
    fn the_default_rules_are_two_hundred_bytes_too() {
        assert_eq!(NameRules::default().max_bytes, MAX_NAME_BYTES);
        assert_eq!(255 - NameRules::default().max_units, 55);
    }

    /// The names a system that counts bytes refused: within the units, over
    /// the bytes. `(character, how many, extension, how many are kept)`.
    const WIDE_NAMES: [(&str, usize, &str, usize); 3] = [
        ("😀", 100, ".png", 50),
        ("文", 90, ".pdf", 66),
        ("я", 130, ".txt", 100),
    ];

    #[test]
    fn a_name_within_the_units_but_over_the_bytes_is_cut_to_fit_both() {
        let rules = NameRules::default();
        for (c, many, ext, kept) in WIDE_NAMES {
            let name = format!("{}{ext}", c.repeat(many));
            assert!(units(&c.repeat(many)) <= MAX_NAME_UNITS, "{name}");
            assert!(name.len() > 255, "{name}");
            // Cut at its end between two characters, as a name over the
            // units is: as many whole characters as the bytes hold.
            let cut = safe(&name).unwrap();
            assert_eq!(cut, c.repeat(kept), "{name}");
            assert!(cut.len() <= MAX_NAME_BYTES && units(&cut) <= MAX_NAME_UNITS);
            assert!(cut.len() + c.len() > MAX_NAME_BYTES, "{name}");
            assert_eq!(entry_name(&name, &rules), Some(cut.clone()));
            // A numbered name keeps its extension, within both limits.
            let two = numbered(&name, 2, &rules).unwrap();
            let stem = two.strip_suffix(&format!(" (2){ext}")).expect(&two);
            assert!(!stem.is_empty() && stem.chars().all(|x| c.starts_with(x)));
            assert!(two.len() <= MAX_NAME_BYTES && units(&two) <= MAX_NAME_UNITS);
            assert!(two.len() + c.len() > MAX_NAME_BYTES, "{two}");
            // Saved under the cut name, not under the one too long to save.
            assert!(same_name(&name, &cut, &rules));
            assert!(!same_name(&name, &name, &rules));
        }
    }

    #[test]
    fn a_name_within_both_limits_is_not_touched() {
        let rules = NameRules::default();
        for name in [
            format!("{}.docx", "文".repeat(65)), // 200 bytes
            format!("{}.png", "😀".repeat(49)),  // 200 bytes
            format!("{}.txt", "я".repeat(98)),   // 200 bytes
            "x".repeat(MAX_NAME_BYTES),
            "é".repeat(100),
            "résumé (2).pdf".to_string(),
        ] {
            assert!(name.len() <= MAX_NAME_BYTES, "{name}");
            assert_eq!(safe(&name).as_deref(), Some(name.as_str()));
            assert_eq!(entry_name(&name, &rules).as_deref(), Some(name.as_str()));
        }
    }

    /// The cut falls between two characters whichever limit makes it, and a
    /// mark is a character: where the units would part a letter from its
    /// mark the bytes do, and where they would not the bytes do not.
    #[test]
    fn a_combining_sequence_is_cut_by_bytes_where_units_would_cut_it() {
        let by_units = |n| {
            NameRules::default()
                .with_max_bytes(usize::MAX)
                .with_max_units(n)
        };
        let by_bytes = |n| {
            NameRules::default()
                .with_max_units(usize::MAX)
                .with_max_bytes(n)
        };
        // "x" and a mark it has no composed form with: 2 units, 3 bytes.
        let name = "x\u{301}".repeat(10);
        for pairs in 1..10 {
            let whole = "x\u{301}".repeat(pairs);
            assert_eq!(safe_name(&name, &by_units(2 * pairs)), Some(whole.clone()));
            assert_eq!(safe_name(&name, &by_bytes(3 * pairs)), Some(whole.clone()));
            // One unit more takes the next letter without its mark; so does
            // one byte more, and two: never a part of the mark's two bytes.
            let parted = format!("{whole}x");
            assert_eq!(
                safe_name(&name, &by_units(2 * pairs + 1)),
                Some(parted.clone())
            );
            assert_eq!(
                safe_name(&name, &by_bytes(3 * pairs + 1)),
                Some(parted.clone())
            );
            assert_eq!(safe_name(&name, &by_bytes(3 * pairs + 2)), Some(parted));
        }
        // An entry's name is composed first: a letter and its mark are one
        // character there, and no cut comes between them.
        let rules = NameRules::default();
        let accents = entry_name(&"e\u{301}".repeat(150), &rules).unwrap();
        assert_eq!(accents, "é".repeat(100));
        let tight = by_bytes(5);
        assert_eq!(
            entry_name("e\u{301}e\u{301}e\u{301}", &tight).as_deref(),
            Some("éé")
        );
        // Composing makes these longer, two characters of three bytes for
        // one: the limit is on the name as it comes back.
        let longer = entry_name(&"\u{958}".repeat(60), &rules).unwrap();
        assert_eq!(longer, "\u{915}\u{93C}".repeat(33));
        assert_eq!(entry_name(&longer, &rules), Some(longer.clone()));
    }

    /// With the byte limit lifted a name is cut as it was before there was
    /// one; with the unit limit lifted, by bytes alone.
    #[test]
    fn either_limit_can_be_lifted() {
        let units_only = NameRules::default().with_max_bytes(usize::MAX);
        for (c, many, ext, _) in WIDE_NAMES {
            let name = format!("{}{ext}", c.repeat(many));
            let cut = safe_name(&name, &units_only).unwrap();
            if units(&name) <= MAX_NAME_UNITS {
                assert_eq!(cut, name);
            } else {
                assert_eq!(cut, "😀".repeat(100));
            }
        }
        let wide = safe_name(&"😀".repeat(150), &units_only).unwrap();
        assert_eq!(units(&wide), MAX_NAME_UNITS);
        assert_eq!(wide.len(), 400);
        assert_eq!(
            numbered(&"😀".repeat(100), 2, &units_only),
            Some(format!("{} (2)", "😀".repeat(98)))
        );

        let bytes_only = NameRules::default().with_max_units(usize::MAX);
        assert_eq!(
            safe_name(&"😀".repeat(150), &bytes_only).unwrap().len(),
            200
        );
        assert_eq!(safe_name(&"x".repeat(300), &bytes_only).unwrap().len(), 200);
        let roomy = bytes_only.clone().with_max_bytes(1000);
        let long = "x".repeat(600);
        assert_eq!(safe_name(&long, &roomy).as_deref(), Some(long.as_str()));
        // Neither limit: nothing is cut.
        let neither = units_only.with_max_units(usize::MAX);
        let huge = "文".repeat(5000);
        assert_eq!(safe_name(&huge, &neither).as_deref(), Some(huge.as_str()));
        assert!(numbered(&huge, 2, &neither).is_some());
    }

    #[test]
    fn the_byte_limit_is_a_setting() {
        let rules = NameRules::default().with_max_bytes(10);
        assert_eq!(rules.max_bytes, 10);
        assert_eq!((rules.max_units, rules.max_number), (200, 10_001));
        assert_eq!(safe_name(&"文".repeat(9), &rules).unwrap(), "文".repeat(3));
        assert_eq!(safe_name(&"x".repeat(20), &rules).unwrap(), "x".repeat(10));
        // No room for one character before " (2).txt": the extension goes.
        assert_eq!(
            numbered("文文文.txt", 2, &rules).as_deref(),
            Some("文文 (2)")
        );
        assert_eq!(
            numbered("文文文.txt", 2, &rules.clone().with_max_bytes(6)),
            None
        );
        // Every number is given, not only those whose digits leave no room.
        let tail = format!("文.{}", "x".repeat(194));
        let two = numbered(&tail, 2, &NameRules::default()).unwrap();
        assert_eq!(two, format!("文.{} (2)", "x".repeat(192)));
        let rules = rules.with_max_bytes(14);
        assert_eq!(
            numbered("文文文.txt", 2, &rules).as_deref(),
            Some("文文 (2).txt")
        );
        // An extension that leaves no room is cut with the name.
        assert_eq!(
            numbered("文.文文文", 2, &rules).as_deref(),
            Some("文.文文 (2)")
        );
        assert_eq!(
            numbered(".文文文文", 2, &rules).as_deref(),
            Some(".文文文 (2)")
        );
        assert_eq!(
            safe_name(".文文文文文", &rules).as_deref(),
            Some(".文文文文")
        );
        assert_eq!(safe_name("文", &rules.clone().with_max_bytes(2)), None);
        assert_eq!(safe_name("文", &rules.clone().with_max_bytes(0)), None);
        // A field of rules a caller holds can be set as well.
        let mut set = NameRules::default().with_max_number(3).with_max_units(2);
        set.max_bytes = 6;
        assert_eq!((set.max_units, set.max_bytes, set.max_number), (2, 6, 3));
    }

    /// The mark of a device name counts in bytes too, and so does the cut
    /// that leaves one.
    #[test]
    fn a_marked_device_name_stays_within_the_byte_limit() {
        let full = format!("NUL.{}b", "文".repeat(65)); // 200 bytes, 70 units
        assert_eq!(safe(&full).unwrap(), format!("_NUL.{}", "文".repeat(65)));
        // 200 units and 202 bytes: the cut drops "文", the trim the spaces.
        let name = format!("NUL{}文", " ".repeat(196));
        assert_eq!(safe(&name).as_deref(), Some("_NUL"));
        let tight = NameRules::default().with_max_bytes(5);
        assert_eq!(safe_name("CON.文", &tight).as_deref(), Some("_CON"));
        assert_eq!(safe_name("CON.txt", &tight).as_deref(), Some("_CON"));
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

    /// A name is cut by UTF-16 units, as Windows counts them, and by bytes,
    /// so that a number and ".part" still fit within 255 of either.
    #[test]
    fn a_long_name_is_cut_to_what_windows_holds_with_room_to_spare() {
        let wide = "😀".repeat(150); // 2 units and 4 bytes each
        let cut = safe(&wide).unwrap();
        assert_eq!(cut.len(), MAX_NAME_BYTES);
        assert_eq!(cut.chars().count(), MAX_NAME_BYTES / 4);
        let numbered = format!("{cut} (10001).part");
        assert!(numbered.encode_utf16().count() <= 255, "{numbered}");
        assert!(numbered.len() <= 255, "{numbered}");
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
        assert_eq!(wide, format!("{} (2)", "😀".repeat(49)));
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
