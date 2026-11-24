//! Paths inside a folder the other side sent.

use std::fmt;

use crate::{NameRules, cleaned_name, fitted, reserved};

/// Limits on a path inside a folder.
///
/// Built from the default, since more limits may come:
/// `PathRules::default().with_max_depth(8)`, or by setting a field of one.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PathRules {
    /// The most parts a path may have, the folder's own name first.
    pub max_depth: usize,
    /// The longest path, in characters: far below what Windows allows
    /// (32,767), so the folder it is saved in fits before it.
    pub max_chars: usize,
    /// The rules for each part.
    pub names: NameRules,
}

impl Default for PathRules {
    fn default() -> Self {
        Self {
            max_depth: 32,
            max_chars: 1024,
            names: NameRules::default(),
        }
    }
}

impl PathRules {
    /// These rules with [`max_depth`](Self::max_depth) set.
    pub const fn with_max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }

    /// These rules with [`max_chars`](Self::max_chars) set.
    pub const fn with_max_chars(mut self, max_chars: usize) -> Self {
        self.max_chars = max_chars;
        self
    }

    /// These rules with [`names`](Self::names) set.
    pub const fn with_names(mut self, names: NameRules) -> Self {
        self.names = names;
        self
    }
}

/// A path in a folder, as it may be created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryPath {
    /// Each part made safe, the folder's name first.
    pub parts: Vec<String>,
    /// A folder (the path ended in `/`), not a file.
    pub folder: bool,
}

/// Why [`check_path`] refused a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathError {
    /// More than [`PathRules::max_chars`] characters.
    TooLong {
        /// The limit it passed.
        most: usize,
    },
    /// It starts at the top of a drive (`/`, `\`, `\\server`) or is empty.
    AtTop,
    /// A part names a drive (`C:`) or a stream (`name:stream`).
    DriveOrStream,
    /// More than [`PathRules::max_depth`] parts.
    TooDeep {
        /// The limit it passed.
        most: usize,
    },
    /// A file with no folder around it.
    NoFolder,
    /// Two separators with nothing between them.
    EmptyPart,
    /// A part is `.` or `..`.
    LeavesFolder,
    /// Nothing usable is left of a part once it is made safe.
    Unusable,
    /// A part is a name Windows keeps for a device (`CON`, `nul.txt`).
    Reserved,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The caller words it; this is the kind for logs and tests.
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for PathError {}

