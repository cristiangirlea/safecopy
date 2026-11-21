# safecopy-resume

Resuming an interrupted transfer.

Keeps a record of what each side holds, so an interrupted transfer continues where it stopped. The receiver states what it has, the sender checks its file is unchanged, and on any doubt the file starts again from zero.

Uses `safecopy-chunks`, `safecopy-journal` and `safecopy-manifest`.

**Status:** nothing is built yet. This crate is a placeholder in the
[safecopy](../../README.md) workspace; its API will come in a later change.

Licence: MIT.
