//! Waveform peaks: the lowest and highest sample of each slice of a sound,
//! at a few zoom levels, mono or stereo, in a compact binary form.
//!
//! Built natively in one pass over a stream of samples ([`Builder`]: the
//! memory it holds is bounded whatever the length, so an hours-long voice
//! memo costs the same as a song), and read in the browser ([`Peaks::decode`],
//! [`Peaks::columns`]).
//!
//! The format (little-endian):
//!
//! ```text
//! "DWPK"          magic
//! u8   version    (1)
//! u8   channels   (1 or 2)
//! u16  levels     (n)
//! u32  sample rate
//! u64  frames     (samples per channel)
//! n × (u32 samples per bucket, u32 buckets)      finest first
//! for each level: buckets × channels × (i8 min, i8 max)   (±127 = full scale)
//! ```

/// The finest level never has more buckets than this (it is halved, its
/// buckets doubled in length, when it would): 8192 buckets × 2 channels ×
/// 2 bytes is 32 KiB, plus about a third more for the coarser levels.
pub const FINEST_MAX: usize = 8192;
/// The shortest bucket, in samples.
pub const MIN_SPAN: u32 = 256;
/// Coarser levels are this many times coarser, down to [`COARSEST`].
pub const STEP: u32 = 4;
pub const COARSEST: usize = 256;

const MAGIC: &[u8; 4] = b"DWPK";
const VERSION: u8 = 1;

/// One zoom level: `buckets` slices of `span` samples, each channel's
/// (min, max) per slice.
#[derive(Clone, Debug, PartialEq)]
pub struct Level {
    pub span: u32,
    /// `[bucket][channel]` → (min, max), interleaved by channel.
    pub data: Vec<(i8, i8)>,
}

