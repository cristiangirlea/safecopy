# safecopy-names

Safe file names.

Checks a file name that came from somewhere else before it touches the
disk: it refuses unsafe and reserved names and paths that would leave
their folder, makes numbered names such as `report (2).pdf`, and finds
names that collide within one folder (by case or Unicode form). No I/O.

Windows' rules apply on every system, so a file saved anywhere can later
be copied to Windows.

- `safe_name`: the last part of a sent name, made safe: no `<>:"|?*`, no
  control or invisible direction characters, no device names (`CON`,
  `nul.txt`, `COM¹`, which get a `_` in front), at most 200 UTF-16 units
  and 200 bytes of UTF-8.
- `entry_name`: the same in composed Unicode form, `/` and `\` made `_`,
  for the entries of a list.
- `check_path`: a path inside a folder, refused with a `PathError` when it
  climbs out, starts at a drive, names a stream or a device, or is too deep
  or too long.
- `candidates`, `numbered`: `name (2).ext`, `name (3).ext` and so on;
  `numbered` within both limits, the extension kept.
- `Siblings`: the names given in one folder, numbered when two would be
  the same file there (`fold`).
- `same_name`: whether a file sent under one name may have been saved
  under another.

Every limit is a field of `NameRules` or `PathRules`, whose defaults are
200 units, 200 bytes, 32 parts and 1,024 characters. Errors are enums;
your program words them.

## A name's length

Windows counts a name in UTF-16 units and Linux, Android and most Unix
file systems in bytes of UTF-8: 255 of either. One unit can be three
bytes, so 90 Chinese characters are 90 units and 270 bytes. The crate
cannot know which system a name is for, so both limits hold everywhere by
default: `max_units` and `max_bytes`, 200 each, and a safe name fits
either kind of system.

- A name over either limit is cut at its end, between two characters:
  never half a surrogate pair, never part of a character's bytes. A mark
  that combines with the letter before it is a character of its own, so a
  cut may part the two; `entry_name` composes a name first, and a letter
  that has a composed form with its mark is one character there. The
  dots and spaces a cut leaves at the end go too.
- The cut takes the extension with the end of the name, as it always
  has: 250 letters and `.txt` are saved as 200 letters. `numbered` keeps
  the extension and cuts the stem; an extension that would leave no room
  for a stem is cut as part of the name (where it gave no name for some
  numbers before, it now does), and a name that is all extension
  (`.profile`) has none.
- A device name that gets its `_` is cut again to fit, and each part of
  a path in `check_path` is cut the same way; a part the cut leaves as a
  device name is refused. `entry_name`'s limits hold for the name it
  returns.
- The defaults leave 55 of the 255, in units and in bytes, for what a
  program puts after a safe name: ` (10001)` and `.part` are 13 of
  either. A caller that sets its own limits leaves its own room.
- A caller that only ever writes to one kind of system lifts the other
  limit: `NameRules::default().with_max_bytes(usize::MAX)` cuts by units
  alone, as before; `with_max_units(usize::MAX)` by bytes alone.

Depends on no other safecopy crate.

## Changes

- **Breaking: a name is limited in bytes too.** `NameRules::max_bytes`,
  200 by default, beside `max_units`. A name within 200 units that is
  over 200 bytes of UTF-8 was returned as it was and is now cut: 100
  emoji (400 bytes) to 50, 90 Chinese characters (270 bytes) to 66, 130
  Cyrillic letters (260 bytes) to 100. Linux, Android and most Unix file
  systems refused such a name ("File name too long"). A name that is
  within 200 bytes once made safe (for `entry_name`, once composed) is
  as it was, and so is every name of ASCII. `numbered`,
  `Siblings`, `check_path` and `entry_name` follow the same limit, and
  `same_name` matches by it, so a file saved earlier under a name over
  200 bytes is no longer taken for the same file.
  `with_max_bytes(usize::MAX)` gives the old behaviour.
- **Breaking: `NameRules` and `PathRules` are `#[non_exhaustive]`.**
  Outside the crate neither can be written as a struct any more, with or
  without `..Default::default()`, so a new limit breaks no caller again.
  Where a caller wrote `NameRules { max_units: 100, ..Default::default() }`
  it now writes `NameRules::default().with_max_units(100)`
  (`with_max_bytes`, `with_max_number`; `PathRules::default()` with
  `with_max_depth`, `with_max_chars`, `with_names`), or sets a field of
  one it holds: `rules.max_units = 100`. The fields are still public.

Licence: MIT.
