//! The Drive app: the file browser for everything a space keeps (its own
//! synced tree, every computer's locations, storages, the apps' documents),
//! with its protection, its devices, and the spine (the one inbox, the one
//! calendar, notes).

mod browse;
mod browser;
mod catalog;
mod files;
mod host;
mod mobile;
mod nav;
mod notes;
mod orgs;
mod projects;
mod rules;
mod shell;
mod sites;
mod tags;

use dioxus::prelude::*;
use drive_dioxus::{DriveProvider, use_doc, use_docs, use_drive};
use drive_spine as spine;

const STYLE: Asset = asset!("/assets/drive.css");

fn main() {
    // Warnings and up: Loro reports its encoding at INFO, which floods the console.
    // Natively: logs to stderr and traces to an OTLP collector when one is
    // named (OTEL_EXPORTER_OTLP_TRACES_ENDPOINT); in a browser, the console.
    #[cfg(feature = "native")]
    let _telemetry = app_shell_native::telemetry::init("drive-app", "warn,drive=info,drive_dioxus=info");
    #[cfg(not(feature = "native"))]
    dioxus::logger::init(dioxus::logger::tracing::Level::WARN).expect("the logger starts once");
    // Drawn by Blitz on a desktop or a phone: the window has no system
    // frame, the shell's title bar is its frame (app-shell-native). Its
    // timers and HTTP are Tokio's and Blitz's event loop runs none, so a
    // runtime is started here and kept for the app's life.
    #[cfg(feature = "native")]
    {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a Tokio runtime");
        let _entered = runtime.enter();
        app_shell_native::launch(
            App,
            no_popout,
            app_shell_native::Config {
                title: "Drive".to_owned(),
                size: (1440.0, 900.0),
                system_decorations: false,
                // `shot <path.png>` on the app's control socket
                // ($XDG_RUNTIME_DIR/app-shell/drive.sock).
                control: Some("drive".to_owned()),
            },
        );
    }
    #[cfg(not(feature = "native"))]
    dioxus::launch(App);
}

/// Drive opens no pop-out windows yet.
#[cfg(feature = "native")]
fn no_popout(_: app_shell::WindowId) -> Element {
    rsx! {}
}

#[component]
fn App() -> Element {
    rsx! {
        document::Title { "Drive" }
        document::Meta { name: "viewport", content: "width=device-width, initial-scale=1, viewport-fit=cover" }
        document::Stylesheet { href: STYLE }
        document::Stylesheet { href: tags::STYLE }
        document::Stylesheet { href: rules::STYLE }
        document::Stylesheet { href: mobile::STYLE }
        DriveProvider { shell::Browser {} }
    }
}

