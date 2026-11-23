//! PDF pages drawn by pdfcraft's renderer onto a canvas: page by page, with
//! zoom, search, the comments already in the file, and a region anchor
//! under the pointer.

use std::cell::RefCell;
use std::rc::Rc;

use dioxus::prelude::*;
use wasm_bindgen::{Clamped, JsCast};

use super::region::{Picked, Region};
use super::{Src, device_pixel_ratio, element};
use crate::pdf::{Comment, Hit, Page, PdfDoc};
use crate::{Anchor, PageEdit};

/// The most device pixels one page is drawn with (memory and time).
const MAX_PIXELS: f32 = 24.0e6;
const ZOOMS: &[f32] = &[0.5, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0];

type Shared = Rc<RefCell<PdfDoc>>;

#[component]
pub fn PdfView(
    src: Src,
    compact: bool,
    on_anchor: Option<EventHandler<Anchor>>,
    on_edit: Option<EventHandler<Vec<PageEdit>>>,
) -> Element {
    let canvas_id = use_hook(super::fresh_id);
    let page_id = use_hook(super::fresh_id);
    let mut doc = use_signal(|| None::<Result<Shared, String>>);
    let mut page = use_signal(|| 0usize);
    // `None`: fit the width.
    let mut zoom = use_signal(|| None::<f32>);
    let mut drawn = use_signal(|| None::<Result<(), String>>);
    let mut query = use_signal(String::new);
    let mut hits = use_signal(Vec::<Hit>::new);
    let mut hit = use_signal(|| 0usize);
    let mut comments = use_signal(Vec::<Comment>::new);

    use_hook({
        let src = src.clone();
        move || {
            spawn(async move {
                let d = match src.bytes().await {
                    Ok(b) => PdfDoc::open(b),
                    Err(e) => Err(e),
                };
                if let Ok(d) = &d {
                    comments.set(d.comments());
                }
                doc.set(Some(d.map(|d| Rc::new(RefCell::new(d)))));
            });
        }
    });

    // Draw the page whenever it, the zoom or the space for it change.
    {
        let canvas_id = canvas_id.clone();
        let page_id = page_id.clone();
        use_effect(move || {
            let p = page();
            let _ = zoom();
            let Some(Ok(d)) = doc.read().clone() else {
                return;
            };
            let Some(holder) = element::<web_sys::HtmlElement>(&page_id) else {
                return;
            };
            let mut d = d.borrow_mut();
            let (pw, ph) = d.size(p);
            let css = holder.client_width().max(1) as f32;
            let mut scale = css / pw.max(1.0) * device_pixel_ratio() as f32;
            let px = pw * ph * scale * scale;
            if px > MAX_PIXELS {
                scale *= (MAX_PIXELS / px).sqrt();
            }
            let r = d.render(p, scale).and_then(|pg| draw(&canvas_id, &pg));
            drop(d);
            if drawn.peek().as_ref() != Some(&r) {
                drawn.set(Some(r));
            }
        });
    }

    let d = match doc() {
        None => return rsx! { p { class: "dv-wait", "Opening the PDF…" } },
        Some(Err(e)) => return rsx! { p { class: "dv-err", "This PDF could not be opened: {e}" } },
        Some(Ok(d)) => d,
    };
    let (pages, (pw, ph), label, title) = {
        let b = d.borrow();
        let p = page();
        (
            b.pages(),
            b.size(p),
            b.label(p),
            b.title().map(str::to_string),
        )
    };
    let (pw, ph) = (pw.max(1.0), ph.max(1.0));
    let p = page();
    let mut go = move |to: usize| {
        if to < pages {
            page.set(to);
        }
    };
    let holder_id = page_id.clone();
    let mut step_zoom = move |up: bool| {
        // Fitting: the zoom it fits at now.
        let cur = zoom().unwrap_or_else(|| {
            element::<web_sys::HtmlElement>(&holder_id)
                .map(|e| e.client_width() as f32 / pw)
                .filter(|z| *z > 0.0)
                .unwrap_or(1.0)
        });
        let next = if up {
            ZOOMS.iter().copied().find(|z| *z > cur + 0.01)
        } else {
            ZOOMS.iter().rev().copied().find(|z| *z < cur - 0.01)
        };
        if let Some(z) = next {
            zoom.set(Some(z));
        }
    };
    let mut find = {
        let d = d.clone();
        move || {
            let found = d.borrow_mut().search(&query());
            if let Some(h) = found.first() {
                page.set(h.page);
            }
            hit.set(0);
            hits.set(found);
        }
    };
    let mut step_hit = move |by: isize| {
        let n = hits.read().len();
        if n == 0 {
            return;
        }
        let i = (hit() as isize + by).rem_euclid(n as isize) as usize;
        hit.set(i);
        let to = hits.read()[i].page;
        page.set(to);
    };
    let pick = {
        let d = d.clone();
        move |pk: Picked| {
            let Some(h) = on_anchor else { return };
            // The layer's CSS pixels → the page's points.
            let k = pw / pk.width.max(1.0);
            let rect = pk.rect.map(|v| v * k);
            let mut b = d.borrow_mut();
            let text = b.text_in(p, rect);
            let user = b.to_user(p, rect);
            h.call(Anchor::Region {
                page: p as u32,
                rect,
                user,
                text,
            });
        }
    };
    let edit = move |e: PageEdit| {
        if let Some(h) = on_edit {
            h.call(vec![e]);
        }
    };
    let pct = |r: [f32; 4]| {
        format!(
            "left:{}%;top:{}%;width:{}%;height:{}%",
            r[0] / pw * 100.0,
            r[1] / ph * 100.0,
            (r[2] - r[0]) / pw * 100.0,
            (r[3] - r[1]) / ph * 100.0
        )
    };
    let page_style = match zoom() {
        Some(z) => format!("width:{}px;aspect-ratio:{pw}/{ph}", pw * z),
        None => format!("width:100%;aspect-ratio:{pw}/{ph}"),
    };
    let zoom_label = match zoom() {
        Some(z) => format!("{:.0}%", z * 100.0),
        None => "Fit".to_string(),
    };
    let here: Vec<(bool, Vec<[f32; 4]>)> = hits
        .read()
        .iter()
        .enumerate()
        .filter(|(_, h)| h.page == p)
        .map(|(i, h)| (i == hit(), h.rects.clone()))
        .collect();
    let notes: Vec<Comment> = comments
        .read()
        .iter()
        .filter(|c| c.page == p)
        .cloned()
        .collect();
    let n_hits = hits.read().len();
    let mut find2 = find.clone();
    rsx! {
        div { class: "dv-bar",
            button { class: "dv-btn", title: "Previous page", disabled: p == 0, onclick: move |_| go(p.wrapping_sub(1)), "‹" }
            span { class: "dv-hint", "Page {label} of {pages}" }
            button { class: "dv-btn", title: "Next page", disabled: p + 1 >= pages, onclick: move |_| go(p + 1), "›" }
            if !compact {
                span { class: "dv-sep" }
                button {
                    class: "dv-btn",
                    title: "Zoom out",
                    onclick: {
                        let mut z = step_zoom.clone();
                        move |_| z(false)
                    },
                    "−"
                }
                button { class: if zoom().is_none() { "dv-btn on" } else { "dv-btn" }, title: "Fit the width", onclick: move |_| zoom.set(None), "{zoom_label}" }
                button { class: "dv-btn", title: "Zoom in", onclick: move |_| step_zoom(true), "+" }
                span { class: "dv-sep" }
                input {
                    class: "dv-find",
                    r#type: "search",
                    placeholder: "Find in this PDF",
                    value: "{query}",
                    oninput: move |e| {
                        query.set(e.value());
                        hits.set(Vec::new());
                        hit.set(0);
                    },
                    onkeydown: move |e: KeyboardEvent| {
                        if e.key().to_string() == "Enter" {
                            if hits.read().is_empty() || e.modifiers().shift() {
                                find2();
                            } else {
                                step_hit(1);
                            }
                        }
                    },
                }
                if !query().trim().is_empty() {
                    if n_hits == 0 {
                        button { class: "dv-btn", onclick: move |_| find(), "Find" }
                    } else {
                        span { class: "dv-hint", "{hit() + 1} of {n_hits}" }
                        button { class: "dv-btn", title: "Previous match", onclick: move |_| step_hit(-1), "‹" }
                        button { class: "dv-btn", title: "Next match", onclick: move |_| step_hit(1), "›" }
                    }
                }
                if on_edit.is_some() {
                    span { class: "dv-gap" }
                    button { class: "dv-btn", title: "Turn this page left", onclick: move |_| edit(PageEdit::Rotate { pages: vec![p], degrees: -90 }), "⟲" }
                    button { class: "dv-btn", title: "Turn this page right", onclick: move |_| edit(PageEdit::Rotate { pages: vec![p], degrees: 90 }), "⟳" }
                    button { class: "dv-btn", title: "Move this page earlier", disabled: p == 0, onclick: move |_| edit(PageEdit::Move { pages: vec![p], to: p.saturating_sub(1) }), "↑" }
                    button { class: "dv-btn", title: "Move this page later", disabled: p + 1 >= pages, onclick: move |_| edit(PageEdit::Move { pages: vec![p], to: p + 1 }), "↓" }
                    button { class: "dv-btn dv-danger", title: "Delete this page", disabled: pages < 2, onclick: move |_| edit(PageEdit::Delete { pages: vec![p] }), "Delete page" }
                }
            }
        }
        if let Some(t) = title.filter(|_| !compact) {
            p { class: "dv-hint dv-title", "{t}" }
        }
        div {
            class: "dv-scroll",
            div { id: "{page_id}", class: "dv-page", style: "{page_style}",
                canvas { id: "{canvas_id}", class: "dv-canvas" }
                for (on, rects) in here {
                    for r in rects {
                        div { class: if on { "dv-hit on" } else { "dv-hit" }, style: pct(r) }
                    }
                }
                for c in notes.iter() {
                    div { class: "dv-note", title: "{c.author}: {c.text}", style: pct(c.rect) }
                }
                if on_anchor.is_some() && !compact {
                    Region { on_pick: pick, div { class: "dv-fill" } }
                }
            }
            if let Some(Err(e)) = drawn() {
                p { class: "dv-err", "This page could not be drawn: {e}" }
            }
        }
        if !compact && !comments.read().is_empty() {
            details { class: "dv-notes",
                summary { "Comments in this PDF ({comments.read().len()})" }
                for c in comments.read().iter().cloned() {
                    button {
                        class: "dv-noteitem",
                        onclick: move |_| go(c.page),
                        span { class: "dv-hint", "p. {c.page + 1} · {c.kind}" }
                        if !c.author.is_empty() {
                            b { " {c.author}" }
                        }
                        span { " {c.text}" }
                    }
                }
            }
        }
    }
}

/// Put a drawn page on the canvas (sized to it).
fn draw(canvas_id: &str, page: &Page) -> Result<(), String> {
    let canvas = element::<web_sys::HtmlCanvasElement>(canvas_id).ok_or("no canvas")?;
    canvas.set_width(page.width);
    canvas.set_height(page.height);
    let ctx = canvas
        .get_context("2d")
        .ok()
        .flatten()
        .and_then(|c| c.dyn_into::<web_sys::CanvasRenderingContext2d>().ok())
        .ok_or("no 2D context")?;
    let img = web_sys::ImageData::new_with_u8_clamped_array_and_sh(
        Clamped(&page.rgba),
        page.width,
        page.height,
    )
    .map_err(|_| "the page is too big to draw")?;
    ctx.put_image_data(&img, 0.0, 0.0)
        .map_err(|_| "the canvas refused the page".to_string())
}
