# safecopy-chunks

Verified copying of one file.

Copies exactly the declared number of bytes over any async byte stream, hashes them with SHA-256 while reading and writing, carries the whole-file hash in a trailer, and verifies before the file is published. A pace hook lets the caller hold a copy between slices.

Uses `safecopy-folder`.

**Status:** nothing is built yet. This crate is a placeholder in the
[safecopy](../../README.md) workspace; its API will come in a later change.

Licence: MIT.
