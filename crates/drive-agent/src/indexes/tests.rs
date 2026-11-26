use super::*;
use crate::adopt::tests::{device, node, songs, wait_until};

#[tokio::test(flavor = "multi_thread")]
async fn a_command_beside_the_running_agent_asks_it_and_opens_nothing() {
    let (url, _node_dir) = node().await;
    let (dir, disk) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = device(&url, dir.path(), "studio", "Studio PC").await;
    let apps = crate::apps::Apps::open(&a).unwrap();
    tokio::spawn(crate::apps::serve(apps));
    let sock = dir.path().join(drive_local::SOCKET);
    wait_until("the app socket", || sock.exists()).await;
    let loc = a.add_location("Songs", &songs(disk.path())).unwrap();
    a.index_location(&loc).await.unwrap();
    let db = a.index_db.db().await.unwrap().clone();

    // A `drive` command's handle, in the same folder.
    let cli = Indexes::new(dir.path());
    assert!(cli.asks_daemon().await);
    assert_eq!(
        cli.meta(&loc.id).await.unwrap(),
        db.meta(&loc.id).await.unwrap()
    );
    assert!(
        cli.meta(&loc.id)
            .await
            .unwrap()
            .is_some_and(|m| m.files == 3)
    );
    assert_eq!(
        cli.list(&loc.id, "").await.unwrap(),
        db.list(&loc.id, "").await.unwrap()
    );
    assert_eq!(
        cli.page_under(&loc.id, "Song A", "", 10).await.unwrap(),
        db.page_under(&loc.id, "Song A", "", 10).await.unwrap()
    );
    let kick = cli
        .get(&loc.id, "Song A/Audio/kick.wav")
        .await
        .unwrap()
        .expect("there");
    assert_eq!(kick.size, 120_000);
    assert_eq!(
        find_existing(dir.path(), "KICK", 10).await.unwrap(),
        Some(db.find("KICK", 10).await.unwrap())
    );
    assert_eq!(
        cli.copies(&kick.sampled, Some(kick.size)).await.unwrap(),
        db.copies(&kick.sampled, Some(kick.size)).await.unwrap()
    );
    let held = cli
        .locations_of(std::slice::from_ref(&kick.sampled))
        .await
        .unwrap();
    assert_eq!(held[&kick.sampled], std::slice::from_ref(&loc.id));
    assert!(cli.duplicates(1).await.unwrap().is_empty());
    let mut groups = 0;
    cli.duplicates_under(&loc.id, "", |_, _, _| groups += 1)
        .await
        .unwrap();
    assert_eq!(groups, 0);

    // Writes too: a walk by the agent, and forgetting the index.
    let songs_dir = std::path::PathBuf::from(&loc.path);
    std::fs::write(songs_dir.join("Song B/take 2.wav"), vec![1u8; 5000]).unwrap();
    let st = cli
        .reindex_by_daemon(&loc.id)
        .await
        .expect("asked")
        .unwrap();
    assert_eq!(st.files, 4);
    assert!(
        db.get(&loc.id, "Song B/take 2.wav")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        cli.reindex_by_daemon("no-such-location")
            .await
            .expect("asked")
            .is_err()
    );
    cli.remove(&loc.id).await.unwrap();
    assert!(db.meta(&loc.id).await.unwrap().is_none());

    assert!(
        cli.0.db.get().is_none(),
        "the command never opened the database"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn with_no_agent_running_a_command_opens_the_database_itself() {
    let dir = tempfile::tempdir().unwrap();
    // The socket a stopped agent left behind: nobody answers on it.
    drop(std::os::unix::net::UnixListener::bind(dir.path().join(drive_local::SOCKET)).unwrap());
    let cli = Indexes::new(dir.path());
    assert!(!cli.asks_daemon().await);
    assert!(cli.meta("loc").await.unwrap().is_none());
    assert!(cli.0.db.get().is_some());
    assert!(cli.path().exists());
}
