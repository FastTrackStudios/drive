//! Audio: peaks from one streaming decode (a packet at a time, so memory
//! stays flat however long the file), and embedded cover art.

use std::fs::File;
use std::path::Path;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardVisualKey, Visual};
use symphonia::core::probe::Hint;

use super::picture::Picture;
use super::{Error, ext};
use crate::peaks::{Builder, Peaks};

fn open(path: &Path, name: &str) -> Result<symphonia::core::probe::ProbeResult, Error> {
    let file = File::open(path).map_err(|e| Error::Failed(e.to_string()))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(&ext(name));
    symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| match e {
            SymError::Unsupported(w) => Error::NoGenerator(format!("this audio format: {w}")),
            e => Error::Failed(e.to_string()),
        })
}

/// The waveform of an audio file, decoded as a stream.
pub fn peaks_of(path: &Path, name: &str) -> Result<Peaks, Error> {
    let probed = open(path, name)?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| Error::Failed("no audio track".into()))?;
    let id = track.id;
    let params = track.codec_params.clone();
    let mut decoder = symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| Error::NoGenerator(format!("this codec: {e}")))?;
    let channels = params.channels.map_or(1, |c| c.count());
    let rate = params.sample_rate.unwrap_or(48_000);
    let mut builder = Builder::new(channels, rate, params.n_frames);
    let mut buf: Option<SampleBuffer<f32>> = None;
    let mut errors = 0u32;
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            // A read error before any sound is a failure; after, the sound
            // so far is kept (a truncated recording still has a waveform).
            Err(e) if builder.frames() == 0 => return Err(Error::Failed(e.to_string())),
            Err(_) => break,
        };
        if packet.track_id() != id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                let spec = *decoded.spec();
                let need = decoded.capacity() as u64;
                if buf
                    .as_ref()
                    .is_none_or(|b| (b.capacity() as u64) < need * spec.channels.count() as u64)
                {
                    buf = Some(SampleBuffer::new(need, spec));
                }
                let b = buf.as_mut().expect("a sample buffer");
                b.copy_interleaved_ref(decoded);
                builder.push(b.samples(), spec.channels.count());
            }
            // A damaged packet: skip it, as players do.
            Err(SymError::DecodeError(_)) | Err(SymError::IoError(_)) => {
                errors += 1;
                if errors > 1000 {
                    return Err(Error::Failed("too many damaged packets".into()));
                }
            }
            Err(e) => return Err(Error::Failed(e.to_string())),
        }
    }
    Ok(builder.finish())
}

/// The cover art in an audio file's tags (ID3 APIC, FLAC PICTURE, MP4
/// `covr`, Vorbis comments), upright, if any decodes.
pub fn cover_art(path: &Path, name: &str) -> Option<Picture> {
    let mut probed = open(path, name).ok()?;
    let mut visuals: Vec<Visual> = Vec::new();
    if let Some(rev) = probed.metadata.get().as_ref().and_then(|m| m.current()) {
        visuals.extend(rev.visuals().iter().cloned());
    }
    if let Some(rev) = probed.format.metadata().current() {
        visuals.extend(rev.visuals().iter().cloned());
    }
    // The front cover first, then whatever there is.
    visuals.sort_by_key(|v| v.usage != Some(StandardVisualKey::FrontCover));
    visuals
        .iter()
        .find_map(|v| Picture::from_bytes(&v.data).ok())
}
