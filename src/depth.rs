//! Works out how near each part of an album cover is, with a small neural
//! network (Depth Anything V2, the small one), so the depth cover style can
//! move the foreground and the background apart.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, anyhow};
use image::{GrayImage, RgbImage, imageops::FilterType};
use ort::session::Session;
use ort::value::Tensor;

use crate::art::DepthMap;
use crate::config;

/// The model: 27 MB, fetched once, the first time the style is used.
pub const MODEL_URL: &str =
    "https://huggingface.co/onnx-community/depth-anything-v2-small/resolve/main/onnx/model_quantized.onnx";

pub fn model_path() -> PathBuf {
    config::cache_dir().join("models").join("depth-anything-v2-small.onnx")
}

/// Download the model. It is written aside and moved into place, so a
/// download cut short is never mistaken for the model.
pub async fn fetch(http: &reqwest::Client) -> Result<()> {
    let path = model_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let bytes = http
        .get(MODEL_URL)
        .timeout(Duration::from_secs(600))
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    if bytes.len() < 1_000_000 {
        return Err(anyhow!("the depth model download came back {} bytes long", bytes.len()));
    }
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// Side of the square the network looks at; it works in patches of 14.
const LOOK: u32 = 252;
/// The colour statistics the network was trained with.
const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const SPREAD: [f32; 3] = [0.229, 0.224, 0.225];

/// Estimate the depth of `cover`. Takes about a tenth of a second and around
/// 150 MB while it runs; the network is loaded for the job and let go after,
/// since a cover only needs doing once.
pub fn estimate(cover: &RgbImage) -> Result<DepthMap> {
    let ort = |e: ort::Error| anyhow!("depth model: {e}");
    let small = image::imageops::resize(cover, LOOK, LOOK, FilterType::CatmullRom);
    let plane = (LOOK * LOOK) as usize;
    let mut input = vec![0f32; 3 * plane];
    for (i, pixel) in small.pixels().enumerate() {
        for c in 0..3 {
            input[c * plane + i] = (pixel.0[c] as f32 / 255.0 - MEAN[c]) / SPREAD[c];
        }
    }

    let mut session = Session::builder()
        .map_err(ort)?
        .with_intra_threads(2)
        .map_err(|e| anyhow!("depth model: {e}"))?
        .commit_from_file(model_path())
        .map_err(ort)?;
    let side = LOOK as usize;
    let tensor = Tensor::from_array(([1usize, 3, side, side], input)).map_err(ort)?;
    let outputs = session.run(ort::inputs!["pixel_values" => tensor]).map_err(ort)?;
    let (_, depth) = outputs["predicted_depth"].try_extract_tensor::<f32>().map_err(ort)?;
    if depth.len() != plane {
        return Err(anyhow!("depth model: answered with {} values for {plane} pixels", depth.len()));
    }

    // The network's scale is its own; all that matters here is the order.
    let (far, near) = depth.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    let span = (near - far).max(1e-6);
    let bytes: Vec<u8> = depth.iter().map(|v| ((v - far) / span * 255.0) as u8).collect();
    let full = GrayImage::from_raw(LOOK, LOOK, bytes).ok_or_else(|| anyhow!("depth model: bad map"))?;
    // Halved: plenty for a cover drawn in character cells, and the averaging
    // softens the edges so things slide past each other without tearing.
    let kept = image::imageops::resize(&full, DepthMap::SIDE, DepthMap::SIDE, FilterType::Triangle);
    Ok(DepthMap { near: kept.into_raw() })
}
