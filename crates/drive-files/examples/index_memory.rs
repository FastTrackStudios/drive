//! How much memory a big location index takes, step by step (drive#47):
//! `cargo run --release -p drive-files --example index_memory -- 2500000`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use drive_files::index::{self, Encoder, IndexEntry, LocationIndex, Tree};

/// Live heap bytes (and the high-water mark), exactly.
struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let now = LIVE.fetch_add(l.size(), Ordering::Relaxed) + l.size();
        PEAK.fetch_max(now, Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
}
#[global_allocator]
static A: Counting = Counting;

fn rss_mb() -> u64 {
    (LIVE.load(Ordering::Relaxed) >> 20) as u64
}
fn reset_peak() {
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
}
fn peak_mb() -> u64 {
    (PEAK.load(Ordering::Relaxed) >> 20) as u64
}

/// A sample library of `n` files: vendor / pack / category / 40 files a
/// folder, each folder before its files (as they sort).
fn library(n: usize) -> impl Iterator<Item = IndexEntry> {
    (0..n.div_ceil(40)).flat_map(move |d| {
        let (v, p, c) = (d / 1250, d / 25 % 50, d % 25);
        let dir = format!("Vendor {v:03}/Pack {p:02} - Something Long/Category {c:02}");
        let folder = IndexEntry { path: dir.clone(), dir: true, size: 0, mtime: 0, sampled: String::new() };
        let files = (0..40).map(move |f| d * 40 + f).take_while(move |&i| i < n).map(move |i| IndexEntry {
            path: format!("{dir}/Sample_{v}_{p}_{c}_{:02} Kick Hard.wav", i % 40),
            dir: false,
            size: 1_000_000 + i as u64,
            mtime: 1_700_000_000_000 + i as i64,
            sampled: format!("{:064x}", i as u128 * 0x9e37_79b9_7f4a_7c15),
        });
        std::iter::once(folder).chain(files)
    })
}

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(2_500_000);
    let base = rss_mb();

    // Encoded an entry at a time (the agent's upload since drive#156, from
    // its index database a page at a time): the index is never whole.
    let count = library(n).count() as u64;
    reset_peak();
    let mut enc = Encoder::new("loc", "/x", 0, count);
    library(n).for_each(|e| enc.push(&e).unwrap());
    let streamed = enc.finish().unwrap();
    let streamed_peak = peak_mb().saturating_sub(base);

    let streamed_bytes = streamed.len();
    let streamed = blake3::hash(&streamed);
    let base = rss_mb();
    let mut entries: Vec<IndexEntry> = Vec::with_capacity(count as usize);
    entries.extend(library(n));
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let idx = LocationIndex { location: "loc".into(), root: "/x".into(), built: 0, entries };
    let after_index = rss_mb();
    reset_peak();
    let bytes = index::encode(&idx);
    let encode_peak = peak_mb();
    assert_eq!(blake3::hash(&bytes), streamed, "the streamed encoding is the same bytes");
    assert_eq!(bytes.len(), streamed_bytes);
    let after_encode = rss_mb();
    drop(idx);
    let after_drop = rss_mb();
    reset_peak();
    let back = index::decode(&bytes).unwrap();
    let decode_peak = peak_mb();
    let after_decode = rss_mb();
    let t0 = std::time::Instant::now();
    let tree = Tree::new(back);
    let built = t0.elapsed();
    let after_tree = rss_mb();
    let t0 = std::time::Instant::now();
    let mut found = 0;
    for e in tree.index.entries.iter().step_by(97) {
        found += usize::from(tree.get(&e.path).is_some());
        found += tree.list(e.parent()).len().min(1);
    }
    println!("Tree::new        {built:?}; {found} lookups+lists in {:?}", t0.elapsed());
    println!("entries          {}", tree.index.entries.len());
    println!("index in memory  {} MB", after_index - base);
    println!("encode (zstd)    peak {} MB while encoding, {} MB on the wire", encode_peak, bytes.len() >> 20);
    println!("encode streamed  peak {} MB while encoding (an entry at a time, same bytes)", streamed_peak);
    println!("decode           peak {} MB while decoding, {} MB after", decode_peak, after_decode.saturating_sub(after_drop) + (bytes.len() >> 20) as u64);
    println!("Tree on top      +{} MB", after_tree.saturating_sub(after_decode));
    let _ = after_encode;
}
