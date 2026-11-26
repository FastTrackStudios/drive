//! A PDF for viewing: its pages as pixels, their text, search, and the
//! comments already in it, through pdfcraft's renderer
//! (`pdfcraft-render`, the hayro rasterizer). Pure Rust: the same code runs
//! in the browser (wasm32) and natively.
//!
//! Coordinates are a page's **view space**: points, y down, after the
//! page's `/Rotate`, what the page looks like on screen. Multiply by the
//! zoom for CSS pixels. [`PdfDoc::to_user`] gives the PDF's own user space
//! (y up), which is what a PDF annotation's `/Rect` is in.

use std::sync::Arc;

use pdfcraft_render::{
    Annotation, DocInfo, PageRenderer, PageText, RenderConfig, RenderRequest, RequestKind, inspect,
};

/// A page drawn: straight (not premultiplied) RGBA8, row-major, ready for a
/// canvas's `ImageData`.
pub struct Page {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A search hit: its page and the boxes to highlight (view space).
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub page: usize,
    pub rects: Vec<[f32; 4]>,
}

/// An existing comment, for listing beside the page.
#[derive(Clone, Debug, PartialEq)]
pub struct Comment {
    pub page: usize,
    pub kind: String,
    pub author: String,
    pub text: String,
    /// Where it is on its page (view space).
    pub rect: [f32; 4],
}

/// The largest number of search hits kept.
pub const MAX_HITS: usize = 1000;

pub struct PdfDoc {
    renderer: PageRenderer,
    info: DocInfo,
    texts: Vec<Option<Arc<PageText>>>,
}

impl PdfDoc {
    /// Open a PDF's bytes (a protected one: [`pdfcraft_render::OpenError`]'s
    /// words).
    pub fn open(bytes: Vec<u8>) -> Result<PdfDoc, String> {
        let bytes = Arc::new(bytes);
        let info = inspect(bytes.clone(), None).map_err(|e| e.to_string())?;
        if info.pages.is_empty() {
            return Err("the PDF has no pages".into());
        }
        let renderer = PageRenderer::new(bytes, RenderConfig::default());
        let texts = vec![None; info.pages.len()];
        Ok(PdfDoc {
            renderer,
            info,
            texts,
        })
    }

    pub fn pages(&self) -> usize {
        self.info.pages.len()
    }

    pub fn title(&self) -> Option<&str> {
        self.info.title.as_deref().filter(|t| !t.trim().is_empty())
    }

    /// A page's label (`/PageLabels`: `iv`, `A-3`), else its number.
    pub fn label(&self, page: usize) -> String {
        self.info
            .pages
            .get(page)
            .map(|p| p.label.clone())
            .unwrap_or_else(|| (page + 1).to_string())
    }

    /// A page's size as shown, in points.
    pub fn size(&self, page: usize) -> (f32, f32) {
        self.info
            .pages
            .get(page)
            .map(|p| (p.width, p.height))
            .unwrap_or((612.0, 792.0))
    }

    /// Draw a page at `scale` device pixels per point.
    pub fn render(&mut self, page: usize, scale: f32) -> Result<Page, String> {
        let r = self.renderer.render(RenderRequest {
            page,
            kind: RequestKind::Pixels,
            scale,
            ..RenderRequest::default()
        });
        if let Some(e) = r.error {
            return Err(e);
        }
        let mut rgba = r.rgba.to_vec();
        unpremultiply(&mut rgba);
        Ok(Page {
            width: r.width,
            height: r.height,
            rgba,
        })
    }

    /// A page's text layer (read once, then kept).
    pub fn text(&mut self, page: usize) -> Option<Arc<PageText>> {
        if let Some(Some(t)) = self.texts.get(page) {
            return Some(t.clone());
        }
        let r = self.renderer.render(RenderRequest {
            page,
            kind: RequestKind::Text,
            scale: 1.0,
            ..RenderRequest::default()
        });
        let t = r.text?;
        if let Some(slot) = self.texts.get_mut(page) {
            *slot = Some(t.clone());
        }
        Some(t)
    }

    /// Every place `needle` occurs (case folded), in page order.
    pub fn search(&mut self, needle: &str) -> Vec<Hit> {
        let needle = needle.trim();
        let mut hits = Vec::new();
        if needle.is_empty() {
            return hits;
        }
        for page in 0..self.pages() {
            let Some(t) = self.text(page) else { continue };
            for range in t.find_opts_limited(needle, false, false, MAX_HITS - hits.len()) {
                hits.push(Hit {
                    page,
                    rects: t.line_rects(range),
                });
            }
            if hits.len() >= MAX_HITS {
                break;
            }
        }
        hits
    }

