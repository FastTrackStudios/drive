//! The space at a glance (what it holds, by kind and by computer) and its
//! protection: each policy (3-2-1 and the like) checked against every
//! computer's index, with the files short of it one click away, in the
//! browser.

use dioxus::prelude::*;
use drive_dioxus::use_doc;
use drive_files::{space, space_doc};
use drive_spine as spine;
use drive_spine::kinds::Kinds;

use crate::browser::{Item, Items, Place, enc, get, human, icon, kind_hue, online, s, use_nav};

/// Refetched whenever the registry changes (a new index, a new policy).
fn use_reg_rev(space: &str) -> u64 {
    use_doc(space_doc(space)).rev()
}

#[component]
pub fn Overview(space: String) -> Element {
    let nav = use_nav();
    let reg = use_doc(space_doc(&space));
    let spine_doc = use_doc(spine::SPINE);
    let rev = use_reg_rev(&space);
    let sp = space.clone();
    let stats = use_resource(use_reactive!(|(sp, rev)| async move {
        let _ = rev;
        get(format!("/stats?space={}", enc(&sp))).await
    }));
    let sp = space.clone();
    let prot = use_resource(use_reactive!(|(sp, rev)| async move {
        let _ = rev;
        get(format!("/protection?space={}&limit=0", enc(&sp))).await
    }));
    let r = reg.loro();
    let kinds = Kinds::load(&spine_doc.loro());
    let devices = space::devices(&r);
    let locations = space::locations(&r);
    let up = online(&reg);
    let st = stats().flatten();
    let files = st.as_ref().and_then(|v| v["files"].as_u64()).unwrap_or(0);
    let bytes = st.as_ref().and_then(|v| v["bytes"].as_u64()).unwrap_or(0);

    // Bytes by top-level kind.
    let mut by_kind: Vec<(String, u64, u64)> = Vec::new();
    if let Some(exts) = st.as_ref().and_then(|v| v["exts"].as_object()) {
        for (ext, nb) in exts {
            let k = kinds.of_file(&format!("x.{ext}"), false);
            let top = if k.parent.is_empty() || k.parent == "doc" {
                k.id.clone()
            } else {
                k.parent.clone()
            };
            let (n, b) = (nb[0].as_u64().unwrap_or(0), nb[1].as_u64().unwrap_or(0));
            match by_kind.iter_mut().find(|x| x.0 == top) {
                Some(x) => {
                    x.1 += n;
                    x.2 += b;
                }
                None => by_kind.push((top, n, b)),
            }
        }
    }
    by_kind.sort_by(|a, b| b.2.cmp(&a.2));

    // Protection: files meeting every policy, over all.
    let reports = prot()
        .flatten()
        .and_then(|v| v["reports"].as_array().cloned())
        .unwrap_or_default();
    let (ok, all) = reports.iter().fold((0, 0), |(o, a), r| {
        (
            o + r["ok"].as_u64().unwrap_or(0),
            a + r["files"].as_u64().unwrap_or(0),
        )
    });
    let score = if all == 0 { None } else { Some(ok * 100 / all) };
    let live = devices.iter().filter(|d| up.contains(&d.id)).count();
    rsx! {
        div { class: "pane wide",
            div { class: "hero",
                div { class: "stat",
                    if st.is_some() {
                        b { "{human(bytes)}" }
                        span { "in {files} files" }
                    } else {
                        // Counting is the node's: the rest of the page is
                        // drawn meanwhile.
                        span { class: "loading", "Counting…" }
                    }
                }
                div { class: "stat",
                    b { "{live} of {devices.len()}" }
                    span { "computers online" }
                }
                div { class: "stat",
                    b { "{locations.len()}" }
                    span { "locations" }
                }
                button { class: "stat", onclick: move |_| nav.go(Place::Protection),
                    b { class: match score { Some(100) => "good", Some(_) => "warn", None => "" },
                        match score {
                            Some(p) => format!("{p}%"),
                            None => "—".to_string(),
                        }
                    }
                    span { if score.is_some() { "protected" } else { "no policy yet" } }
                }
            }
            h2 { class: "sec", "By kind" }
            div { class: "bar",
                for (k , _ , b) in by_kind.iter() {
                    div {
                        key: "{k}",
                        class: "seg",
                        style: format!("flex-grow: {}; background: oklch(.68 .14 {})", (*b).max(1), kind_hue(k)),
                        title: "{kinds.get(k).name}",
                    }
                }
            }
            div { class: "legend",
                for (k , n , b) in by_kind.iter() {
                    {
                        let kind = kinds.get(k).clone();
                        let p = Place::Kind(k.clone());
                        rsx! {
                            button { key: "{k}", class: "lg", onclick: move |_| nav.go(p.clone()),
                                img { src: icon(&kind.icon), alt: "" }
                                span { class: "sw", style: format!("background: oklch(.68 .14 {})", kind_hue(k)) }
                                b { "{kind.name}" }
                                span { "{n} · {human(*b)}" }
                            }
                        }
                    }
                }
            }
            h2 { class: "sec", "By computer" }
            div { class: "machines",
                for d in devices.iter() {
                    {
                        let locs: Vec<&space::Location> = locations.iter().filter(|l| l.device == d.id).collect();
                        let total: i64 = locs.iter().map(|l| l.bytes.max(0)).sum();
                        let first = locs.first().map(|l| Place::Location { loc: l.id.clone(), path: String::new() });
                        let on = up.contains(&d.id);
                        let server = drive_files::placement::Role::of(&d.roles) == drive_files::placement::Role::StorageServer;
                        rsx! {
                            button {
                                key: "{d.id}",
                                class: "machine",
                                onclick: move |_| {
                                    if let Some(p) = first.clone() {
                                        nav.go(p)
                                    }
                                },
                                img { src: icon(if server { "Server" } else { "PC" }), alt: "" }
                                div { class: "mmain",
                                    b { "{d.name}" }
                                    span { "{locs.len()} locations · {human(total as u64)}" }
                                    span { if server { "storage server" } else { "workstation" } }
                                }
                                span { class: if on { "dot on" } else { "dot" } }
                            }
                        }
                    }
                }
            }
            Previews {}
        }
    }
}

