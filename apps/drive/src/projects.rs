//! Projects in the browser: a project's parts (folders on whichever
//! computer has them; nobody needs the whole set), its app documents, and
//! which parts are kept available on the node.

use dioxus::prelude::*;
use drive_dioxus::{Doc, use_doc, use_docs};
use drive_files::projects as proj;
use drive_files::{space, space_doc};
use drive_spine as spine;
use drive_spine::kinds::Kinds;

use crate::browser::{Item, Items, Place, Src, app_label, icon, online, project_icon, use_nav};

/// The schema.org types a project can be.
const KINDS: [(&str, &str); 5] = [
    ("Project", "Project"),
    ("MusicRecording", "Song"),
    ("MusicAlbum", "Album"),
    ("Movie", "Video"),
    ("Collection", "Sample library"),
];

#[component]
pub fn ProjectView(space: String, id: String) -> Element {
    let nav = use_nav();
    let reg = use_doc(space_doc(&space));
    // Parts in the Drive tree (an app's uploads) open at their folder.
    let tree_doc = use_doc(drive_files::files_doc(&space));
    let spine_doc = use_doc(spine::spine_doc(&space));
    let docs = use_docs(format!("{space}/"));
    let r = reg.loro();
    let Some(p) = proj::projects(&r).into_iter().find(|p| p.id == id) else {
        return rsx! {
            div { class: "pane",
                p { class: "empty", "This project is gone." }
            }
        };
    };
    let locations = space::locations(&r);
    let devices = space::devices(&r);
    let storages = space::storages(&r);
    let up = online(&reg);
    let kinds = Kinds::load(&spine_doc.loro());
    let doc_items: Vec<Item> = p
        .docs
        .iter()
        .map(|d| {
            let kind = kinds.of_doc(d).clone();
            let info = docs().into_iter().find(|x| &x.id == d);
            Item {
                name: if kind.doc_prefix.is_empty() {
                    d.clone()
                } else {
                    d.trim_start_matches(&kind.doc_prefix).replace('-', " ")
                },
                dir: false,
                size: info.as_ref().map_or(0, |i| i.bytes),
                mtime: 0,
                at: app_label(&kind.app),
                online: true,
                copies: 1,
                kept: None,
                available: None,
                src: Src::Doc {
                    id: d.clone(),
                    peers: info.map_or(0, |i| i.peers),
                },
                kind,
            }
        })
        .collect();
    let holders: Vec<String> = {
        let mut h: Vec<String> = p
            .parts
            .iter()
            .filter_map(|part| locations.iter().find(|l| l.id == part.location))
            .filter_map(|l| {
                devices
                    .iter()
                    .find(|d| d.id == l.device)
                    .map(|d| d.name.clone())
            })
            .collect();
        h.sort();
        h.dedup();
        h
    };
    let pid = p.id.clone();
    rsx! {
        div { class: "pane wide",
            div { class: "projhead",
                img { src: icon(project_icon(&p.kind)), alt: "" }
                div { class: "pmain",
                    input {
                        class: "pname",
                        value: "{p.name}",
                        onchange: {
                            let reg = reg.clone();
                            let pid = pid.clone();
                            move |e: FormEvent| {
                                let v = e.value();
                                if !v.trim().is_empty() {
                                    reg.edit(|d| {
                                        let _ = proj::rename_project(d, &pid, v.trim());
                                    });
                                }
                            }
                        },
                    }
                    span { class: "s1",
                        "{p.parts.len()} parts on {holders.len()} computers · {p.docs.len()} documents"
                    }
                }
                select {
                    class: "sortpick",
                    onchange: {
                        let reg = reg.clone();
                        let pid = pid.clone();
                        move |e: FormEvent| {
                            let v = e.value();
                            reg.edit(|d| {
                                if let Ok(m) = d.get_map("projects").ensure_mergeable_map(&pid) {
                                    let _ = m.insert("kind", v.as_str());
                                }
                            });
                        }
                    },
                    for (k , label) in KINDS {
                        option { key: "{k}", value: "{k}", selected: p.kind == k, "{label}" }
                    }
                }
            }
            h2 { class: "sec", "Tags" }
            crate::tags::TargetTags {
                key: "{pid}",
                space: space.clone(),
                target: drive_spine::tags::Target::Project(pid.clone()),
                note: None,
            }
            h2 { class: "sec", "Parts" }
            if p.parts.is_empty() {
                p { class: "empty",
                    "No parts yet. Open a folder under a computer (or a storage) and choose “Add to project”: the audio on the engineer's computer, the footage on the editor's."
                }
            }
            div { class: "parts",
                for part in p.parts.iter() {
                    {
                        // A part in another org's location (lent to this
                        // project) names that org; one in the Drive tree, the tree.
                        let foreign = part.space_in(&space) != space;
                        let l = if foreign || part.in_tree() {
                            None
                        } else {
                            locations.iter().find(|l| l.id == part.location)
                        };
                        let owner = if part.in_tree() {
                            "Drive".to_string()
                        } else if foreign {
                            format!("{}'s computers", part.space)
                        } else {
                            l.map(|l| {
                                    if l.storage.is_empty() {
                                        devices.iter().find(|d| d.id == l.device).map_or(String::new(), |d| d.name.clone())
                                    } else {
                                        storages.iter().find(|s| s.id == l.storage).map_or(String::new(), |s| s.name.clone())
                                    }
                                })
                                .unwrap_or_else(|| "a location that is gone".into())
                        };
                        let lname = l.map_or(String::new(), |l| l.name.clone());
                        let live = l.is_some_and(|l| up.contains(&l.device));
                        let go = if part.in_tree() {
                            Place::Drive(
                                drive_files::tree::resolve(&tree_doc.peek(), &part.path)
                                    .flatten()
                                    .map(|e| drive_files::tree::id_string(e.id)),
                            )
                        } else if foreign {
                            Place::Overview
                        } else {
                            Place::Location { loc: part.location.clone(), path: part.path.clone() }
                        };
                        let (reg_a, reg_r) = (reg.clone(), reg.clone());
                        let (pa, pr) = (pid.clone(), pid.clone());
                        let (ia, ir) = (part.id.clone(), part.id.clone());
                        rsx! {
                            div { key: "{part.id}", class: "part",
                                button { class: "partmain", onclick: move |_| nav.go(go.clone()),
                                    img { src: icon("Folder"), alt: "" }
                                    div { class: "pmain",
                                        b { "{part.label}" }
                                        span { class: "s1", "{owner} · {lname} /{part.path}" }
                                    }
                                    span { class: if live { "dot on" } else { "dot" }, title: if live { "online" } else { "offline" } }
                                }
                                label { class: "pin", title: "Keep every file of this part on the node, reachable and shareable while its computer is off",
                                    input {
                                        r#type: "checkbox",
                                        checked: part.available,
                                        onchange: move |e: FormEvent| {
                                            let on = e.checked();
                                            reg_a.edit(|d| {
                                                let _ = proj::set_part_available(d, &pa, &ia, on);
                                            });
                                        },
                                    }
                                    span { "Available anywhere" }
                                }
                                button {
                                    class: "x",
                                    title: "Take this part out of the project (its files stay where they are)",
                                    onclick: move |_| {
                                        reg_r.edit(|d| {
                                            let _ = proj::remove_part(d, &pr, &ir);
                                        });
                                    },
                                    "✕"
                                }
                            }
                        }
                    }
                }
            }
            p { class: "hint",
                "Parts stay on the computers that have them. A part marked available is copied to the node by its computer, so the client mixes can be shared while it is off; charts and documents are kept available wherever they are."
            }
            h2 { class: "sec", "Comments" }
            drive_dioxus::comments::CommentsPanel {
                target: drive_spine::comments::Target::Project(pid.clone()),
                space: space.clone(),
            }
            h2 { class: "sec", "Shared with orgs" }
            SharedWith { reg: reg.clone(), owner: space.clone(), project: pid.clone(), with: p.shared_with.clone() }
            h2 { class: "sec", "Share" }
            ShareLinks { reg: reg.clone(), space: space.clone(), project: pid.clone() }
            h2 { class: "sec", "Documents" }
            if doc_items.is_empty() {
                p { class: "empty", "No documents yet. Pick a song in Apps › Studio and tick this project." }
            } else {
                Items { items: doc_items, show_at: true, empty: "", loading: false }
            }
            div { class: "danger",
                button {
                    class: "primary ghost",
                    onclick: {
                        let reg = reg.clone();
                        let pid = pid.clone();
                        move |_| {
                            reg.edit(|d| {
                                let _ = proj::remove_project(d, &pid);
                            });
                            nav.go(Place::Drive(None));
                        }
                    },
                    "Delete project"
                }
                span { class: "hint", "Only the project goes; every file stays where it is." }
            }
        }
    }
}

