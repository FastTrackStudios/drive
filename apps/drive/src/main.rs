//! The Drive app: the one inbox, the one calendar, notes, and everything
//! every app keeps. It edits only the spine; app documents show as cards
//! that open their app.

mod files;
mod notes;

use dioxus::prelude::*;
use drive_dioxus::ui::{Avatars, ConnectionStatus};
use drive_dioxus::{DriveProvider, use_doc, use_docs, use_drive};
use drive_spine as spine;

const STYLE: Asset = asset!("/assets/drive.css");

fn main() {
    // Warnings and up: Loro reports its encoding at INFO, which floods the console.
    dioxus::logger::init(dioxus::logger::tracing::Level::WARN).expect("the logger starts once");
    dioxus::launch(App);
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Inbox,
    Calendar,
    Notes,
    Files,
    Devices,
    Storage,
    Everything,
}

#[component]
fn App() -> Element {
    rsx! {
        document::Title { "Drive" }
        document::Meta { name: "viewport", content: "width=device-width, initial-scale=1, viewport-fit=cover" }
        document::Stylesheet { href: STYLE }
        DriveProvider { Shell {} }
    }
}

#[component]
fn Shell() -> Element {
    let mut tab = use_signal(|| Tab::Inbox);
    let spine_doc = use_doc(spine::SPINE);
    let tabs = [
        (Tab::Inbox, "Inbox"),
        (Tab::Calendar, "Calendar"),
        (Tab::Notes, "Notes"),
        (Tab::Files, "Files"),
        (Tab::Devices, "Devices"),
        (Tab::Storage, "Storage"),
        (Tab::Everything, "Everything"),
    ];
    rsx! {
        div { class: "app",
            header { class: "top",
                div { class: "brand",
                    span { class: "mark", "D" }
                    b { "Drive" }
                }
                nav { class: "tabs",
                    for (t , label) in tabs {
                        button {
                            key: "{label}",
                            class: if tab() == t { "tab on" } else { "tab" },
                            onclick: move |_| tab.set(t),
                            "{label}"
                        }
                    }
                }
                div { class: "who",
                    ConnectionStatus {}
                    Avatars { doc: spine_doc.clone() }
                }
            }
            main { class: "body",
                match tab() {
                    Tab::Inbox => rsx! {
                        Inbox {}
                    },
                    Tab::Calendar => rsx! {
                        Calendar {}
                    },
                    Tab::Notes => rsx! {
                        notes::Notes {}
                    },
                    Tab::Files => rsx! {
                        files::Spaces { which: "files" }
                    },
                    Tab::Devices => rsx! {
                        files::Spaces { which: "devices" }
                    },
                    Tab::Storage => rsx! {
                        files::Spaces { which: "storage" }
                    },
                    Tab::Everything => rsx! {
                        Everything {}
                    },
                }
            }
        }
    }
}

fn app_label(app: &str) -> &str {
    match app {
        "studio" => "Studio",
        "meals" => "Meals",
        "frame" => "Frame",
        "drive" => "Drive",
        other => other,
    }
}

