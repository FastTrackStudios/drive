//! Removing a location, end to end in one process: a node and devices.
//! Every test checks that no file of the location's folder was touched
//! (same inode, same modified time, same bytes).

use drive_files::space;

use super::*;
use crate::adopt::tests::{device, node, snapshot, songs, wait_until};

async fn listed(a: &Agent, location: &str) -> usize {
    let (node, space_name) = {
        let c = a.cfg.lock().unwrap();
        (c.node_http(), c.space.clone())
    };
    let v: serde_json::Value = a
        .http
        .get(format!(
            "{node}/list?space={space_name}&location={location}&path="
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v["entries"].as_array().map_or(0, Vec::len)
}

async fn stats_locations(a: &Agent) -> serde_json::Value {
    let (node, space_name) = {
        let c = a.cfg.lock().unwrap();
        (c.node_http(), c.space.clone())
    };
    let v: serde_json::Value = a
        .http
        .get(format!("{node}/stats?space={space_name}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v["locations"].clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn rm_takes_the_location_out_of_the_registry_and_the_node_and_leaves_every_file() {
    let (url, _node_dir) = node().await;
    let (dir, disk) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = device(&url, dir.path(), "studio", "Studio PC").await;
    let songs = songs(disk.path());
    let before = snapshot(&songs);

    let loc = a.add_location("Songs", &songs).unwrap();
    a.index_location(&loc).await.unwrap();
    assert!(listed(&a, &loc.id).await > 0);
    assert!(stats_locations(&a).await.get(&loc.id).is_some());
    assert!(a.index_db.meta(&loc.id).await.unwrap().is_some());

    let r = a.remove_location("Songs", false).await.unwrap();
    assert_eq!(r.location.id, loc.id);
    assert!(r.node_dropped.is_ok());
    assert!(space::locations(&a.space).iter().all(|l| l.id != loc.id));
    assert_eq!(listed(&a, &loc.id).await, 0);
    assert!(stats_locations(&a).await.get(&loc.id).is_none());
    for _ in 0..100 {
        if a.index_db.meta(&loc.id).await.unwrap().is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(a.index_db.meta(&loc.id).await.unwrap().is_none());
    assert!(
        !a.dir
            .join("indexes")
            .join(format!("{}.idx", loc.id))
            .exists()
    );

    // Indexing it after (a pass that was under way) publishes nothing and
    // does not bring it back.
    assert!(a.index_location(&loc).await.is_err());
    assert!(space::locations(&a.space).iter().all(|l| l.id != loc.id));
    assert_eq!(listed(&a, &loc.id).await, 0);

    assert_eq!(snapshot(&songs), before, "the folder's files were touched");
    // Gone, so a second rm says so.
    assert!(a.remove_location("Songs", false).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn rm_is_refused_while_a_folder_is_adopted_from_the_location() {
    let (url, _node_dir) = node().await;
    let (dir, disk) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = device(&url, dir.path(), "studio", "Studio PC").await;
    let songs = songs(disk.path());
    let before = snapshot(&songs);

    let ad = a.adopt(&songs, "Studio/Songs").await.unwrap();
    let id = ad.location.id.clone();
    for force in [false, true] {
        let e = a.remove_location(&id, force).await.err().expect("refused");
        assert!(e.contains("Studio/Songs"), "{e}");
        assert!(e.contains("drive unadopt"), "{e}");
    }
    assert!(space::locations(&a.space).iter().any(|l| l.id == id));
    assert!(listed(&a, &id).await > 0);

    a.unadopt("Studio/Songs").unwrap();
    a.remove_location(&id, false).await.unwrap();
    assert!(space::locations(&a.space).iter().all(|l| l.id != id));
    assert_eq!(snapshot(&songs), before, "the folder's files were touched");
}

#[tokio::test(flavor = "multi_thread")]
async fn another_devices_location_needs_force() {
    let (url, _node_dir) = node().await;
    let (da, db, disk) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let studio = device(&url, da.path(), "studio", "Studio PC").await;
    let laptop = device(&url, db.path(), "laptop", "Laptop").await;
    let songs = songs(disk.path());
    let before = snapshot(&songs);
    let loc = studio.add_location("Songs", &songs).unwrap();
    studio.index_location(&loc).await.unwrap();
    wait_until("the laptop sees the location", || {
        space::locations(&laptop.space)
            .iter()
            .any(|l| l.id == loc.id)
    })
    .await;

    let e = laptop
        .remove_location("Songs", false)
        .await
        .err()
        .expect("refused");
    assert!(e.contains("--force") && e.contains("Studio PC"), "{e}");
    assert!(
        space::locations(&laptop.space)
            .iter()
            .any(|l| l.id == loc.id)
    );

    laptop.remove_location("Songs", true).await.unwrap();
    wait_until("the studio sees it gone", || {
        space::locations(&studio.space)
            .iter()
            .all(|l| l.id != loc.id)
    })
    .await;
    assert_eq!(listed(&laptop, &loc.id).await, 0);
    assert_eq!(snapshot(&songs), before, "the folder's files were touched");
}

#[test]
fn names_find_this_devices_location_first_and_ids_always() {
    let l = |id: &str, name: &str, device: &str| Location {
        id: id.into(),
        name: name.into(),
        device: device.into(),
        path: String::new(),
        volume: String::new(),
        storage: String::new(),
        mirror_of: String::new(),
        index: String::new(),
        files: 0,
        bytes: 0,
        built: 0,
    };
    let all = [
        l("1", "Songs", "a"),
        l("2", "Songs", "b"),
        l("3", "Other", "b"),
    ];
    assert_eq!(find(&all, "a", "Songs").unwrap().id, "1");
    assert_eq!(find(&all, "b", "Songs").unwrap().id, "2");
    assert!(find(&all, "c", "Songs").is_err());
    assert_eq!(find(&all, "c", "2").unwrap().id, "2");
    assert_eq!(find(&all, "c", "Other").unwrap().id, "3");
    assert!(find(&all, "c", "None").is_err());
}
