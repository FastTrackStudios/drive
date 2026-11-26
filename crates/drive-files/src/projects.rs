//! Projects: one piece of work (a song, its music video, a sample library)
//! whose parts can be anywhere. Each part is a folder in some location on
//! some computer: the editor's machine has the footage, the mixing
//! engineer's the audio, and nobody needs the whole set. A project also
//! links app documents (Studio's song session, a lighting show).
//!
//! Some of a project should be reachable whatever is switched on: its
//! documents, its charts, the client mixes. A part marked available has
//! every file kept on the node by the device that has it (and so, when the
//! node's blobs live in S3, there); kinds an app marks available are kept
//! the same way wherever they are. [`available`] records what has been put
//! there, so the node can hand a file out while its computer is off.
//!
//! A part may be in another org's location: a song of a church whose
//! stems are on a member's own computers, registered in their personal
//! org. Such a part names that org (`space`), and that org lends the
//! folder to the project ([`lend`], written in its own registry, which
//! only its members can write): the node then lets whoever may see the
//! project read exactly the files of that part, and nothing else of the
//! lending org ([`Lend`], docs/files.md). Without the lend the part does
//! not resolve for anyone outside the lending org.
//!
//! Layout, in the space's registry: `projects`: id → { id, name, kind,
//! created, parts: part id → { id, label, space, location, path,
//! available }, docs: doc id → true }; `available`: location → path → {
//! hash, size, mtime }; `lent`: "org/project/part" → { org, project, part,
//! location, path }.

use loro::{LoroDoc, LoroMap, LoroValue, ValueOrContainer};
use serde::Serialize;

/// A piece of work.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    /// What it is, as a schema.org type: `MusicRecording`, `Movie`,
    /// `Collection` (a sample library), or plain `Project`.
    pub kind: String,
    pub created: i64,
    pub parts: Vec<Part>,
    /// App documents in it (`studio:song:amazing-grace`).
    pub docs: Vec<String>,
    /// Other orgs it is shared with (their slugs): their members see it,
    /// its parts and documents. It still belongs to its own org.
    pub shared_with: Vec<String>,
}

/// A folder of a project, on whichever computer (or storage) has it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Part {
    pub id: String,
    /// `Audio`, `Footage`, `Client mixes`.
    pub label: String,
    /// The org whose location it is; empty: the project's own.
    pub space: String,
    /// A location id of that org, or [`TREE`] for its Drive tree.
    pub location: String,
    /// Inside the location (`""`: all of it).
    pub path: String,
    /// Keep every file of it on the node, reachable when its computer is off.
    pub available: bool,
}

/// A file put on the node so it can be had from anywhere.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Available {
    pub hash: String,
    pub size: i64,
    pub mtime: i64,
}

fn s(m: &LoroMap, k: &str) -> String {
    match m.get(k) {
        Some(ValueOrContainer::Value(LoroValue::String(v))) => v.to_string(),
        _ => String::new(),
    }
}

fn n(m: &LoroMap, k: &str) -> i64 {
    match m.get(k) {
        Some(ValueOrContainer::Value(LoroValue::I64(v))) => v,
        Some(ValueOrContainer::Value(LoroValue::Double(v))) => v as i64,
        _ => 0,
    }
}

fn child(m: &LoroMap, k: &str) -> Option<LoroMap> {
    match m.get(k) {
        Some(ValueOrContainer::Container(c)) => c.into_map().ok(),
        _ => None,
    }
}

fn project_map(doc: &LoroDoc, id: &str) -> loro::LoroResult<LoroMap> {
    doc.get_map("projects").ensure_mergeable_map(id)
}

/// Start a project; returns its id.
pub fn add_project(doc: &LoroDoc, name: &str, kind: &str) -> loro::LoroResult<String> {
    let id = crate::space::new_id();
    let m = project_map(doc, &id)?;
    m.insert("id", id.as_str())?;
    m.insert("name", name)?;
    m.insert("kind", if kind.is_empty() { "Project" } else { kind })?;
    m.insert("created", crate::now_ms())?;
    Ok(id)
}

