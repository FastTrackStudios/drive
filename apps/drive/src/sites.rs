//! Sites: the static sites built from folders of the space's files. List
//! them with how their last build went, make one from a folder, pick its
//! theme, ask for a build, open it. A computer's agent does the building
//! (`drive-agent site build`); asking here sets the site's `requested`,
//! which that agent answers within a few seconds.

use dioxus::prelude::*;
use drive_dioxus::{use_doc, use_drive};
use drive_files::sites::{self, Site};
use drive_files::{files_doc, space, space_doc, tree};

/// Where themes live, by convention (`drive-agent site theme-init`).
const THEMES: &str = "Sites/Themes";

/// Every folder of the tree, by path.
fn folders(doc: &loro::LoroDoc) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack: Vec<(Option<loro::TreeID>, String)> = vec![(None, String::new())];
    while let Some((p, prefix)) = stack.pop() {
        for e in tree::children(doc, p) {
            if !e.dir || e.name.starts_with('.') {
                continue;
            }
            let path = if prefix.is_empty() {
                e.name.clone()
            } else {
                format!("{prefix}/{}", e.name)
            };
            out.push(path.clone());
            stack.push((Some(e.id), path));
        }
    }
    out.sort();
    out
}

/// "3 min ago".
fn ago(ms: i64) -> String {
    let s = ((drive_files::now_ms() - ms) / 1000).max(0);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86400),
    }
}

#[component]
pub fn SitesPage(space: String) -> Element {
    let reg = use_doc(space_doc(&space));
    let listed = sites::listed(&reg.loro());
    let mut adding = use_signal(|| false);
    rsx! {
        section { class: "pane",
            div { class: "panehead",
                h1 { "Sites" }
                span { class: "count", "{listed.len()}" }
                button { class: "primary", onclick: move |_| adding.set(!adding()), if adding() { "Close" } else { "New site" } }
            }
            p { class: "hint",
                "A site is a folder of Markdown pages (an Obsidian wiki works as it is) with a theme. A computer's agent builds it with dodeca; the node serves it, and a public storage can carry a copy."
            }
            if adding() {
                NewSite { space: space.clone(), on_done: move |_| adding.set(false) }
            }
            if listed.is_empty() && !adding() {
                p { class: "empty", "No sites yet." }
            }
            div { class: "sites",
                for (id , _title) in listed {
                    SiteCard { key: "{id}", space: space.clone(), id: id.clone() }
                }
            }
        }
    }
}

#[component]
fn NewSite(space: String, on_done: EventHandler<()>) -> Element {
    let ctx = use_drive();
    let files = use_doc(files_doc(&space));
    let reg = use_doc(space_doc(&space));
    let all = folders(&files.loro());
    let themes: Vec<String> = all
        .iter()
        .filter(|f| {
            f.strip_prefix(&format!("{THEMES}/"))
                .is_some_and(|rest| !rest.contains('/'))
        })
        .cloned()
        .collect();
    let sources: Vec<String> = all
        .iter()
        .filter(|f| !f.starts_with("Sites/"))
        .cloned()
        .collect();
    let r = reg.loro();
    let public = space::public(&r);
    let storages: Vec<(String, String)> = space::storages(&r)
        .into_iter()
        .filter(|s| public.get(&s.id).is_some_and(|u| !u.is_empty()))
        .map(|s| (s.id, s.name))
        .collect();
    let mut title = use_signal(String::new);
    let mut source = use_signal(String::new);
    let mut theme = use_signal(|| format!("{THEMES}/Base"));
    let mut storage = use_signal(String::new);
    let mut error = use_signal(String::new);
    let taken = sites::listed(&r);
    let create = move |_| {
        let t = title().trim().to_string();
        let src = source();
        let id = sites::id_from(&t);
        if t.is_empty() || id.is_empty() {
            error.set("Give it a title.".into());
            return;
        }
        if src.is_empty() {
            error.set("Pick the folder its pages are in.".into());
            return;
        }
        if taken.contains_key(&id) {
            error.set(format!("There is already a site {id}."));
            return;
        }
        let site = Site {
            id: id.clone(),
            title: t.clone(),
            source: src,
            theme: theme(),
            storage: storage(),
            auto: true,
            created: drive_files::now_ms(),
            requested: drive_files::now_ms(),
            ..Site::default()
        };
        let doc_id = sites::site_doc(&space, &id);
        let doc = ctx.drive.open(&doc_id);
        if sites::write(&doc, &site).is_ok() {
            ctx.drive.commit(&doc_id);
            reg.edit(|d| sites::set_listed(d, &id, Some(&t)).ok());
            on_done.call(());
        }
    };
    rsx! {
        div { class: "sitenew",
            label { "Title"
                input {
                    placeholder: "Music Theory",
                    value: "{title}",
                    oninput: move |e| title.set(e.value()),
                }
            }
            label { "Pages"
                select { onchange: move |e| source.set(e.value()),
                    option { value: "", selected: source().is_empty(), "Pick a folder…" }
                    for f in sources {
                        option { key: "{f}", value: "{f}", selected: source() == f, "{f}" }
                    }
                }
            }
            label { "Theme"
                select { onchange: move |e| theme.set(e.value()),
                    if themes.is_empty() {
                        option { value: "{THEMES}/Base", "The starter theme" }
                    }
                    for f in themes {
                        option { key: "{f}", value: "{f}", selected: theme() == f, "{f.trim_start_matches(THEMES).trim_start_matches('/')}" }
                    }
                }
            }
            label { "Publish to"
                select { onchange: move |e| storage.set(e.value()),
                    option { value: "", "This node only (tailnet)" }
                    for (id , name) in storages {
                        option { key: "{id}", value: "{id}", selected: storage() == id, "{name} (public)" }
                    }
                }
            }
            if !error().is_empty() {
                p { class: "siteerr", "{error}" }
            }
            button { class: "primary", onclick: create, "Create and build" }
        }
    }
}