/// The one inbox: anything from anywhere, live for everyone.
#[component]
fn Inbox() -> Element {
    let doc = use_doc(spine::SPINE);
    let ctx = use_drive();
    let mut draft = use_signal(String::new);
    let mut filter = use_signal(|| "Open");
    let mut editing = use_signal(|| None::<String>);
    let tasks = spine::tasks(&doc.loro());
    let others = doc.others();
    let open = tasks.iter().filter(|t| !t.done).count();
    let shown: Vec<spine::Task> = tasks
        .into_iter()
        .filter(|t| match filter() {
            "Open" => !t.done,
            "Done" => t.done,
            _ => true,
        })
        .collect();
    let mut add = {
        let doc = doc.clone();
        let ctx = ctx.clone();
        move || {
            let title = draft().trim().to_string();
            if !title.is_empty() {
                let by = ctx.me.peek().name.clone();
                doc.edit(|d| spine::add_task(d, &title, "drive", "", &by));
                draft.set(String::new());
            }
        }
    };
    rsx! {
        section { class: "pane",
            div { class: "panehead",
                h1 { "Inbox" }
                span { class: "count", "{open} open" }
                div { class: "seg",
                    for f in ["Open", "All", "Done"] {
                        button {
                            key: "{f}",
                            class: if filter() == f { "on" } else { "" },
                            onclick: move |_| filter.set(f),
                            "{f}"
                        }
                    }
                }
            }
            div { class: "capture",
                input {
                    placeholder: "Capture anything, then Enter",
                    value: "{draft}",
                    oninput: move |e| draft.set(e.value()),
                    onkeydown: {
                        let mut add = add.clone();
                        move |e: KeyboardEvent| {
                            if e.key().to_string() == "Enter" {
                                add();
                            }
                        }
                    },
                }
                button { class: "primary", onclick: move |_| add(), "Add" }
            }
            ul { class: "tasks",
                if shown.is_empty() {
                    li { class: "empty", "Nothing here." }
                }
                for t in shown {
                    {
                        let here: Vec<_> = others
                            .iter()
                            .filter(|p| p.data.get("focus").and_then(|f| f.as_str()) == Some(t.id.as_str()))
                            .cloned()
                            .collect();
                        let ring = here
                            .first()
                            .map(|p| format!("box-shadow:inset 3px 0 0 oklch(.62 .16 {});", p.hue))
                            .unwrap_or_default();
                        let id = t.id.clone();
                        rsx! {
                            li { key: "{t.id}", class: if t.done { "task done" } else { "task" }, style: "{ring}",
                                input {
                                    r#type: "checkbox",
                                    checked: t.done,
                                    onchange: {
                                        let doc = doc.clone();
                                        let id = id.clone();
                                        move |e: FormEvent| {
                                            let on = e.checked();
                                            doc.edit(|d| spine::set_done(d, &id, on));
                                        }
                                    },
                                }
                                if editing() == Some(t.id.clone()) {
                                    input {
                                        class: "rename",
                                        value: "{t.title}",
                                        onmounted: move |e| async move {
                                            let _ = e.data().set_focus(true).await;
                                        },
                                        onfocus: {
                                            let doc = doc.clone();
                                            let id = id.clone();
                                            move |_| doc.set_presence(&serde_json::json!({ "focus": id }))
                                        },
                                        onchange: {
                                            let doc = doc.clone();
                                            let id = id.clone();
                                            move |e: FormEvent| {
                                                let v = e.value();
                                                doc.edit(|d| spine::rename_task(d, &id, &v));
                                                editing.set(None);
                                                doc.set_presence(&serde_json::json!({}));
                                            }
                                        },
                                    }
                                } else {
                                    span {
                                        class: "title",
                                        ondoubleclick: {
                                            let id = id.clone();
                                            move |_| editing.set(Some(id.clone()))
                                        },
                                        "{t.title}"
                                    }
                                }
                                for p in here {
                                    span { key: "{p.peer}", class: "editing", style: "--h:{p.hue}", "{p.name} is editing" }
                                }
                                span { class: "app", "{app_label(&t.app)}" }
                                if !t.by.is_empty() {
                                    span { class: "by", "{t.by}" }
                                }
                                if !t.link.is_empty() {
                                    a { class: "open", href: "{t.link}", "Open" }
                                }
                                span { class: "acts",
                                    button {
                                        title: "Up",
                                        onclick: {
                                            let doc = doc.clone();
                                            let id = id.clone();
                                            move |_| doc.edit(|d| spine::move_task(d, &id, -1))
                                        },
                                        "↑"
                                    }
                                    button {
                                        title: "Down",
                                        onclick: {
                                            let doc = doc.clone();
                                            let id = id.clone();
                                            move |_| doc.edit(|d| spine::move_task(d, &id, 1))
                                        },
                                        "↓"
                                    }
                                    button {
                                        title: "Delete",
                                        onclick: {
                                            let doc = doc.clone();
                                            let id = id.clone();
                                            move |_| doc.edit(|d| spine::remove_task(d, &id))
                                        },
                                        "✕"
                                    }
                                }
                            }
                        }
                    }
                }
            }
            p { class: "hint", "Double-click a task to rename it. Studio and Meals add tasks here too." }
        }
    }
}

