//! What the fuzz target `names` runs, kept in the crate so its tests run
//! every saved input through the same code. Built only for the tests and
//! under `cfg(fuzzing)`, which a fuzzer's build sets: an ordinary build has
//! none of it.

use std::ffi::OsStr;

use crate::{
    NameRules, PathRules, Siblings, check_path, entry_name, fold, hidden, numbered, reserved,
    safe_name, stem_and_ext,
};

/// What every name these functions give must be: within both limits. Each
/// is a `String`, so valid UTF-8 with every character whole: a cut inside
/// one would have stopped the run where it was made.
fn kept_the_rules(name: &str, rules: &NameRules) {
    assert!(!name.is_empty());
    assert!(name.encode_utf16().count() <= rules.max_units, "{name:?}");
    assert!(name.len() <= rules.max_bytes, "{name:?}");
    assert!(!reserved(name), "{name:?}");
    assert!(
        !name.ends_with(|c: char| c == '.' || c.is_whitespace()),
        "{name:?}"
    );
    assert!(
        !name.chars().any(|c| hidden(c) || "/\\<>:\"|?*".contains(c)),
        "{name:?}"
    );
}

/// The extension of `name` when a cut under `rules` must keep it with
/// `number` before it: what follows the last dot, of a length the rules
/// keep, without a space or a control character, with room for the stem's
/// first character, which the trim leaves.
fn kept_extension<'a>(name: &'a str, number: &str, rules: &NameRules) -> Option<&'a str> {
    let (stem, ext) = stem_and_ext(name);
    let after = ext.strip_prefix('.')?;
    let first = stem
        .chars()
        .next()
        .filter(|c| *c != '.' && !c.is_whitespace())?;
    let length = first.len_utf8() + number.len() + ext.len();
    let units = first.len_utf16() + number.len() + ext.encode_utf16().count();
    let is_one = (1..=rules.max_extension).contains(&after.chars().count())
        && !after.chars().any(|c| c.is_whitespace() || c.is_control());
    (is_one && length <= rules.max_bytes && units <= rules.max_units).then_some(ext)
}

/// A limit from a character of the input: lifted, the default, or small
/// enough that a mark, a number and an extension do not all fit.
fn limit(c: Option<char>) -> usize {
    match c.map(|c| c as usize) {
        None => 200,
        Some(n) if n % 4 == 0 => usize::MAX,
        Some(n) if n % 4 == 1 => 200,
        Some(n) => (n / 4) % 24,
    }
}

/// The names made under `rules` keep them, whichever limit cuts.
fn names_keep(text: &str, rules: &NameRules) {
    if let Some(name) = safe_name(text, rules) {
        kept_the_rules(&name, rules);
        assert_eq!(safe_name(&name, rules).as_deref(), Some(name.as_str()));
    }
    // The name as it is with nothing cut, then cut: its extension stays.
    let lifted = rules
        .clone()
        .with_max_units(usize::MAX)
        .with_max_bytes(usize::MAX);
    if let Some(whole) = safe_name(text, &lifted)
        && let Some(ext) = kept_extension(&whole, "", rules)
    {
        let cut = safe_name(&whole, rules).expect("a stem and its extension fit");
        assert!(cut.ends_with(ext), "{whole:?} cut to {cut:?}");
        // As a part of a path too, where the cut leaves no device name.
        let path_rules = PathRules::default().with_names(rules.clone());
        if let Ok(path) = check_path(&format!("a/{whole}"), &path_rules) {
            assert!(path.parts[1].ends_with(ext), "{whole:?}");
        }
    }
    if let Some(name) = entry_name(text, rules) {
        kept_the_rules(&name, rules);
        assert_eq!(entry_name(&name, rules).as_deref(), Some(name.as_str()));
        for n in [2, 10, 10_001] {
            if let Some(numbered) = numbered(&name, n, rules) {
                kept_the_rules(&numbered, rules);
                // Numbered by the same rule: the number, then the extension.
                let number = format!(" ({n})");
                if let Some(ext) = kept_extension(&name, &number, rules) {
                    assert!(
                        numbered.ends_with(&format!("{number}{ext}")),
                        "{numbered:?}"
                    );
                }
            }
        }
    }
    let path_rules = PathRules::default().with_names(rules.clone());
    if let Ok(path) = check_path(text, &path_rules) {
        for part in &path.parts {
            kept_the_rules(part, rules);
        }
    }
}

