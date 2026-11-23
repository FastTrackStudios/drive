//! Notes: a collaborative text editor. Everyone types into the same
//! Loro text; each person's caret shows to the others, live.

use dioxus::prelude::*;
use drive_dioxus::loro::{
    LoroDoc, LoroText,
    cursor::{Cursor, PosType, Side},
};
use drive_dioxus::text::set_text;
use drive_dioxus::ui::Avatars;
use drive_dioxus::{Presence, use_doc, use_docs, use_drive};

const NOTE_PREFIX: &str = "drive:note:";

#[component]
pub fn Notes() -> Element {
    let docs = use_docs(NOTE_PREFIX);
    let ctx = use_drive();
    let mut open = use_signal(|| None::<String>);
    let list = docs();
    if open().is_none()
        && let Some(first) = list.first()
    {
        open.set(Some(first.id.clone()));
    }
    rsx! {
        section { class: "pane notes",
            div { class: "panehead",
                h1 { "Notes" }
                button {
                    class: "primary",
                    onclick: move |_| {
                        let id = format!("{NOTE_PREFIX}{}", (crate::host::random() * 1e9) as u64);
                        let doc = ctx.drive.open(&id);
                        let _ = doc.get_text("title").insert(0, "Untitled");
                        ctx.drive.commit(&id);
                        open.set(Some(id));
                    },
                    "New note"
                }
            }
            div { class: "notegrid",
                ul { class: "notelist",
                    if list.is_empty() {
                        li { class: "empty", "No notes yet." }
                    }
                    for d in list.iter() {
                        li {
                            key: "{d.id}",
                            class: if open().as_deref() == Some(d.id.as_str()) { "on" } else { "" },
                            onclick: {
                                let id = d.id.clone();
                                move |_| open.set(Some(id.clone()))
                            },
                            NoteTitle { id: d.id.clone() }
                            if d.peers > 0 {
                                span { class: "live" }
                            }
                        }
                    }
                }
                if let Some(id) = open() {
                    Editor { key: "{id}", id }
                }
            }
        }
    }
}

#[component]
fn NoteTitle(id: String) -> Element {
    let doc = use_doc(id);
    let t = doc.loro().get_text("title").to_string();
    rsx! {
        span { if t.is_empty() { "Untitled" } else { "{t}" } }
    }
}

fn textarea_cursor(text: &LoroText, offset: usize) -> Option<Cursor> {
    let position = text.convert_pos(offset, PosType::Utf16, PosType::Event)?;
    text.get_cursor(position, Side::Middle)
}

fn textarea_offset(doc: &LoroDoc, cursor: &Cursor) -> Option<usize> {
    let position = doc.get_cursor_pos(cursor).ok()?.current.pos;
    doc.get_text("body")
        .convert_pos(position, PosType::Event, PosType::Utf16)
}

fn caret_index(text: &LoroText, offset: usize) -> Option<usize> {
    text.convert_pos(
        offset.min(text.len_utf16()),
        PosType::Utf16,
        PosType::Unicode,
    )
}

/// The caret of each other person, from their presence.
fn caret_of(p: &Presence) -> Option<usize> {
    p.data
        .get("caret")
        .and_then(|c| c.as_u64())
        .map(|c| c as usize)
}

