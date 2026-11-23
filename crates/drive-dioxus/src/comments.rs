//! The comments panel every app embeds: a thing's threads (who, when,
//! where in it), replies, resolving, and approving or sending back a
//! version. Live and offline like any document; the model is
//! [`drive_spine::comments`].
//!
//! ```ignore
//! CommentsPanel { target: Target::File(id), version: hash, now_ms: player_time, on_seek: move |ms| seek(ms) }
//! ```

use dioxus::prelude::*;
use drive_spine::comments::{self, Anchor, New, Target, Thread};

use crate::{use_doc, use_drive};

fn now() -> i64 {
    crate::env::now_ms() as i64
}

fn clock(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

/// "just now", "5 min ago", "yesterday", or the date.
pub fn ago(ms: i64) -> String {
    let d = (now() - ms).max(0) / 1000;
    match d {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", d / 60),
        3600..86_400 => format!("{} h ago", d / 3600),
        86_400..172_800 => "yesterday".into(),
        _ => {
            let (month, day, year) = crate::env::date_parts(ms as f64);
            const M: [&str; 12] = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ];
            format!("{} {}, {}", M[month as usize % 12], day, year)
        }
    }
}

/// Where a comment points, in words.
fn place(a: &Anchor) -> Option<String> {
    match a {
        Anchor::Whole => None,
        Anchor::Time { start_ms, end_ms } if end_ms > start_ms => {
            Some(format!("{}–{}", clock(*start_ms), clock(*end_ms)))
        }
        Anchor::Time { start_ms, .. } => Some(clock(*start_ms)),
        Anchor::Region { page, .. } => Some(format!("p. {}", page + 1)),
        Anchor::Text { quote, .. } => {
            let q: String = quote.chars().take(40).collect();
            Some(format!(
                "“{q}{}”",
                if quote.chars().count() > 40 {
                    "…"
                } else {
                    ""
                }
            ))
        }
    }
}

