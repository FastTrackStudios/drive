//! Text, Markdown and code: the first lines drawn on a page (Inter, from
//! pdfcraft's fonts), as a picture.

use std::io::Read;
use std::path::Path;

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use image::{DynamicImage, Rgb, RgbImage};

use super::Error;
use super::picture::Picture;

/// How much of the file is read.
const HEAD: u64 = 16 << 10;
const W: u32 = 768;
const H: u32 = 1024;
const MARGIN: f32 = 40.0;
const SIZE: f32 = 26.0;
const LEADING: f32 = 36.0;

/// The first lines of a text file on a page (768 × 1024).
pub fn first_lines(path: &Path, name: &str) -> Result<Picture, Error> {
    let mut head = Vec::new();
    std::fs::File::open(path)
        .map_err(|e| Error::Failed(e.to_string()))?
        .take(HEAD)
        .read_to_end(&mut head)
        .map_err(|e| Error::Failed(e.to_string()))?;
    if head.iter().take(4096).any(|&b| b == 0) {
        return Err(Error::NoGenerator("binary, not text".into()));
    }
    let text = String::from_utf8_lossy(&head);
    let md = name.to_ascii_lowercase().ends_with(".md");
    Ok(draw(&text, md))
}

pub(crate) fn draw(text: &str, markdown: bool) -> Picture {
    let font = FontRef::try_from_slice(pdfcraft_fonts::INTER_REGULAR).expect("Inter loads");
    let mut img = RgbImage::from_pixel(W, H, Rgb([255, 255, 255]));
    let lines = ((H as f32 - 2.0 * MARGIN) / LEADING) as usize;
    let mut y = MARGIN + SIZE;
    for line in text.lines().take(lines) {
        let line = line.replace('\t', "    ");
        // A Markdown heading a little bigger and darker.
        let heading = markdown && line.starts_with('#');
        let (body, size, ink) = if heading {
            (line.trim_start_matches('#').trim_start(), SIZE * 1.3, 20u8)
        } else {
            (line.as_str(), SIZE, 60u8)
        };
        draw_line(&mut img, &font, body, MARGIN, y, size, ink);
        y += if heading { LEADING * 1.3 } else { LEADING };
        if y > H as f32 - MARGIN {
            break;
        }
    }
    Picture(DynamicImage::ImageRgb8(img))
}

fn draw_line(
    img: &mut RgbImage,
    font: &FontRef,
    text: &str,
    x0: f32,
    baseline: f32,
    size: f32,
    ink: u8,
) {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut x = x0;
    let mut last = None;
    for ch in text.chars() {
        let id = scaled.glyph_id(ch);
        if let Some(prev) = last {
            x += scaled.kern(prev, id);
        }
        last = Some(id);
        if x > W as f32 - MARGIN {
            break;
        }
        let glyph = id.with_scale_and_position(PxScale::from(size), point(x, baseline));
        x += scaled.h_advance(id);
        let Some(outline) = font.outline_glyph(glyph) else {
            continue;
        };
        let b = outline.px_bounds();
        outline.draw(|gx, gy, cov| {
            let (px, py) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
            if px < 0 || py < 0 || px as u32 >= W || py as u32 >= H {
                return;
            }
            let p = img.get_pixel_mut(px as u32, py as u32);
            for c in &mut p.0 {
                let v = *c as f32 * (1.0 - cov) + ink as f32 * cov;
                *c = v.min(*c as f32).round() as u8;
            }
        });
    }
}