#[component]
fn Editor(id: String) -> Element {
    let doc = use_doc(id.clone());
    let loro = doc.loro();
    let title = loro.get_text("title").to_string();
    let body_text = loro.get_text("body");
    let body = body_text.to_string();
    let others = doc.others();
    // Where my caret is, as a Loro cursor: it stays put as others type.
    let mut mine = use_signal(|| None::<(Cursor, Cursor)>);
    let area_id = format!("note-{}", id.replace(':', "-"));

    // After the text changes, put my caret back where its cursor now is.
    let restore_doc = doc.clone();
    let restore_id = area_id.clone();
    use_effect(move || {
        let _ = restore_doc.rev();
        if let Some((a, b)) = mine.peek().clone() {
            let d = restore_doc.peek();
            if let (Some(a), Some(b)) = (textarea_offset(&d, &a), textarea_offset(&d, &b)) {
                let id_js = serde_json::to_string(&restore_id).unwrap_or_else(|_| "\"\"".into());
                let js = format!(
                    "const e=document.getElementById({id_js}); if (e && document.activeElement===e) e.setSelectionRange({},{});",
                    a, b
                );
                document::eval(&js);
            }
        }
    });

    // Read my selection from the textarea, keep it as cursors, tell others.
    let track = {
        let doc = doc.clone();
        let area_id = area_id.clone();
        move || {
            let doc = doc.clone();
            let area_id = area_id.clone();
            spawn(async move {
                let id_js = serde_json::to_string(&area_id).unwrap_or_else(|_| "\"\"".into());
                let js = format!(
                    "const e=document.getElementById({id_js}); return e ? [e.selectionStart, e.selectionEnd] : [0,0];"
                );
                if let Ok(v) = document::eval(&js).await
                    && let Some(arr) = v.as_array()
                {
                    let a = arr.first().and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                    let b = arr.get(1).and_then(|x| x.as_u64()).unwrap_or(0) as usize;
                    let text = doc.peek().get_text("body");
                    if let (Some(ca), Some(cb)) =
                        (textarea_cursor(&text, a), textarea_cursor(&text, b))
                    {
                        mine.set(Some((ca, cb)));
                    }
                    // Presence uses the textarea's UTF-16 offset across clients.
                    doc.set_presence(&serde_json::json!({ "caret": b }));
                }
            });
        }
    };

    // The text with a marker at each other person's caret.
    let mut marks: Vec<(usize, Presence)> = others
        .iter()
        .filter_map(|p| caret_index(&body_text, caret_of(p)?).map(|c| (c, p.clone())))
        .collect();
    marks.sort_by_key(|m| m.0);
    let chars: Vec<char> = body.chars().collect();
    let mut pieces: Vec<(String, Option<Presence>)> = Vec::new();
    let mut at = 0;
    for (c, p) in marks {
        let c = c.min(chars.len());
        if c > at {
            pieces.push((chars[at..c].iter().collect(), None));
            at = c;
        }
        pieces.push((String::new(), Some(p)));
    }
    pieces.push((chars[at.min(chars.len())..].iter().collect(), None));

    let words = body.split_whitespace().count();
    rsx! {
        div { class: "editor",
            div { class: "edhead",
                input {
                    class: "ntitle",
                    value: "{title}",
                    placeholder: "Untitled",
                    oninput: {
                        let doc = doc.clone();
                        move |e: FormEvent| {
                            let v = e.value();
                            doc.edit(|d: &LoroDoc| set_text(&d.get_text("title"), &v));
                        }
                    },
                }
                Avatars { doc: doc.clone() }
            }
            div { class: "page",
                div { class: "mirror", aria_hidden: "true",
                    for (i , (text , who)) in pieces.into_iter().enumerate() {
                        if let Some(p) = who {
                            span { key: "{i}", class: "caret", style: "--h:{p.hue}",
                                span { class: "caretname", "{p.name}" }
                            }
                        } else {
                            span { key: "{i}", "{text}" }
                        }
                    }
                    "\u{200b}"
                }
                textarea {
                    id: "{area_id}",
                    class: "area",
                    value: "{body}",
                    placeholder: "Write together…",
                    spellcheck: "false",
                    oninput: {
                        let doc = doc.clone();
                        let track = track.clone();
                        move |e: FormEvent| {
                            let v = e.value();
                            doc.edit(|d: &LoroDoc| set_text(&d.get_text("body"), &v));
                            track();
                        }
                    },
                    onkeyup: {
                        let track = track.clone();
                        move |_| track()
                    },
                    onclick: {
                        let track = track.clone();
                        move |_| track()
                    },
                    onfocus: {
                        let track = track.clone();
                        move |_| track()
                    },
                }
            }
            div { class: "edfoot",
                "{words} words · edits merge as you type · works offline"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_caret_after_emoji_marks_the_next_character() {
        let doc = LoroDoc::new();
        let text = doc.get_text("body");
        text.insert(0, "a😀bc").unwrap();
        assert_eq!(caret_index(&text, 3), Some(2));
    }

    #[test]
    fn textarea_cursor_after_emoji_stays_before_the_next_character() {
        let doc = LoroDoc::new();
        let text = doc.get_text("body");
        text.insert(0, "a😀bc").unwrap();
        let captured = textarea_cursor(&text, 3).unwrap();
        let expected = text.get_cursor(2, Side::Middle).unwrap();
        assert_eq!(
            doc.get_cursor_pos(&captured).unwrap().current.pos,
            doc.get_cursor_pos(&expected).unwrap().current.pos
        );
        assert_eq!(textarea_offset(&doc, &expected), Some(3));
    }
}
