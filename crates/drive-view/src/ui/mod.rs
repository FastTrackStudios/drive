//! The viewers as Dioxus components (web). [`Viewer`] is the one entry.

use std::sync::atomic::{AtomicU32, Ordering};

use dioxus::prelude::*;
use wasm_bindgen::JsCast;

use crate::{Anchor, PageEdit, ViewKind};

mod media;
#[cfg(feature = "pdf")]
mod pdf;
mod region;
mod text;

/// The viewers' styles. Colours come from the host app's CSS variables
/// (`--ink`, `--surface`, `--accent`, …) where it has them.
pub const CSS: &str = include_str!("viewers.css");

/// Show a file: the viewer for its kind, reading it from `url`.
///
/// - `kind`, `parent`: the file's kind id and broader kind
///   (`drive_spine::kinds::Kind`), `name` its file name (the extension
///   tells markdown from text);
/// - `size`: its size in bytes when known (big files are not read whole
///   for a waveform);
/// - `bearer`: an `Authorization` value for reading `url`, if it needs one;
/// - `compact`: the small form, for an inspector (no editing or search);
/// - `on_anchor`: where the person points or selects (see [`Anchor`]);
/// - `on_save`: offered for text kinds; gets the edited file's bytes;
/// - `on_pdf_edit`: offered for PDFs; gets page edits to make natively;
/// - `peaks`: for audio, the URL of its waveform peaks made ahead of time
///   (a sidecar, `drive_sidecars::peaks`): drawn from those, the file is
///   only streamed to play. Without them (or when they cannot be read),
///   the browser decodes the file itself, if it is small enough;
/// - `poster`: for video, a picture to show before it plays.
#[component]
pub fn Viewer(
    kind: String,
    #[props(default)] parent: String,
    name: String,
    url: String,
    #[props(default)] size: u64,
    #[props(default)] bearer: Option<String>,
    #[props(default)] compact: bool,
    #[props(default)] on_anchor: Option<EventHandler<Anchor>>,
    #[props(default)] on_save: Option<EventHandler<Vec<u8>>>,
    #[props(default)] on_pdf_edit: Option<EventHandler<Vec<PageEdit>>>,
    #[props(default)] peaks: Option<String>,
    #[props(default)] poster: Option<String>,
) -> Element {
    let Some(vk) = ViewKind::pick(&kind, &parent, &name) else {
        return rsx! {
            style { {CSS} }
            div { class: "dv dv-none", "No preview for this kind of file." }
        };
    };
    let src = Src { url, bearer, size };
    let class = if compact { "dv compact" } else { "dv" };
    // Another file (or a new version of this one: a new URL) is a new
    // viewer, not this one's state over other bytes. Keys only take in a
    // list, hence the list of one.
    let key = src.url.clone();
    rsx! {
        style { {CSS} }
        for key in std::iter::once(key) {
            div { key: "{key}", class: "{class}",
            match vk {
                ViewKind::Markdown | ViewKind::Text | ViewKind::Code => rsx! {
                    text::TextView { src: src.clone(), view: vk, compact, on_anchor, on_save }
                },
                ViewKind::Image => rsx! {
                    media::ImageView { src: src.clone(), compact, on_anchor }
                },
                ViewKind::Audio => rsx! {
                    media::AudioView { src: src.clone(), compact, on_anchor, peaks: peaks.clone() }
                },
                ViewKind::Video => rsx! {
                    media::VideoView { src: src.clone(), compact, on_anchor, poster: poster.clone() }
                },
                ViewKind::Pdf => pdf_view(src.clone(), compact, on_anchor, on_pdf_edit),
            }
            }
        }
    }
}

#[cfg(feature = "pdf")]
fn pdf_view(
    src: Src,
    compact: bool,
    on_anchor: Option<EventHandler<Anchor>>,
    on_edit: Option<EventHandler<Vec<PageEdit>>>,
) -> Element {
    rsx! {
        pdf::PdfView { src, compact, on_anchor, on_edit }
    }
}

#[cfg(not(feature = "pdf"))]
fn pdf_view(
    src: Src,
    _compact: bool,
    _on_anchor: Option<EventHandler<Anchor>>,
    _on_edit: Option<EventHandler<Vec<PageEdit>>>,
) -> Element {
    rsx! {
        iframe { class: "dv-frame", src: "{src.url}" }
    }
}

/// Where a file's bytes are.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Src {
    pub url: String,
    pub bearer: Option<String>,
    pub size: u64,
}

impl Src {
    /// The file's bytes, or why not (the node's own words when it says).
    pub async fn bytes(&self) -> Result<Vec<u8>, String> {
        let mut req = gloo_net::http::Request::get(&self.url);
        if let Some(b) = &self.bearer {
            req = req.header("authorization", b);
        }
        let r = req.send().await.map_err(|e| e.to_string())?;
        if !r.ok() {
            let why = r.text().await.unwrap_or_default();
            return Err(match (r.status(), why.trim()) {
                (401, _) => "Sign in to see this file.".into(),
                (_, w) if !w.is_empty() && w.len() < 200 => capitalise(w),
                (s, _) => format!("The file could not be read ({s})."),
            });
        }
        r.binary().await.map_err(|e| e.to_string())
    }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect::<String>() + ".",
        None => String::new(),
    }
}

/// A fresh element id for this page (`dv-7`).
pub(crate) fn fresh_id() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    format!("dv-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// The element with this id, as a `T`.
pub(crate) fn element<T: JsCast>(id: &str) -> Option<T> {
    web_sys::window()?
        .document()?
        .get_element_by_id(id)?
        .dyn_into::<T>()
        .ok()
}

pub(crate) fn device_pixel_ratio() -> f64 {
    web_sys::window()
        .map(|w| w.device_pixel_ratio())
        .filter(|r| r.is_finite() && *r > 0.0)
        .unwrap_or(1.0)
}

/// A JavaScript string literal of `s`.
pub(crate) fn js_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '<' => out.push_str("\\u003c"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
