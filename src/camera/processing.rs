//! Frame processing between capture and display. Backend-free: the ONNX model lives in
//! `segment.rs`, this module only holds the trait and the pure pixel maths.

use super::Frame;

/// A stage applied to every captured frame on the capture thread.
pub trait FrameProcessor: Send {
  fn process(&mut self, frame: Frame) -> Frame;
}

/// Soft threshold: 0 below `lo`, 1 above `hi`, smooth in between (cleans up fuzzy edges).
pub fn smoothstep(lo: f32, hi: f32, x: f32) -> f32 {
  let t = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
  t * t * (3.0 - 2.0 * t)
}

/// Write `mask` (a `mw` x `mh` map of person probability, 0..1) into the frame's alpha channel,
/// bilinearly upsampled to the frame size.
pub fn apply_mask(frame: &mut Frame, mask: &[f32], mw: usize, mh: usize) {
  let (fw, fh) = (frame.width as usize, frame.height as usize);
  if fw == 0 || fh == 0 || mask.len() != mw * mh {
    return;
  }
  // Per-column source index and weight, computed once.
  let cols: Vec<(usize, usize, f32)> = (0..fw)
    .map(|x| {
      let s = ((x as f32 + 0.5) * mw as f32 / fw as f32 - 0.5).clamp(0.0, (mw - 1) as f32);
      let i0 = s.floor() as usize;
      (i0, (i0 + 1).min(mw - 1), s - i0 as f32)
    })
    .collect();
  for y in 0..fh {
    let s = ((y as f32 + 0.5) * mh as f32 / fh as f32 - 0.5).clamp(0.0, (mh - 1) as f32);
    let j0 = s.floor() as usize;
    let j1 = (j0 + 1).min(mh - 1);
    let wy = s - j0 as f32;
    let (row0, row1) = (&mask[j0 * mw..(j0 + 1) * mw], &mask[j1 * mw..(j1 + 1) * mw]);
    for (x, &(i0, i1, wx)) in cols.iter().enumerate() {
      let top = row0[i0] + (row0[i1] - row0[i0]) * wx;
      let bottom = row1[i0] + (row1[i1] - row1[i0]) * wx;
      let m = top + (bottom - top) * wy;
      frame.rgba[(y * fw + x) * 4 + 3] = (smoothstep(0.3, 0.7, m) * 255.0).round() as u8;
    }
  }
}

/// Replace the background of a frame whose alpha channel holds the person mask with a blurred
/// copy of itself, then make the frame opaque. The blur is normalised by background weight, so
/// the person's colours do not bleed into it.
pub fn composite_blur(frame: &mut Frame) {
  let (fw, fh) = (frame.width as usize, frame.height as usize);
  if fw == 0 || fh == 0 || frame.rgba.len() != fw * fh * 4 {
    return;
  }
  // Work on a small copy (~160 px wide): blurring there is cheap and looks stronger.
  let f = (fw / 160).max(1);
  let (sw, sh) = ((fw / f).max(1), (fh / f).max(1));
  // 4 channels per cell: r*w, g*w, b*w, w, where w is the "is background" weight.
  let mut small = vec![0.0_f32; sw * sh * 4];
  let mut count = vec![0.0_f32; sw * sh];
  for y in 0..fh {
    let sy = (y / f).min(sh - 1);
    for x in 0..fw {
      let sx = (x / f).min(sw - 1);
      let p = &frame.rgba[(y * fw + x) * 4..(y * fw + x) * 4 + 4];
      let w = 1.0 - p[3] as f32 / 255.0;
      let cell = (sy * sw + sx) * 4;
      small[cell] += p[0] as f32 * w;
      small[cell + 1] += p[1] as f32 * w;
      small[cell + 2] += p[2] as f32 * w;
      small[cell + 3] += w;
      count[sy * sw + sx] += 1.0;
    }
  }
  for (cell, n) in count.iter().enumerate() {
    for c in 0..4 {
      small[cell * 4 + c] /= n.max(1.0);
    }
  }
  box_blur(&mut small, sw, sh, 3);
  box_blur(&mut small, sw, sh, 3);

  for y in 0..fh {
    let s = ((y as f32 + 0.5) / f as f32 - 0.5).clamp(0.0, (sh - 1) as f32);
    let (y0, wy) = (s.floor() as usize, s.fract());
    let y1 = (y0 + 1).min(sh - 1);
    for x in 0..fw {
      let s = ((x as f32 + 0.5) / f as f32 - 0.5).clamp(0.0, (sw - 1) as f32);
      let (x0, wx) = (s.floor() as usize, s.fract());
      let x1 = (x0 + 1).min(sw - 1);
      let at = |xx: usize, yy: usize, c: usize| small[(yy * sw + xx) * 4 + c];
      let mix = |c: usize| {
        let top = at(x0, y0, c) + (at(x1, y0, c) - at(x0, y0, c)) * wx;
        let bottom = at(x0, y1, c) + (at(x1, y1, c) - at(x0, y1, c)) * wx;
        top + (bottom - top) * wy
      };
      let weight = mix(3);
      let i = (y * fw + x) * 4;
      let a = frame.rgba[i + 3] as f32 / 255.0;
      for c in 0..3 {
        let blurred = if weight > 1e-3 { mix(c) / weight } else { frame.rgba[i + c] as f32 };
        frame.rgba[i + c] = (frame.rgba[i + c] as f32 * a + blurred * (1.0 - a)).round().clamp(0.0, 255.0) as u8;
      }
      frame.rgba[i + 3] = 255;
    }
  }
}

