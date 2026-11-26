//! Each producer on a fixture made here (no binaries in git).

use std::io::Write;
use std::path::{Path, PathBuf};

use image::{GenericImageView, ImageEncoder, Rgb, RgbImage};

use super::*;
use crate::peaks::Peaks;

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

fn decoded(m: &Made) -> image::DynamicImage {
    image::load_from_memory(&m.bytes).unwrap()
}

fn get(made: &[Made], k: Kind) -> &Made {
    made.iter().find(|m| m.kind == k).unwrap()
}

/// A JPEG, 400 × 200, left half red, with an EXIF orientation of 6 (turn
/// 90° clockwise to view).
fn jpeg_rotated() -> Vec<u8> {
    let mut img = RgbImage::from_pixel(400, 200, Rgb([20, 20, 220]));
    for x in 0..200 {
        for y in 0..200 {
            img.put_pixel(x, y, Rgb([220, 20, 20]));
        }
    }
    let mut jpg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg, 90)
        .write_image(&img, 400, 200, image::ExtendedColorType::Rgb8)
        .unwrap();
    // APP1 Exif: a little-endian TIFF with one entry, Orientation = 6.
    let mut tiff = b"II*\0\x08\0\0\0".to_vec();
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend_from_slice(&0x0112u16.to_le_bytes());
    tiff.extend_from_slice(&3u16.to_le_bytes());
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&6u32.to_le_bytes());
    tiff.extend_from_slice(&0u32.to_le_bytes());
    let mut app1 = b"Exif\0\0".to_vec();
    app1.extend_from_slice(&tiff);
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
    out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(&app1);
    out.extend_from_slice(&jpg[2..]);
    out
}

#[test]
fn images_are_turned_upright_in_two_sizes() {
    let dir = tempfile::tempdir().unwrap();
    let p = write(dir.path(), "IMG_1.jpg", &jpeg_rotated());
    let made = produce(&p, "IMG_1.jpg", &Kind::ALL, &Limits::default()).unwrap();
    assert_eq!(made.len(), 2, "images get no peaks");
    let small = decoded(get(&made, Kind::Thumb256));
    // Upright: 200 wide, 400 tall, so 128 × 256; the red half is on top.
    assert_eq!(small.dimensions(), (128, 256));
    let top = small.get_pixel(64, 20);
    let bottom = small.get_pixel(64, 236);
    assert!(top[0] > 150 && top[2] < 100, "{top:?}");
    assert!(bottom[2] > 150 && bottom[0] < 100, "{bottom:?}");
    assert_eq!(get(&made, Kind::Thumb256).mime, "image/jpeg");
    // Never enlarged.
    assert_eq!(
        decoded(get(&made, Kind::Thumb1024)).dimensions(),
        (200, 400)
    );
}

#[test]
fn transparent_images_stay_png() {
    let dir = tempfile::tempdir().unwrap();
    let img = image::RgbaImage::from_fn(2000, 1000, |x, _| image::Rgba([0, 0, 0, (x % 256) as u8]));
    let p = dir.path().join("logo.png");
    img.save(&p).unwrap();
    let made = produce(&p, "logo.png", &[Kind::Thumb1024], &Limits::default()).unwrap();
    assert_eq!(made.len(), 1);
    assert_eq!(made[0].mime, "image/png");
    assert_eq!(decoded(&made[0]).dimensions(), (1024, 512));
}

/// A PDF with one US Letter page, a black square in its top-left corner.
fn pdf() -> Vec<u8> {
    let content = b"0 0 0 rg 0 692 100 100 re f";
    let objs = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R >>".to_string(),
        format!(
            "<< /Length {} >>\nstream\n{}\nendstream",
            content.len(),
            std::str::from_utf8(content).unwrap()
        ),
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offsets.push(out.len());
        write!(out, "{} 0 obj\n{o}\nendobj\n", i + 1).unwrap();
    }
    let xref = out.len();
    write!(out, "xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).unwrap();
    for o in offsets {
        writeln!(out, "{o:010} 00000 n ").unwrap();
    }
    write!(
        out,
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objs.len() + 1
    )
    .unwrap();
    out
}

#[test]
fn a_pdf_gives_its_first_page() {
    let dir = tempfile::tempdir().unwrap();
    let p = write(dir.path(), "score.pdf", &pdf());
    let made = produce(&p, "score.pdf", &Kind::ALL, &Limits::default()).unwrap();
    let big = decoded(get(&made, Kind::Thumb1024));
    let (w, h) = big.dimensions();
    assert_eq!(h, 1024);
    assert!((790..=792).contains(&w), "{w}");
    // The square is black, the page around it white.
    assert!(big.get_pixel(40, 40)[0] < 40);
    assert!(big.get_pixel(400, 600)[0] > 240);
}

