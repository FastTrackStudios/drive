//! All orgs: every org you are in at a glance (its computers, its
//! projects, what its apps keep), and everything the apps keep across all
//! of them. Opening an org switches every app on this address to it.

use dioxus::prelude::*;
use drive_dioxus::{use_doc, use_docs_everywhere, use_drive};
use drive_files::{projects, space};

use crate::browser::{Place, app_label, get, human, icon, online, project_icon, use_nav};

/// One org's share of an app's documents: (org, how many, their bytes).
type OrgDocs = (String, usize, u64);

#[component]
pub fn AllOrgs(here: String) -> Element {
    let orgs = use_resource(|| async {
        get("/orgs".to_string())
            .await
            .and_then(|v| v["orgs"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|o| o.as_str().map(str::to_string))
            .collect::<Vec<String>>()
    });
    let docs = use_docs_everywhere("");
    let list = orgs().unwrap_or_default();
    // Every app's documents, by app, then by org.
    let mut apps: Vec<(String, Vec<OrgDocs>)> = Vec::new();
    for d in docs().iter() {
        let Some((org, rest)) = d.id.split_once('/') else {
            continue;
        };
        let app = rest.split(':').next().unwrap_or_default().to_string();
        if app == "drive" || app.is_empty() {
            continue;
        }
        let row = match apps.iter_mut().find(|(a, _)| *a == app) {
            Some(r) => r,
            None => {
                apps.push((app.clone(), Vec::new()));
                apps.last_mut().expect("just pushed")
            }
        };
        match row.1.iter_mut().find(|(o, _, _)| o == org) {
            Some(x) => {
                x.1 += 1;
                x.2 += d.bytes;
            }
            None => row.1.push((org.to_string(), 1, d.bytes)),
        }
    }
    apps.sort_by(|a, b| a.0.cmp(&b.0));
    rsx! {
        div { class: "pane wide",
            div { class: "kindtitle",
                img { src: icon("Devices"), alt: "" }
                h1 { "All orgs" }
                span { class: "count", "{list.len()} orgs" }
            }
            div { class: "orgs",
                for o in list.iter() {
                    OrgCard { key: "{o}", org: o.clone(), here: o == &here }
                }
            }
            h2 { class: "sec", "Everything the apps keep" }
            if apps.is_empty() {
                p { class: "empty", "Nothing yet." }
            }
            for (app , by_org) in apps {
                div { key: "{app}", class: "approw",
                    img { src: icon("Widget"), alt: "" }
                    b { "{app_label(&app)}" }
                    div { class: "chips",
                        for (org , n , bytes) in by_org {
                            OrgChip { key: "{org}", org, app: app.clone(), n, bytes, here: here.clone() }
                        }
                    }
                }
            }
            p { class: "hint",
                "Each org keeps its own documents and computers; a project can be shared from the org that owns it to others (on its page)."
            }
        }
    }
}

#[component]
fn OrgChip(org: String, app: String, n: usize, bytes: u64, here: String) -> Element {
    let nav = use_nav();
    let ctx = use_drive();
    let (o, a) = (org.clone(), app.clone());
    rsx! {
        button {
            class: "chip orgchip",
            onclick: move |_| {
                if o == here {
                    nav.go_home(Place::App(a.clone()));
                } else {
                    ctx.set_org(&o);
                }
            },
            "{org} · {n} · {human(bytes)}"
        }
    }
}

/// One org: its computers and projects, live from its registry.
#[component]
fn OrgCard(org: String, here: bool) -> Element {
    let nav = use_nav();
    let ctx = use_drive();
    let reg = use_doc(format!("{org}/drive:space"));
    let r = reg.loro();
    let devices = space::devices(&r);
    let up = online(&reg);
    let live = devices.iter().filter(|d| up.contains(&d.id)).count();
    let projects = projects::projects(&r);
    let o = org.clone();
    rsx! {
        div { class: if here { "orgcard here" } else { "orgcard" },
            div { class: "orghead",
                b { "{org}" }
                if here {
                    span { class: "chip ok", "this org" }
                } else {
                    button { class: "primary ghost", onclick: move |_| ctx.set_org(&o), "Open" }
                }
            }
            span { class: "s1", "{live} of {devices.len()} computers online · {projects.len()} projects" }
            for p in projects.iter().take(6) {
                {
                    let (id, org) = (p.id.clone(), org.clone());
                    rsx! {
                        button {
                            key: "{p.id}",
                            class: "orgproj",
                            onclick: move |_| {
                                if here {
                                    nav.go_home(Place::Project(id.clone()));
                                } else {
                                    nav.go_in(&org, Place::Project(id.clone()));
                                }
                            },
                            img { src: icon(project_icon(&p.kind)), alt: "" }
                            span { "{p.name}" }
                            if !p.shared_with.is_empty() {
                                span { class: "snote", "shared with {p.shared_with.join(\", \")}" }
                            }
                        }
                    }
                }
            }
        }
    }
}
