//! The provider's answers over a real agent and node (in one process).

use super::*;
use crate::adopt::tests::{device, node};

fn names(page: &FpPage) -> Vec<String> {
    page.items.iter().map(|i| i.name.clone()).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn folders_and_files_list_page_by_page_without_the_trash() {
    let (url, _node_dir) = node().await;
    let dir = tempfile::tempdir().unwrap();
    let a = device(&url, dir.path(), "mac", "Mac").await;
    let src = dir.path().join("take.wav");
    std::fs::write(&src, b"a take").unwrap();

    let songs = a.fp_create(ROOT, "Songs", true, None).await.unwrap();
    let take = a
        .fp_create(&songs.id, "take.wav", false, Some(&src))
        .await
        .unwrap();
    a.fp_create(ROOT, "notes.txt", false, None).await.unwrap();
    // Something deleted: the Trash appears in the tree, never here.
    let gone = a.fp_create(ROOT, "old.txt", false, None).await.unwrap();
    a.fp_delete(&gone.id).unwrap();

    assert_eq!(
        names(&a.fp_list(ROOT, None, None).unwrap()),
        ["Songs", "notes.txt"]
    );
    let first = a.fp_list(ROOT, None, Some(1)).unwrap();
    assert_eq!(names(&first), ["Songs"]);
    let rest = a.fp_list(ROOT, first.next.as_deref(), Some(1)).unwrap();
    assert_eq!(
        (names(&rest), rest.next),
        (vec!["notes.txt".to_string()], None)
    );

    assert_eq!(take.parent, songs.id);
    assert_eq!((take.size, take.dir), (6, false));
    assert_eq!(a.fp_item(&take.id).unwrap(), take);
    assert!(
        a.fp_item(&gone.id).is_err(),
        "what is in the Trash is not an item"
    );
    assert!(a.fp_item(ROOT).unwrap().dir);
    assert!(
        a.fp_list(&take.id, None, None).is_err(),
        "a file is not a folder"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn changes_since_an_anchor_are_what_was_added_renamed_and_deleted() {
    let (url, _node_dir) = node().await;
    let dir = tempfile::tempdir().unwrap();
    let a = device(&url, dir.path(), "mac", "Mac").await;
    let kept = a.fp_create(ROOT, "kept.txt", false, None).await.unwrap();
    let moved = a.fp_create(ROOT, "draft.txt", false, None).await.unwrap();
    let doomed = a.fp_create(ROOT, "doomed.txt", false, None).await.unwrap();

    let start = a.fp_anchor();
    let none = a.fp_changes(&start);
    assert!(none.updated.is_empty() && none.deleted.is_empty() && !none.expired);

    let new = a.fp_create(ROOT, "new.txt", false, None).await.unwrap();
    let folder = a.fp_create(ROOT, "Final", true, None).await.unwrap();
    a.fp_modify(&moved.id, Some("final.txt"), Some(&folder.id), None)
        .await
        .unwrap();
    a.fp_delete(&doomed.id).unwrap();

    let c = a.fp_changes(&start);
    let mut updated: Vec<String> = c.updated.iter().map(|i| i.name.clone()).collect();
    updated.sort();
    assert_eq!(updated, ["Final", "final.txt", "new.txt"]);
    assert_eq!(c.deleted, std::slice::from_ref(&doomed.id));
    assert!(
        !c.updated.iter().any(|i| i.id == kept.id),
        "unchanged is not sent"
    );
    let renamed = c.updated.iter().find(|i| i.id == moved.id).unwrap();
    assert_eq!(renamed.parent, folder.id);
    assert_ne!(renamed.meta, moved.meta);
    assert!(c.updated.iter().any(|i| i.id == new.id));

    // Caught up: nothing more.
    let again = a.fp_changes(&c.anchor);
    assert!(again.updated.is_empty() && again.deleted.is_empty());
    // An anchor this tree never had (another document's): start over.
    let other = LoroDoc::new();
    other.get_map("x").insert("k", 1).unwrap();
    other.commit();
    assert!(
        a.fp_changes(&hex(&other.oplog_frontiers().encode()))
            .expired
    );
    assert!(a.fp_changes("not hex").expired);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_is_fetched_into_the_askers_folder_and_writes_are_checked() {
    let (url, _node_dir) = node().await;
    let dir = tempfile::tempdir().unwrap();
    let a = device(&url, dir.path(), "mac", "Mac").await;
    let src = dir.path().join("v1.txt");
    std::fs::write(&src, b"first").unwrap();
    let doc = a
        .fp_create(ROOT, "lyrics.txt", false, Some(&src))
        .await
        .unwrap();
    let staging = dir.path().join("group/staging");

    let got = a.fp_fetch(&doc.id, &staging).await.unwrap();
    assert!(got.path.starts_with(&staging));
    assert_eq!(std::fs::read(&got.path).unwrap(), b"first");
    assert!(got.item.here);
    let empty = a.fp_create(ROOT, "empty.txt", false, None).await.unwrap();
    assert_eq!(
        std::fs::read(a.fp_fetch(&empty.id, &staging).await.unwrap().path).unwrap(),
        b""
    );

    std::fs::write(&src, b"second, longer").unwrap();
    let v2 = a.fp_modify(&doc.id, None, None, Some(&src)).await.unwrap();
    assert_ne!(v2.content, doc.content, "new bytes, new content version");
    assert_eq!(v2.meta, doc.meta, "same name and place");
    assert_eq!(v2.size, 14);

    let err = a
        .fp_create(ROOT, "lyrics.txt", false, None)
        .await
        .unwrap_err();
    assert!(err.starts_with("exists"), "{err}");
    assert!(a.fp_create(ROOT, "a/b", false, None).await.is_err());
    assert!(a.fp_create(ROOT, "Trash", true, None).await.is_err());
    let outer = a.fp_create(ROOT, "Outer", true, None).await.unwrap();
    let inner = a.fp_create(&outer.id, "Inner", true, None).await.unwrap();
    assert!(
        a.fp_modify(&outer.id, None, Some(&inner.id), None)
            .await
            .is_err(),
        "not into itself"
    );
    assert!(a.fp_delete(ROOT).is_err());
}
