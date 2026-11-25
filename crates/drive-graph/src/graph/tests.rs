use super::*;
use crate::space::tests::{song_kind, tree};
use drive_files::projects::Part;
use drive_spine::tags::{self, NewTag};
use loro::LoroDoc;

/// The ids a fixture's things got.
pub(crate) struct Ids {
    pub scales: String,
    pub chords: String,
    pub circle: String,
    pub png: String,
    pub setlists: String,
    pub notes: String,
    pub music: String,
    pub theory: String,
    pub drums: String,
}

fn new_tag(d: &LoroDoc, name: &str, parents: &[String]) -> String {
    tags::create(
        d,
        NewTag {
            name,
            parents,
            ..NewTag::default()
        },
        "test",
        1,
    )
    .unwrap()
}

/// A small space: a vault of three notes and an image, a folder, three
/// tags in a hierarchy, a song document, a task, an event and a project.
pub(crate) fn fixture() -> (Graph, Ids) {
    let (items, extracts) = tree(&[
        ("Notes/", ""),
        ("Notes/.obsidian/", ""),
        (
            "Notes/Scales.md",
            "---\ntags: [theory]\nrelated: \"[[Chords]]\"\naliases: [Modes]\n---\n# Scales\nSee [[Circle#Fifths]], ![[circle.png]], [[Amazing Grace]] and [[Nowhere]].\n",
        ),
        (
            "Notes/Chords.md",
            "---\ntags: [music/drums, unknown-tag]\n---\nFrom [[Scales|scales]] and [[nowhere]].\n",
        ),
        ("Notes/Circle.md", "Back to [scales](Scales.md).\n"),
        ("Notes/circle.png", ""),
        ("Setlists/", ""),
    ]);
    let node = |p: &str| items.iter().find(|i| i.path == p).unwrap().node();
    let tree_id = |p: &str| items.iter().find(|i| i.path == p).unwrap().id.clone();

    let d = LoroDoc::new();
    let music = new_tag(&d, "Music", &[]);
    let theory = new_tag(&d, "Theory", std::slice::from_ref(&music));
    let drums = new_tag(&d, "Drums", std::slice::from_ref(&music));
    tags::tag(
        &d,
        &Target::File(tree_id("Notes/circle.png")),
        &drums,
        "t",
        1,
    )
    .unwrap();
    tags::tag(
        &d,
        &Target::Doc("studio:song:amazing-grace".into()),
        &theory,
        "t",
        1,
    )
    .unwrap();
    tags::tag(&d, &Target::Content("abc".into()), &music, "t", 1).unwrap();
    d.commit();
    let lib = Library::load(&d);

    let tasks = [Task {
        id: "t1".into(),
        title: "Learn the song".into(),
        done: false,
        app: "studio".into(),
        link: "/studio/song/amazing-grace".into(),
        by: "t".into(),
    }];
    let events = [Event {
        id: "e1".into(),
        title: "Sunday".into(),
        link: "/studio/song/amazing-grace?x=1".into(),
        ..Event::default()
    }];
    let projects = [Project {
        id: "p1".into(),
        name: "Easter".into(),
        kind: "Project".into(),
        created: 0,
        parts: vec![Part {
            id: "a".into(),
            location: TREE.into(),
            path: "/Setlists/".into(),
            ..Part::default()
        }],
        docs: vec!["studio:song:amazing-grace".into()],
        shared_with: vec![],
    }];
    let kinds = Kinds::from(vec![song_kind()]);
    let links = [drive_spine::links::Link {
        from: Target::Doc("season:episode:1".into()),
        to: Target::Doc("studio:song:amazing-grace".into()),
        kind: "uses".into(),
        by: "t".into(),
        at: 1,
    }];
    let g = build(&Inputs {
        tree: &items,
        extracts: &extracts,
        tags: &lib,
        tasks: &tasks,
        events: &events,
        projects: &projects,
        kinds: &kinds,
        docs: &[],
        links: &links,
    });
    let ids = Ids {
        scales: node("Notes/Scales.md"),
        chords: node("Notes/Chords.md"),
        circle: node("Notes/Circle.md"),
        png: node("Notes/circle.png"),
        setlists: node("Setlists"),
        notes: node("Notes"),
        music: tag_node(&music),
        theory: tag_node(&theory),
        drums: tag_node(&drums),
    };
    (g, ids)
}

/// The edges out of `src`, folder edges aside (`app_links_and_folders`).
fn edges_from<'a>(g: &'a Graph, src: &str) -> Vec<(&'a str, EdgeKind, &'a str)> {
    g.edges
        .iter()
        .filter(|e| e.src == src && e.kind != EdgeKind::In)
        .map(|e| (e.dst.as_str(), e.kind, e.anchor.as_str()))
        .collect()
}

