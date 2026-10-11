//! nokhwa backend (AVFoundation / Media Foundation / V4L2).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use nokhwa::Camera;
use nokhwa::pixel_format::RgbAFormat;
use nokhwa::utils::{ApiBackend, CameraFormat, FrameFormat, RequestedFormat, RequestedFormatType};

use super::{CameraInfo, Frame};

pub fn request_permission() {
  nokhwa::nokhwa_initialize(|granted| tracing::info!("camera permission granted: {granted}"));
}

/// Filter stale or unavailable devices by initializing each device with a supported format.
/// This excludes built-in cameras that are unavailable while a laptop is closed, as well as
/// disconnected or inaccessible cameras.
pub fn list_available() -> Result<Vec<CameraInfo>> {
  let cameras = nokhwa::query(ApiBackend::Auto).context("querying cameras")?;
  let devices = cameras.into_iter().map(camera_info).collect::<Vec<_>>();
  Ok(filter_available(devices))
}

fn camera_info(camera: nokhwa::utils::CameraInfo) -> CameraInfo {
  let name = camera.human_name();
  let id = camera.misc();
  CameraInfo {
    id: if id.is_empty() { name.clone() } else { id },
    name,
  }
}

fn find_device<'a>(cameras: &'a [nokhwa::utils::CameraInfo], id: Option<&str>) -> Option<&'a nokhwa::utils::CameraInfo> {
  match id {
    Some(id) => cameras.iter().find(|camera| {
      let misc = camera.misc();
      (!misc.is_empty() && misc == id) || (misc.is_empty() && camera.human_name() == id)
    }),
    None => cameras.first(),
  }
}

fn filter_available(cameras: impl IntoIterator<Item = CameraInfo>) -> Vec<CameraInfo> {
  filter_available_with(cameras, |camera| {
    let available_now = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| probe(camera)));
    match available_now {
      Ok(Ok(())) => {
        tracing::info!("camera discovery: {} is available", camera.name);
        true
      }
      Ok(Err(error)) => {
        tracing::info!("camera discovery: {} unavailable: {error:#}", camera.name);
        false
      }
      Err(_) => {
        tracing::warn!("camera discovery: {} backend panicked", camera.name);
        false
      }
    }
  })
}

/// Verify the camera actually delivers a frame. The probe runs on a disposable worker with a
/// timeout, so a closed-lid camera cannot block the UI or be offered as available.
fn probe(camera: &CameraInfo) -> Result<()> {
  const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
  let id = camera.id.clone();
  let (tx, rx) = mpsc::sync_channel(1);
  std::thread::Builder::new()
    .name("camera-probe".into())
    .spawn(move || {
      let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut note = |_| {};
        let mut camera = open(Some(&id), &mut note, false)?;
        camera.frame().context("waiting for probe frame")?;
        camera.stop_stream().map_err(|error| anyhow!("stopping availability probe: {error}"))?;
        Ok(())
      }))
      .unwrap_or_else(|_| Err(anyhow!("camera probe backend panicked")));
      let _ = tx.send(result);
    })
    .context("starting camera probe thread")?;
  match rx.recv_timeout(PROBE_TIMEOUT) {
    Ok(result) => result,
    Err(mpsc::RecvTimeoutError::Timeout) => Err(anyhow!("timed out waiting for a camera frame")),
    Err(mpsc::RecvTimeoutError::Disconnected) => Err(anyhow!("camera probe stopped unexpectedly")),
  }
}

fn filter_available_with(cameras: impl IntoIterator<Item = CameraInfo>, mut is_available: impl FnMut(&CameraInfo) -> bool) -> Vec<CameraInfo> {
  cameras.into_iter().filter(|camera| is_available(camera)).collect()
}

fn open(id: Option<&str>, note: &mut dyn FnMut(String), verify_frame: bool) -> Result<Camera> {
  let cameras = nokhwa::query(ApiBackend::Auto).context("querying cameras")?;
  let info = find_device(&cameras, id).ok_or_else(|| match id {
    Some(id) => anyhow!("camera {id:?} is no longer available"),
    None => anyhow!("no camera found"),
  })?;

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
    if !verify_frame {
      return Ok(camera);
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

  // Preserve the backend-selected format as a last resort. Some cameras report modes that the
  // setter accepts but that AVFoundation cannot activate; their default stream may still work.
  note("requested modes failed; trying the backend default".to_owned());
  camera.open_stream().context("starting backend-default stream")?;
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
  // The frame read can block indefinitely for unavailable cameras. Keep it in the capture loop
  // so the controller can time out startup and move on without blocking its UI thread.
  let mut camera = open(id, note, false)?;
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
  fn unavailable_devices_are_excluded() {
    let cameras = [
      CameraInfo {
        id: "available".to_owned(),
        name: "Available camera".to_owned(),
      },
      CameraInfo {
        id: "closed".to_owned(),
        name: "Closed laptop camera".to_owned(),
      },
    ];
    let filtered = filter_available_with(cameras, |camera| camera.id == "available");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].id, "available");
  }

  #[test]
  fn device_lookup_uses_stable_identifier_and_does_not_fall_back() {
    let cameras = [
      nokhwa::utils::CameraInfo::new("Built-in", "", "builtin-id", nokhwa::utils::CameraIndex::Index(0)),
      nokhwa::utils::CameraInfo::new("External", "", "external-id", nokhwa::utils::CameraIndex::Index(1)),
    ];
    assert_eq!(
      find_device(&cameras, Some("external-id")).map(|camera| camera.human_name()),
      Some("External".to_owned())
    );
    assert!(find_device(&cameras, Some("missing-id")).is_none());
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
