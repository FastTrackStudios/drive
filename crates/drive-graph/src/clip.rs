//! CLIP (OpenAI's ViT-B/32, through candle): images and the words that
//! describe them in one vector space, so "a drum kit on a stage" finds the
//! photo. About 600 MB and a fraction of a second an image on a CPU: the
//! machines with room for it (an always-on box, the big workstations)
//! embed every image they can read, keyed by content, and search with it.

use std::path::Path;

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::clip::{ClipConfig, ClipModel, div_l2_norm};

use crate::embed::{ImageEmbedder, unit};

/// The files the model is made of.
pub const CLIP_FILES: [&str; 2] = ["model.safetensors", "tokenizer.json"];

/// CLIP's text side reads at most this many tokens.
const MAX_TOKENS: usize = 77;

/// A CLIP model ready to embed.
pub struct Clip {
    id: String,
    model: ClipModel,
    tokenizer: tokenizers::Tokenizer,
    pad: u32,
    size: usize,
    device: Device,
}

impl Clip {
    /// The model in `dir` ([`CLIP_FILES`]); `id` names it (`image:…`).
    ///
    /// # Errors
    ///
    /// A file is missing, or not a ViT-B/32 CLIP.
    pub fn load(id: &str, dir: &Path) -> Result<Clip, String> {
        let weights = std::fs::read(dir.join("model.safetensors"))
            .map_err(|e| format!("{}: {e}", dir.join("model.safetensors").display()))?;
        let tokenizer = tokenizers::Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| format!("tokenizer.json: {e}"))?;
        let pad = tokenizer
            .get_vocab(true)
            .get("<|endoftext|>")
            .copied()
            .ok_or("the tokenizer has no <|endoftext|>")?;
        let device = Device::Cpu;
        let config = ClipConfig::vit_base_patch32();
        let vb = VarBuilder::from_buffered_safetensors(weights, DType::F32, &device)
            .map_err(|e| e.to_string())?;
        let model = ClipModel::new(vb, &config).map_err(|e| e.to_string())?;
        Ok(Clip {
            id: id.to_string(),
            model,
            tokenizer,
            pad,
            size: config.image_size,
            device,
        })
    }

    /// An image as CLIP reads it: filled to its square, RGB, in [-1, 1].
    fn pixels(&self, bytes: &[u8]) -> Result<Tensor, String> {
        let img = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
        let side = u32::try_from(self.size).map_err(|e| e.to_string())?;
        let img = img
            .resize_to_fill(side, side, image::imageops::FilterType::Triangle)
            .to_rgb8()
            .into_raw();
        Tensor::from_vec(img, (self.size, self.size, 3), &self.device)
            .and_then(|t| t.permute((2, 0, 1)))
            .and_then(|t| t.to_dtype(DType::F32))
            .and_then(|t| t.affine(2.0 / 255.0, -1.0))
            .map_err(|e| e.to_string())
    }
}

fn rows(t: &Tensor) -> Result<Vec<Vec<f32>>, String> {
    let t = div_l2_norm(t).map_err(|e| e.to_string())?;
    t.to_vec2::<f32>().map_err(|e| e.to_string())
}

impl ImageEmbedder for Clip {
    fn id(&self) -> &str {
        &self.id
    }

    fn embed_images(&self, images: &[Vec<u8>]) -> Result<Vec<Vec<f32>>, String> {
        if images.is_empty() {
            return Ok(Vec::new());
        }
        let pixels: Vec<Tensor> = images
            .iter()
            .map(|b| self.pixels(b))
            .collect::<Result<_, _>>()?;
        let batch = Tensor::stack(&pixels, 0).map_err(|e| e.to_string())?;
        let features = self
            .model
            .get_image_features(&batch)
            .map_err(|e| e.to_string())?;
        rows(&features)
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>, String> {
        let enc = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| e.to_string())?;
        let mut ids: Vec<u32> = enc.get_ids().to_vec();
        ids.truncate(MAX_TOKENS);
        if ids.is_empty() {
            ids.push(self.pad);
        }
        let input = Tensor::new(vec![ids], &self.device).map_err(|e| e.to_string())?;
        let features = self
            .model
            .get_text_features(&input)
            .map_err(|e| e.to_string())?;
        Ok(unit(rows(&features)?.pop().unwrap_or_default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::similarity;

    /// The real model, when `DRIVE_CLIP_MODEL` names a folder holding it;
    /// skipped otherwise.
    #[test]
    fn clip_finds_a_picture_by_its_description() {
        let Some(dir) = std::env::var_os("DRIVE_CLIP_MODEL") else {
            return;
        };
        let clip = Clip::load("image:clip-vit-b32@1", Path::new(&dir)).unwrap();
        let png = |rgb: [u8; 3]| {
            let img = image::RgbImage::from_pixel(64, 64, image::Rgb(rgb));
            let mut out = std::io::Cursor::new(Vec::new());
            img.write_to(&mut out, image::ImageFormat::Png).unwrap();
            out.into_inner()
        };
        let v = clip
            .embed_images(&[png([220, 20, 20]), png([20, 40, 220])])
            .unwrap();
        let red = clip.embed_query("a plain red square").unwrap();
        assert!(
            similarity(&v[0], &red) > similarity(&v[1], &red),
            "red {} vs blue {}",
            similarity(&v[0], &red),
            similarity(&v[1], &red)
        );
    }
}
