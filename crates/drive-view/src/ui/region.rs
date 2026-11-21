//! A layer over a page, an image or a waveform that turns a click into a
//! point and a drag into a rectangle, in its own CSS pixels.

use dioxus::prelude::*;

/// A click (a rectangle with no size) or a drag, in the layer's CSS pixels
/// (`[x0, y0, x1, y1]`, x0 ≤ x1, y0 ≤ y1), and the layer's size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Picked {
    pub rect: [f32; 4],
    pub width: f32,
    pub height: f32,
}

/// Drags shorter than this are clicks.
const CLICK: f64 = 4.0;

#[component]
pub fn Region(
    on_pick: EventHandler<Picked>,
    /// Draw the dragged rectangle as a band across the whole height (a time range).
    #[props(default)]
    band: bool,
    children: Element,
) -> Element {
    let id = use_hook(super::fresh_id);
    let mut from = use_signal(|| None::<(f64, f64)>);
    let mut to = use_signal(|| (0.0f64, 0.0f64));
    let mut kept = use_signal(|| None::<[f64; 4]>);
    let size = {
        let id = id.clone();
        move || {
            super::element::<web_sys::HtmlElement>(&id)
                .map(|e| (e.client_width() as f32, e.client_height() as f32))
                .unwrap_or((0.0, 0.0))
        }
    };
    let shown = match from() {
        Some((x, y)) => Some([x, y, to().0, to().1]),
        None => kept(),
    };
    let sel_style = shown.map(|[x0, y0, x1, y1]| {
        let (l, r) = (x0.min(x1), x0.max(x1));
        let (t, b) = (y0.min(y1), y0.max(y1));
        if band {
            format!("left:{l}px;width:{}px;top:0;bottom:0", r - l)
        } else {
            format!("left:{l}px;top:{t}px;width:{}px;height:{}px", r - l, b - t)
        }
    });
    rsx! {
        div { class: "dv-region",
            {children}
            div {
                id: "{id}",
                class: "dv-pick",
                onpointerdown: move |e: PointerEvent| {
                    let p = e.element_coordinates();
                    from.set(Some((p.x, p.y)));
                    to.set((p.x, p.y));
                    kept.set(None);
                },
                onpointermove: move |e: PointerEvent| {
                    if from.peek().is_some() {
                        let p = e.element_coordinates();
                        to.set((p.x, p.y));
                    }
                },
                onpointerleave: move |_| from.set(None),
                onpointerup: {
                    let size = size.clone();
                    move |e: PointerEvent| {
                        let Some((x0, y0)) = from() else { return };
                        from.set(None);
                        let p = e.element_coordinates();
                        let (mut x1, mut y1) = (p.x, p.y);
                        if (x1 - x0).abs() < CLICK && (y1 - y0).abs() < CLICK {
                            (x1, y1) = (x0, y0);
                        } else {
                            kept.set(Some([x0, y0, x1, y1]));
                        }
                        let (width, height) = size();
                        on_pick.call(Picked {
                            rect: [
                                x0.min(x1) as f32,
                                y0.min(y1) as f32,
                                x0.max(x1) as f32,
                                y0.max(y1) as f32,
                            ],
                            width,
                            height,
                        });
                    }
                },
                if let Some(s) = sel_style {
                    div { class: "dv-sel", style: "{s}" }
                }
            }
        }
    }
}