impl Level {
    pub fn buckets(&self, channels: usize) -> usize {
        self.data.len() / channels.max(1)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Peaks {
    pub channels: u8,
    pub sample_rate: u32,
    pub frames: u64,
    /// Finest first.
    pub levels: Vec<Level>,
}

fn q(v: f32) -> i8 {
    let v = if v.is_finite() {
        v.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    (v * 127.0).round() as i8
}

/// Builds [`Peaks`] from samples as they are decoded.
pub struct Builder {
    channels: usize,
    sample_rate: u32,
    span: u32,
    /// Finished buckets of the finest level, as floats: `[bucket][channel]`.
    done: Vec<(f32, f32)>,
    /// The bucket being filled.
    cur: Vec<(f32, f32)>,
    filled: u32,
    frames: u64,
}

impl Builder {
    /// `channels` of the source (1, 2 or more: more are folded to two,
    /// even ones left, odd ones right), at `sample_rate`. `frames`: the
    /// length when known, to pick the bucket span up front.
    pub fn new(channels: usize, sample_rate: u32, frames: Option<u64>) -> Builder {
        let out = channels.clamp(1, 2);
        let mut span = MIN_SPAN;
        if let Some(n) = frames {
            while n / span as u64 > FINEST_MAX as u64 {
                span *= 2;
            }
        }
        Builder {
            channels: out,
            sample_rate,
            span,
            done: Vec::new(),
            cur: vec![(0.0, 0.0); out],
            filled: 0,
            frames: 0,
        }
    }

    /// Interleaved samples with `in_channels` channels.
    pub fn push(&mut self, interleaved: &[f32], in_channels: usize) {
        let in_channels = in_channels.max(1);
        for frame in interleaved.chunks_exact(in_channels) {
            for (c, &s) in frame.iter().enumerate() {
                let o = if self.channels == 1 { 0 } else { c % 2 };
                let s = if s.is_finite() {
                    s.clamp(-1.0, 1.0)
                } else {
                    0.0
                };
                let b = &mut self.cur[o];
                b.0 = b.0.min(s);
                b.1 = b.1.max(s);
            }
            self.filled += 1;
            self.frames += 1;
            if self.filled == self.span {
                self.close_bucket();
            }
        }
    }

    fn close_bucket(&mut self) {
        self.done.extend(self.cur.iter().copied());
        self.cur.iter_mut().for_each(|b| *b = (0.0, 0.0));
        self.filled = 0;
        if self.done.len() / self.channels > FINEST_MAX {
            self.done = halve(&self.done, self.channels);
            self.span *= 2;
        }
    }

    /// Frames pushed so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn finish(mut self) -> Peaks {
        if self.filled > 0 {
            self.done.extend(self.cur.iter().copied());
        }
        let ch = self.channels;
        let mut levels = vec![(self.span, self.done)];
        while levels.last().is_some_and(|(_, d)| d.len() / ch > COARSEST) {
            let (span, d) = levels.last().expect("a level");
            let mut next = d.clone();
            for _ in 0..STEP.trailing_zeros() {
                next = halve(&next, ch);
            }
            levels.push((span * STEP, next));
        }
        Peaks {
            channels: ch as u8,
            sample_rate: self.sample_rate,
            frames: self.frames,
            levels: levels
                .into_iter()
                .map(|(span, d)| Level {
                    span,
                    data: d.into_iter().map(|(lo, hi)| (q(lo), q(hi))).collect(),
                })
                .collect(),
        }
    }
}

/// Pairs of buckets as one (the last alone when odd).
fn halve(d: &[(f32, f32)], ch: usize) -> Vec<(f32, f32)> {
    let n = d.len() / ch;
    let mut out = Vec::with_capacity(n.div_ceil(2) * ch);
    for b in (0..n).step_by(2) {
        for c in 0..ch {
            let x = d[b * ch + c];
            let y = if b + 1 < n { d[(b + 1) * ch + c] } else { x };
            out.push((x.0.min(y.0), x.1.max(y.1)));
        }
    }
    out
}

impl Peaks {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.push(self.channels);
        out.extend_from_slice(&(self.levels.len() as u16).to_le_bytes());
        out.extend_from_slice(&self.sample_rate.to_le_bytes());
        out.extend_from_slice(&self.frames.to_le_bytes());
        let ch = self.channels as usize;
        for l in &self.levels {
            out.extend_from_slice(&l.span.to_le_bytes());
            out.extend_from_slice(&(l.buckets(ch) as u32).to_le_bytes());
        }
        for l in &self.levels {
            for (lo, hi) in &l.data {
                out.push(*lo as u8);
                out.push(*hi as u8);
            }
        }
        out
    }

    pub fn decode(b: &[u8]) -> Result<Peaks, String> {
        let bad = || "not a peaks file".to_string();
        if b.len() < 20 || &b[..4] != MAGIC {
            return Err(bad());
        }
        if b[4] != VERSION {
            return Err(format!("peaks version {} is not read here", b[4]));
        }
        let channels = b[5];
        if !(1..=2).contains(&channels) {
            return Err(bad());
        }
        let n = u16::from_le_bytes([b[6], b[7]]) as usize;
        let sample_rate = u32::from_le_bytes(b[8..12].try_into().map_err(|_| bad())?);
        let frames = u64::from_le_bytes(b[12..20].try_into().map_err(|_| bad())?);
        let mut at = 20;
        let mut heads = Vec::with_capacity(n);
        for _ in 0..n {
            let h = b.get(at..at + 8).ok_or_else(bad)?;
            let span = u32::from_le_bytes(h[..4].try_into().map_err(|_| bad())?);
            let buckets = u32::from_le_bytes(h[4..].try_into().map_err(|_| bad())?) as usize;
            heads.push((span, buckets));
            at += 8;
        }
        let ch = channels as usize;
        let mut levels = Vec::with_capacity(n);
        for (span, buckets) in heads {
            let len = buckets
                .checked_mul(ch * 2)
                .filter(|l| at + l <= b.len())
                .ok_or_else(bad)?;
            let data = b[at..at + len]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| (p[0] as i8, p[1] as i8))
                .collect();
            at += len;
            levels.push(Level { span, data });
        }
        Ok(Peaks {
            channels,
            sample_rate,
            frames,
            levels,
        })
    }

