//! Images, audio (with its waveform) and video: what the browser plays and
//! draws by itself, with a time readout and anchors.

use dioxus::prelude::*;
use wasm_bindgen::JsCast;

use super::region::{Picked, Region};
use super::{Src, element};
use crate::{Anchor, clock, peaks};

// ── images ──────────────────────────────────────────────────────────────

#[component]
pub fn ImageView(src: Src, compact: bool, on_anchor: Option<EventHandler<Anchor>>) -> Element {
    let id = use_hook(super::fresh_id);
    let mut actual = use_signal(|| false);
    let mut dims = use_signal(|| None::<(u32, u32)>);
    let mut failed = use_signal(|| false);
    let img_id = id.clone();
    let pick = move |p: Picked| {
        let Some(h) = on_anchor else { return };
        let Some(img) = element::<web_sys::HtmlImageElement>(&img_id) else {
            return;
        };
        // CSS pixels → the image's own pixels.
        let k = img.natural_width() as f32 / (img.client_width().max(1) as f32);
        let r = p.rect.map(|v| (v * k).round());
        h.call(Anchor::Region {
            page: 0,
            rect: r,
            user: None,
            text: String::new(),
        });
    };
    let on_load = {
        let id = id.clone();
        move |_| {
            if let Some(img) = element::<web_sys::HtmlImageElement>(&id) {
                dims.set(Some((img.natural_width(), img.natural_height())));
            }
        }
    };
    if failed() {
        return rsx! { p { class: "dv-err", "This image could not be shown." } };
    }
    let class = if actual() { "dv-img actual" } else { "dv-img" };
    rsx! {
        if !compact {
            div { class: "dv-bar",
                button { class: if actual() { "dv-btn" } else { "dv-btn on" }, onclick: move |_| actual.set(false), "Fit" }
                button { class: if actual() { "dv-btn on" } else { "dv-btn" }, onclick: move |_| actual.set(true), "100%" }
                if let Some((w, h)) = dims() {
                    span { class: "dv-hint", "{w} × {h}" }
                }
            }
        }
        div { class: "dv-stage",
            if on_anchor.is_some() && !compact {
                Region { on_pick: pick,
                    img { id: "{id}", class: "{class}", src: "{src.url}", alt: "", draggable: false, onload: on_load, onerror: move |_| failed.set(true) }
                }
            } else {
                img { id: "{id}", class: "{class}", src: "{src.url}", alt: "", onload: on_load, onerror: move |_| failed.set(true) }
            }
        }
    }
}

// ── audio ───────────────────────────────────────────────────────────────

/// Files read whole for their waveform (bigger ones just play).
const WAVEFORM_MAX: u64 = 96 << 20;
const BUCKETS: usize = 600;
const WAVE_H: f32 = 80.0;

enum Wave {
    Loading,
    /// The peaks, and a URL of the bytes already read (the player uses it,
    /// so the file is read once).
    Ready(Vec<(f32, f32)>, String),
    /// No waveform; play from the file's own URL.
    Plain(String),
    Failed(String),
}

