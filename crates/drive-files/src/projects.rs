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
//! Layout, in the space's registry: `projects`: id → { id, name, kind,
//! created, parts: part id → { id, label, location, path, available },
//! docs: doc id → true }; `available`: location → path → { hash, size,
//! mtime }.

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
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Part {
    pub id: String,
    /// `Audio`, `Footage`, `Client mixes`.
    pub label: String,
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

/// Add a folder to a project; returns the part's id.
pub fn add_part(
    doc: &LoroDoc,
    project: &str,
    label: &str,
    location: &str,
    path: &str,
) -> loro::LoroResult<String> {
    let id = crate::space::new_id();
    let m = project_map(doc, project)?
        .ensure_mergeable_map("parts")?
        .ensure_mergeable_map(&id)?;
    m.insert("id", id.as_str())?;
    m.insert("label", label)?;
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
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

/// Whether a file of a location is inside a part marked available.
pub fn in_available_part(projects: &[Project], location: &str, path: &str) -> bool {
    projects.iter().flat_map(|p| &p.parts).any(|part| {
        part.available
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
}

/// Make a share link for a project (the token comes from the caller, who
/// has a good random source: the browser's `crypto.randomUUID()`).
pub fn add_share(
    doc: &LoroDoc,
    token: &str,
    project: &str,
    kinds: &[String],
    by: &str,
) -> loro::LoroResult<()> {
    let m = doc.get_map("shares").ensure_mergeable_map(token)?;
    m.insert("token", token)?;
    m.insert("project", project)?;
    m.insert("kinds", kinds.join(" "))?;
    m.insert("created", crate::now_ms())?;
    m.insert("by", by)?;
    Ok(())
}

pub fn remove_share(doc: &LoroDoc, token: &str) -> loro::LoroResult<()> {
    doc.get_map("shares").delete(token)
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
    out.sort_by(|a, b| b.created.cmp(&a.created));
    out
}

impl Part {
    /// Whether a path of its location is inside this part.
    pub fn contains(&self, path: &str) -> bool {
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
            location: "l".into(),
            path: "Song A/Mixes".into(),
            available: false,
        };
        assert!(part.contains("Song A/Mixes/v3.wav"));
        assert!(!part.contains("Song A/Mixes2/v3.wav"));
        assert_eq!(part.relative("Song A/Mixes/sub/v3.wav"), "sub/v3.wav");
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
}
