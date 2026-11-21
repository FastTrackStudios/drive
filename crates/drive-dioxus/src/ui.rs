//! The pieces every Drive app shows the same way: who is here, whether
//! you are connected, and other people's cursors. Inline styles, so they
//! look the same in any app.

use dioxus::prelude::*;

use crate::{Doc, Me, Status, use_drive};

fn initials(name: &str) -> String {
    let parts: Vec<&str> = name.split_whitespace().collect();
    match parts.as_slice() {
        [] => "?".into(),
        [one] => one.chars().take(2).collect(),
        [.., last] => {
            let first = parts[0].chars().next().unwrap_or('?');
            format!("{first}{}", last.chars().next().unwrap_or('?'))
        }
    }
}

/// You and everyone else in a document, as a stack of coloured initials.
/// Click your own to rename yourself.
#[component]
pub fn Avatars(doc: Doc) -> Element {
    let ctx = use_drive();
    let me = (ctx.me)();
    let others = doc.others();
    let mut editing = use_signal(|| false);
    let mut draft = use_signal(String::new);
    let shown = others.len().min(5);
    let extra = others.len().saturating_sub(5);
    rsx! {
        div { style: "display:flex;align-items:center;gap:0;flex-direction:row-reverse;",
            if extra > 0 {
                span { style: "{AV} background:#2a2a30;color:#ddd;", "+{extra}" }
            }
            for p in others.iter().take(shown) {
                span {
                    key: "{p.peer}",
                    title: "{p.name}",
                    style: "{AV} background:oklch(.62 .16 {p.hue});",
                    "{initials(&p.name)}"
                }
            }
            if editing() {
                input {
                    style: "height:30px;border-radius:15px;border:1px solid #444;background:#111;color:#eee;padding:0 10px;font:inherit;font-size:13px;width:140px;",
                    value: "{draft}",
                    oninput: move |e| draft.set(e.value()),
                    onkeydown: {
                        let ctx = ctx.clone();
                        let me = me.clone();
                        move |e: KeyboardEvent| {
                            if e.key().to_string() == "Enter" {
                                let name = draft().trim().to_string();
                                if !name.is_empty() {
                                    ctx.set_me(Me { name, hue: me.hue });
                                }
                                editing.set(false);
                            }
                        }
                    },
                }
            } else {
                button {
                    title: "You ({me.name}); click to rename",
                    style: "{AV} background:oklch(.62 .16 {me.hue});box-shadow:0 0 0 2px #fff;cursor:pointer;border:0;",
                    onclick: move |_| {
                        draft.set(me.name.clone());
                        editing.set(true);
                    },
                    "{initials(&me.name)}"
                }
            }
        }
    }
}

const AV: &str = "width:30px;height:30px;border-radius:15px;display:grid;place-items:center;font:600 12px/1 system-ui,sans-serif;color:#fff;margin-left:-6px;border:2px solid #0e0e11;";

/// Online, connecting, or offline (with edits kept here).
#[component]
pub fn ConnectionStatus() -> Element {
    let ctx = use_drive();
    let (col, label) = match (ctx.status)() {
        Status::Online => ("#22c55e", "Live"),
        Status::Connecting => ("#f0c24b", "Connecting"),
        Status::Offline => ("#ef4444", "Offline · edits kept"),
    };
    rsx! {
        span { style: "display:inline-flex;align-items:center;gap:6px;font:500 12px/1 system-ui,sans-serif;color:#a1a1aa;",
            span { style: "width:8px;height:8px;border-radius:4px;background:{col};" }
            "{label}"
        }
    }
}

/// Other people's cursors, for presence that carries `"cursor": [x, y]`
/// in the coordinates of the element this is placed in (position it
/// absolutely over that element, `inset: 0`).
#[component]
pub fn Cursors(doc: Doc) -> Element {
    let others = doc.others();
    rsx! {
        div { style: "position:absolute;inset:0;pointer-events:none;overflow:hidden;",
            for p in others {
                if let Some([x, y]) = cursor_of(&p.data) {
                    div {
                        key: "{p.peer}",
                        style: "position:absolute;left:0;top:0;transform:translate({x}px,{y}px);transition:transform .08s linear;",
                        svg {
                            width: "18",
                            height: "18",
                            view_box: "0 0 18 18",
                            path {
                                d: "M2 2 L16 8 L9 10 L7 16 Z",
                                fill: "oklch(.62 .16 {p.hue})",
                                stroke: "white",
                                stroke_width: "1.2",
                            }
                        }
                        span { style: "position:absolute;left:14px;top:14px;white-space:nowrap;padding:2px 7px;border-radius:9px;background:oklch(.62 .16 {p.hue});color:#fff;font:600 11px/1.4 system-ui,sans-serif;",
                            "{p.name}"
                        }
                    }
                }
            }
        }
    }
}

/// A presence value's `cursor`, if it has one.
pub fn cursor_of(data: &serde_json::Value) -> Option<[f64; 2]> {
    let c = data.get("cursor")?.as_array()?;
    Some([c.first()?.as_f64()?, c.get(1)?.as_f64()?])
}