/// Names and paths as the other side sends them: never a panic, and what
/// comes out keeps the rules it promises.
pub fn names(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    // The first three characters also choose the limits of a second pass;
    // the third, the longest extension kept: none, the default, any, few.
    let mut chars = text.chars();
    let chosen = NameRules::default()
        .with_max_units(limit(chars.next()))
        .with_max_bytes(limit(chars.next()));
    let chosen = match chars.next().map(|c| c as usize) {
        None => chosen,
        Some(n) if n % 4 == 0 => chosen.with_max_extension(0),
        Some(n) if n % 4 == 1 => chosen,
        Some(n) if n % 4 == 2 => chosen.with_max_extension(usize::MAX),
        Some(n) => chosen.with_max_extension((n / 4) % 8),
    };
    names_keep(text, &chosen);
    let rules = NameRules::default();
    if let Some(name) = safe_name(text, &rules) {
        kept_the_rules(&name, &rules);
    }
    if let Some(name) = entry_name(text, &rules) {
        kept_the_rules(&name, &rules);
        assert_eq!(entry_name(&name, &rules).as_deref(), Some(name.as_str()));
    }
    let path_rules = PathRules::default();
    if let Ok(path) = check_path(text, &path_rules) {
        assert!(!path.parts.is_empty() && path.parts.len() <= path_rules.max_depth);
        for part in &path.parts {
            assert!(part != "." && part != "..");
            kept_the_rules(part, &rules);
        }
    }
    // Every name given in one folder differs from the others by its fold.
    let mut siblings = Siblings::new(NameRules::default().with_max_number(64));
    let mut folds = std::collections::HashSet::new();
    for name in text.split('\n').take(256) {
        if let Ok(given) = siblings.take(OsStr::new(name)) {
            kept_the_rules(&given, &rules);
            assert!(folds.insert(fold(&given)), "{given:?} given twice");
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
        name: "names",
        run: names,
        whole: |bytes| std::str::from_utf8(bytes).is_ok(),
        seeds,
        cut: "wide-device-cut",
    }];

    /// The names the crate's tests find the edges with: devices, wide
    /// characters at the limits, hidden characters, paths, and names that
    /// meet in one folder.
    fn seeds() -> Vec<Input> {
        let wide = "文";
        let texts: Vec<(&str, String)> = vec![
            ("device-con", "CON".into()),
            ("device-nul-ext", "nul.txt".into()),
            ("device-com1", "COM1".into()),
            ("device-com-superscript", "COM¹".into()),
            ("device-lpt-superscript", "LPT¹".into()),
            ("device-lpt9-log", "LPT9.log".into()),
            ("device-com10", "COM10".into()),
            ("device-lpt-superscript-0", "LPT¹0".into()),
            ("device-hidden", "CON\u{202E}.txt".into()),
            ("device-in-path", "x/CON".into()),
            ("device-above", "CON/x".into()),
            ("device-cut-spaces", format!("NUL{}x", " ".repeat(197))),
            ("device-cut-ext", format!("NUL.{}", "b".repeat(196))),
            ("wide-accents", "x\u{301}".repeat(10)),
            (
                "wide-accents-ext",
                format!("{}.odt", "e\u{301}".repeat(150)),
            ),
            ("wide-decomposes", "\u{958}".repeat(60)),
            ("wide-device", format!("NUL.{}b", wide.repeat(65))),
            ("wide-device-cut", format!("NUL{}{wide}", " ".repeat(196))),
            ("wide-device-ext", format!("NUL.{}.pdf", wide.repeat(64))),
            ("wide-tight", format!("CON.{wide}{wide}")),
            ("wide-emoji", "a\u{200D}é😀.txt".into()),
            ("hidden-override", "photo\u{202E}gpj.exe".into()),
            (
                "hidden-marks",
                "a\u{202A}b\u{202C}c\u{2066}d\u{2069}.txt".into(),
            ),
            ("hidden-tags", "tag\u{E0001}\u{E0041}\u{E007F}.txt".into()),
            ("hidden-only", "\u{202E}\u{2066}".into()),
            ("hidden-joiner-kept", "\u{0646}\u{200C}\u{0647}.txt".into()),
            ("control", "a\u{1}b.txt".into()),
            ("space-ext", format!("{}.a\u{a0}b", "x".repeat(198))),
            ("path", "Photos/2024/beach.jpg".into()),
            ("path-dots", "a/../b/./c".into()),
            (
                "siblings",
                "a.txt\nA.TXT\ncafe\u{301}\ncafé\nCON\ncon\na.txt\n".into(),
            ),
        ];
        texts
            .into_iter()
            .map(|(name, text)| (name.to_owned(), text.into_bytes()))
            .collect()
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
