//! Parts in another org's locations, and who may read them (drive#52):
//! through the node's own guards, signed in with tokens the (unreached)
//! issuer is taken to have answered for.

use std::sync::Arc;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use drive_files::projects::{self, Lend};
use drive_files::space::{self, Location};
use tower::ServiceExt;

use crate::auth::{Auth, Principal};
use crate::rooms::Rooms;

struct World {
    rooms: Arc<Rooms>,
    app: Router,
    project: String,
    part: String,
}

fn location(id: &str, device: &str, path: &str) -> Location {
    Location {
        id: id.into(),
        name: id.into(),
        device: device.into(),
        path: path.into(),
        volume: String::new(),
        storage: String::new(),
        mirror_of: String::new(),
        index: String::new(),
        files: 0,
        bytes: 0,
        built: 0,
    }
}

/// A church's song shared with a band; its stems on a member's own
/// computer, in their personal org `cody`, which lends the folder.
async fn world(dir: &std::path::Path) -> World {
    let store = crate::store::Store::open(&dir.join("drive.db"))
        .await
        .unwrap();
    let rooms = Arc::new(Rooms::new(
        store,
        Auth::new(Some("http://127.0.0.1:9".into())),
    ));
    for (token, who, orgs) in [
        ("tb", "band member", vec!["band"]),
        ("tc", "church member", vec!["church"]),
        ("tm", "cody", vec!["cody", "church"]),
        ("ts", "stranger", vec!["elsewhere"]),
    ] {
        let p = Principal {
            who: who.into(),
            orgs: orgs.into_iter().map(str::to_string).collect(),
            all: false,
        };
        rooms.auth.remember(token, p).await;
    }
    let (project, part) = rooms
        .edit("church/drive:space", "t", |d| {
            space::add_location(d, &location("church-pc", "pc", "/srv/church")).unwrap();
            let p = projects::add_project(d, "Amazing Grace", "MusicRecording").unwrap();
            let x =
                projects::add_part_in(d, &p, "Stems", "cody", "laptop-music", "AG/Stems").unwrap();
            projects::add_part(d, &p, "Charts", "church-pc", "Charts").unwrap();
            projects::share_with_org(d, &p, "band", true).unwrap();
            (p, x)
        })
        .await;
    rooms
        .edit("cody/drive:space", "t", |d| {
            space::add_location(d, &location("laptop-music", "dev1", "/home/c/Music")).unwrap();
            space::add_location(d, &location("private", "dev1", "/home/c/Private")).unwrap();
            projects::lend(
                d,
                &Lend {
                    org: "church".into(),
                    project: project.clone(),
                    part: part.clone(),
                    location: "laptop-music".into(),
                    path: "AG/Stems".into(),
                },
            )
            .unwrap();
        })
        .await;
    let ok = || async { "ok" };
    let app = Router::new()
        .merge(
            Router::new()
                .route("/list", get(ok))
                .route("/search", get(ok).post(ok))
                .route("/fetch/{space}/{location}", get(ok))
                .layer(axum::middleware::from_fn_with_state(
                    rooms.clone(),
                    crate::guard,
                )),
        )
        .merge(Router::new().route("/sidecar/{key}/{kind}", get(ok)).layer(
            axum::middleware::from_fn_with_state(rooms.clone(), crate::guard_sidecars),
        ))
        .merge(
            Router::new()
                .route("/browse/{space}/{device}/file", get(ok))
                .route("/browse/{space}/{device}/list", get(ok))
                .layer(axum::middleware::from_fn_with_state(
                    rooms.clone(),
                    crate::browse::guard,
                )),
        );
    World {
        rooms,
        app,
        project,
        part,
    }
}

