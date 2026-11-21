//! Drive's viewers: a look at any file, and a quick edit where one is
//! cheap, the way Quick Look does on a Mac. Drive's inspector and its
//! full-screen preview use them; any app (Studio, Season) can embed the same
//! components. Heavy editing stays in each kind's own app.
//!
//! [`Viewer`] picks the viewer for a file by its kind (the ids of
//! `drive_spine::kinds`) and its name: markdown, plain text and code, images,
//! audio (with its waveform), video and PDF (pdfcraft's renderer). It reads
//! the file from a URL (a node's `/blobs/{hash}?name=` or `/fetch/…`).
//!
//! Viewers say where a person is pointing, for comments to hang on: an
//! [`Anchor`] (a text range, a time or a time range, a region of a page or an
//! image), sent to `on_anchor`. The comments themselves are not here.

use serde::{Deserialize, Serialize};

#[cfg(feature = "edit")]
pub mod edit;
#[cfg(feature = "ui")]
pub mod markdown;
#[cfg(feature = "pdf")]
pub mod pdf;
pub mod peaks;
#[cfg(feature = "sign")]
pub mod sign;
#[cfg(feature = "ui")]
mod ui;

#[cfg(feature = "ui")]
pub use ui::{CSS, Viewer};

/// Which viewer shows a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewKind {
    Markdown,
    /// Plain text: notes, logs, CSV.
    Text,
    /// Source code and structured text (JSON, TOML, a chart's source).
    Code,
    Image,
    Audio,
    Video,
    Pdf,
}

/// Images a browser draws by itself (not HEIC or TIFF).
const BROWSER_IMAGES: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "svg", "avif", "bmp", "ico",
];
/// Charts kept as text (`kf` is keyflow's source).
const TEXT_CHARTS: &[&str] = &["kf", "musicxml"];

fn ext(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

impl ViewKind {
    /// The viewer for a file of kind `kind` (its broader kind `parent`, for
    /// kinds apps register) named `name`; `None`: nothing to show but its
    /// icon.
    pub fn pick(kind: &str, parent: &str, name: &str) -> Option<ViewKind> {
        let e = ext(name);
        let by_kind = |k: &str| -> Option<ViewKind> {
            Some(match k {
                "text" if e == "md" || e == "markdown" => ViewKind::Markdown,
                "text" => ViewKind::Text,
                "sheet" if e == "csv" || e == "tsv" => ViewKind::Text,
                "code" => ViewKind::Code,
                "chart" if TEXT_CHARTS.contains(&e.as_str()) => ViewKind::Code,
                "image" if BROWSER_IMAGES.contains(&e.as_str()) => ViewKind::Image,
                "audio" if e != "mid" && e != "midi" => ViewKind::Audio,
                "video" => ViewKind::Video,
                "pdf" => ViewKind::Pdf,
                _ => return None,
            })
        };
        by_kind(kind).or_else(|| by_kind(parent))
    }

    /// Whether its quick edit is a text editor.
    pub fn is_text(self) -> bool {
        matches!(self, ViewKind::Markdown | ViewKind::Text | ViewKind::Code)
    }
}

/// One edit to a PDF's pages (0-based page numbers), asked of the native
/// side (`edit::apply`): the browser only says what it wants.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PageEdit {
    /// Turn pages clockwise by a multiple of 90°.
    Rotate { pages: Vec<usize>, degrees: i64 },
    /// Remove pages (never all of them).
    Delete { pages: Vec<usize> },
    /// Move pages so they start at `to` (in the resulting order).
    Move { pages: Vec<usize>, to: usize },
}

/// Where in a file a person is pointing: what a comment hangs on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Anchor {
    /// Characters `start..end` of a text (UTF-16 code units, as the
    /// browser counts a selection).
    Text { start: u32, end: u32 },
    /// A moment of audio or video, in seconds.
    Time { at: f64 },
    /// A stretch of audio or video, in seconds.
    TimeRange { start: f64, end: f64 },
    /// A region of a page (a PDF's, 0-based) or of an image (page 0).
    /// `rect` is `[x0, y0, x1, y1]`, y down: points for a PDF page as shown
    /// (after its rotation), pixels for an image. A click is a region with
    /// no size. For a PDF, `user` is the same region in the page's own PDF
    /// coordinates (y up), and `text` the text it covers.
    Region {
        page: u32,
        rect: [f32; 4],
        #[serde(default, skip_serializing_if = "Option::is_none")]
        user: Option<[f32; 4]>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        text: String,
    },
}

