//! Browsing every computer's disks, live and read-only, without indexing
//! them (docs/browse.md): each computer's volumes under it in the
//! sidebar, a volume's folders in the main area (the browser's own grid
//! or list, Quick Look, the inspector, thumbnails made by the device), and
//! "Index this folder" to make one a location.
//!
//! The browser hooks it in at a few places only (`Place::Volume`,
//! `Src::Browse`, the sidebar's device group and the inspector), so the
//! shell around it can change freely.

use dioxus::prelude::*;
use drive_dioxus::use_doc;
use drive_files::{space, space_doc};
use drive_spine as spine;
use drive_spine::kinds::Kinds;
use serde_json::Value;

use crate::browser::{Crumbs, Item, Items, Place, Src, enc, human, icon, s, use_nav};

const STYLE: Asset = asset!("/assets/browse.css");

/// Entries asked for at a time.
const PAGE: u32 = 500;

/// `/browse/{space}/{device}`.
fn base(space: &str, device: &str) -> String {
    format!("/browse/{}/{}", enc(space), enc(device))
}

/// A GET that also returns the JSON of an error answer (`state`, `why`).
async fn get_any(url: String) -> Option<(u16, Value)> {
    let mut req = gloo_net::http::Request::get(&url);
    if let Some(b) = drive_dioxus::auth::bearer() {
        req = req.header("authorization", &b);
    }
    let r = req.send().await.ok()?;
    let status = r.status();
    let v = serde_json::from_str(&r.text().await.unwrap_or_default()).unwrap_or(Value::Null);
    Some((status, v))
}

/// Where a browsed file's bytes are read (its device online), as a
/// download of its name.
pub fn file_url(space: &str, device: &str, root: &str, path: &str, name: &str) -> String {
    format!(
        "{}/file?root={}&path={}&name={}",
        base(space, device),
        enc(root),
        enc(path),
        enc(name)
    )
}

/// A browsed file's sidecar: found by content on the node (so it shows
/// while the device is off, once made), else made by the device.
pub fn sidecar_url(
    space: &str,
    device: &str,
    root: &str,
    path: &str,
    item: &Item,
    kind: drive_sidecars::Kind,
) -> String {
    format!(
        "{}/sidecar/{}?root={}&path={}&size={}&mtime={}",
        base(space, device),
        kind.as_str(),
        enc(root),
        enc(path),
        item.size,
        item.mtime
    )
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_string()
    } else {
        format!("{path}/{name}")
    }
}

fn last(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A root's icon: home, a plugged-in drive, the system disk, another disk.
fn root_icon(r: &Value) -> &'static str {
    if r["home"].as_bool() == Some(true) {
        "Home"
    } else if r["removable"].as_bool() == Some(true) {
        "HDD"
    } else if s(r, "mount") == "/" {
        "PC"
    } else {
        "Storage"
    }
}

/// The roots of a device: (online, roots, why).
fn use_roots(space: String, device: String) -> Resource<Option<(bool, Vec<Value>, String)>> {
    use_resource(use_reactive!(|(space, device)| async move {
        let (_, v) = get_any(format!("{}/roots", base(&space, &device))).await?;
        Some((
            v["online"].as_bool() == Some(true),
            v["roots"].as_array().cloned().unwrap_or_default(),
            s(&v, "why"),
        ))
    }))
}