pub fn rename_project(doc: &LoroDoc, id: &str, name: &str) -> loro::LoroResult<()> {
    project_map(doc, id)?.insert("name", name)
}

pub fn remove_project(doc: &LoroDoc, id: &str) -> loro::LoroResult<()> {
    doc.get_map("projects").delete(id)
}

/// A part's location when it is a folder of the org's Drive tree (an
/// app's uploads linked to a project).
pub const TREE: &str = "drive";

/// Add a folder to a project; returns the part's id.
pub fn add_part(
    doc: &LoroDoc,
    project: &str,
    label: &str,
    location: &str,
    path: &str,
) -> loro::LoroResult<String> {
    add_part_in(doc, project, label, "", location, path)
}

/// Add a folder of another org's location (`space`; empty: the project's
/// own) to a project; returns the part's id. The other org lends it with
/// [`lend`] in its own registry, so the project's people can read it.
pub fn add_part_in(
    doc: &LoroDoc,
    project: &str,
    label: &str,
    space: &str,
    location: &str,
    path: &str,
) -> loro::LoroResult<String> {
    let id = crate::space::new_id();
    let m = project_map(doc, project)?
        .ensure_mergeable_map("parts")?
        .ensure_mergeable_map(&id)?;
    m.insert("id", id.as_str())?;
    m.insert("label", label)?;
    if !space.is_empty() {
        m.insert("space", space)?;
    }
    m.insert("location", location)?;
    m.insert("path", path.trim_matches('/'))?;
    m.insert("available", false)?;
    Ok(id)
}

pub fn set_part_available(
    doc: &LoroDoc,
    project: &str,
    part: &str,
    on: bool,
) -> loro::LoroResult<()> {
    project_map(doc, project)?
        .ensure_mergeable_map("parts")?
        .ensure_mergeable_map(part)?
        .insert("available", on)
}

pub fn remove_part(doc: &LoroDoc, project: &str, part: &str) -> loro::LoroResult<()> {
    let parts = project_map(doc, project)?.ensure_mergeable_map("parts")?;
    if parts.get(part).is_some() {
        parts.delete(part)?;
    }
    Ok(())
}

/// Put an app document in a project (or take it out).
pub fn link_doc(doc: &LoroDoc, project: &str, doc_id: &str, on: bool) -> loro::LoroResult<()> {
    let docs = project_map(doc, project)?.ensure_mergeable_map("docs")?;
    if on {
        docs.insert(doc_id, true)?;
    } else if docs.get(doc_id).is_some() {
        docs.delete(doc_id)?;
    }
    Ok(())
}

/// Share a project with another org (or stop).
pub fn share_with_org(doc: &LoroDoc, project: &str, org: &str, on: bool) -> loro::LoroResult<()> {
    let with = project_map(doc, project)?.ensure_mergeable_map("shared_with")?;
    if on {
        with.insert(org, true)?;
    } else if with.get(org).is_some() {
        with.delete(org)?;
    }
    Ok(())
}

pub fn projects(doc: &LoroDoc) -> Vec<Project> {
    let map = doc.get_map("projects");
    let mut out: Vec<Project> = map
        .keys()
        .filter_map(|k| child(&map, &k))
        .map(|m| {
            let mut parts: Vec<Part> = child(&m, "parts")
                .map(|p| {
                    p.keys()
                        .filter_map(|k| child(&p, &k))
                        .map(|x| Part {
                            id: s(&x, "id"),
                            label: s(&x, "label"),
                            space: s(&x, "space"),
                            location: s(&x, "location"),
                            path: s(&x, "path"),
                            available: matches!(
                                x.get("available"),
                                Some(ValueOrContainer::Value(LoroValue::Bool(true)))
                            ),
                        })
                        .filter(|x| !x.id.is_empty())
                        .collect()
                })
                .unwrap_or_default();
            parts.sort_by(|a, b| a.label.cmp(&b.label));
            let mut docs: Vec<String> = child(&m, "docs")
                .map(|d| d.keys().map(|k| k.to_string()).collect())
                .unwrap_or_default();
            docs.sort();
            let mut shared_with: Vec<String> = child(&m, "shared_with")
                .map(|d| d.keys().map(|k| k.to_string()).collect())
                .unwrap_or_default();
            shared_with.sort();
            Project {
                id: s(&m, "id"),
                name: s(&m, "name"),
                kind: s(&m, "kind"),
                created: n(&m, "created"),
                parts,
                docs,
                shared_with,
            }
        })
        .filter(|p| !p.id.is_empty())
        .collect();
    out.sort_by_key(|p| p.name.to_lowercase());
    out
}

