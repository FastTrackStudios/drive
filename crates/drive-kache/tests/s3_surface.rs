//! The endpoint answers every S3 call kache's remote makes, as kache's own
//! client (OpenDAL's S3 service) makes them.

mod common;

use drive_kache::Timeouts;
use opendal::ErrorKind;

#[tokio::test]
async fn every_call_kache_makes() {
    let t = tempfile::tempdir().unwrap();
    let a = common::agent(t.path(), "a", 1 << 30, Timeouts::default()).await;
    let op = common::s3_client(&a);
    let pack = "artifacts/v3/packs/serde/0123abcd.tar.zst";
    let manifest = "artifacts/v3/manifests/serde/0123abcd.json";

    // HEAD and GET of a missing key: absence, not an error.
    let e = op.stat(pack).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::NotFound, "{e}");
    let e = op.read(pack).await.unwrap_err();
    assert_eq!(e.kind(), ErrorKind::NotFound);

    // Create-only PUT (If-None-Match: *), then the same again: refused.
    let body: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    op.write_with(pack, body.clone())
        .if_not_exists(true)
        .await
        .unwrap();
    let e = op
        .write_with(pack, b"other".to_vec())
        .if_not_exists(true)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ConditionNotMatch);
    op.write_with(manifest, b"{\"version\":3}".to_vec())
        .content_type("application/json")
        .if_not_exists(true)
        .await
        .unwrap();

    // HEAD: size and a strong ETag; GET: the bytes.
    let meta = op.stat(pack).await.unwrap();
    assert_eq!(meta.content_length(), body.len() as u64);
    let tag = meta.etag().unwrap().to_string();
    assert!(tag.starts_with('"') && !tag.starts_with("W/"));
    assert_eq!(op.read(pack).await.unwrap().to_vec(), body);

    // A merged build manifest: read with its tag, replaced only if it
    // still has it (If-Match), a stale tag refused.
    let merged = "artifacts/_manifests/v3/ns/shards/ff.json";
    op.write_with(merged, b"[1]".to_vec())
        .if_not_exists(true)
        .await
        .unwrap();
    let t1 = op.stat(merged).await.unwrap().etag().unwrap().to_string();
    op.write_with(merged, b"[1,2]".to_vec())
        .if_match(&t1)
        .await
        .unwrap();
    let e = op
        .write_with(merged, b"[1,3]".to_vec())
        .if_match(&t1)
        .await
        .unwrap_err();
    assert_eq!(e.kind(), ErrorKind::ConditionNotMatch);
    assert_eq!(op.read(merged).await.unwrap().to_vec(), b"[1,2]");
    // A plain PUT (predictions) replaces.
    op.write(merged, b"[9]".to_vec()).await.unwrap();
    assert_eq!(op.read(merged).await.unwrap().to_vec(), b"[9]");

    // ListObjectsV2, recursive, over more than one page.
    for i in 0..2500 {
        op.write(
            &format!("artifacts/v3/manifests/many/{i:05}.json"),
            b"{}".to_vec(),
        )
        .await
        .unwrap();
    }
    use futures::TryStreamExt;
    let listed: Vec<String> = op
        .lister_with("artifacts/v3/manifests/")
        .recursive(true)
        .await
        .unwrap()
        .map_ok(|e| e.path().to_string())
        .try_collect()
        .await
        .unwrap();
    assert_eq!(listed.len(), 2501);
    assert!(listed.contains(&manifest.to_string()));
    // An empty prefix lists nothing that is not there.
    let none: Vec<opendal::Entry> = op
        .lister_with("nothing/")
        .recursive(true)
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    assert!(none.is_empty());
}

#[tokio::test]
async fn another_bucket_and_other_calls_are_refused() {
    let t = tempfile::tempdir().unwrap();
    let a = common::agent(t.path(), "a", 1 << 30, Timeouts::default()).await;
    let http = reqwest::Client::new();
    let r = http
        .get(format!("{}/other/key", a.url()))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    assert!(r.text().await.unwrap().contains("NoSuchBucket"));
    let r = http
        .delete(format!("{}/kache/key", a.url()))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 501);
    let r = http
        .post(format!("{}/kache/key?uploads", a.url()))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 501);
}

#[tokio::test]
async fn listens_on_loopback_only() {
    let t = tempfile::tempdir().unwrap();
    let a = common::agent(t.path(), "a", 1 << 30, Timeouts::default()).await;
    let e = drive_kache::s3::serve(a.kache.clone(), "0.0.0.0:0".parse().unwrap())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("loopback"));
}
