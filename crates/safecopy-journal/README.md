# safecopy-journal

Crash records and safe cleanup.

Records each partial file while it is being written. After a crash, cleanup removes only a file whose identity still matches its record; a renamed or replaced file is left alone.

Uses `safecopy-folder` and `safecopy-manifest`.

**Status:** nothing is built yet. This crate is a placeholder in the
[safecopy](../../README.md) workspace; its API will come in a later change.

Licence: MIT.
