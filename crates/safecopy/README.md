# safecopy

The umbrella crate.

Re-exports every safecopy crate under a short name (`safecopy::names`, `safecopy::folder`, and so on). Depend on it for all of them, or on the one crate you need.

**Status:** every crate is built. See the [safecopy](../../README.md) workspace.

Features: `quic` (off by default) turns on `safecopy-bundle`'s helpers for
quinn's streams, `safecopy::bundle::quic`.

Licence: MIT.
