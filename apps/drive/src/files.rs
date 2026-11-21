//! Files, devices and storage: a space's file tree with where each file's
//! bytes are, its devices (online or not) and its storages, live. The
//! browser is a client too: it uploads to the node and downloads from it.

use dioxus::prelude::*;
use drive_dioxus::{Doc, use_doc, use_docs, use_drive};
use drive_files::{files_doc, space, space_doc, tree};

/// The space shown: the first on the node, or the one picked.
#[component]
pub fn Spaces(which: &'static str) -> Element {
    let spaces = use_docs("drive:space:");
    let mut picked = use_signal(|| None::<String>);
    let list: Vec<String> = spaces()
        .into_iter()
        .map(|d| d.id.trim_start_matches("drive:space:").to_string())
        .collect();
    let current = picked().or_else(|| list.first().cloned());
    rsx! {
        section { class: "pane",
            div { class: "panehead",
                h1 {
                    match which {
                        "devices" => "Devices",
                        "storage" => "Storage",
                        "computers" => "Computers",
                        "protection" => "Protection",
                        _ => "Files",
                    }
                }
                if list.len() > 1 {
                    select {
                        class: "spacepick",
                        onchange: move |e| picked.set(Some(e.value())),
                        for s in list.iter() {
                            option { key: "{s}", value: "{s}", selected: Some(s) == current.as_ref(), "{s}" }
                        }
                    }
                }
            }
            match current {
                None => rsx! {
                    p { class: "empty",
                        "No space yet. On a computer: "
                        code { "drive-agent init --node <this node>/sync --space home --name \"My PC\"" }
                    }
                },
                Some(space) => match which {
                    "devices" => rsx! {
                        Devices { key: "{space}", space }
                    },
                    "storage" => rsx! {
                        Storage { key: "{space}", space }
                    },
                    "computers" => rsx! {
                        crate::catalog::Computers { key: "{space}", space }
                    },
                    "protection" => rsx! {
                        crate::catalog::Protection { key: "{space}", space }
                    },
                    _ => rsx! {
                        Files { key: "{space}", space }
                    },
                },
            }
        }
    }
}

/// Devices whose agent is connected now (its presence in the registry).
fn online(reg: &Doc) -> Vec<String> {
    reg.others()
        .into_iter()
        .filter_map(|p| p.data.get("device").and_then(|d| d.as_str()).map(str::to_string))
        .collect()
}

fn human(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KB", n as f64 / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MB", n as f64 / 1_048_576.0),
        _ => format!("{:.1} GB", n as f64 / 1_073_741_824.0),
    }
}