/// Whether a file of a location is inside a part marked available.
pub fn in_available_part(projects: &[Project], location: &str, path: &str) -> bool {
    projects.iter().flat_map(|p| &p.parts).any(|part| {
        part.available
            && part.space.is_empty()
            && part.location == location
            && (part.path.is_empty()
                || path == part.path
                || path.starts_with(&format!("{}/", part.path)))
    })
}

/// Record that a location's file is on the node.
pub fn set_available(
    doc: &LoroDoc,
    location: &str,
    path: &str,
    a: Option<&Available>,
) -> loro::LoroResult<()> {
    let loc = doc.get_map("available").ensure_mergeable_map(location)?;
    match a {
        Some(a) => {
            let m = loc.ensure_mergeable_map(path)?;
            m.insert("hash", a.hash.as_str())?;
            m.insert("size", a.size)?;
            m.insert("mtime", a.mtime)?;
        }
        None => {
            if loc.get(path).is_some() {
                loc.delete(path)?;
            }
        }
    }
    Ok(())
}

/// What of a location is on the node: path → its record.
pub fn available(doc: &LoroDoc, location: &str) -> std::collections::HashMap<String, Available> {
    child(&doc.get_map("available"), location)
        .map(|loc| {
            loc.keys()
                .filter_map(|k| {
                    child(&loc, &k).map(|m| {
                        (
                            k.to_string(),
                            Available {
                                hash: s(&m, "hash"),
                                size: n(&m, "size"),
                                mtime: n(&m, "mtime"),
                            },
                        )
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Every file kept available, in every location: (location, path, record).
pub fn all_available(doc: &LoroDoc) -> Vec<(String, String, Available)> {
    let map = doc.get_map("available");
    let mut out = Vec::new();
    for loc in map.keys() {
        for (path, a) in available(doc, &loc) {
            out.push((loc.to_string(), path, a));
        }
    }
    out
}

/// A link that lets anyone holding it see and download a project's files
/// (all of them, or only some kinds: "all the audio"). The files come from
/// whichever computer has them and is on, through the node; nothing is
/// copied anywhere first.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Share {
    /// The secret in the link (a random UUID).
    pub token: String,
    pub project: String,
    /// Only these kinds (`audio`, and the kinds under it); empty: everything.
    pub kinds: Vec<String>,
    pub created: i64,
    /// Who made it (a device or person name), for the list of links.
    pub by: String,
    /// Whether the link allows downloading files and the project zip.
    pub download: bool,
    /// Whether the link allows reading and posting comments.
    pub comments: bool,
}

/// Make a share link for a project (the token comes from the caller, who
/// has a good random source: the browser's `crypto.randomUUID()`), allowing
/// both downloads and comments.
pub fn add_share(
    doc: &LoroDoc,
    token: &str,
    project: &str,
    kinds: &[String],
    by: &str,
) -> loro::LoroResult<()> {
    add_share_with(doc, token, project, kinds, by, true, true)
}

/// Make a share link with explicit `download` and `comments` permissions.
pub fn add_share_with(
    doc: &LoroDoc,
    token: &str,
    project: &str,
    kinds: &[String],
    by: &str,
    download: bool,
    comments: bool,
) -> loro::LoroResult<()> {
    let m = doc.get_map("shares").ensure_mergeable_map(token)?;
    m.insert("token", token)?;
    m.insert("project", project)?;
    m.insert("kinds", kinds.join(" "))?;
    m.insert("created", crate::now_ms())?;
    m.insert("by", by)?;
    m.insert("download", download)?;
    m.insert("comments", comments)?;
    Ok(())
}

pub fn remove_share(doc: &LoroDoc, token: &str) -> loro::LoroResult<()> {
    doc.get_map("shares").delete(token)
}

fn flag_or_true(m: &LoroMap, k: &str) -> bool {
    match m.get(k) {
        Some(ValueOrContainer::Value(LoroValue::Bool(b))) => b,
        _ => true,
    }
}

fn share_of(m: &LoroMap) -> Share {
    Share {
        token: s(m, "token"),
        project: s(m, "project"),
        kinds: s(m, "kinds")
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        created: n(m, "created"),
        by: s(m, "by"),
        download: flag_or_true(m, "download"),
        comments: flag_or_true(m, "comments"),
    }
}

/// The share with this token, if it exists (and its project still does).
pub fn share(doc: &LoroDoc, token: &str) -> Option<Share> {
    if token.len() < 16 {
        return None;
    }
    let sh = child(&doc.get_map("shares"), token)
        .map(|m| share_of(&m))
        .filter(|x| x.token == token)?;
    projects(doc)
        .iter()
        .any(|p| p.id == sh.project)
        .then_some(sh)
}

/// A project's share links, newest first.
pub fn shares(doc: &LoroDoc, project: &str) -> Vec<Share> {
    let map = doc.get_map("shares");
    let mut out: Vec<Share> = map
        .keys()
        .filter_map(|k| child(&map, &k))
        .map(|m| share_of(&m))
        .filter(|x| x.project == project)
        .collect();
    out.sort_by_key(|x| std::cmp::Reverse(x.created));
    out
}

/// A folder of this org's location lent to another org's project part
/// (kept in the lending org's registry; see the module's docs).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Lend {
    /// The project's org.
    pub org: String,
    pub project: String,
    pub part: String,
    /// The folder lent: a location of this org, and a path in it.
    pub location: String,
    pub path: String,
}

fn lend_key(org: &str, project: &str, part: &str) -> String {
    format!("{org}/{project}/{part}")
}

/// Lend a folder of this org (its registry, `doc`) to another org's
/// project part.
pub fn lend(doc: &LoroDoc, l: &Lend) -> loro::LoroResult<()> {
    let m = doc
        .get_map("lent")
        .ensure_mergeable_map(&lend_key(&l.org, &l.project, &l.part))?;
    m.insert("org", l.org.as_str())?;
    m.insert("project", l.project.as_str())?;
    m.insert("part", l.part.as_str())?;
    m.insert("location", l.location.as_str())?;
    m.insert("path", l.path.trim_matches('/'))?;
    Ok(())
}

/// Take a lend back: the part stops resolving outside this org.
pub fn unlend(doc: &LoroDoc, org: &str, project: &str, part: &str) -> loro::LoroResult<()> {
    let lent = doc.get_map("lent");
    let k = lend_key(org, project, part);
    if lent.get(&k).is_some() {
        lent.delete(&k)?;
    }
    Ok(())
}

/// Everything this org lends.
pub fn lends(doc: &LoroDoc) -> Vec<Lend> {
    let map = doc.get_map("lent");
    map.keys()
        .filter_map(|k| child(&map, &k))
        .map(|m| Lend {
            org: s(&m, "org"),
            project: s(&m, "project"),
            part: s(&m, "part"),
            location: s(&m, "location"),
            path: s(&m, "path"),
        })
        .filter(|l| !l.org.is_empty() && !l.part.is_empty())
        .collect()
}

impl Lend {
    /// Whether the part `part` of `project` in org `self.org` is the one
    /// this lends (`space` being the lending org): the same folder, both
    /// sides agreeing, so taking either back ends it.
    pub fn matches(&self, space: &str, project: &Project, part: &Part) -> bool {
        self.project == project.id
            && self.part == part.id
            && part.space == space
            && part.location == self.location
            && part.path == self.path
    }
}

impl Part {
    /// The org whose location this part is in, for a project of `org`.
    pub fn space_in<'a>(&'a self, org: &'a str) -> &'a str {
        if self.space.is_empty() {
            org
        } else {
            &self.space
        }
    }

    /// A folder of the org's Drive tree, not of a location.
    pub fn in_tree(&self) -> bool {
        self.location == TREE
    }

    /// Whether a path of its location is inside this part. A path with
    /// `.` or `..` in it, or a leading `/`, is never inside: it could name
    /// a place outside the part while starting with the part's path.
    pub fn contains(&self, path: &str) -> bool {
        if path.starts_with('/') || path.split('/').any(|s| s == "." || s == "..") {
            return false;
        }
        self.path.is_empty() || path == self.path || path.starts_with(&format!("{}/", self.path))
    }

    /// A path of its location, relative to the part (`Mixes/v3.wav`).
    pub fn relative<'a>(&self, path: &'a str) -> &'a str {
        if self.path.is_empty() {
            path
        } else {
            path.strip_prefix(&self.path)
                .unwrap_or(path)
                .trim_start_matches('/')
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_project_whose_parts_are_on_different_computers() {
        let d = LoroDoc::new();
        let p = add_project(&d, "Amazing Grace", "MusicRecording").unwrap();
        let audio = add_part(&d, &p, "Audio", "engineer-projects", "Amazing Grace").unwrap();
        let mixes = add_part(
            &d,
            &p,
            "Client mixes",
            "engineer-projects",
            "Amazing Grace/Mixes",
        )
        .unwrap();
        add_part(&d, &p, "Footage", "editor-footage", "AG video").unwrap();
        set_part_available(&d, &p, &mixes, true).unwrap();
        link_doc(&d, &p, "studio:song:amazing-grace", true).unwrap();
        d.commit();

        let all = projects(&d);
        assert_eq!(all.len(), 1);
        assert_eq!(
            all[0]
                .parts
                .iter()
                .map(|x| x.label.as_str())
                .collect::<Vec<_>>(),
            ["Audio", "Client mixes", "Footage"]
        );
        assert_eq!(all[0].docs, ["studio:song:amazing-grace"]);

        // Only the mixes are kept on the node.
        assert!(in_available_part(
            &all,
            "engineer-projects",
            "Amazing Grace/Mixes/v3.wav"
        ));
        assert!(!in_available_part(
            &all,
            "engineer-projects",
            "Amazing Grace/Kick.wav"
        ));
        assert!(!in_available_part(
            &all,
            "engineer-projects",
            "Amazing Grace/Mixes2/x.wav"
        ));
        assert!(!in_available_part(
            &all,
            "editor-footage",
            "AG video/A001.mov"
        ));

        remove_part(&d, &p, &audio).unwrap();
        assert_eq!(projects(&d)[0].parts.len(), 2);

        let a = Available {
            hash: "h".into(),
            size: 3,
            mtime: 9,
        };
        set_available(
            &d,
            "engineer-projects",
            "Amazing Grace/Mixes/v3.wav",
            Some(&a),
        )
        .unwrap();
        assert_eq!(
            available(&d, "engineer-projects").get("Amazing Grace/Mixes/v3.wav"),
            Some(&a)
        );
        set_available(&d, "engineer-projects", "Amazing Grace/Mixes/v3.wav", None).unwrap();
        assert!(available(&d, "engineer-projects").is_empty());
    }

    #[test]
    fn share_links() {
        let d = LoroDoc::new();
        let p = add_project(&d, "Song A", "MusicRecording").unwrap();
        let token = "0b6f6a4e-5d1c-4c8e-9d7e-2a3b4c5d6e7f";
        add_share(&d, token, &p, &["audio".to_string()], "THEBATTLESHIP").unwrap();
        assert_eq!(share(&d, token).unwrap().kinds, ["audio"]);
        assert!(share(&d, "short").is_none());
        assert!(share(&d, "0b6f6a4e-5d1c-4c8e-9d7e-000000000000").is_none());
        assert_eq!(shares(&d, &p).len(), 1);

        // A share dies with its project.
        remove_project(&d, &p).unwrap();
        assert!(share(&d, token).is_none());

        let part = Part {
            id: "x".into(),
            label: "Mixes".into(),
            space: String::new(),
            location: "l".into(),
            path: "Song A/Mixes".into(),
            available: false,
        };
        assert!(part.contains("Song A/Mixes/v3.wav"));
        assert!(!part.contains("Song A/Mixes2/v3.wav"));
        // `..` or `.` never makes a path outside the part count as in it.
        assert!(!part.contains("Song A/Mixes/../../secret.wav"));
        assert!(!part.contains("Song A/Mixes/./v3.wav"));
        assert!(!part.contains("Song A/Mixes/.."));
        let whole = Part {
            path: String::new(),
            ..part.clone()
        };
        assert!(whole.contains("Song A/v3.wav"));
        assert!(!whole.contains("../secret.wav"));
        assert!(!whole.contains("/etc/passwd"));
        assert_eq!(part.relative("Song A/Mixes/sub/v3.wav"), "sub/v3.wav");
    }

    #[test]
    fn share_link_permissions() {
        let d = LoroDoc::new();
        let p = add_project(&d, "Song B", "MusicRecording").unwrap();
        let full = "11111111-1111-4111-8111-111111111111";
        let comment_only = "22222222-2222-4222-8222-222222222222";
        let view_only = "33333333-3333-4333-8333-333333333333";
        add_share(&d, full, &p, &[], "t").unwrap();
        add_share_with(&d, comment_only, &p, &[], "t", false, true).unwrap();
        add_share_with(&d, view_only, &p, &[], "t", false, false).unwrap();

        let s_full = share(&d, full).unwrap();
        assert!(s_full.download && s_full.comments);
        let s_comment = share(&d, comment_only).unwrap();
        assert!(!s_comment.download && s_comment.comments);
        let s_view = share(&d, view_only).unwrap();
        assert!(!s_view.download && !s_view.comments);

        // Existing share records without download/comments keys default to both enabled.
        let legacy = "44444444-4444-4444-8444-444444444444";
        let m = d.get_map("shares").ensure_mergeable_map(legacy).unwrap();
        m.insert("token", legacy).unwrap();
        m.insert("project", p.as_str()).unwrap();
        let s_legacy = share(&d, legacy).unwrap();
        assert!(s_legacy.download && s_legacy.comments);
    }

    #[test]
    fn projects_shared_with_other_orgs() {
        let d = LoroDoc::new();
        let p = add_project(&d, "Recital", "MusicRecording").unwrap();
        share_with_org(&d, &p, "cbu", true).unwrap();
        share_with_org(&d, &p, "tombrooksmusic", true).unwrap();
        share_with_org(&d, &p, "cbu", false).unwrap();
        assert_eq!(projects(&d)[0].shared_with, ["tombrooksmusic"]);
    }

    #[test]
    fn a_part_in_another_orgs_location_and_its_lend() {
        // The church's song; its stems on a member's own computer.
        let church = LoroDoc::new();
        let p = add_project(&church, "Amazing Grace", "MusicRecording").unwrap();
        let part = add_part_in(&church, &p, "Stems", "cody", "laptop-music", "AG/Stems").unwrap();
        let pr = &projects(&church)[0];
        let x = &pr.parts[0];
        assert_eq!((x.space.as_str(), x.space_in("church")), ("cody", "cody"));
        // Not the church's own location: never kept available by the church.
        assert!(!in_available_part(
            &projects(&church),
            "laptop-music",
            "AG/Stems/a.wav"
        ));

        let cody = LoroDoc::new();
        let l = Lend {
            org: "church".into(),
            project: p.clone(),
            part: part.clone(),
            location: "laptop-music".into(),
            path: "/AG/Stems/".into(),
        };
        lend(&cody, &l).unwrap();
        let got = lends(&cody);
        assert_eq!(got.len(), 1);
        assert!(got[0].matches("cody", pr, x));
        assert!(!got[0].matches("someone-else", pr, x));
        // A part moved elsewhere no longer matches its lend.
        let mut moved = x.clone();
        moved.path = "AG".into();
        assert!(!got[0].matches("cody", pr, &moved));
        unlend(&cody, "church", &p, &part).unwrap();
        assert!(lends(&cody).is_empty());
    }
}
