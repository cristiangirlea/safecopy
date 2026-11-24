//! Names and paths as the other side sends them: never a panic, and what
//! comes out keeps the rules it promises.
#![no_main]

use std::ffi::OsStr;

use libfuzzer_sys::fuzz_target;
use safecopy::names::{
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
    assert!(!name.ends_with(|c: char| c == '.' || c.is_whitespace()), "{name:?}");
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
    let first = stem.chars().next().filter(|c| *c != '.' && !c.is_whitespace())?;
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

fuzz_target!(|data: &[u8]| {
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
});
