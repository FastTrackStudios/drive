//! Pictures: an image decoded (turned upright by its EXIF orientation), a
//! PDF's first page, a waveform drawn; and the two thumbnail sizes made
//! from any of them.

use std::io::Cursor;
use std::path::Path;

use image::{DynamicImage, ImageDecoder, ImageReader, Rgba, RgbaImage};

use super::{Error, Made};
use crate::Kind;
use crate::peaks::Peaks;

/// A picture to make thumbnails from.
pub struct Picture(pub DynamicImage);

/// Most memory a decoded image may take.
const MAX_ALLOC: u64 = 1 << 30;

impl Picture {
    /// An image file, upright (its EXIF orientation applied).
    pub fn open(path: &Path) -> Result<Picture, Error> {
        let r = ImageReader::open(path)
            .map_err(|e| Error::Failed(e.to_string()))?
            .with_guessed_format()
            .map_err(|e| Error::Failed(e.to_string()))?;
        decode(r)
    }

    /// Image bytes (cover art), upright.
    pub fn from_bytes(bytes: &[u8]) -> Result<Picture, Error> {
        let r = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| Error::Failed(e.to_string()))?;
        decode(r)
    }

    /// A PDF's first page, on white, about 1024 px on its longer side.
    pub fn pdf_first_page(path: &Path) -> Result<Picture, Error> {
        use pdfcraft_render::{PageRenderer, RenderConfig, RenderRequest, RequestKind, inspect};
        use std::sync::Arc;
        let bytes = Arc::new(std::fs::read(path).map_err(|e| Error::Failed(e.to_string()))?);
        let info = inspect(bytes.clone(), None).map_err(|e| Error::Failed(e.to_string()))?;
        let first = info
            .pages
            .first()
            .ok_or_else(|| Error::Failed("the PDF has no pages".into()))?;
        let longer = first.width.max(first.height).max(1.0);
        let mut renderer = PageRenderer::new(bytes, RenderConfig::default());
        let r = renderer.render(RenderRequest {
            page: 0,
            kind: RequestKind::Pixels,
            scale: 1024.0 / longer,
            ..RenderRequest::default()
        });
        if let Some(e) = r.error {
            return Err(Error::Failed(e));
        }
        // Premultiplied RGBA over white.
        let mut rgba = r.rgba.to_vec();
        for px in rgba.as_chunks_mut::<4>().0 {
            let under = 255 - px[3] as u16;
            for c in &mut px[..3] {
                *c = (*c as u16 + under).min(255) as u8;
            }
            px[3] = 255;
        }
        let img = RgbaImage::from_raw(r.width, r.height, rgba)
            .ok_or_else(|| Error::Failed("a page of the wrong size".into()))?;
        Ok(Picture(DynamicImage::ImageRgba8(img)))
    }

    /// The two thumbnails: about 1024 and 256 px on the longer side (never
    /// enlarged). JPEG when opaque, PNG when it has transparency.
    pub fn thumbs(&self) -> Vec<Made> {
        let big = fit(&self.0, 1024);
        let small = fit(&big, 256);
        [(Kind::Thumb1024, big), (Kind::Thumb256, small)]
            .into_iter()
            .filter_map(|(kind, img)| {
                let (mime, bytes) = encode(&img)?;
                Some(Made { kind, mime, bytes })
            })
            .collect()
    }
}

fn decode(mut r: ImageReader<impl std::io::BufRead + std::io::Seek>) -> Result<Picture, Error> {
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(MAX_ALLOC);
    r.limits(limits);
    let mut dec = r.into_decoder().map_err(|e| Error::Failed(e.to_string()))?;
    let orientation = dec.orientation().ok();
    let mut img = DynamicImage::from_decoder(dec).map_err(|e| Error::Failed(e.to_string()))?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    Ok(Picture(img))
}

fn fit(img: &DynamicImage, max: u32) -> DynamicImage {
    if img.width() <= max && img.height() <= max {
        img.clone()
    } else {
        img.thumbnail(max, max)
    }
}

fn has_alpha(img: &DynamicImage) -> bool {
    img.color().has_alpha() && img.to_rgba8().pixels().any(|p| p[3] < 255)
}

fn encode(img: &DynamicImage) -> Option<(&'static str, Vec<u8>)> {
    let mut out = Vec::new();
    if has_alpha(img) {
        img.to_rgba8()
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .ok()?;
        Some(("image/png", out))
    } else {
        let rgb = img.to_rgb8();
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 82);
        rgb.write_with_encoder(enc).ok()?;
        Some(("image/jpeg", out))
    }
}

/// A waveform drawn (for audio with no cover art): 1024 × 512, the peaks
/// as bars on a transparent ground.
pub fn waveform_picture(p: &Peaks) -> Picture {
    const W: u32 = 1024;
    const H: u32 = 512;
    let mut img = RgbaImage::from_pixel(W, H, Rgba([0, 0, 0, 0]));
    let cols = p.columns(W as usize / 4);
    let ink = Rgba([86, 120, 214, 255]);
    let mid = H as f32 / 2.0;
    let step = W as f32 / cols.len().max(1) as f32;
    for (i, (lo, hi)) in cols.iter().enumerate() {
        let x0 = (i as f32 * step) as u32;
        let x1 = (((i + 1) as f32 * step) as u32)
            .saturating_sub(1)
            .max(x0 + 1)
            .min(W);
        let y0 = (mid - hi * mid * 0.95).floor().clamp(0.0, H as f32 - 1.0) as u32;
        let y1 = (mid - lo * mid * 0.95)
            .ceil()
            .clamp(y0 as f32 + 2.0, H as f32) as u32;
        for x in x0..x1 {
            for y in y0..y1.min(H) {
                img.put_pixel(x, y, ink);
            }
        }
    }
    Picture(DynamicImage::ImageRgba8(img))
}