    /// How long the sound is, in seconds.
    pub fn seconds(&self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.frames as f64 / self.sample_rate as f64
        }
    }

    /// `width` columns over the whole sound, channels folded into one, each
    /// (min, max) in -1..=1: from the coarsest level that still has a
    /// bucket per column. Fewer columns when the sound has fewer buckets.
    pub fn columns(&self, width: usize) -> Vec<(f32, f32)> {
        let ch = self.channels.max(1) as usize;
        let Some(level) = self
            .levels
            .iter()
            .rev()
            .find(|l| l.buckets(ch) >= width)
            .or_else(|| self.levels.first())
        else {
            return Vec::new();
        };
        let n = level.buckets(ch);
        if n == 0 || width == 0 {
            return Vec::new();
        }
        let width = width.min(n);
        let per = n as f64 / width as f64;
        (0..width)
            .map(|c| {
                let from = (c as f64 * per) as usize;
                let to = (((c + 1) as f64 * per) as usize).clamp(from + 1, n);
                let mut lo = 0i8;
                let mut hi = 0i8;
                for b in from..to {
                    for k in 0..ch {
                        let (l, h) = level.data[b * ch + k];
                        lo = lo.min(l);
                        hi = hi.max(h);
                    }
                }
                (lo as f32 / 127.0, hi as f32 / 127.0)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(frames: usize, ch: usize) -> Vec<f32> {
        (0..frames * ch)
            .map(|i| {
                let f = i / ch;
                let c = i % ch;
                let amp = if c == 0 { 0.5 } else { 0.25 };
                amp * ((f as f32) * 0.05).sin()
            })
            .collect()
    }

    #[test]
    fn builds_levels_and_round_trips() {
        let mut b = Builder::new(2, 48_000, None);
        let s = tone(300_000, 2);
        // In uneven pieces, as a decoder hands them over.
        for piece in s.chunks(2 * 1153) {
            b.push(piece, 2);
        }
        let p = b.finish();
        assert_eq!(p.frames, 300_000);
        assert_eq!(p.channels, 2);
        assert_eq!(p.levels[0].span, MIN_SPAN);
        assert_eq!(p.levels[0].buckets(2), 300_000usize.div_ceil(256));
        // Each level is STEP times coarser, down to COARSEST or fewer.
        for w in p.levels.windows(2) {
            assert_eq!(w[1].span, w[0].span * STEP);
        }
        assert!(p.levels.last().unwrap().buckets(2) <= COARSEST);
        // Left peaks near ±0.5, right near ±0.25.
        let (lo, hi) = p.levels[0].data[2];
        assert!(
            (60..=64).contains(&hi) && (-64..=-60).contains(&lo),
            "{lo} {hi}"
        );
        let (lo, hi) = p.levels[0].data[3];
        assert!((30..=32).contains(&hi) && (-32..=-30).contains(&lo));
        let back = Peaks::decode(&p.encode()).unwrap();
        assert_eq!(back, p);
        let cols = back.columns(600);
        assert_eq!(cols.len(), 600);
        assert!(cols.iter().all(|(l, h)| *h > 0.45 && *l < -0.45));
        assert!((back.seconds() - 6.25).abs() < 1e-9);
    }

    #[test]
    fn long_sounds_stay_small() {
        // Three hours at 48 kHz, length unknown up front: the finest
        // level is halved as it grows and never passes FINEST_MAX.
        let mut b = Builder::new(1, 48_000, None);
        let chunk = vec![0.1f32; 48_000];
        for _ in 0..3 * 3600 {
            b.push(&chunk, 1);
        }
        let p = b.finish();
        assert_eq!(p.frames, 3 * 3600 * 48_000);
        let finest = p.levels[0].buckets(1);
        assert!(
            finest <= FINEST_MAX + 1 && finest > FINEST_MAX / 2,
            "{finest}"
        );
        assert!(p.encode().len() < 24 << 10);
        // Known up front: the span is picked at once, same bound.
        let b = Builder::new(1, 48_000, Some(3 * 3600 * 48_000));
        assert!(518_400_000 / b.span as u64 <= FINEST_MAX as u64);
    }

    #[test]
    fn folds_many_channels_and_refuses_junk() {
        let mut b = Builder::new(6, 44_100, Some(1000));
        let s: Vec<f32> = (0..6000)
            .map(|i| if i % 6 == 5 { -1.0 } else { 0.2 })
            .collect();
        b.push(&s, 6);
        let p = b.finish();
        assert_eq!(p.channels, 2);
        assert_eq!(p.levels[0].data[1], (-127, 25));
        assert!(Peaks::decode(b"nope").is_err());
        let mut bytes = p.encode();
        bytes.truncate(bytes.len() - 1);
        assert!(Peaks::decode(&bytes).is_err());
        assert!(Peaks::decode(&Builder::new(1, 1, None).finish().encode()).is_ok());
    }
}
