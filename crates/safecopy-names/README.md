# safecopy-names

Safe file names.

Checks a file name that came from somewhere else before it touches the disk: it refuses unsafe and reserved names and paths that would leave their folder, makes numbered names such as `report (2).pdf`, and finds names that collide within one folder (by case or Unicode form).

Depends on no other safecopy crate.

**Status:** nothing is built yet. This crate is a placeholder in the
[safecopy](../../README.md) workspace; its API will come in a later change.

Licence: MIT.
