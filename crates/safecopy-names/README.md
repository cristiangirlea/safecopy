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
  `nul.txt`, `COM¹`, which get a `_` in front), at most 200 UTF-16 units.
- `entry_name`: the same in composed Unicode form, `/` and `\` made `_`,
  for the entries of a list.
- `check_path`: a path inside a folder, refused with a `PathError` when it
  climbs out, starts at a drive, names a stream or a device, or is too deep
  or too long.
- `candidates`, `numbered`: `name (2).ext`, `name (3).ext` and so on,
  within the limit.
- `Siblings`: the names given in one folder, numbered when two would be
  the same file there (`fold`).
- `same_name`: whether a file sent under one name may have been saved
  under another.

Every limit is a field of `NameRules` or `PathRules`, whose defaults are
200 units, 32 parts and 1,024 characters. Errors are enums; your program
words them.

Depends on no other safecopy crate.

Licence: MIT.
