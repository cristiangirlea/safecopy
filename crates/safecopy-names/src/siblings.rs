//! Names given within one folder, so no two collide.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fmt;

use crate::{NameRules, entry_name, fold, numbered};

/// Why [`Siblings::take`] gave no name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NameError {
    /// The name is not valid Unicode, so it cannot travel without loss.
    NotUnicode,
    /// Nothing usable is left once the name is made safe.
    Unusable,
    /// Every number up to [`NameRules::max_number`] is taken.
    NoNumberLeft {
        /// The highest number tried.
        most: usize,
    },
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The caller words it; this is the kind for logs and tests.
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for NameError {}

/// The names given in one folder: each name is made safe
/// ([`entry_name`]) and numbered when another in the folder already has it
/// apart from case, Unicode form or a cut ([`fold`]).
#[derive(Debug, Clone, Default)]
pub struct Siblings {
    rules: NameRules,
    taken: HashSet<String>,
    /// The number to try next for a name already taken, so many names cut
    /// alike do not count up from 2 each time.
    next: HashMap<String, usize>,
}

impl Siblings {
    /// An empty folder whose names follow `rules`.
    pub fn new(rules: NameRules) -> Self {
        Self {
            rules,
            ..Self::default()
        }
    }

    /// The name for `source` in this folder.
    pub fn take(&mut self, source: &OsStr) -> Result<String, NameError> {
        // Names go without loss or not at all.
        let name = source.to_str().ok_or(NameError::NotUnicode)?;
        let safe = entry_name(name, &self.rules).ok_or(NameError::Unusable)?;
        let key = fold(&safe);
        if self.taken.insert(key.clone()) {
            return Ok(safe);
        }
        let first = self.next.get(&key).copied().unwrap_or(2);
        for n in first..=self.rules.max_number {
            if let Some(candidate) = numbered(&safe, n, &self.rules)
                && self.taken.insert(fold(&candidate))
            {
                self.next.insert(key, n + 1);
                return Ok(candidate);
            }
        }
        Err(NameError::NoNumberLeft {
            most: self.rules.max_number,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_NAME_UNITS: usize = 200;

    #[test]
    fn siblings_that_collide_after_cleaning_are_numbered() {
        let mut siblings = Siblings::default();
        let mut take = |name: &str| siblings.take(OsStr::new(name)).unwrap();
        assert_eq!(take("Report.txt"), "Report.txt");
        assert_eq!(take("report.txt"), "report (2).txt");
        assert_eq!(take("REPORT.TXT"), "REPORT (3).TXT");
        assert_eq!(take("café"), "café");
        assert_eq!(take("cafe\u{301}"), "café (2)");
        assert_eq!(take("CAFÉ"), "CAFÉ (3)");
        assert_eq!(take("a?"), "a_");
        assert_eq!(take("a_"), "a_ (2)");
        assert_eq!(take("CON"), "_CON");
        assert_eq!(take("_con"), "_con (2)");
    }

    /// Files on disk that differ only in case or Unicode form (as Linux
    /// keeps them) get names of their own in one folder.
    #[test]
    fn files_on_disk_that_differ_only_in_case_or_accents_are_numbered() {
        let mut siblings = Siblings::default();
        let mut take = |name: &str| siblings.take(OsStr::new(name)).unwrap();
        assert_eq!(take("A.txt"), "A.txt");
        assert_eq!(take("a.txt"), "a (2).txt");
        assert_eq!(take("café"), "café");
        assert_eq!(take("cafe\u{301}"), "café (2)");
    }

    #[test]
    fn names_cut_to_the_same_length_are_numbered_within_it() {
        let long = "x".repeat(250);
        let mut siblings = Siblings::default();
        let one = siblings.take(OsStr::new(&format!("{long}1.txt"))).unwrap();
        let two = siblings.take(OsStr::new(&format!("{long}2.txt"))).unwrap();
        // Both are cut to the same stem and keep ".txt"; the number takes
        // four more of the stem.
        assert_eq!(one, format!("{}.txt", "x".repeat(MAX_NAME_UNITS - 4)));
        assert_eq!(two, format!("{} (2).txt", "x".repeat(MAX_NAME_UNITS - 8)));
        assert_eq!(two.encode_utf16().count(), MAX_NAME_UNITS);
        // Without an extension the whole name is the stem.
        let one = siblings.take(OsStr::new(&format!("{long}1"))).unwrap();
        let two = siblings.take(OsStr::new(&format!("{long}2"))).unwrap();
        assert_eq!(one, "x".repeat(MAX_NAME_UNITS));
        assert_eq!(two, format!("{} (2)", "x".repeat(MAX_NAME_UNITS - 4)));
    }

    /// A numbered name fits the bytes as well as the units, with its
    /// extension, up to the highest number the rules allow.
    #[test]
    fn a_numbered_name_at_the_byte_limit_still_fits() {
        let fits = |name: &str| name.len() <= 200 && name.encode_utf16().count() <= 200;
        let name = format!("{}.docx", "文".repeat(65)); // 200 bytes, 70 units
        let mut siblings = Siblings::default();
        assert_eq!(siblings.take(OsStr::new(&name)).unwrap(), name);
        let two = siblings.take(OsStr::new(&name)).unwrap();
        assert_eq!(two, format!("{} (2).docx", "文".repeat(63)));
        assert!(fits(&two), "{two}");
        // A longer name is cut to that one, extension kept, and numbered
        // as it is.
        let long = format!("{}.docx", "文".repeat(90));
        let mut siblings = Siblings::default();
        assert_eq!(siblings.take(OsStr::new(&long)).unwrap(), name);
        let two = siblings.take(OsStr::new(&long)).unwrap();
        assert_eq!(two, format!("{} (2).docx", "文".repeat(63)));
        assert!(fits(&two), "{two}");

        let rules = NameRules::default();
        let last = numbered(&name, rules.max_number, &rules).unwrap();
        assert_eq!(last, format!("{} (10001).docx", "文".repeat(62)));
        assert!(fits(&last), "{last}");
        // Every number on the way, in a folder that allows few.
        let mut siblings = Siblings::new(NameRules::default().with_max_number(12));
        for n in 1..=12 {
            let given = siblings.take(OsStr::new(&name)).unwrap();
            assert!(fits(&given) && given.ends_with(".docx"), "{n}: {given}");
        }
        assert_eq!(
            siblings.take(OsStr::new(&name)),
            Err(NameError::NoNumberLeft { most: 12 })
        );
    }

    #[test]
    fn a_name_that_is_not_unicode_is_refused_not_guessed() {
        #[cfg(unix)]
        let name = {
            use std::os::unix::ffi::OsStrExt;
            OsStr::from_bytes(b"caf\xe9.txt").to_os_string()
        };
        #[cfg(windows)]
        let name = {
            use std::os::windows::ffi::OsStringExt;
            std::ffi::OsString::from_wide(&[0x61, 0xd800, 0x62])
        };
        assert_eq!(Siblings::default().take(&name), Err(NameError::NotUnicode));
    }

    #[test]
    fn a_name_with_nothing_usable_is_refused() {
        let mut siblings = Siblings::default();
        for name in ["..", "", "\u{202E}", " . "] {
            assert_eq!(siblings.take(OsStr::new(name)), Err(NameError::Unusable));
        }
    }

    #[test]
    fn many_names_cut_alike_keep_counting_where_they_left_off() {
        let long = "x".repeat(250);
        let mut siblings = Siblings::default();
        let mut last = String::new();
        for i in 0..3000 {
            last = siblings.take(OsStr::new(&format!("{long}{i}"))).unwrap();
        }
        assert_eq!(last, format!("{} (3000)", "x".repeat(MAX_NAME_UNITS - 7)));
        assert_eq!(siblings.next[&"x".repeat(MAX_NAME_UNITS)], 3001);
    }

    #[test]
    fn a_name_past_the_last_number_is_refused() {
        let mut siblings = Siblings::new(NameRules {
            max_number: 3,
            ..NameRules::default()
        });
        for expected in ["a", "a (2)", "a (3)"] {
            assert_eq!(siblings.take(OsStr::new("a")).unwrap(), expected);
        }
        assert_eq!(
            siblings.take(OsStr::new("A")),
            Err(NameError::NoNumberLeft { most: 3 })
        );
    }
}
