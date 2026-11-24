use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt;

use super::*;

fn local() -> Settings {
    Settings {
        relay: None,
        local_only: true,
    }
}

/// Bytes that differ everywhere (a ranged read in the wrong place shows).
fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

async fn provider(dir: &Path, allow: Allow) -> Provider {
    Provider::start(&dir.join("agent"), &local(), allow)
        .await
        .unwrap()
}

async fn fetcher(dir: &Path, name: &str) -> Arc<Fetcher> {
    Arc::new(
        Fetcher::start(&dir.join(name).join("iroh.key"), &local())
            .await
            .unwrap(),
    )
}

async fn collect(
    s: impl futures::Stream<Item = std::io::Result<bytes::Bytes>>,
) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut s = std::pin::pin!(s);
    while let Some(b) = s.next().await {
        out.extend_from_slice(&b?);
    }
    Ok(out)
}

fn allow_only(ids: Vec<EndpointId>) -> Allow {
    Arc::new(move |id| ids.contains(id))
}

/// A provider that lets in one fetcher, and that fetcher.
async fn pair(t: &tempfile::TempDir) -> (Provider, Arc<Fetcher>) {
    let f = fetcher(t.path(), "node").await;
    let p = provider(t.path(), allow_only(vec![f.id()])).await;
    (p, f)
}

fn source(p: &Provider, got: &Provided) -> Source {
    Source {
        addr: p.addr(),
        hash: got.hash.parse().unwrap(),
        size: got.size,
    }
}

#[tokio::test]
async fn ranges_are_exactly_the_files_bytes() {
    let t = tempfile::tempdir().unwrap();
    let (p, f) = pair(&t).await;
    let data = noise(5_000_123, 7);
    let file = t.path().join("take.wav");
    std::fs::write(&file, &data).unwrap();
    let got = p.provide(&file).await.unwrap();
    // The hash is the file's BLAKE3: the same name Drive's blobs have.
    assert_eq!(got.hash, Hash::new(&data).to_hex().to_string());
    assert_eq!(got.size, data.len() as u64);
    let src = source(&p, &got);
    let n = data.len() as u64;
    for (a, b) in [
        (0, 0),
        (0, 99),
        (16_383, 16_385),
        (1_000_000, 3_456_789),
        (n - 1, n - 1),
        (n - 20_000, n + 50),
        (0, n - 1),
    ] {
        let bytes = collect(f.stream(src.clone(), a, b)).await.unwrap();
        let b = b.min(n - 1);
        assert_eq!(bytes, &data[a as usize..=b as usize], "range {a}-{b}");
    }
    // Asked again: no second hashing (the same answer from the list).
    assert_eq!(p.provide(&file).await.unwrap(), got);
    p.shutdown().await;
}

#[tokio::test]
async fn a_changed_file_is_hashed_again() {
    let t = tempfile::tempdir().unwrap();
    let (p, f) = pair(&t).await;
    let file = t.path().join("mix.wav");
    std::fs::write(&file, noise(100_000, 1)).unwrap();
    let first = p.provide(&file).await.unwrap();
    let new = noise(120_000, 2);
    std::fs::write(&file, &new).unwrap();
    let second = p.provide(&file).await.unwrap();
    assert_ne!(first.hash, second.hash);
    let got = collect(f.stream(source(&p, &second), 0, u64::MAX))
        .await
        .unwrap();
    assert_eq!(got, new);
    p.shutdown().await;
}

/// A file let go and written again at the same size and time (a kache
/// object replaced within a millisecond) is hashed again, not answered
/// with the hash it had before, now or after a restart.
#[tokio::test]
async fn a_forgotten_file_is_hashed_again() {
    let t = tempfile::tempdir().unwrap();
    let (p, _f) = pair(&t).await;
    let file = t.path().join("obj");
    std::fs::write(&file, noise(50_000, 1)).unwrap();
    let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
    let first = p.provide(&file).await.unwrap();
    p.forget(&file).await;
    std::fs::write(&file, noise(50_000, 2)).unwrap();
    std::fs::File::open(&file)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    let want = hash_of(&noise(50_000, 2));
    let second = p.provide(&file).await.unwrap();
    assert_ne!(second.hash, first.hash);
    assert_eq!(second.hash, want);
    p.forget(&file).await;
    p.shutdown().await;
    let p = provider(t.path(), allow_only(Vec::new())).await;
    std::fs::write(&file, noise(50_000, 3)).unwrap();
    std::fs::File::open(&file)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    assert_eq!(p.provide(&file).await.unwrap().hash, hash_of(&noise(50_000, 3)));
    p.shutdown().await;
}