async fn status(app: &Router, token: &str, url: &str) -> StatusCode {
    let req = axum::http::Request::get(url)
        .header("authorization", format!("Bearer {token}"))
        .body(axum::body::Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

const STEM: &str = "/fetch/cody/laptop-music?path=AG/Stems/kick.wav";

#[tokio::test]
async fn the_shared_projects_people_read_exactly_its_parts() {
    let dir = tempfile::tempdir().unwrap();
    let w = world(dir.path()).await;
    let allowed = [
        // The band (the project is shared with it): the lent stems…
        ("tb", "/list?space=cody&location=laptop-music&path=AG/Stems"),
        (
            "tb",
            "/list?space=cody&location=laptop-music&path=AG%2FStems%2FTake%202",
        ),
        ("tb", STEM),
        (
            "tb",
            "/sidecar/abc/waveform?space=cody&location=laptop-music&path=AG/Stems/kick.wav",
        ),
        (
            "tb",
            "/browse/cody/dev1/file?root=/home/c&path=Music/AG/Stems/kick.wav",
        ),
        // …and the church's own part.
        ("tb", "/fetch/church/church-pc?path=Charts/a.pdf"),
        // The church's members (the project's org), not in cody.
        ("tc", STEM),
        // Cody: everything of their own org.
        ("tm", "/fetch/cody/private?path=diary.txt"),
        ("tm", "/search?space=cody"),
    ];
    for (t, url) in allowed {
        assert_eq!(status(&w.app, t, url).await, StatusCode::OK, "{t} {url}");
    }
}

#[tokio::test]
async fn nothing_else_of_either_org() {
    let dir = tempfile::tempdir().unwrap();
    let w = world(dir.path()).await;
    let denied = [
        // Above the part, beside it, out of it, another location.
        ("tb", "/list?space=cody&location=laptop-music&path=AG"),
        ("tb", "/list?space=cody&location=laptop-music"),
        ("tb", "/fetch/cody/laptop-music?path=AG/Stems2/x.wav"),
        ("tb", "/fetch/cody/laptop-music?path=AG/Stems/../../secret"),
        ("tb", "/fetch/cody/private?path=diary.txt"),
        ("tb", "/fetch/church/church-pc?path=Finance/budget.xlsx"),
        // Never the orgs' searches.
        ("tb", "/search?space=cody"),
        ("tb", "/search?space=church"),
        ("tc", "/search?space=cody"),
        // A device's disks: one file of the part, never a listing.
        (
            "tb",
            "/browse/cody/dev1/list?root=/home/c&path=Music/AG/Stems",
        ),
        (
            "tb",
            "/browse/cody/dev1/file?root=/home/c&path=Music/AG/Other.wav",
        ),
        (
            "tb",
            "/browse/cody/dev1/file?root=/home/c&path=Music/AG/Stems/../../Private/diary.txt",
        ),
        (
            "tb",
            "/sidecar/abc/waveform?space=cody&location=private&path=diary.txt",
        ),
        // Someone in none of the orgs: nothing.
        ("ts", STEM),
        ("ts", "/fetch/church/church-pc?path=Charts/a.pdf"),
        (
            "ts",
            "/browse/cody/dev1/file?root=/home/c&path=Music/AG/Stems/kick.wav",
        ),
    ];
    for (t, url) in denied {
        assert_eq!(
            status(&w.app, t, url).await,
            StatusCode::FORBIDDEN,
            "{t} {url}"
        );
    }
}

#[tokio::test]
async fn a_part_without_its_lend_is_not_readable() {
    let dir = tempfile::tempdir().unwrap();
    let w = world(dir.path()).await;
    // A church member points a part at another folder of cody's: cody
    // never lent it.
    let (p, x) = (w.project.clone(), w.part.clone());
    w.rooms
        .edit("church/drive:space", "t", |d| {
            projects::add_part_in(d, &p, "Diary", "cody", "private", "").unwrap();
        })
        .await;
    assert_eq!(
        status(&w.app, "tb", "/fetch/cody/private?path=diary.txt").await,
        StatusCode::FORBIDDEN
    );
    // Cody takes the stems back: the band loses them, the lend's own
    // members keep them.
    w.rooms
        .edit("cody/drive:space", "t", |d| {
            projects::unlend(d, "church", &p, &x).unwrap();
        })
        .await;
    assert_eq!(status(&w.app, "tb", STEM).await, StatusCode::FORBIDDEN);
    assert_eq!(status(&w.app, "tc", STEM).await, StatusCode::FORBIDDEN);
    assert_eq!(status(&w.app, "tm", STEM).await, StatusCode::OK);
}

#[tokio::test]
async fn only_an_orgs_members_change_its_registry() {
    let dir = tempfile::tempdir().unwrap();
    let w = world(dir.path()).await;
    let band = w.rooms.auth.principal(Some("tb"), false).await.unwrap();
    let church = w.rooms.auth.principal(Some("tc"), false).await.unwrap();
    // The band sees the church's registry (its shared project) but cannot
    // rewrite it, nor cody's (where the lends are).
    assert!(w.rooms.may(&band, "church/drive:space").await);
    assert!(!w.rooms.may_write(&band, "church/drive:space").await);
    assert!(!w.rooms.may_write(&band, "cody/drive:space").await);
    assert!(w.rooms.may_write(&church, "church/drive:space").await);
    assert!(!w.rooms.may_write(&church, "cody/drive:space").await);
}

#[tokio::test]
async fn a_share_link_offers_a_lent_part_and_only_while_it_is_lent() {
    use drive_files::index::{IndexEntry, LocationIndex};
    let dir = tempfile::tempdir().unwrap();
    let w = world(dir.path()).await;
    let file = |path: &str| IndexEntry {
        path: path.into(),
        dir: false,
        size: 10,
        mtime: 1,
        sampled: String::new(),
    };
    for (space, loc, files) in [
        (
            "cody",
            "laptop-music",
            vec!["AG/Stems/kick.wav", "AG/Other.wav"],
        ),
        ("church", "church-pc", vec!["Charts/a.pdf"]),
    ] {
        let index = LocationIndex {
            location: loc.into(),
            root: String::new(),
            built: 1,
            entries: files.into_iter().map(file).collect(),
        };
        w.rooms.store.put_index(space, &index).await.unwrap();
    }
    let token = "0b6f6a4e-5d1c-4c8e-9d7e-2a3b4c5d6e7f";
    let p = w.project.clone();
    w.rooms
        .edit("church/drive:space", "t", |d| {
            projects::add_share(d, token, &p, &[], "t").unwrap();
        })
        .await;
    let blobs = drive_files::storage::StorageConfig::Fs {
        root: dir.path().join("blobs").to_string_lossy().into_owned(),
    }
    .blobs()
    .unwrap();
    let catalog = crate::catalog::Catalog {
        rooms: w.rooms.clone(),
        blobs,
        names: Default::default(),
        reports: Default::default(),
    };
    let app = Router::new()
        .route("/s/{space}/{token}", get(crate::share::page))
        .with_state(catalog);
    let page = |app: Router| async move {
        let req = axum::http::Request::get(format!("/s/church/{token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        String::from_utf8_lossy(&body).into_owned()
    };
    let html = page(app.clone()).await;
    assert!(html.contains("kick.wav"), "the lent stems are offered");
    assert!(html.contains("a.pdf"), "and the church's own charts");
    assert!(!html.contains("Other.wav"), "nothing beside the part");

    let x = w.part.clone();
    w.rooms
        .edit("cody/drive:space", "t", |d| {
            projects::unlend(d, "church", &p, &x).unwrap();
        })
        .await;
    let html = page(app).await;
    assert!(!html.contains("kick.wav"), "taken back: no longer offered");
    assert!(html.contains("a.pdf"));
}

#[tokio::test]
async fn duplicate_or_mismatched_org_query_params_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let w = world(dir.path()).await;
    let bad = [
        ("tb", "/list?space=band&space=cody&location=private"),
        ("tb", "/search?space=band&space=church&q=a"),
        ("tb", "/list?org=band&space=cody&location=private"),
        ("tb", "/search?org=band&q=a"),
        ("tb", "/list?location=private"),
        ("tb", "/sidecar/abc/waveform?org=band&location=private&path=diary.txt"),
    ];
    for (t, url) in bad {
        assert_eq!(
            status(&w.app, t, url).await,
            StatusCode::BAD_REQUEST,
            "{t} {url}"
        );
    }
}

#[tokio::test]
async fn share_link_permissions_and_guest_name_dedup() {
    use drive_files::index::{IndexEntry, LocationIndex};
    let dir = tempfile::tempdir().unwrap();
    let w = world(dir.path()).await;
    w.rooms
        .store
        .put_index(
            "church",
            &LocationIndex {
                location: "church-pc".into(),
                root: String::new(),
                built: 1,
                entries: vec![IndexEntry {
                    path: "Charts/mix.wav".into(),
                    dir: false,
                    size: 42,
                    mtime: 1,
                    sampled: String::new(),
                }],
            },
        )
        .await
        .unwrap();

    let comment_tok = "1b6f6a4e-5d1c-4c8e-9d7e-2a3b4c5d6e7f";
    let view_tok = "2b6f6a4e-5d1c-4c8e-9d7e-2a3b4c5d6e7f";
    let p = w.project.clone();
    let charts_part = w
        .rooms
        .read("church/drive:space", |d| {
            projects::projects(d)
                .into_iter()
                .find(|pr| pr.id == p)
                .unwrap()
                .parts
                .into_iter()
                .find(|x| x.location == "church-pc")
                .unwrap()
                .id
        })
        .await;
    w.rooms
        .edit("church/drive:space", "t", |d| {
            projects::add_share_with(d, comment_tok, &p, &[], "t", false, true).unwrap();
            projects::add_share_with(d, view_tok, &p, &[], "t", false, false).unwrap();
        })
        .await;

    let blobs = drive_files::storage::StorageConfig::Fs {
        root: dir.path().join("blobs").to_string_lossy().into_owned(),
    }
    .blobs()
    .unwrap();
    let catalog = crate::catalog::Catalog {
        rooms: w.rooms.clone(),
        blobs,
        names: Default::default(),
        reports: Default::default(),
    };
    let app = Router::new()
        .route("/s/{space}/{token}", get(crate::share::page))
        .route("/s/{space}/{token}/file", get(crate::share::file))
        .route("/s/{space}/{token}/zip", get(crate::share::zip))
        .route(
            "/s/{space}/{token}/comments",
            get(crate::share::comments).post(crate::share::comment),
        )
        .with_state(catalog);

    // Comment-only link: no zip button, comment form shown even when computer is off.
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::get(format!("/s/church/{comment_tok}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let html = String::from_utf8_lossy(
        &axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .into_owned();
    assert!(!html.contains("/zip"), "no zip download on comment-only share");
    assert!(html.contains("<form>"), "comment form rendered");

    // Zip and non-inline file download are forbidden.
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::get(format!("/s/church/{comment_tok}/zip"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::get(format!(
                "/s/church/{comment_tok}/file?part={charts_part}&path=Charts/mix.wav"
            ))
            .body(axum::body::Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // Posting a comment as "Alice (guest)" strips trailing "(guest)" so it is not duplicated.
    let post_body = serde_json::json!({
        "part": charts_part,
        "path": "Charts/mix.wav",
        "name": "Alice (guest)",
        "body": "Love the chorus"
    })
    .to_string();
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::post(format!("/s/church/{comment_tok}/comments"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(post_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let res = app
        .clone()
        .oneshot(
            axum::http::Request::get(format!(
                "/s/church/{comment_tok}/comments?part={charts_part}&path=Charts/mix.wav"
            ))
            .body(axum::body::Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let list: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(list["threads"][0]["by"], "Alice (guest)");

    // View-only link (neither download nor comments): hides form and refuses comments API.
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::get(format!("/s/church/{view_tok}"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let html = String::from_utf8_lossy(
        &axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .into_owned();
    assert!(!html.contains("/zip"));
    assert!(!html.contains("<form>"));

    let res = app
        .oneshot(
            axum::http::Request::get(format!(
                "/s/church/{view_tok}/comments?part={charts_part}&path=Charts/mix.wav"
            ))
            .body(axum::body::Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_posted_search_is_guarded_as_the_get() {
    let dir = tempfile::tempdir().unwrap();
    let w = world(dir.path()).await;
    let post = |token: &'static str, url: &'static str| {
        let app = w.app.clone();
        async move {
            let req = axum::http::Request::post(url)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(r#"{"sampled":[]}"#))
                .unwrap();
            app.oneshot(req).await.unwrap().status()
        }
    };
    assert_eq!(post("tm", "/search?space=cody").await, StatusCode::OK);
    assert_eq!(
        post("tb", "/search?space=cody").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(post("tm", "/search").await, StatusCode::BAD_REQUEST);
}