    /// The text inside a region of a page (empty for a click).
    pub fn text_in(&mut self, page: usize, rect: [f32; 4]) -> String {
        if (rect[2] - rect[0]).abs() < 1.0 || (rect[3] - rect[1]).abs() < 1.0 {
            return String::new();
        }
        let Some(t) = self.text(page) else {
            return String::new();
        };
        let glyphs = t.glyphs_in(rect);
        t.column_text(&glyphs)
    }

    /// A region of a page in the PDF's own coordinates (user space, y up).
    pub fn to_user(&self, page: usize, rect: [f32; 4]) -> Option<[f32; 4]> {
        self.info.pages.get(page).map(|p| p.view_rect_to_user(rect))
    }

    /// The comments already in the PDF (markup annotations; not links or
    /// form fields), in page order.
    pub fn comments(&self) -> Vec<Comment> {
        self.info
            .annotations
            .iter()
            .filter(|a| !matches!(a.subtype.as_str(), "Link" | "Widget" | "Popup"))
            .map(|a| comment(&self.info, a))
            .collect()
    }
}

fn comment(info: &DocInfo, a: &Annotation) -> Comment {
    let rect = info
        .pages
        .get(a.page)
        .map(|p| {
            let [x0, y0] = p.user_to_view(a.rect[0], a.rect[1]);
            let [x1, y1] = p.user_to_view(a.rect[2], a.rect[3]);
            [x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)]
        })
        .unwrap_or(a.rect);
    Comment {
        page: a.page,
        kind: a.subtype.clone(),
        author: a.author.clone().unwrap_or_default(),
        text: a.contents.clone().unwrap_or_default(),
        rect,
    }
}

/// Premultiplied RGBA8 → straight, in place.
fn unpremultiply(px: &mut [u8]) {
    for p in px.as_chunks_mut::<4>().0 {
        let a = p[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A one-page PDF saying "Hello Drive" (offsets computed, so the xref
    /// table is right).
    pub(crate) fn hello() -> Vec<u8> {
        let content = "BT /F1 24 Tf 72 700 Td (Hello Drive) Tj ET";
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R 6 0 R] /Count 2 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> /Annots [7 0 R] >>".to_string(),
            format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 400] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_string(),
            "<< /Type /Annot /Subtype /Text /Rect [72 600 92 620] /Contents (Check the bridge) /T (Cody) >>".to_string(),
        ];
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
        for o in offsets {
            out.extend(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objs.len() + 1
            )
            .as_bytes(),
        );
        out
    }

    #[test]
    fn renders_searches_and_reads_comments() {
        let mut d = PdfDoc::open(hello()).expect("opens");
        assert_eq!(d.pages(), 2);
        assert_eq!(d.size(1), (300.0, 400.0));
        let p = d.render(0, 0.5).expect("renders");
        assert_eq!((p.width, p.height), (306, 396));
        assert_eq!(p.rgba.len(), 306 * 396 * 4);
        // Somewhere on the page is ink.
        assert!(p.rgba.as_chunks::<4>().0.iter().any(|px| px[0] < 128));

        // Page 2 is too small to show its text (it sits at y = 700): one hit.
        let hits = d.search("hello DRIVE");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].page, 0);
        let r = hits[0].rects[0];
        // Near the top left: 72 pt in, 92 pt down (y flipped from 700).
        assert!(
            (60.0..90.0).contains(&r[0]) && (60.0..100.0).contains(&r[1]),
            "{r:?}"
        );
        assert_eq!(
            d.text_in(0, [60.0, 60.0, 300.0, 100.0]).trim(),
            "Hello Drive"
        );
        assert_eq!(d.text_in(0, [60.0, 60.0, 60.0, 60.0]), "");
        let user = d.to_user(0, [72.0, 92.0, 92.0, 112.0]).unwrap();
        assert_eq!(user, [72.0, 680.0, 92.0, 700.0]);

        let c = d.comments();
        assert_eq!(c.len(), 1);
        assert_eq!(
            (c[0].page, c[0].author.as_str(), c[0].text.as_str()),
            (0, "Cody", "Check the bridge")
        );
        assert_eq!(c[0].rect, [72.0, 172.0, 92.0, 192.0]);
        assert!(PdfDoc::open(b"not a pdf".to_vec()).is_err());
    }

    #[test]
    fn straight_alpha() {
        let mut px = [64, 32, 0, 128, 10, 20, 30, 255, 0, 0, 0, 0];
        unpremultiply(&mut px);
        assert_eq!(px, [128, 64, 0, 128, 10, 20, 30, 255, 0, 0, 0, 0]);
    }
}