/// A path the other side sent, checked before anything is created: `/` or
/// `\` between parts, a trailing `/` for a folder. No part may climb out
/// (`..`), start at a drive or its top (`C:`, `\`), name a stream (`:`) or
/// a device (`CON`); the path is neither deeper nor longer than `rules`
/// allow. Each part is made safe as [`safe_name`](crate::safe_name) makes a
/// name, cut to both of its limits, except that a device name is refused
/// rather than renamed.
pub fn check_path(name: &str, rules: &PathRules) -> Result<EntryPath, PathError> {
    if name.chars().count() > rules.max_chars {
        return Err(PathError::TooLong {
            most: rules.max_chars,
        });
    }
    let (body, folder) = match name.strip_suffix('/') {
        Some(body) => (body, true),
        None => (name, false),
    };
    if body.is_empty() || body.starts_with(['/', '\\']) {
        return Err(PathError::AtTop);
    }
    if body.contains(':') {
        return Err(PathError::DriveOrStream);
    }
    let raw: Vec<&str> = body.split(['/', '\\']).collect();
    if raw.len() > rules.max_depth {
        return Err(PathError::TooDeep {
            most: rules.max_depth,
        });
    }
    if raw.len() < 2 && !folder {
        return Err(PathError::NoFolder);
    }
    let mut parts = Vec::with_capacity(raw.len());
    for part in raw {
        if part.is_empty() {
            return Err(PathError::EmptyPart);
        }
        if matches!(part.trim(), "." | "..") {
            return Err(PathError::LeavesFolder);
        }
        let fitted = cleaned_name(part)
            .and_then(|cleaned| fitted(&cleaned, "", &rules.names))
            .ok_or(PathError::Unusable)?;
        // After the cut: a cut can leave a device name ("NUL", then spaces).
        if reserved(&fitted) {
            return Err(PathError::Reserved);
        }
        parts.push(fitted);
    }
    Ok(EntryPath { parts, folder })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(path: &str) -> Result<EntryPath, PathError> {
        check_path(path, &PathRules::default())
    }

    #[test]
    fn the_default_rules_are_thirty_two_parts_and_1024_characters() {
        let rules = PathRules::default();
        assert_eq!((rules.max_depth, rules.max_chars), (32, 1024));
        assert_eq!(rules.names, NameRules::default());
    }

    #[test]
    fn safe_paths_are_kept_part_by_part() {
        let parts = |path: &str| check(path).map(|p| (p.parts, p.folder));
        let owned = |parts: &[&str]| parts.iter().map(|p| p.to_string()).collect::<Vec<_>>();
        assert_eq!(
            parts("Photos/2024/a.jpg"),
            Ok((owned(&["Photos", "2024", "a.jpg"]), false))
        );
        assert_eq!(
            parts("Photos\\2024\\a.jpg"),
            Ok((owned(&["Photos", "2024", "a.jpg"]), false))
        );
        assert_eq!(
            parts("Photos/empty/"),
            Ok((owned(&["Photos", "empty"]), true))
        );
        assert_eq!(parts("Photos/"), Ok((owned(&["Photos"]), true)));
        // What Windows refuses in a name is replaced, as in a file's name.
        assert_eq!(
            parts("a?b/c<d>.txt"),
            Ok((owned(&["a_b", "c_d_.txt"]), false))
        );
        assert_eq!(
            parts("a/trailing. "),
            Ok((owned(&["a", "trailing"]), false))
        );
        assert_eq!(
            parts("a/compass.txt"),
            Ok((owned(&["a", "compass.txt"]), false))
        );
        // Nor do folders keep what would disguise a name on disk.
        assert_eq!(
            parts("Invoices\u{202E}fdp/x\u{2066}.exe"),
            Ok((owned(&["Invoicesfdp", "x.exe"]), false))
        );
        assert_eq!(parts("a/\u{202E}/b.txt"), Err(PathError::Unusable));
    }

    /// Every [`PathError`], each from the input that gives it.
    #[test]
    fn hostile_paths_are_refused() {
        use PathError::*;
        for (path, why) in [
            ("../x", LeavesFolder),
            ("a/../../x", LeavesFolder),
            ("a/./x", LeavesFolder),
            ("a\\..\\x", LeavesFolder),
            ("a/ .. /x", LeavesFolder),
            ("C:/x", DriveOrStream),
            ("a/C:x", DriveOrStream),
            ("a/b:stream", DriveOrStream),
            ("a:stream", DriveOrStream),
            ("/a/x", AtTop),
            ("\\a\\x", AtTop),
            ("\\\\server\\share\\x", AtTop),
            ("", AtTop),
            ("/", AtTop),
            ("CON/x", Reserved),
            ("a/nul.txt", Reserved),
            ("a/COM1/x", Reserved),
            ("a/COM¹/x", Reserved),
            ("a/lpt³.txt", Reserved),
            ("a/Com² .log", Reserved),
            ("a//x", EmptyPart),
            ("a/x//", EmptyPart),
            ("a/...", Unusable),
            ("a/ /x", Unusable),
            ("x.txt", NoFolder),
        ] {
            assert_eq!(check(path), Err(why), "{path:?}");
        }
        let deep = "a/".repeat(32) + "x";
        assert_eq!(check(&deep), Err(TooDeep { most: 32 }));
        let deepest = "a/".repeat(31) + "x";
        assert!(check(&deepest).is_ok());
        let long = format!("a/{}", "x".repeat(1024));
        assert_eq!(check(&long), Err(TooLong { most: 1024 }));
        let longest = format!("a/{}", "x".repeat(1022));
        assert!(check(&longest).is_ok());
        // A part is cut by UTF-16 units, as Windows counts them, and by
        // bytes: 50 of these are 200 bytes.
        let wide = check(&format!("a/{}", "😀".repeat(150))).unwrap();
        assert_eq!(wide.parts[1], "😀".repeat(50));
        // A part the cut and the trim leave as "NUL" is refused too.
        let cut_to_device = format!("a/NUL{}x/b.txt", " ".repeat(197));
        assert_eq!(check(&cut_to_device), Err(Reserved));
    }

    /// Each part is cut to the byte limit as [`safe_name`](crate::safe_name)
    /// cuts a name, and a part the cut leaves a device name is refused.
    #[test]
    fn a_part_over_the_byte_limit_is_cut_as_one_over_the_unit_limit_is() {
        let names = NameRules::default();
        for name in [
            format!("{}.png", "😀".repeat(100)),
            format!("{}.pdf", "文".repeat(90)),
            format!("{}.txt", "я".repeat(130)),
        ] {
            let safe = crate::safe_name(&name, &names).unwrap();
            assert!(safe.len() <= 200 && safe != name, "{name}");
            // As a folder on the way, as the last folder and as a file.
            for path in [
                format!("{name}/b.txt"),
                format!("a/{name}/"),
                format!("a/{name}"),
            ] {
                let checked = check(&path).unwrap();
                assert!(checked.parts.contains(&safe), "{path}");
                for part in &checked.parts {
                    assert!(part.len() <= 200, "{path}");
                    assert!(part.encode_utf16().count() <= 200, "{path}");
                }
            }
        }
        // 200 units and 202 bytes: the cut drops "文", the trim the spaces.
        let cut_to_device = format!("a/NUL{}文/b.txt", " ".repeat(196));
        assert_eq!(check(&cut_to_device), Err(PathError::Reserved));
        // The limits are the path's own names' limits.
        let rules = PathRules::default().with_names(names.with_max_bytes(6));
        assert_eq!(
            check_path("文文文/文.txt", &rules).unwrap().parts,
            ["文文", "文.tx"]
        );
        let lifted =
            PathRules::default().with_names(NameRules::default().with_max_bytes(usize::MAX));
        let part = "文".repeat(90);
        assert_eq!(
            check_path(&format!("a/{part}"), &lifted).unwrap().parts[1],
            part
        );
    }

    #[test]
    fn the_path_rules_are_built_from_the_default() {
        let rules = PathRules::default()
            .with_max_depth(2)
            .with_max_chars(8)
            .with_names(NameRules::default().with_max_units(3));
        assert_eq!((rules.max_depth, rules.max_chars), (2, 8));
        assert_eq!(rules.names.max_units, 3);
        // A field of rules a caller holds can be set as well.
        let mut set = rules.clone();
        set.max_depth = 5;
        assert_eq!(set, rules.with_max_depth(5));
    }

    #[test]
    fn the_path_limits_are_settings() {
        let rules = PathRules {
            max_depth: 2,
            max_chars: 8,
            names: NameRules {
                max_units: 3,
                ..NameRules::default()
            },
        };
        assert_eq!(
            check_path("a/b/c", &rules),
            Err(PathError::TooDeep { most: 2 })
        );
        assert_eq!(
            check_path("a/bcdefgh", &rules),
            Err(PathError::TooLong { most: 8 })
        );
        assert_eq!(
            check_path("abcd/efg", &rules).unwrap().parts,
            ["abc", "efg"]
        );
    }
}