#[test]
fn notes_link_embed_relate_and_tag() {
    let (g, ids) = fixture();
    let mut got = edges_from(&g, &ids.scales);
    got.sort();
    let mut want = vec![
        (ids.circle.as_str(), EdgeKind::Link, "Fifths"),
        (ids.png.as_str(), EdgeKind::Embed, ""),
        ("doc:studio:song:amazing-grace", EdgeKind::Link, ""),
        ("missing:nowhere", EdgeKind::Link, ""),
        (ids.chords.as_str(), EdgeKind::Related, ""),
        (ids.theory.as_str(), EdgeKind::FmTag, ""),
    ];
    want.sort();
    assert_eq!(got, want);
    let chords = edges_from(&g, &ids.chords);
    assert!(
        chords.contains(&(ids.drums.as_str(), EdgeKind::FmTag, "")),
        "music/drums by its last part"
    );
    assert!(chords.contains(&("label:unknown-tag", EdgeKind::FmTag, "")));
    assert!(
        chords.contains(&("missing:nowhere", EdgeKind::Link, "")),
        "one missing node for any case"
    );
    assert_eq!(
        edges_from(&g, &ids.circle),
        [(ids.scales.as_str(), EdgeKind::Link, "")]
    );
    assert_eq!(g.nodes["missing:nowhere"].kind, NodeKind::Missing);
    assert_eq!(g.nodes[&ids.scales].label, "Scales");
    assert_eq!(g.nodes[&ids.scales].path, "Notes/Scales.md");
    assert!(g.aliases.contains(&("modes".into(), ids.scales.clone())));
}

#[test]
fn tags_spine_and_projects() {
    let (g, ids) = fixture();
    assert_eq!(
        edges_from(&g, &ids.png),
        [(ids.drums.as_str(), EdgeKind::Tagged, "")]
    );
    assert_eq!(
        edges_from(&g, &ids.theory),
        [(ids.music.as_str(), EdgeKind::Parent, "")]
    );
    assert_eq!(
        edges_from(&g, &ids.drums),
        [(ids.music.as_str(), EdgeKind::Parent, "")]
    );
    assert_eq!(
        edges_from(&g, "doc:studio:song:amazing-grace"),
        [(ids.theory.as_str(), EdgeKind::Tagged, "")]
    );
    assert_eq!(g.nodes["content:abc"].kind, NodeKind::Content);
    assert_eq!(
        edges_from(&g, "task:t1"),
        [("doc:studio:song:amazing-grace", EdgeKind::Task, "")]
    );
    assert_eq!(
        edges_from(&g, "event:e1"),
        [("doc:studio:song:amazing-grace", EdgeKind::Event, "")]
    );
    let mut p = edges_from(&g, "project:p1");
    p.sort();
    assert_eq!(
        p,
        [
            ("doc:studio:song:amazing-grace", EdgeKind::Part, ""),
            (ids.setlists.as_str(), EdgeKind::Part, ""),
        ]
    );
}

#[test]
fn app_links_and_folders() {
    let (g, ids) = fixture();
    let ep: Vec<&Edge> = g
        .edges
        .iter()
        .filter(|e| e.src == "doc:season:episode:1")
        .collect();
    assert_eq!(ep.len(), 1);
    assert_eq!(
        (ep[0].dst.as_str(), ep[0].kind, ep[0].raw.as_str()),
        ("doc:studio:song:amazing-grace", EdgeKind::Linked, "uses")
    );
    assert!(
        g.edges.iter().any(|e| e.src == ids.scales
            && e.kind == EdgeKind::In
            && g.nodes[&e.dst].path == "Notes")
    );
    assert!(
        !g.edges
            .iter()
            .any(|e| e.kind == EdgeKind::In && e.src == ids.setlists),
        "top-level folders are in nothing"
    );
}

#[test]
fn a_note_whose_bytes_are_elsewhere_still_gets_its_backlinks() {
    let (items, mut extracts) = tree(&[("A.md", "[[B]]"), ("B.md", "[[A]]")]);
    let b = items[1].clone();
    extracts.remove(&b.hash);
    let lib = Library::default();
    let kinds = Kinds::from(vec![]);
    let g = build(&Inputs {
        tree: &items,
        extracts: &extracts,
        tags: &lib,
        tasks: &[],
        events: &[],
        projects: &[],
        kinds: &kinds,
        docs: &[],
        links: &[],
    });
    assert_eq!(g.edges.len(), 1);
    assert_eq!(g.edges[0].dst, b.node());
    assert_eq!(g.nodes[&b.node()].label, "B.md");
}

#[test]
fn spine_links_name_documents_by_kind() {
    let kinds = Kinds::from(vec![song_kind()]);
    assert_eq!(
        doc_of_link(&kinds, "/studio/song/x#y").as_deref(),
        Some("studio:song:x")
    );
    assert_eq!(doc_of_link(&kinds, "/studio/song/"), None);
    assert_eq!(doc_of_link(&kinds, "/studio/song/a/b"), None);
    assert_eq!(doc_of_link(&kinds, "/meals/plan/x"), None);
}
