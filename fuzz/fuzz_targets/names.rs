//! Names and paths as the other side sends them: never a panic, and what
//! comes out keeps the rules it promises.
#![no_main]

use std::ffi::OsStr;

use libfuzzer_sys::fuzz_target;
use safecopy::names::{
    NameRules, PathRules, Siblings, check_path, entry_name, fold, hidden, numbered, reserved,
    safe_name,
};

/// What every name these functions give must be: within both limits. Each
/// is a `String`, so valid UTF-8 with every character whole: a cut inside
/// one would have stopped the run where it was made.
fn kept_the_rules(name: &str, rules: &NameRules) {
    assert!(!name.is_empty());
    assert!(name.encode_utf16().count() <= rules.max_units, "{name:?}");
    assert!(name.len() <= rules.max_bytes, "{name:?}");
    assert!(!reserved(name), "{name:?}");
    assert!(!name.ends_with(['.', ' ']), "{name:?}");
    assert!(
        !name.chars().any(|c| hidden(c) || "/\\<>:\"|?*".contains(c)),
        "{name:?}"
    );
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
    if let Some(name) = entry_name(text, rules) {
        kept_the_rules(&name, rules);
        assert_eq!(entry_name(&name, rules).as_deref(), Some(name.as_str()));
        for n in [2, 10, 10_001] {
            if let Some(numbered) = numbered(&name, n, rules) {
                kept_the_rules(&numbered, rules);
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
    // The first two characters also choose the limits of a second pass.
    let mut chars = text.chars();
    let chosen = NameRules::default()
        .with_max_units(limit(chars.next()))
        .with_max_bytes(limit(chars.next()));
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
