//! A list's bytes: unsigned LEB128 varints and length-prefixed UTF-8, the
//! `postcard` encoding of the same values.

use crate::{Entry, Kind, Limits, Manifest, ManifestError, check};

pub(crate) fn encode(manifest: &Manifest, most: usize) -> Result<Vec<u8>, ManifestError> {
    let mut bytes = Vec::new();
    varint(&mut bytes, manifest.id);
    varint(&mut bytes, manifest.entries.len() as u64);
    for entry in &manifest.entries {
        varint(&mut bytes, entry.path.len() as u64);
        bytes.extend_from_slice(entry.path.as_bytes());
        varint(&mut bytes, kind_number(entry.kind));
        varint(&mut bytes, entry.size);
        // Stops as soon as it is too long, not after building it all.
        if bytes.len() > most {
            return Err(ManifestError::TooLong { most });
        }
    }
    if bytes.len() > most {
        return Err(ManifestError::TooLong { most });
    }
    Ok(bytes)
}

pub(crate) fn decode(bytes: &[u8], limits: &Limits) -> Result<Manifest, ManifestError> {
    if bytes.len() > limits.encoded {
        return Err(ManifestError::TooLong {
            most: limits.encoded,
        });
    }
    let mut reader = Reader(bytes);
    let id = reader.varint(u64::BITS)?;
    let count = reader.varint(u32::BITS)? as usize;
    let most = limits.most_entries();
    if count > most {
        return Err(ManifestError::TooMany { most });
    }
    // An entry takes at least three bytes: no more room than the bytes can fill.
    let mut entries = Vec::with_capacity(count.min(reader.0.len() / 3));
    for _ in 0..count {
        let path = reader.text()?;
        let kind = match reader.varint(u32::BITS)? {
            0 => Kind::File,
            1 => Kind::Folder,
            _ => return Err(ManifestError::Damaged),
        };
        let size = reader.varint(u64::BITS)?;
        entries.push(Entry { path, kind, size });
    }
    if !reader.0.is_empty() {
        return Err(ManifestError::Damaged);
    }
    check(id, entries, limits)
}

fn kind_number(kind: Kind) -> u64 {
    match kind {
        Kind::File => 0,
        Kind::Folder => 1,
    }
}

fn varint(bytes: &mut Vec<u8>, mut n: u64) {
    loop {
        let low = (n & 0x7F) as u8;
        n >>= 7;
        if n == 0 {
            bytes.push(low);
            return;
        }
        bytes.push(low | 0x80);
    }
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    /// A varint of a `bits`-wide number. As `postcard` reads it: at most
    /// as many bytes as the width needs, the last of them within the bits
    /// left; a longer form than needed is taken.
    fn varint(&mut self, bits: u32) -> Result<u64, ManifestError> {
        let longest = bits.div_ceil(7);
        let last_most = (1u8 << (bits % 7)) - 1;
        let mut n = 0u64;
        for i in 0..longest {
            let (&byte, rest) = self.0.split_first().ok_or(ManifestError::Damaged)?;
            self.0 = rest;
            n |= u64::from(byte & 0x7F) << (7 * i);
            if byte & 0x80 == 0 {
                if i == longest - 1 && byte > last_most {
                    return Err(ManifestError::Damaged);
                }
                return Ok(n);
            }
        }
        Err(ManifestError::Damaged)
    }

    fn text(&mut self) -> Result<String, ManifestError> {
        let len = self.varint(usize::BITS)?;
        let len = usize::try_from(len).map_err(|_| ManifestError::Damaged)?;
        if len > self.0.len() {
            return Err(ManifestError::Damaged);
        }
        let (text, rest) = self.0.split_at(len);
        self.0 = rest;
        std::str::from_utf8(text)
            .map(str::to_owned)
            .map_err(|_| ManifestError::Damaged)
    }
}