/// "Add to project" for the folder being browsed.
#[component]
pub fn AddToProject(reg: Doc, location: String, path: String, label: String) -> Element {
    let r = reg.loro();
    let projects = proj::projects(&r);
    let mut note = use_signal(String::new);
    let inside: Vec<String> = projects
        .iter()
        .filter(|p| {
            p.parts
                .iter()
                .any(|x| x.location == location && x.path == path.trim_matches('/'))
        })
        .map(|p| p.name.clone())
        .collect();
    if projects.is_empty() {
        return rsx! {};
    }
    rsx! {
        if !inside.is_empty() {
            span { class: "chip ok", "In {inside.join(\", \")}" }
        }
        select {
            class: "sortpick",
            value: "",
            onchange: move |e: FormEvent| {
                let pid = e.value();
                if pid.is_empty() {
                    return;
                }
                let name = projects.iter().find(|p| p.id == pid).map(|p| p.name.clone()).unwrap_or_default();
                reg.edit(|d| {
                    let _ = proj::add_part(d, &pid, &label, &location, &path);
                });
                note.set(format!("Added to {name}"));
            },
            option { value: "", selected: true, "Add to project…" }
            for p in projects.iter() {
                option { key: "{p.id}", value: "{p.id}", "{p.name}" }
            }
        }
        if !note().is_empty() {
            span { class: "count", "{note}" }
        }
    }
}