/// The node's sidecars (thumbnails, waveforms): how many, the space they
/// take, and a way to clear them (they are made again when wanted).
#[component]
fn Previews() -> Element {
    let mut rev = use_signal(|| 0u32);
    let mut busy = use_signal(|| false);
    let st = use_resource(move || async move {
        let _ = rev();
        get("/sidecars/stats".to_string()).await
    });
    let Some(st) = st().flatten() else {
        // Still asked, or the node has none to say: a line, not a gap.
        return rsx! {
            div { class: "previews loading", "Previews: asking the node…" }
        };
    };
    let count = st["count"].as_u64().unwrap_or(0);
    let bytes = st["bytes"].as_u64().unwrap_or(0);
    let kinds = st["kinds"].as_object().cloned().unwrap_or_default();
    let part = |k: &str| {
        let v = &kinds.get(k).cloned().unwrap_or_default();
        (
            v["count"].as_u64().unwrap_or(0),
            v["bytes"].as_u64().unwrap_or(0),
        )
    };
    let thumbs = (
        part("thumb-256").0 + part("thumb-1024").0,
        part("thumb-256").1 + part("thumb-1024").1,
    );
    let peaks = part("peaks");
    let clear = move |_| {
        busy.set(true);
        spawn(async move {
            let _ = crate::host::request(
                "POST",
                "/sidecars/purge",
                Some("application/json"),
                Some(br#"{"all":true}"#.to_vec()),
            )
            .await;
            busy.set(false);
            rev += 1;
        });
    };
    rsx! {
        h2 { class: "sec", "Previews" }
        div { class: "previews",
            div { class: "stat",
                b { "{human(bytes)}" }
                span { "{count} thumbnails and waveforms on the node" }
            }
            div { class: "pvparts",
                span { "Thumbnails · {thumbs.0} · {human(thumbs.1)}" }
                span { "Waveforms · {peaks.0} · {human(peaks.1)}" }
                span { class: "hint", "Made once per file content; cleared ones are made again when needed." }
            }
            button { class: "primary ghost", disabled: busy() || count == 0, onclick: clear,
                if busy() { "Clearing…" } else { "Clear" }
            }
        }
    }
}

#[component]
pub fn Protection(space: String) -> Element {
    let nav = use_nav();
    let reg = use_doc(space_doc(&space));
    let rev = use_reg_rev(&space);
    let r = reg.loro();
    let locations = space::locations(&r);
    let policies = space::policies(&r);
    let sp = space.clone();
    let report = use_resource(use_reactive!(|(sp, rev)| async move {
        let _ = rev;
        get(format!("/protection?space={}&limit=0", enc(&sp))).await
    }));
    let lname = |id: &str| {
        locations
            .iter()
            .find(|l| l.id == id)
            .map_or(id.to_string(), |l| l.name.clone())
    };
    let reports = report()
        .flatten()
        .and_then(|v| v["reports"].as_array().cloned())
        .unwrap_or_default();
    rsx! {
        div { class: "pane",
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
                    let short = files - ok.min(files);
                    let at = Place::AtRisk(s(p, "id"));
                    rsx! {
                        div { key: "{i}", class: "policy",
                            div { class: "polhead",
                                img { src: icon("Lock"), alt: "" }
                                div { class: "pmain",
                                    b { "{s(p, \"name\")}" }
                                    span { class: "s1",
                                        "{p[\"copies\"]} copies · {p[\"media\"]} media · {p[\"offsite\"]} offsite"
                                    }
                                }
                                span { class: if pct == 100 { "score good" } else { "score warn" }, "{pct}%" }
                            }
                            div { class: "meter",
                                div { class: "fill", style: "width: {pct}%" }
                            }
                            span { class: "count", "{ok} of {files} files meet it" }
                            for l in rep["locations"].as_array().cloned().unwrap_or_default() {
                                {
                                    let lf = l["files"].as_u64().unwrap_or(0);
                                    let lo = l["ok"].as_u64().unwrap_or(0);
                                    let lp = if lf == 0 { 100 } else { lo * 100 / lf };
                                    rsx! {
                                        div { class: "locbar",
                                            span { class: "ln", "{lname(l[\"location\"].as_str().unwrap_or_default())}" }
                                            div { class: "meter small",
                                                div { class: "fill", style: "width: {lp}%" }
                                            }
                                            span { class: "count", "{lo} of {lf}" }
                                        }
                                    }
                                }
                            }
                            if short > 0 {
                                button { class: "primary", onclick: move |_| nav.go(at.clone()),
                                    "Show {short} files at risk · {human(risk)}"
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
}

/// The files a policy finds short, in the browser.
#[component]
pub fn AtRisk(space: String, policy: String) -> Element {
    let spine_doc = use_doc(spine::SPINE);
    let reg = use_doc(space_doc(&space));
    let rev = use_reg_rev(&space);
    let kinds = Kinds::load(&spine_doc.loro());
    let name = space::policies(&reg.loro())
        .into_iter()
        .find(|p| p.id == policy)
        .map_or(policy.clone(), |p| p.name);
    let (sp, po) = (space.clone(), policy.clone());
    let rows = use_resource(use_reactive!(|(sp, po, rev)| async move {
        let _ = rev;
        get(format!(
            "/at-risk?space={}&policy={}&limit=500",
            enc(&sp),
            enc(&po)
        ))
        .await
    }));
    let items: Vec<Item> = rows()
        .flatten()
        .and_then(|v| v["results"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|h| crate::browser::hit_item(h, &kinds))
        .collect();
    rsx! {
        div { class: "viewhead",
            div { class: "kindtitle",
                img { src: icon("Lock"), alt: "" }
                h1 { "Short of {name}" }
                span { class: "count", "{items.len()} files" }
            }
        }
        Items {
            items,
            show_at: true,
            empty: "Every file meets it.",
            loading: rows().is_none(),
        }
    }
}
