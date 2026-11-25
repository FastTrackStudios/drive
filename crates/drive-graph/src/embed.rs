//! Vectors: what a text means, as numbers, so a search finds notes about
//! the same thing in other words.
//!
//! An [`Embedder`] turns texts into vectors; vectors from different models
//! never meet (each is kept under its model's [`Embedder::id`]). The one
//! every peer can run is [`StaticModel`], a Model2Vec static embedding
//! (`minishlab/potion-base-8M`: a tokenizer and one table of token
//! vectors, about 30 MB): a text's vector is the mean of its tokens' rows,
//! normalised. Pure Rust, microseconds a note, the same on a phone, in a
//! browser and on the always-on machines. Heavier models (a transformer on
//! the big machines, CLIP for images) are other `Embedder`s.
//!
//! Also here, pure: cutting a note into [`chunks`], and fusing ranked
//! lists ([`fuse`]) for hybrid search.

use std::collections::HashMap;
use std::path::Path;

/// Turns texts into vectors.
pub trait Embedder: Send + Sync {
    /// The model and its version (`potion-base-8M@1`): vectors are kept
    /// under it, and only compared with vectors of the same id.
    fn id(&self) -> &str;

    /// How long each vector is.
    fn dims(&self) -> usize;

    /// One vector per text, unit length (cosine is a dot product).
    ///
    /// # Errors
    ///
    /// The model could not read a text.
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String>;
}

/// A Model2Vec static model: a tokenizer and a token → vector table.
pub struct StaticModel {
    id: String,
    tokenizer: tokenizers::Tokenizer,
    /// `vocab × dims`, row-major.
    table: Vec<f32>,
    dims: usize,
    unk: Option<u32>,
    normalize: bool,
}

/// The files a Model2Vec model is made of.
pub const MODEL_FILES: [&str; 3] = ["config.json", "tokenizer.json", "model.safetensors"];

impl StaticModel {
    /// A model from its files' bytes; `id` names it (and its version).
    ///
    /// # Errors
    ///
    /// The tokenizer or the table could not be read.
    pub fn from_bytes(
        id: &str,
        tokenizer: &[u8],
        safetensors: &[u8],
        config: &[u8],
    ) -> Result<StaticModel, String> {
        let tokenizer = tokenizers::Tokenizer::from_bytes(tokenizer)
            .map_err(|e| format!("tokenizer.json: {e}"))?;
        let st = safetensors::SafeTensors::deserialize(safetensors)
            .map_err(|e| format!("model.safetensors: {e}"))?;
        let view = st
            .tensor("embeddings")
            .map_err(|e| format!("model.safetensors has no embeddings table: {e}"))?;
        let [rows, dims] = view.shape() else {
            return Err(format!(
                "the embeddings table is {:?}, not rows × dims",
                view.shape()
            ));
        };
        let table = floats(view.dtype(), view.data())?;
        if table.len() != rows * dims {
            return Err("the embeddings table's size does not match its shape".into());
        }
        let normalize = serde_json::from_slice::<serde_json::Value>(config)
            .ok()
            .and_then(|c| c["normalize"].as_bool())
            .unwrap_or(true);
        let unk = ["[UNK]", "<unk>"]
            .iter()
            .find_map(|t| tokenizer.token_to_id(t));
        Ok(StaticModel {
            id: id.to_string(),
            tokenizer,
            table,
            dims: *dims,
            unk,
            normalize,
        })
    }

    /// A model from a folder holding [`MODEL_FILES`].
    ///
    /// # Errors
    ///
    /// A file is missing or unreadable.
    pub fn load(id: &str, dir: &Path) -> Result<StaticModel, String> {
        let read = |f: &str| {
            std::fs::read(dir.join(f)).map_err(|e| format!("{}: {e}", dir.join(f).display()))
        };
        StaticModel::from_bytes(
            id,
            &read("tokenizer.json")?,
            &read("model.safetensors")?,
            &read("config.json")?,
        )
    }

