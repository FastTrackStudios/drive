//! Two agents of one build group in one process: what one stores, the
//! other finds and fetches (verified) on a miss; a miss answers quickly,
//! with peers or without; a device outside the group gets nothing.

mod common;

use std::time::{Duration, Instant};

use drive_kache::{Peer, Timeouts};
use opendal::ErrorKind;

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

#[tokio::test]
async fn a_peers_object_is_fetched_on_a_miss() {
    let t = tempfile::tempdir().unwrap();
    let a = common::agent(t.path(), "a", 1 << 30, Timeouts::default()).await;
    let b = common::agent(t.path(), "b", 1 << 30, Timeouts::default()).await;
    common::group(&a, &b);
    let (sa, sb) = (common::s3_client(&a), common::s3_client(&b));
    let key = "artifacts/v3/packs/tiny/feed.tar.zst";
    let body = noise(3_000_000, 5);
    sa.write_with(key, body.clone())
        .if_not_exists(true)
        .await
        .unwrap();

    // B: HEAD finds it on A, GET fetches it, and B keeps it.
    let meta = sb.stat(key).await.unwrap();
    assert_eq!(meta.content_length(), body.len() as u64);
    assert_eq!(sb.read(key).await.unwrap().to_vec(), body);
    assert!(b.kache.store.get(key).is_some());
    // The same tag on both (the content's BLAKE3).
    assert_eq!(
        sa.stat(key).await.unwrap().etag(),
        sb.stat(key).await.unwrap().etag()
    );
    // B lists A's keys too.
    use futures::TryStreamExt;
    sa.write("artifacts/v3/manifests/tiny/feed.json", b"{}".to_vec())
        .await
        .unwrap();
    let listed: Vec<String> = sb
        .lister_with("artifacts/v3/manifests/")
        .recursive(true)
        .await
        .unwrap()
        .map_ok(|e| e.path().to_string())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(listed, ["artifacts/v3/manifests/tiny/feed.json"]);
}

#[tokio::test]
async fn a_corrupted_copy_is_not_served() {
    let t = tempfile::tempdir().unwrap();
    let a = common::agent(t.path(), "a", 1 << 30, Timeouts::default()).await;
    let b = common::agent(t.path(), "b", 1 << 30, Timeouts::default()).await;
    common::group(&a, &b);
    let sa = common::s3_client(&a);
    let key = "artifacts/v3/packs/tiny/bad.tar.zst";
    sa.write(key, noise(100_000, 9)).await.unwrap();
    // Flip a byte of A's kept file behind its back.
    let (path, _) = a.kache.store.get(key).unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[500] ^= 0xff;
    std::fs::write(&path, bytes).unwrap();
    let e = common::s3_client(&b).read(key).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::NotFound);
    assert!(b.kache.store.get(key).is_none());
    // A let its bad copy go.
    assert!(a.kache.store.get(key).is_none());
}

#[tokio::test]
async fn misses_answer_quickly() {
    let t = tempfile::tempdir().unwrap();
    let timeouts = Timeouts {
        lookup: Duration::from_millis(400),
        ..Timeouts::default()
    };
    let a = common::agent(t.path(), "a", 1 << 30, timeouts.clone()).await;
    let b = common::agent(t.path(), "b", 1 << 30, timeouts).await;
    common::group(&a, &b);
    let sb = common::s3_client(&b);
    // Warm: the connection to A made by one lookup first.
    let _ = sb.stat("artifacts/v3/manifests/x/warm.json").await;
    let mut worst = Duration::ZERO;
    for i in 0..20 {
        let started = Instant::now();
        let e = sb
            .stat(&format!("artifacts/v3/manifests/x/{i}.json"))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::NotFound);
        worst = worst.max(started.elapsed());
    }
    // A peer that answers: a miss is one round trip on loopback.
    eprintln!("miss, a peer answering: worst of 20 {worst:?}");
    assert!(worst < Duration::from_millis(100), "a miss took {worst:?}");

    // A peer that cannot be reached: the first miss waits for the lookup
    // timeout at most, the next ones not at all (it is skipped a while).
    let gone = iroh::SecretKey::generate().public();
    let mut addr = iroh::EndpointAddr::new(gone);
    addr = addr.with_ip_addr("127.0.0.1:9".parse().unwrap());
    b.group.lock().unwrap().push(Peer {
        id: gone,
        addr,
        name: "gone".into(),
    });
    let started = Instant::now();
    let e = sb
        .stat("artifacts/v3/manifests/x/a.json")
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::NotFound);
    let first = started.elapsed();
    assert!(
        first < Duration::from_millis(400 + 150),
        "first miss {first:?}"
    );
    let started = Instant::now();
    let _ = sb
        .stat("artifacts/v3/manifests/x/b.json")
        .await
        .unwrap_err();
    let next = started.elapsed();
    eprintln!("miss, a peer unreachable: first {first:?}, next {next:?}");
    assert!(next < Duration::from_millis(100), "next miss {next:?}");

    // Without any peer: a miss is local only.
    let c = common::agent(t.path(), "c", 1 << 30, Timeouts::default()).await;
    let started = Instant::now();
    let _ = common::s3_client(&c).stat("nope").await.unwrap_err();
    eprintln!("miss, no peers: {:?}", started.elapsed());
    assert!(started.elapsed() < Duration::from_millis(50));
}

#[tokio::test]
async fn devices_outside_the_group_get_nothing() {
    let t = tempfile::tempdir().unwrap();
    let a = common::agent(t.path(), "a", 1 << 30, Timeouts::default()).await;
    let b = common::agent(t.path(), "b", 1 << 30, Timeouts::default()).await;
    // B counts A as its group; A does not count B (another OS, say).
    b.group.lock().unwrap().push(a.peer());
    let key = "artifacts/v3/packs/tiny/private.tar.zst";
    common::s3_client(&a)
        .write(key, b"x".to_vec())
        .await
        .unwrap();
    let e = common::s3_client(&b).stat(key).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::NotFound);
}

#[tokio::test]
async fn a_fetched_object_counts_against_the_budget() {
    let t = tempfile::tempdir().unwrap();
    let a = common::agent(t.path(), "a", 1 << 30, Timeouts::default()).await;
    let b = common::agent(t.path(), "b", 250_000, Timeouts::default()).await;
    common::group(&a, &b);
    let (sa, sb) = (common::s3_client(&a), common::s3_client(&b));
    for i in 0..3 {
        sa.write(&format!("p/{i}"), noise(100_000, i + 1))
            .await
            .unwrap();
    }
    for i in 0..3 {
        assert_eq!(sb.read(&format!("p/{i}")).await.unwrap().len(), 100_000);
    }
    let (n, bytes) = b.kache.store.stats();
    assert!(bytes <= 250_000 && n == 2, "{n} objects, {bytes} bytes");
    // The newest stays; A still has everything.
    assert!(b.kache.store.get("p/2").is_some());
    assert_eq!(a.kache.store.stats().0, 3);
}
