# safecopy-folder

Writing through a held folder.

Writes files into a folder the caller holds open. Links and junctions are never followed, a file already there is never replaced, a finished file is published under its final name only when that name is free, and removal goes through a handle checked to be the same file. It can also read and set a file's modified time and execute bit, within rules the caller gives.

Uses `safecopy-names`.

**Status:** nothing is built yet. This crate is a placeholder in the
[safecopy](../../README.md) workspace; its API will come in a later change.

Licence: MIT.
