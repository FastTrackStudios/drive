//! Every computer's files, from the node's catalog of location indexes:
//! search them all (devices that are off included), browse a computer's
//! locations, see where each file's copies are, and check the space's
//! protection policies (3-2-1 and the like).

use dioxus::prelude::*;
use drive_dioxus::use_doc;
use drive_files::{space, space_doc};
use serde_json::Value;

async fn get(url: String) -> Option<Value> {
    let r = gloo_net::http::Request::get(&url).send().await.ok()?;
    if !r.ok() {
        return None;
    }
    serde_json::from_str(&r.text().await.ok()?).ok()
}

fn enc(s: &str) -> String {
    js_sys::encode_uri_component(s).into()
}

fn human(n: u64) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KB", n as f64 / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MB", n as f64 / 1_048_576.0),
        _ => format!("{:.1} GB", n as f64 / 1_073_741_824.0),
    }
}

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}


#[component]
pub fn Computers(space: String) -> Element {
    let reg = use_doc(space_doc(&space));
    let mut query = use_signal(String::new);
    let mut place = use_signal(|| None::<(String, String)>);
    let mut picked = use_signal(|| None::<Value>);
    let r = reg.loro();
    let devices = space::devices(&r);
    let locations = space::locations(&r);
    let storages = space::storages(&r);
    let online: Vec<String> = reg
        .others()
        .into_iter()
        .filter_map(|p| p.data.get("device").and_then(|d| d.as_str()).map(str::to_string))
        .collect();

    let sp = space.clone();
    let results = use_resource(move || {
        let (q, at, sp) = (query(), place(), sp.clone());
        async move {
            if !q.trim().is_empty() {
                get(format!("/search?space={}&q={}&limit=80", enc(&sp), enc(q.trim()))).await.map(|v| v["results"].clone())
            } else if let Some((loc, path)) = at {
                get(format!("/list?space={}&location={}&path={}", enc(&sp), enc(&loc), enc(&path)))
                    .await
                    .map(|v| v["entries"].clone())
            } else {
                None
            }
        }
    });
    let sp = space.clone();
    let copies = use_resource(move || {
        let (f, sp) = (picked(), sp.clone());
        async move {
            let f = f?;
            get(format!("/copies?space={}&sampled={}&size={}", enc(&sp), s(&f, "sampled"), f["size"])).await
        }
    });

    let rows: Vec<Value> = results().flatten().and_then(|v| v.as_array().cloned()).unwrap_or_default();
    let searching = !query().trim().is_empty();
    rsx! {
        div { class: "capture",
            input {
                placeholder: "Search every computer's files",
                value: "{query}",
                oninput: move |e| query.set(e.value()),
            }
        }
        if !searching && place().is_none() {
            div { class: "devices",
                for d in devices.iter().filter(|d| locations.iter().any(|l| l.device == d.id)) {
                    {
                        let live = online.contains(&d.id);
                        rsx! {
                            div { key: "{d.id}", class: "computer",
                                div { class: "device",
                                    span { class: if live { "dot on" } else { "dot" } }
                                    div { class: "dmain",
                                        b { "{d.name}" }
                                        span { class: "s1", "{d.kind} · {d.os}" }
                                    }
                                    span { class: if live { "badge on" } else { "badge" },
                                        if live { "online" } else { "offline · its index is here" }
                                    }
                                }
                                for l in locations.iter().filter(|l| l.device == d.id) {
                                    {
                                        let id = l.id.clone();
                                        let where_ = match storages.iter().find(|x| x.id == l.storage) {
                                            Some(st) => format!("{} · {}", st.name, l.path),
                                            None => l.path.clone(),
                                        };
                                        rsx! {
                                            button {
                                                key: "{l.id}",
                                                class: "locrow",
                                                onclick: move |_| place.set(Some((id.clone(), String::new()))),
                                                span { class: "ficon", "▸" }
                                                span { class: "lname", "{l.name}" }
                                                span { class: "s1", "{where_}" }
                                                span { class: "count", "{l.files} files · {human(l.bytes.max(0) as u64)}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if locations.is_empty() {
                    p { class: "empty",
                        "No computer has a location yet. On a computer: "
                        code { "drive-agent location add Samples ~/Samples" }
                    }
                }
            }
        }
        if let (false, Some((loc, path))) = (searching, place()) {
            {
                let l = locations.iter().find(|x| x.id == loc);
                let dev = l.and_then(|l| devices.iter().find(|d| d.id == l.device)).map_or(String::new(), |d| d.name.clone());
                let lname = l.map_or(String::new(), |l| l.name.clone());
                let mut acc = String::new();
                let crumbs: Vec<(String, String)> = path
                    .split('/')
                    .filter(|p| !p.is_empty())
                    .map(|p| {
                        acc = if acc.is_empty() { p.to_string() } else { format!("{acc}/{p}") };
                        (p.to_string(), acc.clone())
                    })
                    .collect();
                let loc2 = loc.clone();
                rsx! {
                    div { class: "crumbs",
                        button { onclick: move |_| place.set(None), "Computers" }
                        span { class: "sep", "/" }
                        span { class: "crumbtext", "{dev}" }
                        span { class: "sep", "/" }
                        button {
                            onclick: move |_| place.set(Some((loc2.clone(), String::new()))),
                            "{lname}"
                        }
                        for (name , full) in crumbs {
                            {
                                let loc = loc.clone();
                                rsx! {
                                    Fragment { key: "{full}",
                                        span { class: "sep", "/" }
                                        button { onclick: move |_| place.set(Some((loc.clone(), full.clone()))), "{name}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if searching || place().is_some() {
            div { class: "filelist",
                if rows.is_empty() {
                    p { class: "empty", if searching { "Nothing by that name on any computer." } else { "Empty." } }
                }
                for (i , h) in rows.into_iter().enumerate() {
                    {
                        let dir = h["dir"].as_bool() == Some(true);
                        let live = h["online"].as_bool() == Some(true);
                        let n = h["copies"].as_u64().unwrap_or(1);
                        let path = s(&h, "path");
                        let loc = s(&h, "location");
                        let fetch = format!("/fetch/{}/{}?path={}", enc(&space), enc(&loc), enc(&path));
                        let hh = h.clone();
                        rsx! {
                            div { key: "{i}-{path}", class: "frow",
                                if dir {
                                    button {
                                        class: "fname dir",
                                        onclick: move |_| {
                                            query.set(String::new());
                                            place.set(Some((loc.clone(), path.clone())));
                                        },
                                        span { class: "ficon", "▸" }
                                        "{h[\"name\"].as_str().unwrap_or_default()}"
                                    }
                                } else {
                                    button {
                                        class: "fname",
                                        onclick: move |_| picked.set(Some(hh.clone())),
                                        span { class: "ficon", "·" }
                                        "{h[\"name\"].as_str().unwrap_or_default()}"
                                    }
                                }
                                if searching {
                                    span { class: "where", "{s(&h, \"device_name\")} · {s(&h, \"location_name\")} · {s(&h, \"path\")}" }
                                }
                                span { class: "fsize", if !dir { "{human(h[\"size\"].as_u64().unwrap_or(0))}" } }
                                span { class: "holders",
                                    if !dir && n > 1 {
                                        span { class: "hold k-storage", "×{n} copies" }
                                    }
                                    if !dir && n == 1 {
                                        span { class: "hold waiting", "only copy" }
                                    }
                                    span { class: if live { "hold k-device online" } else { "hold" },
                                        if live { "online" } else { "offline" }
                                    }
                                }
                                if !dir && live {
                                    a { class: "open", href: "{fetch}", "Download" }
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(f) = picked() {
            div { class: "copies",
                div { class: "panehead",
                    h2 { "Copies of {s(&f, \"name\")}" }
                    button { class: "x", onclick: move |_| picked.set(None), "✕" }
                }
                for (i , c) in copies().flatten().and_then(|v| v["copies"].as_array().cloned()).unwrap_or_default().into_iter().enumerate() {
                    div { key: "{i}", class: "frow",
                        span { class: if c["online"].as_bool() == Some(true) { "dot on" } else { "dot" } }
                        span { class: "fname", "{s(&c, \"device_name\")} · {s(&c, \"location_name\")}" }
                        span { class: "where", "{s(&c, \"path\")}" }
                        span { class: "hold k-node",
                            if s(&c, "site").is_empty() { "no site" } else { "{s(&c, \"site\")}" }
                        }
                    }
                }
                p { class: "hint", "Copies are found by content (a sampled BLAKE3 hash and the size), across every computer's index." }
            }
        }
        p { class: "hint",
            "Every computer's index lives on the node, so its files can be searched and browsed while it is off; downloading needs it on."
        }
    }
}

#[component]
pub fn Protection(space: String) -> Element {
    let reg = use_doc(space_doc(&space));
    let r = reg.loro();
    let locations = space::locations(&r);
    let policies = space::policies(&r);
    let sp = space.clone();
    // Re-checked whenever the registry changes (a new index, a new policy).
    let version = format!("{:?}", r.oplog_vv());
    let mut seen = use_signal(String::new);
    if *seen.peek() != version {
        seen.set(version);
    }
    let report = use_resource(move || {
        let sp = sp.clone();
        let _ = seen();
        async move { get(format!("/protection?space={}&limit=12", enc(&sp))).await }
    });
    let lname = |id: &str| locations.iter().find(|l| l.id == id).map_or(id.to_string(), |l| l.name.clone());
    let reports = report().flatten().and_then(|v| v["reports"].as_array().cloned()).unwrap_or_default();
    rsx! {
        if policies.is_empty() {
            p { class: "empty",
                "No policies yet. The 3-2-1 rule for a location: "
                code { "drive-agent policy add 3-2-1 --scope Samples" }
            }
        }
        for (i , rep) in reports.into_iter().enumerate() {
            {
                let p = &rep["policy"];
                let files = rep["files"].as_u64().unwrap_or(0);
                let ok = rep["ok"].as_u64().unwrap_or(0);
                let bytes = rep["bytes"].as_u64().unwrap_or(0);
                let risk = bytes.saturating_sub(rep["bytes_ok"].as_u64().unwrap_or(0));
                let pct = if files == 0 { 100 } else { ok * 100 / files };
                rsx! {
                    div { key: "{i}", class: "policy",
                        div { class: "polhead",
                            b { "{s(p, \"name\")}" }
                            span { class: "s1",
                                "{p[\"copies\"]} copies · {p[\"media\"]} media · {p[\"offsite\"]} offsite"
                            }
                            span { class: if ok == files { "badge on" } else { "badge warn" },
                                if ok == files { "all kept" } else { "{human(risk)} at risk" }
                            }
                        }
                        div { class: "meter",
                            div { class: "fill", style: "width: {pct}%" }
                        }
                        span { class: "count", "{ok} of {files} files meet it" }
                        for l in rep["locations"].as_array().cloned().unwrap_or_default() {
                            div { class: "frow",
                                span { class: "fname", "{lname(l[\"location\"].as_str().unwrap_or_default())}" }
                                span { class: "count", "{l[\"ok\"]} of {l[\"files\"]}" }
                            }
                        }
                        for (j , sh) in rep["short"].as_array().cloned().unwrap_or_default().into_iter().enumerate() {
                            div { key: "{j}", class: "frow short",
                                span { class: "fname", "{lname(sh[\"location\"].as_str().unwrap_or_default())}/{s(&sh, \"path\")}" }
                                span { class: "fsize", "{human(sh[\"size\"].as_u64().unwrap_or(0))}" }
                                span { class: "holders",
                                    span { class: "hold", "{sh[\"kept\"][\"copies\"]} copies" }
                                    span { class: "hold", "{sh[\"kept\"][\"media\"]} media" }
                                    span { class: "hold", "{sh[\"kept\"][\"offsite\"]} offsite" }
                                }
                            }
                        }
                    }
                }
            }
        }
        p { class: "hint",
            "A copy is a location holding the file; media are distinct disks or storages; offsite copies are at another site than the file's own. Back a location up into a storage with "
            code { "drive-agent backup LOCATION STORAGE" }
            "."
        }
    }
}