impl Anchor {
    /// Said for people: `Page 2, 120×40 pt at 72, 300`, `1:02.5`.
    pub fn describe(&self) -> String {
        match self {
            Anchor::Text { start, end } if start == end => format!("At character {start}"),
            Anchor::Text { start, end } => format!("Characters {start}–{end}"),
            Anchor::Time { at } => clock(*at),
            Anchor::TimeRange { start, end } => format!("{} – {}", clock(*start), clock(*end)),
            Anchor::Region { page, rect, .. } => {
                let (w, h) = ((rect[2] - rect[0]).abs(), (rect[3] - rect[1]).abs());
                let at = format!("{:.0}, {:.0}", rect[0].min(rect[2]), rect[1].min(rect[3]));
                if w < 0.5 && h < 0.5 {
                    format!("Page {}, at {at}", page + 1)
                } else {
                    format!("Page {}, {w:.0}×{h:.0} at {at}", page + 1)
                }
            }
        }
    }
}

/// `m:ss.d` (or `h:mm:ss`) for a time in seconds.
pub fn clock(secs: f64) -> String {
    let secs = if secs.is_finite() { secs.max(0.0) } else { 0.0 };
    let whole = secs.floor() as u64;
    let (h, m, s) = (whole / 3600, (whole / 60) % 60, whole % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!(
            "{m}:{s:02}.{}",
            ((secs - whole as f64) * 10.0).floor() as u64
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_a_viewer_by_kind_and_name() {
        assert_eq!(
            ViewKind::pick("text", "document", "README.md"),
            Some(ViewKind::Markdown)
        );
        assert_eq!(
            ViewKind::pick("text", "document", "log.txt"),
            Some(ViewKind::Text)
        );
        assert_eq!(
            ViewKind::pick("sheet", "document", "a.csv"),
            Some(ViewKind::Text)
        );
        assert_eq!(ViewKind::pick("sheet", "document", "a.xlsx"), None);
        assert_eq!(ViewKind::pick("code", "", "main.rs"), Some(ViewKind::Code));
        assert_eq!(ViewKind::pick("chart", "", "Song.kf"), Some(ViewKind::Code));
        assert_eq!(ViewKind::pick("image", "", "a.HEIC"), None);
        assert_eq!(ViewKind::pick("image", "", "a.PNG"), Some(ViewKind::Image));
        assert_eq!(
            ViewKind::pick("audio", "", "Kick.wav"),
            Some(ViewKind::Audio)
        );
        assert_eq!(ViewKind::pick("midi", "audio", "groove.mid"), None);
        assert_eq!(
            ViewKind::pick("pdf", "document", "Chart.pdf"),
            Some(ViewKind::Pdf)
        );
        // An app's kind falls back to its broader kind.
        assert_eq!(
            ViewKind::pick("studio.chart", "pdf", "x.pdf"),
            Some(ViewKind::Pdf)
        );
        assert_eq!(ViewKind::pick("project", "", "Song.rpp"), None);
    }

    #[test]
    fn anchors_travel_as_json() {
        let a = Anchor::Region {
            page: 1,
            rect: [72.0, 300.0, 192.0, 340.0],
            user: None,
            text: String::new(),
        };
        let j = serde_json::to_string(&a).unwrap();
        assert_eq!(
            j,
            r#"{"kind":"region","page":1,"rect":[72.0,300.0,192.0,340.0]}"#
        );
        assert_eq!(serde_json::from_str::<Anchor>(&j).unwrap(), a);
        assert_eq!(a.describe(), "Page 2, 120×40 at 72, 300");
        assert_eq!(
            Anchor::TimeRange {
                start: 62.5,
                end: 3725.0
            }
            .describe(),
            "1:02.5 – 1:02:05"
        );
    }
}