/// The one calendar: every app's events, by day.
#[component]
fn Calendar() -> Element {
    let doc = use_doc(spine::SPINE);
    let events = spine::events(&doc.loro());
    let mut title = use_signal(String::new);
    let mut date = use_signal(|| today());
    let mut time = use_signal(|| "7:00 PM".to_string());
    let mut days: Vec<String> = events.iter().map(|e| e.date.clone()).collect();
    days.dedup();
    let add = {
        let doc = doc.clone();
        move |_| {
            let t = title().trim().to_string();
            if t.is_empty() {
                return;
            }
            doc.edit(|d| {
                let id = spine::new_id(d, "e");
                spine::put_event(
                    d,
                    &spine::Event {
                        id,
                        title: t,
                        app: "drive".into(),
                        link: String::new(),
                        date: date(),
                        time: time(),
                        place: String::new(),
                    },
                )
            });
            title.set(String::new());
        }
    };
    rsx! {
        section { class: "pane",
            div { class: "panehead",
                h1 { "Calendar" }
                span { class: "count", "{events.len()} events" }
            }
            div { class: "capture",
                input {
                    placeholder: "Meeting, rehearsal, anything",
                    value: "{title}",
                    oninput: move |e| title.set(e.value()),
                }
                input {
                    r#type: "date",
                    value: "{date}",
                    oninput: move |e| date.set(e.value()),
                }
                input {
                    class: "time",
                    value: "{time}",
                    oninput: move |e| time.set(e.value()),
                }
                button { class: "primary", onclick: add, "Add" }
            }
            if events.is_empty() {
                p { class: "empty", "No events yet. Plan a set in Studio or a prep session in Meals." }
            }
            for day in days.iter() {
                div { key: "{day}", class: "day",
                    h2 { "{day_label(day)}" }
                    for e in events.iter().filter(|e| &e.date == day) {
                        div { key: "{e.id}", class: "event",
                            span { class: "time", "{e.time}" }
                            span { class: "etitle", "{e.title}" }
                            if !e.place.is_empty() {
                                span { class: "place", "{e.place}" }
                            }
                            span { class: "app", "{app_label(&e.app)}" }
                            if !e.link.is_empty() {
                                a { class: "open", href: "{e.link}", "Open" }
                            }
                            button {
                                class: "x",
                                title: "Remove",
                                onclick: {
                                    let doc = doc.clone();
                                    let id = e.id.clone();
                                    move |_| doc.edit(|d| spine::remove_event(d, &id))
                                },
                                "✕"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Everything the node keeps, by app, with who is in it right now.
#[component]
fn Everything() -> Element {
    let docs = use_docs("");
    let list = docs();
    let mut apps: Vec<&str> = list.iter().map(|d| d.id.split(':').next().unwrap_or("")).collect();
    apps.dedup();
    rsx! {
        section { class: "pane",
            div { class: "panehead",
                h1 { "Everything" }
                span { class: "count", "{list.len()} documents" }
            }
            p { class: "hint", "Every document on this node, from every app. Live: the dot shows who has it open." }
            for app in apps {
                div { key: "{app}", class: "group",
                    h2 { "{app_label(app)}" }
                    div { class: "cards",
                        for d in list.iter().filter(|d| d.id.split(':').next() == Some(app)) {
                            a { key: "{d.id}", class: "card", href: "{link_for(&d.id)}",
                                b { "{name_for(&d.id)}" }
                                span { class: "meta",
                                    if d.peers > 0 {
                                        span { class: "live" }
                                        "{d.peers} here · "
                                    }
                                    "{kb(d.bytes)}"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn link_for(id: &str) -> String {
    let mut parts = id.splitn(3, ':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some("studio"), Some("set"), Some(set)) => format!("/studio/set/{set}"),
        (Some("meals"), _, _) => "/meals/".into(),
        (Some("frame"), Some(rest), more) => format!("/frame/?doc={}", more.map_or(rest.to_string(), |m| format!("{rest}:{m}"))),
        _ => "/".into(),
    }
}

fn name_for(id: &str) -> String {
    id.split_once(':').map_or(id.to_string(), |(_, rest)| rest.replace(':', " · "))
}

fn kb(bytes: u64) -> String {
    if bytes < 1024 { format!("{bytes} B") } else { format!("{:.1} KB", bytes as f64 / 1024.0) }
}

fn today() -> String {
    let d = js_sys::Date::new_0();
    format!("{:04}-{:02}-{:02}", d.get_full_year(), d.get_month() + 1, d.get_date())
}

fn day_label(iso: &str) -> String {
    let d = js_sys::Date::new(&format!("{iso}T12:00:00").into());
    if d.get_time().is_nan() {
        return iso.to_string();
    }
    const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let label = format!("{}, {} {}", DAYS[d.get_day() as usize], MONTHS[d.get_month() as usize], d.get_date());
    if iso == today() { format!("Today · {label}") } else { label }
}
