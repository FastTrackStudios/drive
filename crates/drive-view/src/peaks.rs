//! A waveform's outline: the lowest and highest sample of each slice of a
//! sound, for drawing it small.

/// `buckets` (min, max) pairs over `samples`, each in -1..=1. Fewer when
/// there are fewer samples than buckets.
pub fn peaks(samples: &[f32], buckets: usize) -> Vec<(f32, f32)> {
    if samples.is_empty() || buckets == 0 {
        return Vec::new();
    }
    let buckets = buckets.min(samples.len());
    let per = samples.len() as f64 / buckets as f64;
    (0..buckets)
        .map(|b| {
            let from = (b as f64 * per) as usize;
            let to = (((b + 1) as f64 * per) as usize).clamp(from + 1, samples.len());
            samples[from..to].iter().fold((0.0f32, 0.0f32), |(lo, hi), &s| {
                let s = if s.is_finite() { s.clamp(-1.0, 1.0) } else { 0.0 };
                (lo.min(s), hi.max(s))
            })
        })
        .collect()
}

/// Two channels' peaks as one: the widest of each pair.
pub fn merge(a: &[(f32, f32)], b: &[(f32, f32)]) -> Vec<(f32, f32)> {
    a.iter()
        .zip(b)
        .map(|(x, y)| (x.0.min(y.0), x.1.max(y.1)))
        .collect()
}

/// An SVG path drawing `peaks` in a `width` × `height` box: one vertical
/// stroke per bucket around the middle.
pub fn svg_path(peaks: &[(f32, f32)], width: f32, height: f32) -> String {
    use std::fmt::Write;
    let mid = height / 2.0;
    let step = width / peaks.len().max(1) as f32;
    let mut d = String::with_capacity(peaks.len() * 24);
    for (i, (lo, hi)) in peaks.iter().enumerate() {
        let x = (i as f32 + 0.5) * step;
        // At least a hairline, so silence still shows where the sound is.
        let (y0, y1) = (mid - hi * mid, (mid - lo * mid).max(mid - hi * mid + 0.5));
        let _ = write!(d, "M{x:.1} {y0:.1}V{y1:.1}");
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_a_sound() {
        let s: Vec<f32> = (0..1000).map(|i| if i < 500 { 0.5 } else { -0.25 }).collect();
        let p = peaks(&s, 4);
        assert_eq!(p, vec![(0.0, 0.5), (0.0, 0.5), (-0.25, 0.0), (-0.25, 0.0)]);
        assert_eq!(peaks(&[0.1, f32::NAN, 2.0], 10), vec![(0.0, 0.1), (0.0, 0.0), (0.0, 1.0)]);
        assert!(peaks(&[], 10).is_empty());
        assert_eq!(merge(&[(-0.1, 0.2)], &[(-0.3, 0.1)]), vec![(-0.3, 0.2)]);
        assert_eq!(svg_path(&[(-1.0, 1.0)], 10.0, 20.0), "M5.0 0.0V20.0");
    }
}
