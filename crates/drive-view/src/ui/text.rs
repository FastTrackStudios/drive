//! Markdown, plain text and code: read, and edited in place.

use dioxus::prelude::*;

use super::{Src, element, js_str};
use crate::{Anchor, ViewKind};

/// Lines shown in the compact form.
const COMPACT_LINES: usize = 60;
/// Lines shown at most (a log of millions of lines is a download).
const MAX_LINES: usize = 20_000;

#[component]
pub fn TextView(
    src: Src,
    view: ViewKind,
    compact: bool,
    on_anchor: Option<EventHandler<Anchor>>,
    on_save: Option<EventHandler<Vec<u8>>>,
) -> Element {
    let id = use_hook(super::fresh_id);
    let mut text = use_signal(|| None::<Result<String, String>>);
    let mut editing = use_signal(|| false);
    let mut draft = use_signal(String::new);
    let mut rendered = use_signal(|| true);
    use_hook({
        let src = src.clone();
        move || {
            spawn(async move {
                let t = src
                    .bytes()
                    .await
                    .map(|b| String::from_utf8_lossy(&b).into_owned());
                text.set(Some(t));
            });
        }
    });
    let body = match text() {
        None => return rsx! { p { class: "dv-wait", "Loading…" } },
        Some(Err(e)) => return rsx! { p { class: "dv-err", "{e}" } },
        Some(Ok(t)) => t,
    };
    let select = {
        let id = id.clone();
        move || {
            let Some(h) = on_anchor else { return };
            if let Some((start, end)) = selection(&id) {
                h.call(Anchor::Text { start, end });
            }
        }
    };

    if editing() {
        let save = move |_| {
            let t = draft();
            if let Some(h) = on_save {
                h.call(t.clone().into_bytes());
            }
            text.set(Some(Ok(t)));
            editing.set(false);
        };
        return rsx! {
            div { class: "dv-bar",
                span { class: "dv-hint", "Editing" }
                span { class: "dv-gap" }
                button { class: "dv-btn", onclick: move |_| editing.set(false), "Cancel" }
                button { class: "dv-btn on", onclick: save, "Save" }
            }
            textarea {
                id: "{id}",
                class: "dv-edit",
                spellcheck: view == ViewKind::Markdown,
                value: "{draft}",
                oninput: move |e| draft.set(e.value()),
                onmouseup: {
                    let select = select.clone();
                    move |_| select()
                },
                onkeyup: move |_| select(),
            }
        };
    }

    let limit = if compact { COMPACT_LINES } else { MAX_LINES };
    let total = body.lines().count();
    let shown: String = if total > limit {
        body.lines().take(limit).collect::<Vec<_>>().join("\n")
    } else {
        body.clone()
    };
    let more = total.saturating_sub(limit);
    let md = view == ViewKind::Markdown && rendered();
    rsx! {
        if !compact {
            div { class: "dv-bar",
                if view == ViewKind::Markdown {
                    button {
                        class: if rendered() { "dv-btn on" } else { "dv-btn" },
                        onclick: move |_| rendered.set(true),
                        "Rendered"
                    }
                    button {
                        class: if rendered() { "dv-btn" } else { "dv-btn on" },
                        onclick: move |_| rendered.set(false),
                        "Source"
                    }
                }
                span { class: "dv-hint", "{total} lines" }
                span { class: "dv-gap" }
                if on_save.is_some() {
                    button {
                        class: "dv-btn",
                        onclick: {
                            let body = body.clone();
                            move |_| {
                                draft.set(body.clone());
                                editing.set(true);
                            }
                        },
                        "Edit"
                    }
                }
            }
        }
        if md {
            div { class: "dv-md", dangerous_inner_html: crate::markdown::to_html(&shown) }
        } else {
            div { class: "dv-code",
                pre { class: "dv-gutter",
                    {(1..=shown.lines().count().max(1)).map(|n| n.to_string()).collect::<Vec<_>>().join("\n")}
                }
                pre { id: "{id}", class: "dv-src", onmouseup: move |_| select(), "{shown}" }
            }
        }
        if more > 0 {
            p { class: "dv-hint", "{more} more lines" }
        }
    }
}

/// The selection inside element `id`, as UTF-16 offsets into its text: a
/// textarea's own, or the page's selection within a `pre`.
fn selection(id: &str) -> Option<(u32, u32)> {
    if let Some(t) = element::<web_sys::HtmlTextAreaElement>(id) {
        let s = t.selection_start().ok()??;
        let e = t.selection_end().ok()??;
        return Some((s, e));
    }
    let js = format!(
        "(() => {{ const el = document.getElementById({}); const s = getSelection(); \
         if (!el || !s || !s.rangeCount) return null; const r = s.getRangeAt(0); \
         if (!el.contains(r.startContainer)) return null; \
         const pre = document.createRange(); pre.selectNodeContents(el); \
         pre.setEnd(r.startContainer, r.startOffset); const a = pre.toString().length; \
         return [a, a + r.toString().length]; }})()",
        js_str(id)
    );
    let v = js_sys::eval(&js).ok()?;
    if v.is_null() || v.is_undefined() {
        return None;
    }
    let a = js_sys::Array::from(&v);
    if a.length() != 2 {
        return None;
    }
    Some((a.get(0).as_f64()? as u32, a.get(1).as_f64()? as u32))
}
