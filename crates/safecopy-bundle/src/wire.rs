//! The table's bytes.

use safecopy_resume::TransferKey;

use crate::{BundleError, BundleLimits, TableEntry};

/// The table's count before its entries.
pub(crate) const COUNT_LEN: u64 = 4;

/// An entry's bytes besides its metadata block: size, key, block length.
const ENTRY_FIXED: u64 = 8 + 16 + 4;

/// The bytes `entry` takes in the table.
pub(crate) fn entry_len(entry: &TableEntry) -> u64 {
    ENTRY_FIXED + entry.meta.len() as u64
}

/// Checks the entry at `index` against the limits, with `bytes` the sizes
/// of the entries before it; gives the sizes up to and with it.
pub(crate) fn check_entry(
    index: u32,
    size: u64,
    meta_len: u64,
    bytes: u64,
    limits: &BundleLimits,
) -> Result<u64, BundleError> {
    if size > limits.small {
        return Err(BundleError::TooLarge {
            entry: index,
            size,
            most: limits.small,
        });
    }
    if meta_len > u64::from(limits.meta_bytes) {
        return Err(BundleError::MetaTooLong {
            entry: index,
            len: meta_len,
            most: limits.meta_bytes.into(),
        });
    }
    match bytes.checked_add(size) {
        Some(total) if total <= limits.bytes => Ok(total),
        total => Err(BundleError::TooManyBytes {
            bytes: total.unwrap_or(u64::MAX),
            most: limits.bytes,
        }),
    }
}

/// The table after its length: the count, then each entry.
pub(crate) fn encode(entries: &[TableEntry]) -> Vec<u8> {
    let len = COUNT_LEN + entries.iter().map(entry_len).sum::<u64>();
    let mut out = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    let count = u32::try_from(entries.len()).expect("checked against the limits");
    out.extend_from_slice(&count.to_le_bytes());
    for entry in entries {
        out.extend_from_slice(&entry.size.to_le_bytes());
        out.extend_from_slice(&entry.key.0);
        let meta = u32::try_from(entry.meta.len()).expect("checked against the limits");
        out.extend_from_slice(&meta.to_le_bytes());
        out.extend_from_slice(&entry.meta);
    }
    out
}

/// Takes `n` bytes from the front of `bytes`.
fn take<'a>(bytes: &mut &'a [u8], n: usize) -> Result<&'a [u8], BundleError> {
    if bytes.len() < n {
        return Err(BundleError::CountMismatch);
    }
    let (head, rest) = bytes.split_at(n);
    *bytes = rest;
    Ok(head)
}

fn array<const N: usize>(bytes: &mut &[u8]) -> Result<[u8; N], BundleError> {
    Ok(take(bytes, N)?.try_into().expect("N bytes"))
}

/// Reads the table after its length, checking every bound: the count
/// first, then each entry as it comes, then that nothing is left.
pub(crate) fn decode(
    mut bytes: &[u8],
    limits: &BundleLimits,
) -> Result<Vec<TableEntry>, BundleError> {
    let count = u32::from_le_bytes(array(&mut bytes)?);
    if count > limits.files {
        return Err(BundleError::TooMany {
            count,
            most: limits.files,
        });
    }
    // Bounded by the table's bytes too, whatever `files` allows.
    let fits = bytes.len() / ENTRY_FIXED as usize;
    let mut entries = Vec::with_capacity((count as usize).min(fits));
    let mut total = 0;
    for index in 0..count {
        let size = u64::from_le_bytes(array(&mut bytes)?);
        let key = TransferKey(array(&mut bytes)?);
        let meta_len = u32::from_le_bytes(array(&mut bytes)?);
        total = check_entry(index, size, meta_len.into(), total, limits)?;
        let meta = take(&mut bytes, meta_len as usize)?.to_vec();
        entries.push(TableEntry { key, meta, size });
    }
    if !bytes.is_empty() {
        return Err(BundleError::CountMismatch);
    }
    Ok(entries)
}
