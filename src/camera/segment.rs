//! Person segmentation with MediaPipe selfie segmentation (landscape, 256x144) via ONNX Runtime.
//! The model (462 KB, Apache-2.0) is embedded in the binary. Only this file names `ort`.

use anyhow::{Result, anyhow};
use ort::session::Session;
use ort::value::Tensor;

use super::Frame;
use super::processing::{FrameProcessor, apply_mask};

const MODEL: &[u8] = include_bytes!("../../assets/selfie_segmentation_landscape.onnx");
const W: usize = 256;
const H: usize = 144;
/// Weight of the newest mask in the temporal blend (1.0 = no smoothing).
const NEW_WEIGHT: f32 = 0.6;

pub struct Segmenter {
  session: Session,
  /// Smoothed mask from the previous frame.
  prev: Vec<f32>,
  input: Vec<f32>,
  warned: bool,
}

impl Segmenter {
  pub fn new() -> Result<Self> {
    let session = Session::builder()
      .map_err(|e| anyhow!("creating ONNX session: {e}"))?
      .with_intra_threads(2)
      .map_err(|e| anyhow!("configuring ONNX session: {e}"))?
      .commit_from_memory(MODEL)
      .map_err(|e| anyhow!("loading segmentation model: {e}"))?;
    Ok(Self {
      session,
      prev: Vec::new(),
      input: vec![0.0; 3 * W * H],
      warned: false,
    })
  }

  /// Fill `self.input` (CHW, 0..1) from the frame, bilinearly downscaled.
  fn prepare(&mut self, frame: &Frame) {
    let (fw, fh) = (frame.width as usize, frame.height as usize);
    let plane = W * H;
    for y in 0..H {
      let sy = ((y as f32 + 0.5) * fh as f32 / H as f32 - 0.5).clamp(0.0, (fh - 1) as f32);
      let (y0, wy) = (sy.floor() as usize, sy.fract());
      let y1 = (y0 + 1).min(fh - 1);
      for x in 0..W {
        let sx = ((x as f32 + 0.5) * fw as f32 / W as f32 - 0.5).clamp(0.0, (fw - 1) as f32);
        let (x0, wx) = (sx.floor() as usize, sx.fract());
        let x1 = (x0 + 1).min(fw - 1);
        for c in 0..3 {
          let p = |xx: usize, yy: usize| frame.rgba[(yy * fw + xx) * 4 + c] as f32;
          let top = p(x0, y0) + (p(x1, y0) - p(x0, y0)) * wx;
          let bottom = p(x0, y1) + (p(x1, y1) - p(x0, y1)) * wx;
          self.input[c * plane + y * W + x] = (top + (bottom - top) * wy) / 255.0;
        }
      }
    }
  }

  fn infer(&mut self, frame: &Frame) -> Result<()> {
    self.prepare(frame);
    let tensor = Tensor::from_array(([1usize, 3, H, W], self.input.clone())).map_err(|e| anyhow!("building input tensor: {e}"))?;
    let outputs = self
      .session
      .run(ort::inputs!["pixel_values" => tensor])
      .map_err(|e| anyhow!("running segmentation: {e}"))?;
    let (_, alphas) = outputs["alphas"].try_extract_tensor::<f32>().map_err(|e| anyhow!("reading mask: {e}"))?;
    if alphas.len() != W * H {
      anyhow::bail!("unexpected mask size {}", alphas.len());
    }
    if self.prev.len() != W * H {
      self.prev = alphas.to_vec();
    } else {
      for (p, &n) in self.prev.iter_mut().zip(alphas) {
        *p += (n - *p) * NEW_WEIGHT;
      }
    }
    Ok(())
  }
}

impl FrameProcessor for Segmenter {
  fn process(&mut self, mut frame: Frame) -> Frame {
    if frame.width == 0 || frame.height == 0 {
      return frame;
    }
    match self.infer(&frame) {
      Ok(()) => apply_mask(&mut frame, &self.prev, W, H),
      Err(e) => {
        if !self.warned {
          tracing::warn!("background removal failed: {e:#}");
          self.warned = true;
        }
      }
    }
    frame
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn model_loads_and_runs_on_a_synthetic_frame() {
    let mut seg = Segmenter::new().expect("model loads");
    let frame = Frame {
      width: 640,
      height: 360,
      rgba: vec![128; 640 * 360 * 4],
    };
    let out = seg.process(frame);
    assert_eq!((out.width, out.height), (640, 360));
    assert_eq!(out.rgba.len(), 640 * 360 * 4);
    assert!(!seg.warned, "inference should not fail");
    assert_eq!(seg.prev.len(), 256 * 144);
    assert!(seg.prev.iter().all(|v| (-0.01..=1.01).contains(v)), "mask should be a probability");
  }
}
