//! Names and paths as the other side sends them: never a panic, and what
//! comes out keeps the rules it promises.
#![no_main]

use std::ffi::OsStr;

use libfuzzer_sys::fuzz_target;
use safecopy::names::{
    NameRules, PathRules, Siblings, check_path, entry_name, fold, hidden, reserved, safe_name,
};

/// What every name these functions give must be.
fn kept_the_rules(name: &str, rules: &NameRules) {
    assert!(!name.is_empty());
    assert!(name.encode_utf16().count() <= rules.max_units, "{name:?}");
    assert!(!reserved(name), "{name:?}");
    assert!(!name.ends_with(['.', ' ']), "{name:?}");
    assert!(
        !name.chars().any(|c| hidden(c) || "/\\<>:\"|?*".contains(c)),
        "{name:?}"
    );
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
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
    let mut siblings = Siblings::new(NameRules {
        max_number: 64,
        ..NameRules::default()
    });
    let mut folds = std::collections::HashSet::new();
    for name in text.split('\n').take(256) {
        if let Ok(given) = siblings.take(OsStr::new(name)) {
            kept_the_rules(&given, &rules);
            assert!(folds.insert(fold(&given)), "{given:?} given twice");
        }
    }
});