#[component]
pub fn AudioView(
    src: Src,
    compact: bool,
    on_anchor: Option<EventHandler<Anchor>>,
    peaks: Option<String>,
) -> Element {
    let id = use_hook(super::fresh_id);
    let mut wave = use_signal(|| Wave::Loading);
    let mut now = use_signal(|| 0.0f64);
    let mut length = use_signal(|| 0.0f64);
    let mut range = use_signal(|| None::<(f64, f64)>);
    use_hook({
        let src = src.clone();
        move || {
            spawn(async move {
                // Peaks made ahead of time: the file itself is only
                // streamed, to play.
                if let Some(url) = &peaks
                    && let Some(p) = fetch_peaks(url, src.bearer.as_deref()).await
                {
                    if p.frames > 0 && p.sample_rate > 0 {
                        length.set(p.seconds());
                    }
                    wave.set(Wave::Ready(p.columns(BUCKETS), src.url.clone()));
                    return;
                }
                if src.size > WAVEFORM_MAX {
                    wave.set(Wave::Plain(src.url.clone()));
                    return;
                }
                let w = match src.bytes().await {
                    Err(e) => Wave::Failed(e),
                    Ok(bytes) => {
                        let url = object_url(&bytes).unwrap_or_else(|| src.url.clone());
                        match decode_peaks(&bytes).await {
                            Some(p) => Wave::Ready(p, url),
                            None => Wave::Plain(url),
                        }
                    }
                };
                wave.set(w);
            });
        }
    });
    use_drop({
        let wave = wave;
        move || {
            if let Ok(w) = wave.try_peek()
                && let Wave::Ready(_, u) | Wave::Plain(u) = &*w
                && u.starts_with("blob:")
            {
                let _ = web_sys::Url::revoke_object_url(u);
            }
        }
    });
    let mut read_time = {
        let id = id.clone();
        move || {
            if let Some(a) = element::<web_sys::HtmlMediaElement>(&id) {
                now.set(a.current_time());
                let d = a.duration();
                if d.is_finite() {
                    length.set(d);
                }
            }
        }
    };
    let emit_time = {
        let id = id.clone();
        move |_| {
            let (Some(h), Some(a)) = (on_anchor, element::<web_sys::HtmlMediaElement>(&id)) else {
                return;
            };
            h.call(Anchor::Time {
                at: a.current_time(),
            });
        }
    };
    let seek_id = id.clone();
    let pick = move |p: Picked| {
        let len = length();
        if len <= 0.0 || p.width <= 0.0 {
            return;
        }
        let t = |x: f32| (x / p.width).clamp(0.0, 1.0) as f64 * len;
        let (a, b) = (t(p.rect[0]), t(p.rect[2]));
        if let Some(m) = element::<web_sys::HtmlMediaElement>(&seek_id) {
            m.set_current_time(a);
        }
        if b - a < 0.05 {
            range.set(None);
            if let Some(h) = on_anchor {
                h.call(Anchor::Time { at: a });
            }
        } else {
            range.set(Some((a, b)));
            if let Some(h) = on_anchor {
                h.call(Anchor::TimeRange { start: a, end: b });
            }
        }
    };
    let (peaks, play) = match &*wave.read() {
        Wave::Loading => return rsx! { p { class: "dv-wait", "Reading the sound…" } },
        Wave::Failed(e) => return rsx! { p { class: "dv-err", "{e}" } },
        Wave::Ready(p, u) => (Some(p.clone()), u.clone()),
        Wave::Plain(u) => (None, u.clone()),
    };
    let len = length();
    let head = if len > 0.0 {
        (now() / len * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    let mut t1 = read_time.clone();
    rsx! {
        if let Some(p) = peaks {
            div { class: "dv-wave",
                Region { on_pick: pick, band: true,
                    svg {
                        view_box: "0 0 {BUCKETS} {WAVE_H}",
                        preserve_aspect_ratio: "none",
                        path { d: peaks::svg_path(&p, BUCKETS as f32, WAVE_H), stroke: "currentColor", stroke_width: "1" }
                    }
                    if let Some((a, b)) = range() {
                        if len > 0.0 {
                            div { class: "dv-band", style: "left:{a / len * 100.0}%;width:{(b - a) / len * 100.0}%" }
                        }
                    }
                    div { class: "dv-head", style: "left:{head}%" }
                }
            }
        } else if !compact {
            p { class: "dv-hint", "Too long to draw its waveform here." }
        }
        audio {
            id: "{id}",
            class: "dv-audio",
            controls: true,
            preload: "metadata",
            src: "{play}",
            ontimeupdate: move |_| t1(),
            onloadedmetadata: move |_| read_time(),
            // Not on `seeked`: a click on the waveform seeks, and its own
            // anchor (a time or a range) is the one that counts.
            onpause: emit_time,
        }
        div { class: "dv-time",
            span { "{clock(now())}" }
            span { class: "dv-hint", " / {clock(len)}" }
            if let Some((a, b)) = range() {
                span { class: "dv-hint", " · selected {clock(a)} – {clock(b)}" }
            }
        }
    }
}

/// A sound's peaks made ahead of time (a sidecar), if they can be had.
async fn fetch_peaks(url: &str, bearer: Option<&str>) -> Option<drive_sidecars::peaks::Peaks> {
    let mut req = gloo_net::http::Request::get(url);
    if let Some(b) = bearer {
        req = req.header("authorization", b);
    }
    let r = req.send().await.ok()?;
    if !r.ok() {
        return None;
    }
    drive_sidecars::peaks::Peaks::decode(&r.binary().await.ok()?).ok()
}

/// A `blob:` URL of these bytes.
fn object_url(bytes: &[u8]) -> Option<String> {
    let arr = js_sys::Array::new();
    arr.push(&js_sys::Uint8Array::from(bytes));
    let blob = web_sys::Blob::new_with_u8_array_sequence(&arr).ok()?;
    web_sys::Url::create_object_url_with_blob(&blob).ok()
}

/// The sound's peaks, decoded by the browser (every format it plays).
async fn decode_peaks(bytes: &[u8]) -> Option<Vec<(f32, f32)>> {
    let ctx = web_sys::AudioContext::new().ok()?;
    let buf = js_sys::Uint8Array::from(bytes).buffer();
    let decoded = match ctx.decode_audio_data(&buf) {
        Ok(p) => wasm_bindgen_futures::JsFuture::from(p).await,
        Err(e) => Err(e),
    };
    let _ = ctx.close();
    let audio: web_sys::AudioBuffer = decoded.ok()?.dyn_into().ok()?;
    let mut out: Option<Vec<(f32, f32)>> = None;
    for ch in 0..audio.number_of_channels().min(2) {
        let data = audio.get_channel_data(ch).ok()?;
        let p = peaks::peaks(&data, BUCKETS);
        out = Some(match out {
            Some(o) => peaks::merge(&o, &p),
            None => p,
        });
    }
    out
}

// ── video ───────────────────────────────────────────────────────────────

#[component]
pub fn VideoView(
    src: Src,
    compact: bool,
    on_anchor: Option<EventHandler<Anchor>>,
    poster: Option<String>,
) -> Element {
    let id = use_hook(super::fresh_id);
    let mut now = use_signal(|| 0.0f64);
    let mut length = use_signal(|| 0.0f64);
    let mut failed = use_signal(|| false);
    let mut read_time = {
        let id = id.clone();
        move || {
            if let Some(v) = element::<web_sys::HtmlMediaElement>(&id) {
                now.set(v.current_time());
                let d = v.duration();
                if d.is_finite() {
                    length.set(d);
                }
            }
        }
    };
    let emit_time = {
        let id = id.clone();
        move |_| {
            let (Some(h), Some(v)) = (on_anchor, element::<web_sys::HtmlMediaElement>(&id)) else {
                return;
            };
            h.call(Anchor::Time {
                at: v.current_time(),
            });
        }
    };
    let mut t1 = read_time.clone();
    rsx! {
        if failed() {
            p { class: "dv-err", "This video cannot be played in the browser." }
        }
        video {
            id: "{id}",
            class: "dv-video",
            controls: true,
            preload: "metadata",
            playsinline: true,
            poster: poster.unwrap_or_default(),
            src: "{src.url}",
            ontimeupdate: move |_| t1(),
            onloadedmetadata: move |_| read_time(),
            onpause: emit_time.clone(),
            onseeked: emit_time,
            onerror: move |_| failed.set(true),
        }
        if !compact {
            div { class: "dv-time",
                span { "{clock(now())}" }
                span { class: "dv-hint", " / {clock(length())}" }
            }
        }
    }
}