fn hash_of(b: &[u8]) -> String {
    iroh_blobs::Hash::new(b).to_hex().to_string()
}

#[tokio::test]
async fn strangers_are_refused() {
    let t = tempfile::tempdir().unwrap();
    let node = fetcher(t.path(), "node").await;
    let stranger = fetcher(t.path(), "stranger").await;
    let p = provider(t.path(), allow_only(vec![node.id()])).await;
    let file = t.path().join("a.wav");
    std::fs::write(&file, noise(50_000, 3)).unwrap();
    let got = p.provide(&file).await.unwrap();
    let src = source(&p, &got);
    assert!(collect(stranger.stream(src.clone(), 0, 99)).await.is_err());
    assert_eq!(collect(node.stream(src, 0, 99)).await.unwrap().len(), 100);
    p.shutdown().await;
}

#[tokio::test]
async fn a_dropped_connection_resumes_where_it_stopped() {
    let t = tempfile::tempdir().unwrap();
    let (p, f) = pair(&t).await;
    let data = noise(48 << 20, 9);
    let file = t.path().join("long.wav");
    std::fs::write(&file, &data).unwrap();
    let got = p.provide(&file).await.unwrap();
    let mut s = std::pin::pin!(f.stream(source(&p, &got), 1_000, data.len() as u64 - 1));
    let mut out = Vec::new();
    let mut dropped = 0;
    while let Some(b) = s.next().await {
        out.extend_from_slice(&b.unwrap());
        // Twice, at a quarter and at half way.
        if (dropped == 0 && out.len() > data.len() / 4)
            || (dropped == 1 && out.len() > data.len() / 2)
        {
            f.drop_connections();
            dropped += 1;
        }
    }
    assert_eq!(dropped, 2);
    assert_eq!(out.len(), data.len() - 1_000);
    assert!(out == data[1_000..], "the resumed bytes differ");
    p.shutdown().await;
}

fn snapshot(base: &Path) -> Vec<(PathBuf, u64, std::time::SystemTime, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![base.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut es: Vec<_> = std::fs::read_dir(&d).unwrap().flatten().collect();
        es.sort_by_key(|e| e.path());
        for e in es {
            let m = std::fs::symlink_metadata(e.path()).unwrap();
            let bytes = if m.is_file() {
                std::fs::read(e.path()).unwrap()
            } else {
                Vec::new()
            };
            out.push((e.path(), m.len(), m.modified().unwrap(), bytes));
            if m.is_dir() {
                stack.push(e.path());
            }
        }
    }
    out
}

/// Files made read-only (0444 in 0555 folders) are served whole and in
/// ranges, and afterwards every name, size, time and byte is the same.
#[cfg(unix)]
#[tokio::test]
async fn serving_changes_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let (p, f) = pair(&t).await;
    let lib = t.path().join("Library");
    std::fs::create_dir_all(lib.join("Kits")).unwrap();
    let files = [
        ("Kits/kick.wav", noise(10, 1)),
        ("Kits/snare.wav", noise(70_000, 2)),
        ("memo.wav", noise(3 << 20, 3)),
        ("empty.txt", Vec::new()),
    ];
    for (n, b) in &files {
        std::fs::write(lib.join(n), b).unwrap();
    }
    let before = snapshot(&lib);
    let mode = |file: u32, dir: u32| {
        for (n, _) in &files {
            std::fs::set_permissions(lib.join(n), std::fs::Permissions::from_mode(file)).unwrap();
        }
        std::fs::set_permissions(lib.join("Kits"), std::fs::Permissions::from_mode(dir)).unwrap();
        std::fs::set_permissions(&lib, std::fs::Permissions::from_mode(dir)).unwrap();
    };
    mode(0o444, 0o555);
    for (n, b) in &files {
        let got = p.provide(&lib.join(n)).await.unwrap();
        let src = source(&p, &got);
        let all = collect(f.stream(src.clone(), 0, u64::MAX)).await.unwrap();
        assert_eq!(&all, b);
        if b.len() > 100 {
            let part = collect(f.stream(src, 5, 99)).await.unwrap();
            assert_eq!(part, b[5..=99]);
        }
    }
    p.shutdown().await;
    mode(0o644, 0o755);
    assert_eq!(snapshot(&lib), before);
}

