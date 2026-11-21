# safecopy-manifest

A bounded list of files to send.

Builds a list of files and folders to send, within limits the caller sets (count, depth, total size), encodes and decodes it, keeps the opened sources, and says when a source changed after it was listed.

Uses `safecopy-folder`.

**Status:** nothing is built yet. This crate is a placeholder in the
[safecopy](../../README.md) workspace; its API will come in a later change.

Licence: MIT.