/// Which projects an app document is in, in the inspector.
#[component]
pub fn DocProjects(reg: Doc, doc_id: String) -> Element {
    let r = reg.loro();
    let projects = proj::projects(&r);
    if projects.is_empty() {
        return rsx! {};
    }
    rsx! {
        section { class: "isec",
            h4 { "Projects" }
            for p in projects {
                {
                    let on = p.docs.contains(&doc_id);
                    let (reg, pid, did) = (reg.clone(), p.id.clone(), doc_id.clone());
                    rsx! {
                        label { key: "{p.id}", class: "pin",
                            input {
                                r#type: "checkbox",
                                checked: on,
                                onchange: move |e: FormEvent| {
                                    let v = e.checked();
                                    reg.edit(|d| {
                                        let _ = proj::link_doc(d, &pid, &did, v);
                                    });
                                },
                            }
                            img { class: "sicon", src: icon(project_icon(&p.kind)), alt: "" }
                            span { "{p.name}" }
                        }
                    }
                }
            }
        }
    }
}

/// A project's share links: anyone holding one sees its files and can
/// download and/or comment on them, according to the link's permissions.
#[component]
fn ShareLinks(reg: Doc, space: String, project: String) -> Element {
    let ctx = drive_dioxus::use_drive();
    let r = reg.loro();
    let links = proj::shares(&r, &project);
    let origin = crate::host::origin();
    let mut allow_download = use_signal(|| true);
    let mut allow_comments = use_signal(|| true);
    let make = {
        let (reg, project) = (reg.clone(), project.clone());
        move |kinds: Vec<String>| {
            let token = crate::host::uuid();
            if token.len() < 16 {
                return;
            }
            let by = ctx.me.peek().name.clone();
            let dl = allow_download();
            let cm = allow_comments();
            reg.edit(|d| {
                let _ = proj::add_share_with(d, &token, &project, &kinds, &by, dl, cm);
            });
        }
    };
    let (all, audio) = (make.clone(), make);
    rsx! {
        div { class: "actions sharebar",
            label { class: "pin",
                input {
                    r#type: "checkbox",
                    checked: allow_download(),
                    onchange: move |e: FormEvent| allow_download.set(e.checked()),
                }
                span { "Downloads" }
            }
            label { class: "pin",
                input {
                    r#type: "checkbox",
                    checked: allow_comments(),
                    onchange: move |e: FormEvent| allow_comments.set(e.checked()),
                }
                span { "Comments" }
            }
            button { class: "primary ghost", onclick: move |_| all(Vec::new()), "Share all files" }
            button { class: "primary ghost", onclick: move |_| audio(vec!["audio".to_string()]), "Share the audio" }
        }
        for l in links {
            {
                let url = format!("{origin}/s/{}/{}", space, l.token);
                let scope = if l.kinds.is_empty() { "All files".to_string() } else { format!("Only {}", l.kinds.join(", ")) };
                let perm = match (l.download, l.comments) {
                    (true, true) => "download & comment",
                    (true, false) => "download only",
                    (false, true) => "comment only",
                    (false, false) => "view only",
                };
                let what = format!("{scope} · {perm}");
                let (reg, token, u) = (reg.clone(), l.token.clone(), url.clone());
                rsx! {
                    div { key: "{l.token}", class: "sharelink",
                        div { class: "pmain",
                            b { "{what}" }
                            a { class: "s1", href: "{url}", target: "_blank", "{url}" }
                        }
                        button {
                            class: "primary ghost",
                            onclick: move |_| {
                                crate::host::copy_text(&u);
                            },
                            "Copy"
                        }
                        button {
                            class: "x",
                            title: "Take this link back: it stops working at once",
                            onclick: move |_| {
                                reg.edit(|d| {
                                    let _ = proj::remove_share(d, &token);
                                });
                            },
                            "✕"
                        }
                    }
                }
            }
        }
        p { class: "hint",
            "Files kept available come from the fast storage (R2) or this node; the rest come from the storage server or the computer that has them, when it is on. Nothing is uploaded ahead of time."
        }
    }
}

