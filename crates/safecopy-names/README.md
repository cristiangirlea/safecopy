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
  and 200 bytes of UTF-8. A longer name is cut in its stem and keeps its
  extension.
- `entry_name`: the same in composed Unicode form, `/` and `\` made `_`,
  for the entries of a list.
- `check_path`: a path inside a folder, refused with a `PathError` when it
  climbs out, starts at a drive, names a stream or a device, or is too deep
  or too long.
- `candidates`, `numbered`: `name (2).ext`, `name (3).ext` and so on;
  `numbered` within both limits, cut by the rule `safe_name` cuts by.
- `Siblings`: the names given in one folder, numbered when two would be
  the same file there (`fold`).
- `same_name`: whether a file sent under one name may have been saved
  under another.

Every limit is a field of `NameRules` or `PathRules`, whose defaults are
200 units, 200 bytes, an extension of 16 characters, 32 parts and 1,024
characters. Errors are enums; your program words them.

## A name's length

Windows counts a name in UTF-16 units and Linux, Android and most Unix
file systems in bytes of UTF-8: 255 of either. One unit can be three
bytes, so 90 Chinese characters are 90 units and 270 bytes. The crate
cannot know which system a name is for, so both limits hold everywhere by
default: `max_units` and `max_bytes`, 200 each, and a safe name fits
either kind of system.

- A name over either limit is cut between two characters: never half a
  surrogate pair, never part of a character's bytes. A mark that
  combines with the letter before it is a character of its own, so a
  cut may part the two; `entry_name` composes a name first, and a letter
  that has a composed form with its mark is one character there.
- The cut keeps the extension, see "A cut keeps the extension" below.
- A device name gets its `_` and the whole name is cut again behind it,
  and each part of a path in `check_path` is cut the same way; a part
  the cut leaves as a device name is refused. `entry_name`'s limits hold
  for the name it returns.
- The defaults leave 55 of the 255, in units and in bytes, for what a
  program puts after a safe name: ` (10001)` and `.part` are 13 of
  either. A caller that sets its own limits leaves its own room.
- A caller that only ever writes to one kind of system lifts the other
  limit: `NameRules::default().with_max_bytes(usize::MAX)` cuts by units
  alone, as before; `with_max_units(usize::MAX)` by bytes alone.

## A cut keeps the extension

A file saved under a name without its extension no longer opens by its
type, so a cut takes from the stem: 250 letters and `.txt` are saved as
196 letters and `.txt`, 90 Chinese characters and `.pdf` as 65 and
`.pdf`. One rule cuts every name: `safe_name`, `entry_name`, each part
in `check_path`, and `numbered`, where the number stands between the
stem and the extension (`name (2).ext`).

- The extension is what follows the last dot of a name that does not
  start with that dot. A cut keeps it when it has at most
  `NameRules::max_extension` characters after the dot (16 by default),
  none of them a space or a control character, and both limits leave
  room for one character of the stem before it. The stem is then cut,
  between two characters, to what both limits leave.
- Anything else is cut with the name, at its end, as every name was
  before, and a number follows it (`name.e (2)`): a name that is all
  extension (`.profile`), an extension longer than the limits or one
  that leaves no room for a stem, and what follows a dot without being
  a file's type, such as the end of a sentence (`Dr. No and the rest`)
  or 17 characters and more. The bound is on the length because that is
  what tells a type from text: a system takes whatever follows the last
  dot for the type, and the types in use are a few characters long.
- Of a double extension the last part is kept: where a name is 8 units,
  `archive.tar.gz` is cut to `archi.gz`, as `numbered` always kept `.gz`
  alone.
- What the cut leaves of the stem loses the dots and spaces at its end
  (spaces of every kind, as at the end of any name made safe), so no
  cut name has a space or a dot before its extension's dot; a
  stem with nothing else in reach is no stem, and the name is cut at
  its end. A name within both limits is not cut and not touched, a
  space before its dot included (`a .txt`).
- A cut can leave a device's name with the extension (`NUL`, many
  spaces, `x.txt` is cut to `NUL.txt`): `safe_name` marks it
  (`_NUL.txt`) and `check_path` refuses it, as Windows refuses a device
  name with any extension.
- `NameRules::default().with_max_extension(0)` keeps no extension and
  cuts every name at its end: `safe_name`, `entry_name`, `check_path`
  and `same_name` then give what they gave before a cut kept one.
  `numbered` does not: it kept whatever followed the last dot, which
  `with_max_extension(usize::MAX)` still does unless a space follows
  the dot.

Depends on no other safecopy crate.

## Changes

- **Behaviour change: a name that is cut now keeps its extension.**
  `safe_name`, `entry_name` and `check_path` cut a name over a limit at
  its end, extension and all; they now cut the stem, by the rule
  `numbered` follows too ("A cut keeps the extension"). The same long
  name gives another safe name than before:

  | name | before | now |
  | --- | --- | --- |
  | 250 letters, `.txt` | 200 letters | 196 letters, `.txt` |
  | 90 Chinese characters, `.pdf` | 66 characters | 65 characters, `.pdf` |
  | 100 emoji, `.png` | 50 emoji | 49 emoji, `.png` |
  | 130 Cyrillic letters, `.txt` | 100 letters | 98 letters, `.txt` |
  | `NUL`, 197 spaces, `x.txt` | `_NUL` | `_NUL.txt` |
  | two of 250 letters and `.txt` in one folder (`Siblings`) | 200 letters; 196 letters, ` (2)` | 196 letters, `.txt`; 192 letters, ` (2).txt` |
  | `CON.txt` where a name is 5 bytes | `_CON` | `C.txt` |
  | `NUL.`, 64 Chinese characters, `.pdf` (200 bytes) | `_NUL.`, 64 characters, `.pd` | `_NUL.`, 63 characters, `.pdf` |

  A name within both limits is as it was, unless it is a device's name
  that its `_` puts over a limit (the last row), and so is a cut name
  without an extension to keep. `NUL.` and 196 letters is still `_NUL.`
  and 195 letters: 196 letters after a dot are no extension. More
  follows from the one rule:
  - `numbered` kept whatever followed the last dot when it cut; it now
    keeps what the rule calls an extension. A name that must be cut for
    its number and ends in a dot and more than 16 characters, or in a
    dot and text with a space, has the number at its end
    (`name.seventeen-letters (2)`) where it had it before the dot. A
    name that needs no cut is numbered as before, before its last dot.
  - A device name is marked and the whole name cut again behind the
    mark, where the cut name was marked and cut a second time. Nothing
    changes for a name that is cut at its end.
  - A space of any kind at the end of a name or of a cut is dropped,
    where a no-break or a wide space after a dot or at a cut stayed and
    was dropped when the name was made safe again: a safe name made
    safe again is now always the same name.
  - Two programs on different versions give one long name two safe
    names. `same_name` matches by this version's cut, so a file an
    older version saved under 200 letters is not taken for the 250
    letters and `.txt` sent again; `with_max_extension(0)` matches as
    before. Names already cut (in a list, a journal, on disk) are
    within the limits and are left as they are by either version.
- **`NameRules::max_extension`**, 16 by default, with
  `with_max_extension`: the longest extension a cut keeps, in characters
  after the dot. 0 gives the cut of before for every name but a
  numbered one.
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
