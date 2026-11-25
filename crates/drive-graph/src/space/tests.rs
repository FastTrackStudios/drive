use super::*;
use crate::extract::{Form, extract};
use drive_spine::kinds::Kind;

pub(crate) fn song_kind() -> Kind {
    Kind {
        id: "studio.song".into(),
        name: "Song".into(),
        parent: "doc".into(),
        icon: "Audio".into(),
        app: "studio".into(),
        extensions: vec![],
        doc_prefix: "studio:song:".into(),
        open: "/studio/song/{id}".into(),
        available: false,
        max_available: 0,
    }
}

/// A tree from paths (a trailing `/` is a folder); each file's hash is
/// its path, and its text (for notes) is given.
pub(crate) fn tree(paths: &[(&str, &str)]) -> (Vec<TreeItem>, HashMap<String, Extract>) {
    let mut items = Vec::new();
    let mut extracts = HashMap::new();
    for (i, (p, text)) in paths.iter().enumerate() {
        let dir = p.ends_with('/');
        let path = p.trim_end_matches('/').to_string();
        let hash = if dir {
            String::new()
        } else {
            format!("h:{path}")
        };
        if is_markdown(&path) {
            extracts.insert(hash.clone(), extract(text));
        }
        items.push(TreeItem {
            id: format!("{i}@1"),
            path,
            dir,
            hash,
        });
    }
    (items, extracts)
}

fn link(form: Form, target: &str) -> Link {
    Link {
        form,
        target: target.into(),
        anchor: None,
        label: None,
        raw: String::new(),
    }
}

fn wiki(target: &str) -> Link {
    link(Form::Wiki, target)
}

fn node_of(items: &[TreeItem], path: &str) -> String {
    items
        .iter()
        .find(|i| i.path == path)
        .unwrap_or_else(|| panic!("no {path}"))
        .node()
}

fn space() -> (Vec<TreeItem>, Space) {
    let (items, extracts) = tree(&[
        ("Notes/", ""),
        ("Notes/.obsidian/", ""),
        ("Notes/.obsidian/app.json", ""),
        ("Notes/Scales.md", "---\naliases: [Modes]\n---\n"),
        ("Notes/Theory/", ""),
        ("Notes/Theory/Scales.md", "# Theory scales\n"),
        ("Notes/Theory/Here.md", ""),
        ("Notes/circle.png", ""),
        ("Wiki/", ""),
        ("Wiki/.obsidian/", ""),
        ("Wiki/Scales.md", ""),
        ("Wiki/Only In Wiki.md", "---\ntitle: Wiki Title\n---\n"),
        ("Loose.md", ""),
        ("Scales.md", ""),
        (".trash/Gone.md", ""),
    ]);
    let docs = [
        DocRef {
            id: "studio:song:amazing-grace".into(),
            title: "Amazing Grace".into(),
        },
        DocRef {
            id: "studio:song:x1".into(),
            title: "How Great".into(),
        },
    ];
    let kinds = Kinds::from(vec![song_kind()]);
    let s = Space::new(&items, &extracts, &docs, &kinds);
    (items, s)
}

#[test]
fn same_folder_then_the_vault() {
    let (items, s) = space();
    let here = "Notes/Theory/Here.md";
    assert_eq!(
        s.resolve(here, &wiki("Scales")),
        Resolved::Node(node_of(&items, "Notes/Theory/Scales.md")),
        "the note's own folder first"
    );
    assert_eq!(
        s.resolve("Notes/Scales.md", &wiki("Scales")),
        Resolved::Node(node_of(&items, "Notes/Scales.md"))
    );
    assert_eq!(
        s.resolve(here, &wiki("Modes")),
        Resolved::Node(node_of(&items, "Notes/Scales.md")),
        "an alias in the vault"
    );
    assert_eq!(
        s.resolve(here, &wiki("Theory scales")),
        Resolved::Node(node_of(&items, "Notes/Theory/Scales.md")),
        "a title"
    );
    assert_eq!(
        s.resolve(here, &link(Form::Embed, "circle.png")),
        Resolved::Node(node_of(&items, "Notes/circle.png"))
    );
    assert_eq!(
        s.resolve(here, &wiki("Theory")),
        Resolved::Node(node_of(&items, "Notes/Theory")),
        "a folder by name"
    );
}

#[test]
fn then_other_vaults_and_full_paths() {
    let (items, s) = space();
    assert_eq!(
        s.resolve("Notes/Theory/Here.md", &wiki("Wiki Title")),
        Resolved::Node(node_of(&items, "Wiki/Only In Wiki.md")),
        "another vault, by title"
    );
    assert_eq!(
        s.resolve("Notes/Theory/Here.md", &wiki("Wiki/Scales")),
        Resolved::Node(node_of(&items, "Wiki/Scales.md")),
        "a full Drive path"
    );
    assert_eq!(
        s.resolve("Wiki/Scales.md", &wiki("Scales")),
        Resolved::Node(node_of(&items, "Wiki/Scales.md")),
        "a vault's own note before the others'"
    );
    assert_eq!(
        s.resolve("Loose.md", &wiki("Scales")),
        Resolved::Node(node_of(&items, "Scales.md")),
        "outside any vault: the Drive is the vault"
    );
    assert_eq!(
        s.resolve("Loose.md", &wiki("Gone")),
        Resolved::Missing("missing:gone".into()),
        "hidden folders are not linked"
    );
}

#[test]
fn relative_markdown_links_are_paths() {
    let (items, s) = space();
    assert_eq!(
        s.resolve(
            "Notes/Theory/Here.md",
            &link(Form::Markdown, "../Scales.md")
        ),
        Resolved::Node(node_of(&items, "Notes/Scales.md"))
    );
    assert_eq!(
        s.resolve("Notes/Theory/Here.md", &link(Form::Image, "../circle.png")),
        Resolved::Node(node_of(&items, "Notes/circle.png"))
    );
    assert_eq!(
        s.resolve("Loose.md", &link(Form::Markdown, "Nope/Missing.md")),
        Resolved::Missing("missing:nope/missing.md".into())
    );
}

#[test]
fn then_app_documents() {
    let (_, s) = space();
    let from = "Loose.md";
    assert_eq!(
        s.resolve(from, &wiki("Amazing Grace")),
        Resolved::Node("doc:studio:song:amazing-grace".into()),
        "a kind's prefix and the slug"
    );
    assert_eq!(
        s.resolve(from, &wiki("how great")),
        Resolved::Node("doc:studio:song:x1".into()),
        "by title"
    );
    assert_eq!(
        s.resolve(from, &wiki("studio:song:unknown")),
        Resolved::Node("doc:studio:song:unknown".into()),
        "an app document by id, even one not seen yet"
    );
    assert_eq!(
        s.resolve(from, &wiki("Nowhere")),
        Resolved::Missing("missing:nowhere".into())
    );
}