/// Which other orgs see this project. It stays this org's; their members
/// see its parts, documents and share links.
#[component]
fn SharedWith(reg: Doc, owner: String, project: String, with: Vec<String>) -> Element {
    let nav = use_nav();
    let orgs = use_resource(|| async {
        crate::browser::get("/orgs".to_string())
            .await
            .and_then(|v| v["orgs"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|o| o.as_str().map(str::to_string))
            .collect::<Vec<String>>()
    });
    let mut others: Vec<String> = orgs()
        .unwrap_or_default()
        .into_iter()
        .filter(|o| *o != owner)
        .collect();
    for w in &with {
        if !others.contains(w) {
            others.push(w.clone());
        }
    }
    let visiting = (nav.within)().is_some();
    rsx! {
        if visiting {
            p { class: "note", "Shared with you by {owner}. It belongs to {owner}." }
        }
        div { class: "orgpicks",
            for o in others {
                {
                    let on = with.contains(&o);
                    let (reg, pid, org) = (reg.clone(), project.clone(), o.clone());
                    rsx! {
                        label { key: "{o}", class: "pin",
                            input {
                                r#type: "checkbox",
                                checked: on,
                                disabled: visiting,
                                onchange: move |e: FormEvent| {
                                    if visiting {
                                        return;
                                    }
                                    let v = e.checked();
                                    reg.edit(|d| {
                                        let _ = proj::share_with_org(d, &pid, &org, v);
                                    });
                                },
                            }
                            span { "{o}" }
                        }
                    }
                }
            }
        }
    }
}