/// A 16-bit PCM WAV of a tone.
fn wav(secs: f32, rate: u32, channels: u16) -> Vec<u8> {
    let frames = (secs * rate as f32) as u32;
    let data_len = frames * channels as u32 * 2;
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
    out.extend_from_slice(&(channels * 2).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for f in 0..frames {
        // Louder over time: the waveform grows left to right.
        let amp = 0.1 + 0.8 * f as f32 / frames as f32;
        let v = amp * (f as f32 * 0.07).sin();
        for c in 0..channels {
            let s = if c == 0 { v } else { v / 2.0 };
            out.extend_from_slice(&((s * 32767.0) as i16).to_le_bytes());
        }
    }
    out
}

#[test]
fn audio_gets_peaks_and_a_waveform_picture() {
    let dir = tempfile::tempdir().unwrap();
    let p = write(dir.path(), "take.wav", &wav(3.0, 44_100, 2));
    let made = produce(&p, "take.wav", &Kind::ALL, &Limits::default()).unwrap();
    assert_eq!(made.len(), 3);
    let peaks = Peaks::decode(&get(&made, Kind::Peaks).bytes).unwrap();
    assert_eq!(peaks.channels, 2);
    assert_eq!(peaks.sample_rate, 44_100);
    assert_eq!(peaks.frames, 132_300);
    let cols = peaks.columns(100);
    assert!(cols[5].1 < cols[95].1, "it grows");
    assert!(cols[95].1 > 0.8);
    // Left is twice as loud as right.
    let finest = &peaks.levels[0].data;
    let n = finest.len() / 2;
    let (l, r) = (finest[(n - 2) * 2].1, finest[(n - 2) * 2 + 1].1);
    assert!((l as i32 - 2 * r as i32).abs() <= 2, "{l} {r}");
    // No cover: the waveform, drawn, with transparency.
    let t = get(&made, Kind::Thumb256);
    assert_eq!(t.mime, "image/png");
    assert_eq!(decoded(t).dimensions(), (256, 128));
}

/// An MP3: an ID3v2.3 tag with a front cover (APIC), then a second of
/// silent MPEG-1 Layer III frames (128 kb/s, 44.1 kHz).
fn mp3_with_cover(cover_png: &[u8]) -> Vec<u8> {
    let mut apic = vec![0u8]; // Latin-1
    apic.extend_from_slice(b"image/png\0");
    apic.push(3); // front cover
    apic.push(0); // no description
    apic.extend_from_slice(cover_png);
    let mut frame = b"APIC".to_vec();
    frame.extend_from_slice(&(apic.len() as u32).to_be_bytes());
    frame.extend_from_slice(&[0, 0]);
    frame.extend_from_slice(&apic);
    let n = frame.len() as u32;
    let syncsafe = [
        (n >> 21) as u8 & 0x7f,
        (n >> 14) as u8 & 0x7f,
        (n >> 7) as u8 & 0x7f,
        n as u8 & 0x7f,
    ];
    let mut out = b"ID3\x03\0\0".to_vec();
    out.extend_from_slice(&syncsafe);
    out.extend_from_slice(&frame);
    for _ in 0..38 {
        let mut f = vec![0u8; 417];
        f[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        out.extend_from_slice(&f);
    }
    out
}

#[test]
fn cover_art_comes_first() {
    let dir = tempfile::tempdir().unwrap();
    let mut png = Vec::new();
    RgbImage::from_pixel(64, 64, Rgb([0, 200, 0]))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let p = write(dir.path(), "song.mp3", &mp3_with_cover(&png));
    let made = produce(&p, "song.mp3", &[Kind::Thumb256], &Limits::default()).unwrap();
    assert_eq!(made.len(), 1);
    let t = decoded(&made[0]);
    assert_eq!(t.dimensions(), (64, 64));
    assert!(t.get_pixel(10, 10)[1] > 150);
}

#[test]
fn text_is_drawn_and_binary_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let p = write(
        dir.path(),
        "notes.md",
        b"# Notes\n\nThe bridge goes to the IV.\n\tthen home\n",
    );
    let made = produce(&p, "notes.md", &Kind::ALL, &Limits::default()).unwrap();
    let big = decoded(get(&made, Kind::Thumb1024)).to_rgb8();
    assert_eq!(big.dimensions(), (768, 1024));
    let inked = big.pixels().filter(|p| p[0] < 128).count();
    assert!(inked > 200, "{inked} dark pixels");
    let p = write(dir.path(), "data.json", &[0u8, 1, 2, 3]);
    assert!(matches!(
        produce(&p, "data.json", &Kind::ALL, &Limits::default()),
        Err(Error::NoGenerator(_))
    ));
}

#[test]
fn what_cannot_be_read_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let p = write(dir.path(), "IMG_2.heic", b"not really");
    assert!(matches!(
        produce(&p, "IMG_2.heic", &Kind::ALL, &Limits::default()),
        Err(Error::NoGenerator(_))
    ));
    let p = write(dir.path(), "broken.png", b"\x89PNG broken");
    assert!(matches!(
        produce(&p, "broken.png", &Kind::ALL, &Limits::default()),
        Err(Error::Failed(_))
    ));
    let p = write(dir.path(), "big.jpg", &jpeg_rotated());
    let tiny = Limits {
        whole_file_max: 10,
        stream_max: 10,
    };
    assert!(matches!(
        produce(&p, "big.jpg", &Kind::ALL, &tiny),
        Err(Error::NoGenerator(_))
    ));
}

#[test]
fn video_needs_ffmpeg() {
    let dir = tempfile::tempdir().unwrap();
    let Some(ff) = ffmpeg() else {
        let p = write(dir.path(), "clip.mp4", b"not a video");
        assert!(matches!(
            produce(&p, "clip.mp4", &Kind::ALL, &Limits::default()),
            Err(Error::NoGenerator(_))
        ));
        return;
    };
    let p = dir.path().join("clip.mp4");
    let ok = std::process::Command::new(ff)
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=10:duration=4",
            "-c:v",
            "mpeg4",
        ])
        .arg(&p)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let made = produce(&p, "clip.mp4", &Kind::ALL, &Limits::default()).unwrap();
    assert_eq!(decoded(get(&made, Kind::Thumb256)).dimensions(), (256, 192));
}
