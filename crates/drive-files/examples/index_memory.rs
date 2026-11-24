//! How much memory a big location index takes, step by step (drive#47):
//! `cargo run --release -p drive-files --example index_memory -- 2500000`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use drive_files::index::{self, IndexEntry, LocationIndex, Tree};

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

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(2_500_000);
    let base = rss_mb();
    let mut entries = Vec::with_capacity(n + n / 40);
    // A sample library: vendor / pack / category / 40 files a folder.
    let mut i = 0usize;
    'outer: for v in 0.. {
        for p in 0..50 {
            for c in 0..25 {
                let dir = format!("Vendor {v:03}/Pack {p:02} - Something Long/Category {c:02}");
                entries.push(IndexEntry { path: dir.clone(), dir: true, size: 0, mtime: 0, sampled: String::new() });
                for f in 0..40 {
                    entries.push(IndexEntry {
                        path: format!("{dir}/Sample_{v}_{p}_{c}_{f:02} Kick Hard.wav"),
                        dir: false,
                        size: 1_000_000 + i as u64,
                        mtime: 1_700_000_000_000 + i as i64,
                        sampled: format!("{:064x}", i as u128 * 0x9e37_79b9_7f4a_7c15),
                    });
                    i += 1;
                    if i >= n {
                        break 'outer;
                    }
                }
            }
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let idx = LocationIndex { location: "loc".into(), root: "/x".into(), built: 0, entries };
    let after_index = rss_mb();
    reset_peak();
    let bytes = index::encode(&idx);
    let encode_peak = peak_mb();
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
    println!("decode           peak {} MB while decoding, {} MB after", decode_peak, after_decode.saturating_sub(after_drop) + (bytes.len() >> 20) as u64);
    println!("Tree on top      +{} MB", after_tree.saturating_sub(after_decode));
    let _ = after_encode;
}
