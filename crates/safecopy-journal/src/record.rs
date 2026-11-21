//! A record's bytes: unsigned LEB128 varints and length-prefixed UTF-8, the
//! `postcard` encoding of the same values, written whole each time.

use crate::{JournalError, Partial, Record, Settings};

pub(crate) fn encode(record: &Record, settings: &Settings) -> Result<Vec<u8>, JournalError> {
    let most = settings.record_bytes;
    let mut bytes = Vec::new();
    text(&mut bytes, &record.volume);
    varint(&mut bytes, u128::from(record.serial));
    varint(&mut bytes, record.partials.len() as u128);
    for partial in &record.partials {
        varint(&mut bytes, partial.folder);
        text(&mut bytes, &partial.name);
        varint(&mut bytes, partial.file);
        // Stops as soon as it is too long, not after building it all.
        if bytes.len() > most {
            return Err(JournalError::TooLarge { most });
        }
    }
    if bytes.len() > most {
        return Err(JournalError::TooLarge { most });
    }
    Ok(bytes)
}

pub(crate) fn decode(bytes: &[u8], settings: &Settings) -> Result<Record, JournalError> {
    let most = settings.record_bytes;
    if bytes.len() > most {
        return Err(JournalError::TooLarge { most });
    }
    let mut reader = Reader(bytes);
    let volume = reader.text()?;
    let serial = reader.varint(u64::BITS)? as u64;
    let count = reader.varint(usize::BITS)? as usize;
    // A partial takes at least three bytes: no more room than the bytes can fill.
    let mut partials = Vec::with_capacity(count.min(reader.0.len() / 3));
    for _ in 0..count {
        let folder = reader.varint(u128::BITS)?;
        let name = reader.text()?;
        let file = reader.varint(u128::BITS)?;
        partials.push(Partial { folder, name, file });
    }
    if !reader.0.is_empty() {
        return Err(JournalError::Damaged);
    }
    Ok(Record {
        volume,
        serial,
        partials,
    })
}

fn varint(bytes: &mut Vec<u8>, mut n: u128) {
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

fn text(bytes: &mut Vec<u8>, text: &str) {
    varint(bytes, text.len() as u128);
    bytes.extend_from_slice(text.as_bytes());
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    /// A varint of a `bits`-wide number. As `postcard` reads it: at most
    /// as many bytes as the width needs, the last of them within the bits
    /// left; a longer form than needed is taken.
    fn varint(&mut self, bits: u32) -> Result<u128, JournalError> {
        let longest = bits.div_ceil(7);
        let last_most = (1u8 << (bits % 7)) - 1;
        let mut n = 0u128;
        for i in 0..longest {
            let (&byte, rest) = self.0.split_first().ok_or(JournalError::Damaged)?;
            self.0 = rest;
            n |= u128::from(byte & 0x7F) << (7 * i);
            if byte & 0x80 == 0 {
                if i == longest - 1 && byte > last_most {
                    return Err(JournalError::Damaged);
                }
                return Ok(n);
            }
        }
        Err(JournalError::Damaged)
    }

    fn text(&mut self) -> Result<String, JournalError> {
        let len = self.varint(usize::BITS)?;
        let len = usize::try_from(len).map_err(|_| JournalError::Damaged)?;
        if len > self.0.len() {
            return Err(JournalError::Damaged);
        }
        let (text, rest) = self.0.split_at(len);
        self.0 = rest;
        std::str::from_utf8(text)
            .map(str::to_owned)
            .map_err(|_| JournalError::Damaged)
    }
}