const CSS: &str = "
.dcm{display:flex;flex-direction:column;gap:8px;font:14px/1.45 -apple-system,system-ui,sans-serif;color:var(--ink,#f4f4f5)}
.dcm-head{display:flex;align-items:center;gap:8px;flex-wrap:wrap}
.dcm-head b{font-size:12px;letter-spacing:.06em;text-transform:uppercase;color:var(--ink-3,#8b8b94)}
.dcm-rev{margin-left:auto;display:flex;gap:4px}
.dcm-rev button,.dcm-act button,.dcm-compose button{border:0;border-radius:8px;padding:4px 9px;font:inherit;font-size:12.5px;font-weight:650;cursor:pointer;background:var(--surface-2,#1f1f23);color:var(--ink-2,#d4d4d8)}
.dcm-rev button.on-ok{background:rgba(34,197,94,.18);color:#86efac}
.dcm-rev button.on-no{background:rgba(240,194,75,.18);color:#f5d68a}
.dcm-t{background:var(--surface,#141417);border:1px solid #1d1d22;border-radius:12px;padding:9px 11px}
.dcm-t.res{opacity:.6}
.dcm-c{display:flex;flex-direction:column;gap:2px}
.dcm-meta{display:flex;align-items:center;gap:6px;flex-wrap:wrap;font-size:12.5px;color:var(--ink-3,#8b8b94)}
.dcm-meta b{color:var(--ink,#f4f4f5);font-size:13.5px}
.dcm-at{border:0;border-radius:7px;padding:0 7px;background:#2dd4bf;color:#042f2a;font-weight:700;font-size:12px;cursor:pointer}
.dcm-old{font-size:11px;padding:0 6px;border-radius:6px;background:var(--surface-2,#1f1f23)}
.dcm-body{white-space:pre-wrap;overflow-wrap:anywhere}
.dcm-reply{margin:6px 0 0 12px;padding-left:9px;border-left:2px solid var(--rule,#2b2b31)}
.dcm-act{display:flex;gap:4px;margin-top:5px}
.dcm-compose{display:flex;flex-direction:column;gap:6px}
.dcm-compose textarea{min-height:56px;resize:vertical;background:var(--surface,#141417);color:inherit;border:1px solid var(--rule,#2b2b31);border-radius:10px;padding:8px 10px;font:inherit;outline:none}
.dcm-compose textarea:focus{border-color:var(--accent,#2dd4bf)}
.dcm-row{display:flex;align-items:center;gap:8px}
.dcm-row label{font-size:12.5px;color:var(--ink-3,#8b8b94);display:flex;align-items:center;gap:5px}
.dcm-row button.send{margin-left:auto;background:var(--ink,#f4f4f5);color:#000}
.dcm-empty{color:var(--ink-3,#8b8b94);font-size:13px}
";

/// A thing's comments. `version`: the file version on show (its content
/// hash; "" for documents). `now_ms`: the player's time, when there is a
/// player (a new comment can be pinned there). `on_seek`: jump the player
/// to a comment's time.
#[component]
pub fn CommentsPanel(
    target: Target,
    #[props(default)] version: String,
    #[props(default)] now_ms: Option<i64>,
    #[props(default)] on_seek: Option<EventHandler<i64>>,
    /// The DOM id of an `<audio>`/`<video>` element on the page: comments
    /// can be pinned at its current time, and a time plays it from there.
    #[props(default)]
    player: Option<String>,
    /// The org that owns the target (when viewing a shared project from
    /// another org); empty defaults to the active org.
    #[props(default)]
    space: String,
) -> Element {
    let has_clock = now_ms.is_some() || player.is_some();
    let read_clock = {
        let player = player.clone();
        move || -> Option<i64> {
            if let Some(ms) = now_ms {
                return Some(ms);
            }
            let id = player.as_ref()?;
            crate::env::eval_f64(&format!(
                "(document.getElementById({id:?})||{{}}).currentTime"
            ))
            .map(|s| (s * 1000.0).round() as i64)
        }
    };
    let seek: Option<EventHandler<i64>> = match (on_seek, player.clone()) {
        (Some(h), _) => Some(h),
        (None, Some(id)) => Some(EventHandler::new(move |ms: i64| {
            crate::env::eval(&format!(
                "(()=>{{const p=document.getElementById({id:?});if(p){{p.currentTime={};p.play()}}}})()",
                ms as f64 / 1000.0
            ));
        })),
        _ => None,
    };
    let ctx = use_drive();
    let doc_id = if space.is_empty() {
        target.doc_id()
    } else {
        drive_proto::qualify(&space, &target.doc_id())
    };
    let doc = use_doc(doc_id);
    let mut draft = use_signal(String::new);
    let mut pin = use_signal(|| true);
    let mut replying = use_signal(|| None::<String>);
    let mut reply = use_signal(String::new);
    let mut show_resolved = use_signal(|| false);
    let author = if ctx.who.is_empty() {
        ctx.me.peek().name.clone()
    } else {
        ctx.who.clone()
    };
    let by = ctx.me.peek().name.clone();
    let d = doc.loro();
    let threads: Vec<Thread> = comments::threads(&d)
        .into_iter()
        .filter(|t| !t.root.deleted || !t.replies.is_empty())
        .collect();
    let open = threads.iter().filter(|t| !t.root.resolved).count();
    let resolved = threads.len() - open;
    let review = comments::reviews(&d)
        .into_iter()
        .find(|r| r.version == version);
    let shown: Vec<Thread> = threads
        .into_iter()
        .filter(|t| show_resolved() || !t.root.resolved)
        .collect();

    let send = {
        let (doc, author, by, version) = (doc.clone(), author.clone(), by.clone(), version.clone());
        move |_| {
            let text = draft().trim().to_string();
            if text.is_empty() {
                return;
            }
            let anchor = match (pin().then(&read_clock).flatten(), has_clock) {
                (Some(ms), true) => Anchor::Time {
                    start_ms: ms,
                    end_ms: ms,
                },
                _ => Anchor::Whole,
            };
            doc.edit(|d| {
                let _ = comments::add(
                    d,
                    New {
                        author: &author,
                        by: &by,
                        body: &text,
                        anchor,
                        version: &version,
                        parent: None,
                    },
                    now(),
                );
            });
            draft.set(String::new());
        }
    };
    let rstate = review.as_ref().map(|r| r.state.clone()).unwrap_or_default();
    let set_review = {
        let (doc, author, version, current) =
            (doc.clone(), author.clone(), version.clone(), rstate.clone());
        move |state: &'static str| {
            let next = if current == state { "" } else { state };
            doc.edit(|d| {
                let _ = comments::set_review(d, &version, next, &author, now());
            });
        }
    };
    let (approve, changes) = (set_review.clone(), set_review);
    rsx! {
        document::Style { {CSS} }
        div { class: "dcm",
            div { class: "dcm-head",
                b { "Comments · {open}" }
                if resolved > 0 {
                    button {
                        class: "dcm-old",
                        style: "border:0;cursor:pointer;color:inherit",
                        onclick: move |_| {
                            let v = !show_resolved();
                            show_resolved.set(v);
                        },
                        if show_resolved() { "hide {resolved} resolved" } else { "show {resolved} resolved" }
                    }
                }
                if !version.is_empty() {
                    div { class: "dcm-rev",
                        button {
                            class: if rstate == "approved" { "on-ok" } else { "" },
                            title: "Approve this version",
                            onclick: move |_| approve("approved"),
                            "Approve"
                        }
                        button {
                            class: if rstate == "changes" { "on-no" } else { "" },
                            title: "Ask for changes to this version",
                            onclick: move |_| changes("changes"),
                            "Changes"
                        }
                    }
                }
            }
            if let Some(r) = &review {
                span { class: "dcm-empty",
                    if r.state == "approved" { "Approved by {r.by}, {ago(r.at)}" } else { "Changes asked by {r.by}, {ago(r.at)}" }
                }
            }
            if shown.is_empty() {
                span { class: "dcm-empty", "No comments yet." }
            }
            for t in shown {
                {
                    let rid = t.root.id.clone();
                    let (doc_r, doc_d) = (doc.clone(), doc.clone());
                    let (rid_r, rid_d, rid_p) = (rid.clone(), rid.clone(), rid.clone());
                    let mine = t.root.author == author;
                    let res = t.root.resolved;
                    let author_r = author.clone();
                    let older = !version.is_empty() && !t.root.version.is_empty() && t.root.version != version;
                    rsx! {
                        div { key: "{rid}", class: if res { "dcm-t res" } else { "dcm-t" },
                            Comment1 { c: t.root.clone(), older, on_seek: seek }
                            for x in t.replies.iter() {
                                div { key: "{x.id}", class: "dcm-reply",
                                    Comment1 { c: x.clone(), older: false, on_seek: None }
                                }
                            }
                            div { class: "dcm-act",
                                button { onclick: move |_| replying.set(Some(rid_p.clone())), "Reply" }
                                button {
                                    onclick: move |_| {
                                        doc_r.edit(|d| {
                                            let _ = comments::resolve(d, &rid_r, !res, &author_r, now());
                                        });
                                    },
                                    if res { "Reopen" } else { "Resolve" }
                                }
                                if mine {
                                    button {
                                        onclick: move |_| {
                                            doc_d.edit(|d| {
                                                let _ = comments::delete(d, &rid_d);
                                            });
                                        },
                                        "Delete"
                                    }
                                }
                            }
                            if replying() == Some(rid.clone()) {
                                {
                                    let (doc, author, by, version, parent) = (doc.clone(), author.clone(), by.clone(), version.clone(), rid.clone());
                                    rsx! {
                                        div { class: "dcm-compose", style: "margin-top:6px",
                                            textarea {
                                                placeholder: "Reply",
                                                value: "{reply}",
                                                oninput: move |e| reply.set(e.value()),
                                            }
                                            div { class: "dcm-row",
                                                button { onclick: move |_| replying.set(None), "Cancel" }
                                                button {
                                                    class: "send",
                                                    onclick: move |_| {
                                                        let text = reply().trim().to_string();
                                                        if !text.is_empty() {
                                                            doc.edit(|d| {
                                                                let _ = comments::add(
                                                                    d,
                                                                    New { author: &author, by: &by, body: &text, anchor: Anchor::Whole, version: &version, parent: Some(&parent) },
                                                                    now(),
                                                                );
                                                            });
                                                        }
                                                        reply.set(String::new());
                                                        replying.set(None);
                                                    },
                                                    "Reply"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            div { class: "dcm-compose",
                textarea {
                    placeholder: "Add a comment",
                    value: "{draft}",
                    oninput: move |e| draft.set(e.value()),
                }
                div { class: "dcm-row",
                    if has_clock {
                        label {
                            input {
                                r#type: "checkbox",
                                checked: pin(),
                                onchange: move |e: FormEvent| pin.set(e.checked()),
                            }
                            match now_ms {
                                Some(ms) => rsx! { "at {clock(ms)}" },
                                None => rsx! { "at the current time" },
                            }
                        }
                    }
                    button { class: "send", onclick: send, "Comment" }
                }
            }
        }
    }
}

#[component]
fn Comment1(c: comments::Comment, older: bool, on_seek: Option<EventHandler<i64>>) -> Element {
    let at = match c.anchor {
        Anchor::Time { start_ms, .. } => Some(start_ms),
        _ => None,
    };
    let where_ = place(&c.anchor);
    rsx! {
        div { class: "dcm-c",
            div { class: "dcm-meta",
                if let (Some(ms), Some(label)) = (at, where_.clone()) {
                    button {
                        class: "dcm-at",
                        title: "Play from here",
                        onclick: move |_| {
                            if let Some(h) = on_seek {
                                h.call(ms)
                            }
                        },
                        "{label}"
                    }
                } else if let Some(label) = where_ {
                    span { class: "dcm-old", "{label}" }
                }
                b { "{c.by}" }
                span { "{ago(c.created)}" }
                if c.edited > 0 {
                    span { "· edited" }
                }
                if older {
                    span { class: "dcm-old", "earlier version" }
                }
                if c.resolved {
                    span { class: "dcm-old", "resolved by {c.resolved_by}" }
                }
            }
            if c.deleted {
                div { class: "dcm-body", style: "font-style:italic;opacity:.6", "Deleted" }
            } else {
                div { class: "dcm-body", "{c.body}" }
            }
        }
    }
}
