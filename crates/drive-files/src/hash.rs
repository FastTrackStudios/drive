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

/// A sampled hash: the size, and the first, middle and last 64 KiB. Equal
/// sampled hashes mean "probably the same"; only the full hash says so.
pub fn sampled(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let mut h = blake3::Hasher::new();
    h.update(&len.to_le_bytes());
    let mut buf = vec![0u8; SAMPLE as usize];
    for at in [0, len / 2, len.saturating_sub(SAMPLE)] {
        if at >= len && len > 0 {
            continue;
        }
        f.seek(SeekFrom::Start(at))?;
        let n = f.read(&mut buf)?;
        h.update(&buf[..n]);
        if len <= SAMPLE {
            break;
        }
    }
    Ok(h.finalize().to_hex().to_string())
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