    fn row(&self, id: u32) -> Option<&[f32]> {
        let at = usize::try_from(id).ok()? * self.dims;
        self.table.get(at..at + self.dims)
    }
}

/// A table's numbers as `f32`, from `f32` or `f16` little-endian bytes.
fn floats(dtype: safetensors::Dtype, data: &[u8]) -> Result<Vec<f32>, String> {
    match dtype {
        safetensors::Dtype::F32 => Ok(data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| f32::from_le_bytes(*b))
            .collect()),
        safetensors::Dtype::F16 => Ok(data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| half::f16::from_le_bytes(*b).to_f32())
            .collect()),
        other => Err(format!(
            "an embeddings table of {other:?} (f32 or f16 only)"
        )),
    }
}

impl Embedder for StaticModel {
    fn id(&self) -> &str {
        &self.id
    }

    fn dims(&self) -> usize {
        self.dims
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        texts
            .iter()
            .map(|t| {
                let enc = self
                    .tokenizer
                    .encode(*t, false)
                    .map_err(|e| e.to_string())?;
                let rows: Vec<&[f32]> = enc
                    .get_ids()
                    .iter()
                    .filter(|id| Some(**id) != self.unk)
                    .filter_map(|id| self.row(*id))
                    .collect();
                let v = mean(&rows, self.dims);
                Ok(if self.normalize { unit(v) } else { v })
            })
            .collect()
    }
}

fn mean(rows: &[&[f32]], dims: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dims];
    for r in rows {
        for (a, b) in v.iter_mut().zip(r.iter()) {
            *a += b;
        }
    }
    let n = rows.len().max(1) as f32;
    v.iter_mut().for_each(|a| *a /= n);
    v
}

/// `v` at unit length (a zero vector stays zero).
#[must_use]
pub fn unit(mut v: Vec<f32>) -> Vec<f32> {
    let len = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if len > 0.0 {
        v.iter_mut().for_each(|x| *x /= len);
    }
    v
}

/// Cosine similarity of two unit vectors.
#[must_use]
pub fn similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// A note cut into pieces of at most about `max` characters, at headings
/// and blank lines (a piece over `max` on its own is cut at words), so
/// each vector stands for one part of a long note.
#[must_use]
pub fn chunks(text: &str, max: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if !cur.trim().is_empty() {
            out.push(cur.trim().to_string());
        }
        cur.clear();
    };
    for para in text.split("\n\n") {
        let heading = para.trim_start().starts_with('#');
        if heading || cur.len() + para.len() > max {
            flush(&mut cur, &mut out);
        }
        if para.len() > max {
            for piece in by_words(para, max) {
                out.push(piece);
            }
            continue;
        }
        if !cur.is_empty() {
            cur.push_str("\n\n");
        }
        cur.push_str(para);
    }
    flush(&mut cur, &mut out);
    out
}

