//! BLAKE3, two ways (Spacedrive's method): a **sampled** hash, cheap at
//! any size, to find likely duplicates; and the **full** hash, which names
//! a blob and is checked before anything is reused, trusted or deleted.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const SAMPLE: u64 = 64 * 1024;

/// The full BLAKE3 hash of some bytes, as hex.
pub fn of_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// The full BLAKE3 hash of a file, read in a stream, as hex.
pub fn of_file(path: &Path) -> std::io::Result<String> {
    let mut h = blake3::Hasher::new();
    h.update_reader(std::fs::File::open(path)?)?;
    Ok(h.finalize().to_hex().to_string())
}

/// The byte ranges a sampled hash reads from something `len` long.
pub fn sample_ranges(len: u64) -> Vec<std::ops::Range<u64>> {
    if len <= SAMPLE {
        return vec![0..len];
    }
    [0, len / 2, len - SAMPLE].into_iter().map(|at| at..(at + SAMPLE).min(len)).collect()
}

/// A sampled hash from its ranges' bytes (read anywhere: a disk, a bucket).
pub fn sampled_of(len: u64, parts: &[&[u8]]) -> String {
    let mut h = blake3::Hasher::new();
    h.update(&len.to_le_bytes());
    for p in parts {
        h.update(p);
    }
    h.finalize().to_hex().to_string()
}

/// A sampled hash: the size, and the first, middle and last 64 KiB. Equal
/// sampled hashes mean "probably the same"; only the full hash says so.
pub fn sampled(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let mut parts = Vec::new();
    for r in sample_ranges(len) {
        let mut buf = vec![0u8; (r.end - r.start) as usize];
        f.seek(SeekFrom::Start(r.start))?;
        f.read_exact(&mut buf)?;
        parts.push(buf);
    }
    Ok(sampled_of(len, &parts.iter().map(Vec::as_slice).collect::<Vec<_>>()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_and_sampled() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&a, &big).unwrap();
        let mut changed = big.clone();
        changed[100_000] ^= 1; // outside every sample
        std::fs::write(&b, &changed).unwrap();
        assert_eq!(of_file(&a).unwrap(), of_bytes(&big));
        // The samples cannot tell these apart; the full hash can.
        assert_eq!(sampled(&a).unwrap(), sampled(&b).unwrap());
        assert_ne!(of_file(&a).unwrap(), of_file(&b).unwrap());
    }
}