#[component]
fn Files(space: String) -> Element {
    let files = use_doc(files_doc(&space));
    let reg = use_doc(space_doc(&space));
    let mut folder = use_signal(|| None::<String>);
    let mut status = use_signal(String::new);
    let f = files.loro();
    let r = reg.loro();
    let cur = folder().and_then(|s| tree::parse_id(&s));
    let rows = tree::children(&f, cur);
    let devices = space::devices(&r);
    let storages = space::storages(&r);
    let up = online(&reg);
    // The path, for breadcrumbs.
    let mut crumbs: Vec<(Option<String>, String)> = Vec::new();
    let mut c = cur;
    while let Some(id) = c {
        if let Some(e) = tree::entry(&f, id) {
            crumbs.push((Some(tree::id_string(id)), e.name));
        }
        c = tree::parent_of(&f, id);
    }
    crumbs.push((None, space.clone()));
    crumbs.reverse();

    let name_of = {
        let devices = devices.clone();
        let storages = storages.clone();
        move |who: &str| -> (String, &'static str) {
            if who == "node" {
                ("the node".into(), "node")
            } else if let Some(d) = devices.iter().find(|d| d.id == who) {
                (d.name.clone(), "device")
            } else if let Some(s) = storages.iter().find(|s| s.id == who) {
                (s.name.clone(), "storage")
            } else {
                (who.chars().take(6).collect(), "device")
            }
        }
    };

    let upload = {
        let files = files.clone();
        let reg = reg.clone();
        move |e: FormEvent| {
            let files = files.clone();
            let reg = reg.clone();
            async move {
                for file in e.files() {
                    let name = file.name();
                    status.set(format!("Uploading {name}…"));
                    let Ok(bytes) = file.read_bytes().await else { continue };
                    let hash = drive_files::hash::of_bytes(&bytes);
                    let body = js_sys::Uint8Array::from(bytes.as_ref());
                    let sent = gloo_net::http::Request::put(&format!("/blobs/{hash}")).body(body);
                    match sent {
                        Ok(req) => match req.send().await {
                            Ok(r) if r.ok() => {
                                files.edit(|d| {
                                    let id = match tree::lookup(d, cur, &name) {
                                        Some(x) if !x.dir => Some(x.id),
                                        Some(_) => None,
                                        None => tree::create_file(d, cur, &name).ok(),
                                    };
                                    if let Some(id) = id {
                                        let _ = tree::set_content(d, id, bytes.len() as u64, &hash);
                                    }
                                });
                                reg.edit(|d| {
                                    let _ = space::set_held(d, "node", &hash, Some("stored"));
                                });
                                status.set(format!("Uploaded {name}"));
                            }
                            _ => status.set(format!("The node refused {name}")),
                        },
                        Err(_) => status.set(format!("Could not send {name}")),
                    }
                }
            }
        }
    };

    let mut new_folder = use_signal(String::new);
    rsx! {
        div { class: "crumbs",
            for (i , (id , name)) in crumbs.iter().enumerate() {
                Fragment { key: "{i}",
                    if i > 0 {
                        span { class: "sep", "/" }
                    }
                    button {
                        onclick: {
                            let id = id.clone();
                            move |_| folder.set(id.clone())
                        },
                        "{name}"
                    }
                }
            }
        }
        div { class: "capture",
            label { class: "primary uploadbtn",
                "Upload"
                input { r#type: "file", multiple: true, onchange: upload }
            }
            input {
                placeholder: "New folder",
                value: "{new_folder}",
                oninput: move |e| new_folder.set(e.value()),
                onkeydown: {
                    let files = files.clone();
                    move |e: KeyboardEvent| {
                        if e.key().to_string() == "Enter" && !new_folder().trim().is_empty() {
                            let n = new_folder().trim().to_string();
                            files.edit(|d| {
                                let _ = tree::mkdir(d, cur, &n);
                            });
                            new_folder.set(String::new());
                        }
                    }
                },
            }
            span { class: "count", "{status}" }
        }
        div { class: "filelist",
            if rows.is_empty() {
                p { class: "empty", "Empty. Upload here, or drop files into the Drive folder on a computer." }
            }
            for e in rows {
                {
                    let holders = if e.dir { Vec::new() } else { space::holders(&r, &e.hash) };
                    let id = tree::id_string(e.id);
                    let download = format!("/blobs/{}?name={}", e.hash, js_sys::encode_uri_component(&e.name));
                    rsx! {
                        div { key: "{id}", class: "frow",
                            if e.dir {
                                button {
                                    class: "fname dir",
                                    onclick: {
                                        let id = id.clone();
                                        move |_| folder.set(Some(id.clone()))
                                    },
                                    span { class: "ficon", "▸" }
                                    "{e.name}"
                                }
                            } else {
                                span { class: "fname",
                                    span { class: "ficon", "·" }
                                    "{e.name}"
                                }
                            }
                            span { class: "fsize", if !e.dir { "{human(e.size)}" } }
                            span { class: "holders",
                                if !e.dir && e.hash.is_empty() {
                                    span { class: "hold waiting", "uploading…" }
                                }
                                for (who , how) in holders {
                                    {
                                        let (name, kind) = name_of(&who);
                                        let live = kind == "node" || up.contains(&who);
                                        let cls = format!("hold k-{kind} {how}{}", if live { " online" } else { "" });
                                        rsx! {
                                            span { key: "{who}", class: "{cls}", title: "{name}: {how}",
                                                if how == "pinned" {
                                                    "● "
                                                }
                                                "{name}"
                                            }
                                        }
                                    }
                                }
                            }
                            if !e.dir && !e.hash.is_empty() {
                                a { class: "open", href: "{download}", "Download" }
                            }
                        }
                    }
                }
            }
        }
        p { class: "hint",
            "Each chip is somewhere the file's bytes are: a device (green when its agent is online), a storage, or the node. ● is pinned (kept offline)."
        }
    }
}

#[component]
fn Devices(space: String) -> Element {
    let reg = use_doc(space_doc(&space));
    let ctx = use_drive();
    let mut code = use_signal(|| None::<String>);
    let r = reg.loro();
    let up = online(&reg);
    let devices = space::devices(&r);
    rsx! {
        div { class: "devices",
            for d in devices.iter() {
                {
                    let live = up.contains(&d.id);
                    let held = r
                        .get_map("holdings")
                        .get(&d.id)
                        .and_then(|v| v.into_container().ok())
                        .and_then(|c| c.into_map().ok())
                        .map_or(0, |m| m.len());
                    let by = devices.iter().find(|x| x.id == d.approved_by).map_or(d.approved_by.clone(), |x| x.name.clone());
                    let server = drive_files::placement::Role::of(&d.roles) == drive_files::placement::Role::StorageServer;
                    let active = space::active(&r, &d.id);
                    let keeps = if server {
                        "storage server · keeps everything".to_string()
                    } else if active.is_empty() {
                        "workstation · keeps its pins".to_string()
                    } else {
                        format!("workstation · working on {}", active.iter().map(|p| if p.is_empty() { "everything".to_string() } else { format!("/{p}") }).collect::<Vec<_>>().join(", "))
                    };
                    rsx! {
                        div { key: "{d.id}", class: "device",
                            span { class: if live { "dot on" } else { "dot" } }
                            div { class: "dmain",
                                b { "{d.name}" }
                                span { class: "s1", "{d.kind} · {d.os} · approved by {by}" }
                                span { class: if server { "s1 role server" } else { "s1 role" }, "{keeps}" }
                            }
                            span { class: "count", "{held} blobs here" }
                            span { class: if live { "badge on" } else { "badge" },
                                if live { "online" } else { "offline" }
                            }
                        }
                    }
                }
            }
        }
        div { class: "capture",
            button {
                class: "primary",
                onclick: move |_| {
                    let c = reg.edit(|d| space::invite(d, "the Drive app").ok());
                    code.set(c);
                },
                "Add a device"
            }
        }
        if let Some(c) = code() {
            div { class: "pairing",
                b { "Pairing code: {c}" }
                span { "Valid ten minutes. On the new computer:" }
                code {
                    "drive-agent join --node wss://{host()}/sync --space {space} --name \"New PC\" --code {c}"
                }
            }
        }
        p { class: "hint", "A device is online while its agent is connected. {ctx.me.peek().name} (this browser) is a guest, not a device." }
    }
}

fn host() -> String {
    js_sys::eval("location.host").ok().and_then(|v| v.as_string()).unwrap_or_default()
}

#[component]
fn Storage(space: String) -> Element {
    let reg = use_doc(space_doc(&space));
    let r = reg.loro();
    let devices = space::devices(&r);
    let up = online(&reg);
    let count = |who: &str| -> usize {
        r.get_map("holdings")
            .get(who)
            .and_then(|v| v.into_container().ok())
            .and_then(|c| c.into_map().ok())
            .map_or(0, |m| m.len())
    };
    rsx! {
        div { class: "devices",
            div { class: "device",
                span { class: "dot on" }
                div { class: "dmain",
                    b { "The node" }
                    span { class: "s1", "always on · this server's disk" }
                }
                span { class: "count", "{count(\"node\")} blobs" }
                span { class: "badge on", "online" }
            }
            for s in space::storages(&r) {
                {
                    let by = devices.iter().find(|d| d.id == s.device).map_or("?".to_string(), |d| d.name.clone());
                    let live = up.contains(&s.device);
                    rsx! {
                        div { key: "{s.id}", class: "device",
                            span { class: if live { "dot on" } else { "dot" } }
                            div { class: "dmain",
                                b { "{s.name}" }
                                span { class: "s1", "{s.scheme} · {s.location} · run by {by}" }
                            }
                            span { class: "count", "{count(&s.id)} blobs" }
                            span { class: if live { "badge on" } else { "badge" },
                                if live { "reachable" } else { "its device is offline" }
                            }
                        }
                    }
                }
            }
        }
        p { class: "hint",
            "Add storage from the device that will run it (its credentials stay there): "
            code { "drive-agent storage add fs|s3|webdav NAME …" }
        }
    }
}