/// In-place separable box blur of a 4-channel f32 image with edge clamping.
fn box_blur(buf: &mut [f32], w: usize, h: usize, radius: usize) {
  let n = (2 * radius + 1) as f32;
  let mut line = vec![0.0_f32; w.max(h) * 4];
  // Horizontal pass.
  for y in 0..h {
    for x in 0..w {
      for c in 0..4 {
        let mut sum = 0.0;
        for k in 0..=2 * radius {
          let xx = (x + k).saturating_sub(radius).min(w - 1);
          sum += buf[(y * w + xx) * 4 + c];
        }
        line[x * 4 + c] = sum / n;
      }
    }
    buf[y * w * 4..(y + 1) * w * 4].copy_from_slice(&line[..w * 4]);
  }
  // Vertical pass.
  for x in 0..w {
    for y in 0..h {
      for c in 0..4 {
        let mut sum = 0.0;
        for k in 0..=2 * radius {
          let yy = (y + k).saturating_sub(radius).min(h - 1);
          sum += buf[(yy * w + x) * 4 + c];
        }
        line[y * 4 + c] = sum / n;
      }
    }
    for y in 0..h {
      for c in 0..4 {
        buf[(y * w + x) * 4 + c] = line[y * 4 + c];
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn frame(w: u32, h: u32) -> Frame {
    Frame {
      width: w,
      height: h,
      rgba: vec![255; (w * h * 4) as usize],
    }
  }

  #[test]
  fn smoothstep_clamps_and_is_monotonic() {
    assert_eq!(smoothstep(0.3, 0.7, 0.0), 0.0);
    assert_eq!(smoothstep(0.3, 0.7, 1.0), 1.0);
    assert!((smoothstep(0.3, 0.7, 0.5) - 0.5).abs() < 1e-6);
  }

  #[test]
  fn mask_sets_alpha_left_person_right_background() {
    let mut f = frame(8, 4);
    // 2x1 mask: left = person, right = background.
    apply_mask(&mut f, &[1.0, 0.0], 2, 1);
    assert_eq!(f.rgba[3], 255);
    assert_eq!(f.rgba[(7) * 4 + 3], 0);
  }

  #[test]
  fn blur_keeps_person_and_makes_frame_opaque() {
    // Left half person (alpha 255, red), right half background (alpha 0, blue).
    let mut f = frame(40, 10);
    for x in 0..40 {
      for y in 0..10 {
        let i = (y * 40 + x) * 4;
        let person = x < 20;
        f.rgba[i..i + 4].copy_from_slice(&if person { [255, 0, 0, 255] } else { [0, 0, 255, 0] });
      }
    }
    composite_blur(&mut f);
    assert!(f.rgba.chunks(4).all(|p| p[3] == 255));
    assert_eq!(&f.rgba[0..3], &[255, 0, 0]);
    let bg = &f.rgba[(39) * 4..(39) * 4 + 3];
    assert!(bg[2] > 200 && bg[0] < 50, "background stays blue, no red bleed: {bg:?}");
  }

  #[test]
  fn wrong_mask_size_is_ignored() {
    let mut f = frame(4, 4);
    apply_mask(&mut f, &[0.0; 3], 2, 2);
    assert!(f.rgba.chunks(4).all(|p| p[3] == 255));
  }
}
