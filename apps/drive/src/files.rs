//! A space's devices (online or not, their roles, pairing a new one) and
//! its storages, live.

use dioxus::prelude::*;
use drive_dioxus::{use_doc, use_drive};
use drive_files::{space, space_doc};

use crate::browser::online;

/// Devices and storages, with the debug switch for raw documents.
#[component]
pub fn DevicesPage(space: String) -> Element {
    let mut nav = crate::browser::use_nav();
    if space.is_empty() {
        return rsx! {
            div { class: "pane",
                p { class: "empty",
                    "No space yet. On a computer: "
                    code { "drive-agent init --node <this node>/sync --space home --name \"My PC\"" }
                }
            }
        };
    }
    rsx! {
        div { class: "pane",
            h2 { class: "sec", "Devices" }
            Devices { space: space.clone() }
            h2 { class: "sec", "Storage" }
            Storage { space: space.clone() }
            h2 { class: "sec", "Developer" }
            label { class: "pin",
                input {
                    r#type: "checkbox",
                    checked: (nav.debug)(),
                    onchange: move |e: FormEvent| nav.debug.set(e.checked()),
                }
                span { "Show raw documents in the sidebar" }
            }
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
    js_sys::eval("location.host")
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_default()
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