/// The sidebar's volumes of one computer, under its name: each with its
/// icon and how full it is.
#[component]
pub fn DeviceVolumes(space: String, device: String, live: bool) -> Element {
    let nav = use_nav();
    // Asked again when the device comes or goes.
    let roots = use_roots(space.clone(), device.clone());
    let mut was = use_signal(|| live);
    if *was.peek() != live {
        was.set(live);
        roots.clone().restart();
    }
    let place = (nav.place)();
    let list = roots().flatten().map(|(_, r, _)| r).unwrap_or_default();
    rsx! {
        document::Stylesheet { href: STYLE }
        for r in list {
            {
                let mount = s(&r, "mount");
                let p = Place::Volume { device: device.clone(), root: mount.clone(), path: String::new() };
                let on = matches!(&place, Place::Volume { device: d, root, .. } if *d == device && *root == mount);
                let (total, free) = (r["total"].as_u64().unwrap_or(0), r["free"].as_u64().unwrap_or(0));
                let used = if total > 0 { 100 - (free * 100 / total) } else { 0 };
                let full = if used >= 90 { " full" } else { "" };
                let title = if total > 0 { format!("{mount} · {} free of {}", human(free), human(total)) } else { mount.clone() };
                rsx! {
                    button {
                        key: "{mount}",
                        class: if on { "side sub vol on" } else { "side sub vol" },
                        title: "{title}",
                        onclick: move |_| nav.go_home(p.clone()),
                        img { class: "sicon", src: icon(root_icon(&r)), alt: "" }
                        span { class: "vmain",
                            span { class: "slabel", "{s(&r, \"name\")}" }
                            if total > 0 {
                                span { class: "vbar{full}",
                                    span { class: "vfill", style: "width: {used}%" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// A folder on a computer's disk, live: the browser's grid or list of it,
/// a page at a time; offline, the last look the node kept.
#[component]
pub fn VolumeFolder(space: String, device: String, root: String, path: String) -> Element {
    let nav = use_nav();
    let reg = use_doc(space_doc(&space));
    let spine_doc = use_doc(spine::SPINE);
    let kinds = Kinds::load(&spine_doc.loro());
    let mut hidden = use_signal(|| false);
    // Pages after the first ("Show more").
    let mut more = use_signal(Vec::<Value>::new);
    let mut loading_more = use_signal(|| false);
    let mut status = use_signal(String::new);
    let mut made = use_signal(|| None::<String>);
    let roots = use_roots(space.clone(), device.clone());
    let (sp, dv, rt, pt) = (space.clone(), device.clone(), root.clone(), path.clone());
    let first = use_resource(use_reactive!(|(sp, dv, rt, pt)| {
        let h = hidden();
        async move {
            more.set(Vec::new());
            let answer = get_any(format!(
                "{}/list?root={}&path={}&limit={PAGE}{}",
                base(&sp, &dv),
                enc(&rt),
                enc(&pt),
                if h { "&hidden=1" } else { "" }
            ))
            .await;
            // Which folder it answers (the last answer stays until the next
            // one comes: never show it under another folder's name).
            (format!("{dv}\0{rt}\0{pt}\0{h}"), answer)
        }
    }));
    let devices = space::devices(&reg.loro());
    let dname = devices
        .iter()
        .find(|d| d.id == device)
        .map_or(device.clone(), |d| d.name.clone());
    let root_name = roots()
        .flatten()
        .and_then(|(_, rs, _)| rs.into_iter().find(|r| s(r, "mount") == root))
        .map_or_else(|| root.clone(), |r| s(&r, "name"));
    let here = format!("{device}\0{root}\0{path}\0{}", hidden());
    let answer = first().and_then(|(at, a)| (at == here).then_some(a).flatten());
    let (code, v) = answer.clone().unwrap_or((0, Value::Null));
    let ok = code == 200;
    let online = ok && v["online"].as_bool() == Some(true);
    let total = v["total"].as_u64().unwrap_or(0);
    let left_out = v["hidden"].as_u64().unwrap_or(0);
    let mut entries: Vec<Value> = v["entries"].as_array().cloned().unwrap_or_default();
    entries.extend(more());
    let shown = entries.len() as u64;
    let at = format!("{dname} · {root_name}");
    let items: Vec<Item> = entries
        .iter()
        .map(|e| {
            let name = s(e, "name");
            let dir = e["dir"].as_bool() == Some(true);
            Item {
                kind: kinds.of_file(&name, dir).clone(),
                size: e["size"].as_u64().unwrap_or(0),
                mtime: e["mtime"].as_i64().unwrap_or(0),
                at: format!("{at} · /{path}"),
                online,
                copies: 0,
                kept: None,
                available: None,
                src: Src::Browse {
                    device: device.clone(),
                    root: root.clone(),
                    path: join(&path, &name),
                },
                name,
                dir,
            }
        })
        .collect();

    let mut crumbs = vec![
        (dname.clone(), None),
        (
            root_name.clone(),
            Some(Place::Volume {
                device: device.clone(),
                root: root.clone(),
                path: String::new(),
            }),
        ),
    ];
    let mut acc = String::new();
    for part in path.split('/').filter(|p| !p.is_empty()) {
        acc = join(&acc, part);
        crumbs.push((
            part.to_string(),
            Some(Place::Volume {
                device: device.clone(),
                root: root.clone(),
                path: acc.clone(),
            }),
        ));
    }
    let folder_name = if path.is_empty() {
        root_name.clone()
    } else {
        last(&path).to_string()
    };
    let index = {
        let (sp, dv, rt, pt, name, dn) = (
            space.clone(),
            device.clone(),
            root.clone(),
            path.clone(),
            folder_name.clone(),
            dname.clone(),
        );
        move |_| {
            let (sp, dv, rt, pt, name, dn) = (
                sp.clone(),
                dv.clone(),
                rt.clone(),
                pt.clone(),
                name.clone(),
                dn.clone(),
            );
            async move {
                status.set(format!("Indexing {name}…"));
                match index_folder(&sp, &dv, &rt, &pt, &name).await {
                    Ok(id) => {
                        status.set(format!("{name} is a location now, indexed by {dn}."));
                        made.set(Some(id));
                    }
                    Err(why) => status.set(why),
                }
            }
        }
    };
    let next_page = {
        let (sp, dv, rt, pt) = (space.clone(), device.clone(), root.clone(), path.clone());
        move |_| {
            let (sp, dv, rt, pt) = (sp.clone(), dv.clone(), rt.clone(), pt.clone());
            async move {
                loading_more.set(true);
                let offset = shown;
                let url = format!(
                    "{}/list?root={}&path={}&offset={offset}&limit={PAGE}{}",
                    base(&sp, &dv),
                    enc(&rt),
                    enc(&pt),
                    if hidden() { "&hidden=1" } else { "" }
                );
                if let Some((200, v)) = get_any(url).await {
                    let page = v["entries"].as_array().cloned().unwrap_or_default();
                    more.write().extend(page);
                }
                loading_more.set(false);
            }
        }
    };
    let why = if ok {
        s(&v, "why")
    } else {
        match s(&v, "why") {
            w if !w.is_empty() => w,
            _ if code == 403 => "Not one of your orgs.".to_string(),
            _ if code == 401 => "Sign in to browse this computer.".to_string(),
            _ => String::new(),
        }
    };
    let offline_state = !ok && s(&v, "state") == "offline";
    let empty = if !ok {
        if offline_state {
            format!("{dname} is offline. Folders seen before show here while it is off.")
        } else if why.is_empty() {
            "This folder cannot be read.".to_string()
        } else {
            why.clone()
        }
    } else if left_out > 0 && !hidden() {
        format!(
            "Nothing here but {left_out} hidden item{}.",
            if left_out == 1 { "" } else { "s" }
        )
    } else {
        "Empty folder.".to_string()
    };
    rsx! {
        document::Stylesheet { href: STYLE }
        div { class: "viewhead",
            Crumbs { parts: crumbs }
            div { class: "actions",
                if online {
                    span { class: "badge on", "live" }
                } else if answer.is_some() {
                    span { class: "badge warn", title: "{why}",
                        if ok { "{dname} is offline · last seen" } else if offline_state { "{dname} is offline" } else { "unreadable" }
                    }
                }
                if ok {
                    span { class: "count",
                        if shown < total { "{shown} of {total}" } else { "{total} items" }
                    }
                }
                label { class: "hidetoggle", title: "Dot files and system folders",
                    input {
                        r#type: "checkbox",
                        checked: hidden(),
                        onchange: move |e| hidden.set(e.checked()),
                    }
                    "Hidden"
                }
                if let Some(id) = made() {
                    button {
                        class: "primary ghost",
                        onclick: move |_| nav.go_home(Place::Location { loc: id.clone(), path: String::new() }),
                        "Open location"
                    }
                } else {
                    button {
                        class: "primary",
                        disabled: !online,
                        title: if online { "Index this folder: searchable, with its copies counted, while the computer is off" } else { "The computer is offline" },
                        onclick: index,
                        "Index this folder"
                    }
                }
            }
        }
        if !status().is_empty() {
            p { class: "status", "{status}" }
        }
        Items {
            items,
            show_at: false,
            empty,
            loading: answer.is_none(),
        }
        if ok && shown < total {
            div { class: "more",
                button { class: "primary ghost", disabled: loading_more(), onclick: next_page,
                    if loading_more() { "Loading…" } else { "Show {(total - shown).min(PAGE as u64)} more" }
                }
            }
        }
    }
}

/// Ask the node to make this folder a location: its id, or why not.
async fn index_folder(
    space: &str,
    device: &str,
    root: &str,
    path: &str,
    name: &str,
) -> Result<String, String> {
    let body = serde_json::json!({ "root": root, "path": path, "name": name }).to_string();
    let mut req = gloo_net::http::Request::post(&format!("{}/index", base(space, device)))
        .header("content-type", "application/json");
    if let Some(b) = drive_dioxus::auth::bearer() {
        req = req.header("authorization", &b);
    }
    let r = req
        .body(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&r.text().await.unwrap_or_default()).unwrap_or(Value::Null);
    if r.ok() {
        Ok(s(&v, "location"))
    } else {
        Err(match s(&v, "why") {
            w if !w.is_empty() => w,
            _ => format!("The node refused ({})", r.status()),
        })
    }
}

/// The inspector's part for a picked folder on a computer's disk.
#[component]
pub fn InspectorActions(space: String, item: Item) -> Element {
    let Src::Browse { device, root, path } = item.src.clone() else {
        return rsx! {};
    };
    let mut status = use_signal(String::new);
    if !item.dir {
        return rsx! {};
    }
    let name = item.name.clone();
    let online = item.online;
    rsx! {
        section { class: "isec",
            h4 { "This folder" }
            p { class: "ihint", "Indexed, it is searchable and its copies count while the computer is off." }
            button {
                class: "primary ghost",
                disabled: !online,
                onclick: move |_| {
                    let (sp, dv, rt, pt, nm) = (space.clone(), device.clone(), root.clone(), path.clone(), name.clone());
                    async move {
                        status.set("Indexing…".into());
                        match index_folder(&sp, &dv, &rt, &pt, &nm).await {
                            Ok(_) => status.set(format!("{nm} is a location now.")),
                            Err(why) => status.set(why),
                        }
                    }
                },
                "Index this folder"
            }
            if !status().is_empty() {
                p { class: "ihint", "{status}" }
            }
        }
    }
}