use browser::app_label;

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
                                        // Left unchanged (no change event) or with
                                        // Escape: stop editing, and stop showing
                                        // others "is editing" (drive#64).
                                        onblur: {
                                            let doc = doc.clone();
                                            move |_| {
                                                editing.set(None);
                                                doc.set_presence(&serde_json::json!({}));
                                            }
                                        },
                                        onkeydown: {
                                            let doc = doc.clone();
                                            move |e: KeyboardEvent| {
                                                if e.key().to_string() == "Escape" {
                                                    editing.set(None);
                                                    doc.set_presence(&serde_json::json!({}));
                                                }
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
                                if let Some(link) = browser::safe_href(t.link.clone()) {
                                    a { class: "open", href: "{link}", "Open" }
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
    // From the earliest one-off event (or today) to twelve weeks ahead:
    // recurring events show each of their meetings in that span.
    let stored = spine::events(&doc.loro());
    let now = today();
    let from = stored
        .iter()
        .filter(|e| e.repeat.is_empty() && !e.date.is_empty())
        .map(|e| e.date.clone())
        .min()
        .filter(|d| *d < now)
        .unwrap_or_else(|| now.clone());
    let to = spine::recur::days(&now)
        .map(|d| spine::recur::iso(d + 84))
        .unwrap_or_else(|| now.clone());
    let events = spine::calendar(&doc.loro(), &from, &to);
    let mut title = use_signal(String::new);
    let mut date = use_signal(|| today());
    let mut time = use_signal(|| "7:00 PM".to_string());
    let mut weekly = use_signal(|| false);
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
                        date: date(),
                        time: time(),
                        // Every week on the same weekday as its first date.
                        repeat: if weekly() {
                            spine::recur::days(&date())
                                .map(|d| {
                                    spine::recur::Rule::weekly(&[spine::recur::weekday(d)])
                                        .to_string()
                                })
                                .unwrap_or_default()
                        } else {
                            String::new()
                        },
                        ..Default::default()
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
                label { class: "weekly",
                    input {
                        r#type: "checkbox",
                        checked: weekly(),
                        onchange: move |e: FormEvent| weekly.set(e.checked()),
                    }
                    " Every week"
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
                            span { class: "etitle",
                                "{e.title}"
                                if !e.series.is_empty() {
                                    span { class: "repeats", title: "A recurring event", " ↻" }
                                }
                            }
                            if !e.place.is_empty() {
                                span { class: "place", "{e.place}" }
                            }
                            span { class: "app", "{app_label(&e.app)}" }
                            if let Some(link) = browser::safe_href(e.link.clone()) {
                                a { class: "open", href: "{link}", "Open" }
                            }
                            button {
                                class: "x",
                                title: if e.is_generated() { "Skip this one" } else { "Remove" },
                                onclick: {
                                    let doc = doc.clone();
                                    let ev = e.clone();
                                    // One meeting of a recurring event is left
                                    // out; anything else is removed.
                                    move |_| {
                                        doc.edit(|d| {
                                            if ev.is_generated() {
                                                spine::skip_occurrence(d, &ev.series, &ev.occurrence, true)
                                            } else {
                                                spine::remove_event(d, &ev.id)
                                            }
                                        })
                                    }
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

fn document_apps<'a>(ids: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let mut apps: Vec<_> = ids
        .into_iter()
        .map(|id| id.split(':').next().unwrap_or(""))
        .collect();
    apps.sort_unstable();
    apps.dedup();
    apps
}

#[cfg(test)]
#[test]
fn document_app_groups_are_unique_for_interleaved_documents() {
    assert_eq!(
        document_apps(["studio:set:one", "meals:recipe:two", "studio:set:three"]),
        ["meals", "studio"]
    );
}

/// Everything the node keeps, by app, with who is in it right now.
#[component]
fn Everything() -> Element {
    let docs = use_docs("");
    let list = docs();
    let apps = document_apps(list.iter().map(|doc| doc.id.as_str()));
    rsx! {
        section { class: "pane",
            div { class: "panehead",
                h1 { "Raw documents" }
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
        (Some("frame"), Some(rest), more) => format!(
            "/frame/?doc={}",
            more.map_or(rest.to_string(), |m| format!("{rest}:{m}"))
        ),
        _ => "/".into(),
    }
}

fn name_for(id: &str) -> String {
    id.split_once(':')
        .map_or(id.to_string(), |(_, rest)| rest.replace(':', " · "))
}

fn kb(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}

fn today() -> String {
    let (month, day, year) = host::date_parts(host::now_ms());
    format!("{year:04}-{:02}-{day:02}", month + 1)
}

fn day_label(iso: &str) -> String {
    let mut parts = iso.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (Some(Some(year)), Some(Some(month)), Some(Some(day))) =
        (parts.next(), parts.next(), parts.next())
    else {
        return iso.to_string();
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return iso.to_string();
    }
    let (month, day) = (month as u32, day as u32);
    const DAYS: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let label = format!(
        "{}, {} {}",
        DAYS[host::weekday(year, month, day)],
        MONTHS[(month - 1) as usize],
        day
    );
    if iso == today() {
        format!("Today · {label}")
    } else {
        label
    }
}
