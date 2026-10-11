//! nokhwa backend (AVFoundation / Media Foundation / V4L2).

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, anyhow};
use nokhwa::Camera;
use nokhwa::pixel_format::RgbAFormat;
use nokhwa::utils::{ApiBackend, CameraFormat, FrameFormat, RequestedFormat, RequestedFormatType};

use super::{CameraInfo, Frame};

pub fn request_permission() {
  nokhwa::nokhwa_initialize(|granted| tracing::info!("camera permission granted: {granted}"));
}

pub fn list() -> Result<Vec<CameraInfo>> {
  let cameras = nokhwa::query(ApiBackend::Auto).context("querying cameras")?;
  Ok(
    cameras
      .iter()
      .map(|c| CameraInfo {
        id: c.human_name(),
        name: c.human_name(),
      })
      .collect(),
  )
}

fn open(id: Option<&str>, note: &mut dyn FnMut(String)) -> Result<Camera> {
  let cameras = nokhwa::query(ApiBackend::Auto).context("querying cameras")?;
  let info = match id {
    Some(id) => cameras.iter().find(|c| c.human_name() == id),
    None => None,
  }
  .or(cameras.first())
  .ok_or_else(|| anyhow!("no camera found"))?;

  // The backend's own default (640x480@15 YUYV) does not exist on every camera, so try several
  // initial requests. A request only has to open the device: the real mode is chosen below.
  let mut camera = None;
  let mut last_error = String::new();
  for (label, kind) in [
    ("highest resolution", RequestedFormatType::AbsoluteHighestResolution),
    ("highest frame rate", RequestedFormatType::AbsoluteHighestFrameRate),
    ("default", RequestedFormatType::None),
  ] {
    let index = info.index().clone();
    let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Camera::new(index, RequestedFormat::new::<RgbAFormat>(kind))));
    match opened {
      Ok(Ok(c)) => {
        note(format!("opened with the {label} request"));
        camera = Some(c);
        break;
      }
      Ok(Err(e)) => last_error = format!("{label}: {e}"),
      Err(_) => last_error = format!("{label}: backend crashed"),
    }
    note(format!("open ({label}) failed: {last_error}"));
  }
  let mut camera = camera.ok_or_else(|| anyhow!("opening camera: {last_error}"))?;

  // Rank the modes the device really offers. An exact-match request often fails (the default
  // is a tiny 160x120 mode), and a mode can be "selected" yet deliver another size, so each
  // candidate is verified against the size of a real frame.
  let formats = camera.compatible_camera_formats().unwrap_or_else(|e| {
    tracing::warn!("could not list camera modes: {e}");
    Vec::new()
  });
  let modes: Vec<String> = formats.iter().map(|f| f.to_string()).collect();
  note(format!("{} modes offered: {}", modes.len(), modes.join(", ")));

  for mode in rank_formats(&formats).into_iter().take(MAX_ATTEMPTS) {
    let request = RequestedFormat::new::<RgbAFormat>(RequestedFormatType::Exact(mode));
    if let Err(e) = camera.set_camera_requset(request) {
      note(format!("select {mode} failed: {e}"));
      continue;
    }
    if let Err(e) = camera.open_stream() {
      note(format!("stream {mode} failed: {e}"));
      continue;
    }
    match camera.frame() {
      Ok(buffer) => {
        let res = buffer.resolution();
        if res.width() >= mode.width() && res.height() >= mode.height() {
          note(format!("using {mode} (frames {res})"));
          return Ok(camera);
        }
        note(format!("asked {mode} but frames are {res}"));
      }
      Err(e) => note(format!("no frame at {mode}: {e}")),
    }
    let _ = camera.stop_stream();
  }

  // Last resort: whatever the backend defaults to.
  note("no mode verified; using the backend default".to_owned());
  camera.open_stream().context("starting stream")?;
  Ok(camera)
}

const MAX_ATTEMPTS: usize = 6;

/// Order modes best-first: the smallest one that is at least 720p, else the largest available;
/// usable frame rates (>= 15 fps) first, favouring ~30 fps, with MJPEG winning ties.
fn rank_formats(formats: &[CameraFormat]) -> Vec<CameraFormat> {
  const TARGET: u32 = 1280 * 720;
  let mut list = formats.to_vec();
  list.sort_by_key(|f| {
    let a = f.width() * f.height();
    let size_rank = if a >= TARGET { a - TARGET } else { (TARGET - a) + u32::MAX / 2 };
    let slow = u32::from(f.frame_rate() < 15);
    let fps_rank = f.frame_rate().abs_diff(30);
    let fmt_rank = u32::from(f.format() != FrameFormat::MJPEG);
    (slow, size_rank, fps_rank, fmt_rank)
  });
  list
}

/// Capture until `stop` is set, passing each frame to `sink`.
pub fn capture(id: Option<&str>, stop: &AtomicBool, note: &mut dyn FnMut(String), mut sink: impl FnMut(Frame)) -> Result<()> {
  let mut camera = open(id, note)?;
  while !stop.load(Ordering::Relaxed) {
    let buffer = camera.frame().context("reading frame")?;
    let image = buffer.decode_image::<RgbAFormat>().context("decoding frame")?;
    let (width, height) = (image.width(), image.height());
    let mut rgba = image.into_raw();
    // nokhwa accepts an oversized buffer (seen when switching cameras): trim it, and drop
    // frames that are too short instead of passing a size mismatch on to the UI.
    let expected = width as usize * height as usize * 4;
    if rgba.len() < expected {
      continue;
    }
    rgba.truncate(expected);
    sink(Frame { width, height, rgba });
  }
  let _ = camera.stop_stream();
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn f(w: u32, h: u32, fps: u32, fmt: FrameFormat) -> CameraFormat {
    CameraFormat::new_from(w, h, fmt, fps)
  }

  #[test]
  fn prefers_smallest_720p_or_better_at_30fps() {
    let list = [
      f(160, 120, 30, FrameFormat::NV12),
      f(1920, 1080, 30, FrameFormat::MJPEG),
      f(1280, 720, 30, FrameFormat::NV12),
      f(1280, 720, 5, FrameFormat::MJPEG),
    ];
    let best = rank_formats(&list)[0];
    assert_eq!((best.width(), best.height(), best.frame_rate()), (1280, 720, 30));
  }

  #[test]
  fn falls_back_to_largest_when_below_720p() {
    let list = [f(160, 120, 30, FrameFormat::NV12), f(640, 480, 30, FrameFormat::NV12)];
    assert_eq!(rank_formats(&list)[0].width(), 640);
  }
}