/// The provider's own source opens nothing for writing and changes no
/// file (outside these tests); what it remembers is `served.rs`'s, in the
/// agent's folder.
#[test]
fn provider_never_writes() {
    let src = include_str!("provide.rs");
    for bad in [
        "fs::write",
        "File::create",
        "OpenOptions",
        "remove_file",
        "remove_dir",
        "fs::rename",
        "set_permissions",
        "hard_link",
        "fs::copy",
        ".write(true)",
        "set_len",
        "write_all",
        "ImportMode::Copy",
        "export",
        "RequestMode::None",
    ] {
        assert!(!src.contains(bad), "provide.rs must not use {bad}");
    }
    // Pushes stay refused: the default events disable them.
    assert_eq!(
        iroh_blobs::provider::events::EventMask::DEFAULT.push,
        iroh_blobs::provider::events::RequestMode::Disabled
    );
}

#[test]
fn addresses_round_trip_as_text() {
    let id = iroh::SecretKey::generate().public();
    let a = EndpointAddr::new(id).with_ip_addr("192.168.1.4:5000".parse().unwrap());
    assert_eq!(addr_from_str(&addr_to_string(&a)).unwrap(), a);
    assert_eq!(addr_from_str(&id.to_string()).unwrap().id, id);
}

/// Dialing through n0's public relays only (direct addresses left out of
/// the address the fetcher dials; iroh may then find a direct path on its
/// own): needs the internet, so not in the default run.
/// `cargo test -p drive-transfer -- --ignored`.
#[tokio::test]
#[ignore]
async fn through_n0s_relay() {
    let t = tempfile::tempdir().unwrap();
    let n0 = Settings::default();
    let f = Arc::new(
        Fetcher::start(&t.path().join("node/iroh.key"), &n0)
            .await
            .unwrap(),
    );
    let id = f.id();
    let p = Provider::start(&t.path().join("agent"), &n0, Arc::new(move |x| *x == id))
        .await
        .unwrap();
    p.online(std::time::Duration::from_secs(10)).await;
    let data = noise(32 << 20, 5);
    let file = t.path().join("memo.wav");
    std::fs::write(&file, &data).unwrap();
    let got = p.provide(&file).await.unwrap();
    let full = p.addr();
    let relay_only = EndpointAddr::from_parts(
        full.id,
        full.addrs
            .iter()
            .filter(|a| matches!(a, iroh::TransportAddr::Relay(_)))
            .cloned(),
    );
    assert!(!relay_only.addrs.is_empty(), "no relay: {full:?}");
    let src = Source {
        addr: relay_only,
        hash: got.hash.parse().unwrap(),
        size: got.size,
    };
    let began = std::time::Instant::now();
    let mut s = std::pin::pin!(f.stream(src.clone(), 0, u64::MAX));
    let mut n = 0;
    let mut first = None;
    while let Some(b) = s.next().await {
        n += b.unwrap().len();
        first.get_or_insert(began.elapsed());
    }
    let total = began.elapsed();
    assert_eq!(n, data.len());
    tracing::info!(
        first_byte_ms = first.unwrap().as_millis() as u64,
        total_ms = total.as_millis() as u64,
        path = f.path_kind(&src.addr.id),
        "through a relay"
    );
    p.shutdown().await;
}

#[test]
fn the_endpoint_key_is_private_and_kept() {
    let t = tempfile::tempdir().unwrap();
    let f = t.path().join("keys/iroh.key");
    let k = secret_key(&f).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&f).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    assert_eq!(secret_key(&f).unwrap().to_bytes(), k.to_bytes());
}