fn by_words(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for w in text.split_whitespace() {
        if !cur.is_empty() && cur.len() + 1 + w.len() > max {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(w);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The constant of reciprocal rank fusion (the usual 60).
pub const RRF_K: f64 = 60.0;

/// Ranked lists of ids fused into one (reciprocal rank fusion: each list
/// adds `1 / (RRF_K + rank)` to an id), best first; ties by id.
#[must_use]
pub fn fuse(lists: &[Vec<String>]) -> Vec<(String, f64)> {
    let mut score: HashMap<&str, f64> = HashMap::new();
    for list in lists {
        for (rank, id) in list.iter().enumerate() {
            *score.entry(id.as_str()).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
        }
    }
    let mut out: Vec<(String, f64)> = score.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A tiny model: words of a vocabulary, each a direction, with
    /// "song"/"hymn" close together.
    pub(crate) fn tiny() -> StaticModel {
        let words = [
            "[UNK]", "song", "hymn", "preset", "bass", "warm", "grace", "the",
        ];
        let vocab: serde_json::Map<String, serde_json::Value> = words
            .iter()
            .enumerate()
            .map(|(i, w)| (w.to_string(), serde_json::json!(i)))
            .collect();
        let tokenizer = serde_json::json!({
            "version": "1.0", "truncation": null, "padding": null, "added_tokens": [],
            "normalizer": { "type": "Lowercase" },
            "pre_tokenizer": { "type": "Whitespace" },
            "post_processor": null, "decoder": null,
            "model": { "type": "WordLevel", "vocab": vocab, "unk_token": "[UNK]" }
        });
        let dims = 4;
        let rows: [[f32; 4]; 8] = [
            [9.0, 9.0, 9.0, 9.0], // [UNK]: never counted
            [1.0, 0.1, 0.0, 0.0], // song
            [0.9, 0.2, 0.0, 0.0], // hymn: near song
            [0.0, 0.0, 1.0, 0.0], // preset
            [0.0, 0.0, 0.8, 0.3], // bass
            [0.0, 0.0, 0.2, 1.0], // warm
            [0.6, 0.6, 0.0, 0.0], // grace
            [0.0, 0.0, 0.0, 0.0], // the
        ];
        let bytes: Vec<u8> = rows
            .iter()
            .flatten()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        let view =
            safetensors::tensor::TensorView::new(safetensors::Dtype::F32, vec![8, dims], &bytes)
                .unwrap();
        let st = safetensors::serialize([("embeddings", view)], None).unwrap();
        StaticModel::from_bytes(
            "tiny@1",
            tokenizer.to_string().as_bytes(),
            &st,
            br#"{"normalize": true}"#,
        )
        .unwrap()
    }

    #[test]
    fn a_static_model_embeds_by_its_tokens() {
        let m = tiny();
        assert_eq!((m.id(), m.dims()), ("tiny@1", 4));
        let v = m
            .embed(&["The song", "a HYMN", "warm bass preset", "zzz"])
            .unwrap();
        let len = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((len(&v[0]) - 1.0).abs() < 1e-5, "unit length");
        assert!(similarity(&v[0], &v[1]) > 0.95, "song and hymn are near");
        assert!(similarity(&v[0], &v[2]) < 0.1, "a song is not a preset");
        assert_eq!(v[3], vec![0.0; 4], "unknown words count for nothing");
    }

    /// The real model, when `DRIVE_EMBED_MODEL` names a folder holding it
    /// (`drive graph model fetch`); skipped otherwise.
    #[test]
    fn the_real_model_finds_meaning() {
        let Some(dir) = std::env::var_os("DRIVE_EMBED_MODEL") else {
            return;
        };
        let m = StaticModel::load("potion-base-8M@1", Path::new(&dir)).unwrap();
        assert_eq!(m.dims(), 256);
        let v = m
            .embed(&[
                "a hymn for the Sunday service",
                "worship song for church",
                "warm analog bass synth preset",
            ])
            .unwrap();
        assert!(
            similarity(&v[0], &v[1]) > similarity(&v[0], &v[2]) + 0.1,
            "hymn~song {} vs hymn~bass {}",
            similarity(&v[0], &v[1]),
            similarity(&v[0], &v[2])
        );
    }

    #[test]
    fn notes_are_cut_at_headings_and_paragraphs() {
        let text =
            "intro\n\n# One\n\npara a\n\npara b\n\n# Two\n\n".to_string() + &"word ".repeat(50);
        let c = chunks(&text, 60);
        assert_eq!(c[0], "intro");
        assert_eq!(c[1], "# One\n\npara a\n\npara b");
        assert_eq!(c[2], "# Two");
        assert!(c[3..].iter().all(|p| p.len() <= 60));
        assert_eq!(c[3..].join(" ").split_whitespace().count(), 50);
        assert!(chunks("", 10).is_empty());
    }

    #[test]
    fn ranked_lists_fuse() {
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let fused = fuse(&[l(&["a", "b", "c"]), l(&["b", "a", "d"])]);
        let ids: Vec<&str> = fused.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c", "d"]);
        assert!(fused[0].1 > fused[2].1);
    }
}