#[component]
fn SiteCard(space: String, id: String) -> Element {
    let doc = use_doc(sites::site_doc(&space, &id));
    let files = use_doc(files_doc(&space));
    let reg = use_doc(space_doc(&space));
    let d = doc.loro();
    let Some(site) = sites::read(&d) else {
        return rsx! {
            div { class: "site", p { class: "empty", "{id}: loading…" } }
        };
    };
    let build = sites::last_build(&d);
    let r = reg.loro();
    let builder = space::devices(&r)
        .into_iter()
        .find(|x| x.id == site.builder)
        .map_or("the first computer to see it".to_string(), |x| x.name);
    let all = folders(&files.loro());
    let themes: Vec<String> = all
        .iter()
        .filter(|f| {
            f.strip_prefix(&format!("{THEMES}/"))
                .is_some_and(|rest| !rest.contains('/'))
        })
        .cloned()
        .collect();
    let waiting = build.as_ref().is_none_or(|b| site.requested > b.at);
    let node_url = sites::node_path(&space, &id);
    let public_url = build.as_ref().and_then(|b| {
        b.published
            .iter()
            .find(|p| p.starts_with("https://") || p.starts_with("http://"))
            .cloned()
    });
    let (state_class, state) = match &build {
        None if waiting => ("sitestate wait", "Waiting for its first build".to_string()),
        None => ("sitestate", "Not built".to_string()),
        Some(b) if b.ok => (
            "sitestate ok",
            format!(
                "Built {} · {} pages · {} ms",
                ago(b.at),
                b.pages,
                b.duration_ms
            ),
        ),
        Some(b) => ("sitestate bad", format!("Failed {}", ago(b.at))),
    };
    let request = {
        let doc = doc.clone();
        move |_| {
            doc.edit(|d| sites::request_build(d).ok());
        }
    };
    let set_theme = {
        let doc = doc.clone();
        let site = site.clone();
        move |e: FormEvent| {
            let mut s = site.clone();
            s.theme = e.value();
            s.requested = drive_files::now_ms();
            doc.edit(|d| sites::write(d, &s).ok());
        }
    };
    let toggle_auto = {
        let doc = doc.clone();
        let site = site.clone();
        move |_| {
            let mut s = site.clone();
            s.auto = !s.auto;
            doc.edit(|d| sites::write(d, &s).ok());
        }
    };
    rsx! {
        div { class: "site",
            div { class: "sitehead",
                div { class: "sitetitle",
                    b { "{site.title}" }
                    span { class: "sitepath", "{site.source}" }
                }
                if build.is_some() {
                    a { class: "open", href: "{node_url}", target: "_blank", "Open" }
                }
            }
            div { class: "{state_class}",
                span { "{state}" }
                if waiting && build.is_some() {
                    span { class: "sitewait", " · a build is asked for" }
                }
            }
            if let Some(b) = &build {
                if !b.ok {
                    pre { class: "sitelog", "{b.message}" }
                }
                if b.ok && (!b.broken_links.is_empty() || !b.unresolved.is_empty()) {
                    details { class: "siteissues",
                        summary { "{b.broken_links.len()} broken links · {b.unresolved.len()} unresolved wikilinks" }
                        ul {
                            for l in b.broken_links.iter() {
                                li { key: "b{l}", "Broken: {l}" }
                            }
                            for l in b.unresolved.iter() {
                                li { key: "u{l}", "Unresolved: {l}" }
                            }
                        }
                    }
                }
            }
            div { class: "siterows",
                div { class: "siterow",
                    span { class: "ik", "Theme" }
                    select { onchange: set_theme,
                        if !themes.contains(&site.theme) {
                            option { value: "{site.theme}", selected: true, "{site.theme}" }
                        }
                        for f in themes {
                            option { key: "{f}", value: "{f}", selected: site.theme == f, "{f.trim_start_matches(THEMES).trim_start_matches('/')}" }
                        }
                    }
                }
                if !site.overrides.is_empty() {
                    div { class: "siterow",
                        span { class: "ik", "Overrides" }
                        span { "{site.overrides}" }
                    }
                }
                div { class: "siterow",
                    span { class: "ik", "Built by" }
                    span { "{builder}" }
                }
                div { class: "siterow",
                    span { class: "ik", "Address" }
                    a { class: "sitelink", href: "{node_url}", target: "_blank", "{node_url}" }
                }
                if let Some(u) = public_url {
                    div { class: "siterow",
                        span { class: "ik", "Public" }
                        a { class: "sitelink", href: "{u}", target: "_blank", "{u}" }
                    }
                }
            }
            div { class: "siteacts",
                button { class: "primary", disabled: waiting && build.is_some(), onclick: request, "Build now" }
                button { class: if site.auto { "chip on" } else { "chip" }, onclick: toggle_auto,
                    if site.auto { "Rebuilds on change" } else { "Manual builds" }
                }
            }
        }
    }
}
