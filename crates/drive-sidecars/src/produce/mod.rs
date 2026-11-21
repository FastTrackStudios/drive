//! The native producers: a file on disk in, its sidecars out. Everything
//! here blocks (run it on a blocking thread) and reads files as streams
//! where it can: audio is decoded packet by packet, video is read by
//! ffmpeg; images, PDFs and text are read whole only under a size cap.

mod audio;
mod picture;
mod text;
mod video;

use std::path::Path;

use crate::{Generator, Kind, generator_for};

pub use audio::{cover_art, peaks_of};
pub use picture::{Picture, waveform_picture};
pub use video::ffmpeg;

/// One sidecar made: its kind, its MIME type and its bytes.
#[derive(Clone, Debug)]
pub struct Made {
    pub kind: Kind,
    pub mime: &'static str,
    pub bytes: Vec<u8>,
}

/// Why nothing was made.
#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    /// No producer here can read it (HEIC, video without ffmpeg, a file
    /// over the size cap): not worth asking again until a producer changes.
    NoGenerator(String),
    /// A producer tried and failed (a damaged file).
    Failed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoGenerator(w) => write!(f, "no generator: {w}"),
            Error::Failed(w) => write!(f, "failed: {w}"),
        }
    }
}

impl std::error::Error for Error {}

/// Limits on what the producers read.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Images and PDFs bigger than this are not read (they are read whole).
    pub whole_file_max: u64,
    /// Audio and video bigger than this are not read (they are streamed,
    /// so this is about time, not memory).
    pub stream_max: u64,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            whole_file_max: 256 << 20,
            stream_max: 16 << 30,
        }
    }
}

/// Make the sidecars of `kinds` for the file at `path` (named `name`: its
/// extension picks the producer). Kinds the file does not get are skipped.
pub fn produce(
    path: &Path,
    name: &str,
    kinds: &[Kind],
    limits: &Limits,
) -> Result<Vec<Made>, Error> {
    let generator = generator_for(name)
        .ok_or_else(|| Error::NoGenerator(format!("nothing reads .{}", ext(name))))?;
    let want: Vec<Kind> = crate::kinds_for(name)
        .into_iter()
        .filter(|k| kinds.contains(k))
        .collect();
    if want.is_empty() {
        return Ok(Vec::new());
    }
    let size = std::fs::metadata(path)
        .map_err(|e| Error::Failed(e.to_string()))?
        .len();
    let thumbs = want.iter().any(|k| k.family() == "thumb");
    let picture = |p: Result<Picture, Error>| -> Result<Vec<Made>, Error> {
        let p = p?;
        Ok(p.thumbs()
            .into_iter()
            .filter(|m| want.contains(&m.kind))
            .collect())
    };
    match generator {
        Generator::Image => {
            cap(size, limits.whole_file_max)?;
            picture(Picture::open(path))
        }
        Generator::Pdf => {
            cap(size, limits.whole_file_max)?;
            picture(Picture::pdf_first_page(path))
        }
        Generator::Text => picture(text::first_lines(path, name)),
        Generator::Video => {
            cap(size, limits.stream_max)?;
            picture(video::frame(path))
        }
        Generator::Audio => {
            cap(size, limits.stream_max)?;
            let mut out = Vec::new();
            let cover = if thumbs { cover_art(path, name) } else { None };
            let needs_peaks = want.contains(&Kind::Peaks) || (thumbs && cover.is_none());
            let peaks = if needs_peaks {
                Some(peaks_of(path, name)?)
            } else {
                None
            };
            if let Some(p) = &peaks
                && want.contains(&Kind::Peaks)
            {
                out.push(Made {
                    kind: Kind::Peaks,
                    mime: "application/x-drive-peaks",
                    bytes: p.encode(),
                });
            }
            if thumbs {
                let pic = match (cover, &peaks) {
                    (Some(c), _) => c,
                    (None, Some(p)) => waveform_picture(p),
                    (None, None) => return Ok(out),
                };
                out.extend(pic.thumbs().into_iter().filter(|m| want.contains(&m.kind)));
            }
            Ok(out)
        }
    }
}

fn cap(size: u64, max: u64) -> Result<(), Error> {
    if size > max {
        Err(Error::NoGenerator(format!(
            "{} MB is over the {} MB cap",
            size >> 20,
            max >> 20
        )))
    } else {
        Ok(())
    }
}

fn ext(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
