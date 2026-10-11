//! Webcam capture. Only `nokhwa.rs` names nokhwa types; everything else uses this API.
//!
//! A [`CameraStream`] owns one capture thread that keeps only the newest frame. The UI
//! polls [`CameraStream::take_frame`] and is woken through `egui::Context::request_repaint`.

pub mod controller;
mod nokhwa;
pub mod overlay;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::Result;

/// A selectable capture device. `id` is the device name, which is more stable than an index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraInfo {
  pub id: String,
  pub name: String,
}

/// One decoded RGBA frame.
pub struct Frame {
  pub width: u32,
  pub height: u32,
  pub rgba: Vec<u8>,
}

/// Ask the OS for camera access (macOS shows a prompt once). Call from the main thread.
pub fn request_permission() {
  nokhwa::request_permission();
}

/// Enumerate available cameras.
pub fn list_cameras() -> Result<Vec<CameraInfo>> {
  nokhwa::list()
}

#[derive(Default)]
struct Shared {
  latest: Mutex<Option<Frame>>,
  error: Mutex<Option<String>>,
  /// Diagnostic lines about the chosen capture mode (shown in the settings popover).
  diag: Mutex<Vec<String>>,
}

/// A running capture. Dropping it stops the thread and releases the device.
pub struct CameraStream {
  shared: Arc<Shared>,
  stop: Arc<AtomicBool>,
  join: Option<JoinHandle<()>>,
}

impl CameraStream {
  /// Start capturing from the camera with the given id (`None` = first device).
  pub fn start(id: Option<String>, ctx: egui::Context) -> Self {
    let shared = Arc::new(Shared::default());
    let stop = Arc::new(AtomicBool::new(false));
    let join = {
      let shared = shared.clone();
      let stop = stop.clone();
      std::thread::Builder::new()
        .name("camera".into())
        .spawn(move || {
          let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut note = |line: String| {
              tracing::info!("camera: {line}");
              if let Ok(mut d) = shared.diag.lock() {
                d.push(line);
              }
              ctx.request_repaint();
            };
            nokhwa::capture(id.as_deref(), &stop, &mut note, |frame| {
              if let Ok(mut slot) = shared.latest.lock() {
                *slot = Some(frame);
              }
              ctx.request_repaint();
            })
          }));
          let message = match result {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(format!("{e:#}")),
            Err(_) => Some("camera backend crashed".to_string()),
          };
          if let Some(message) = message {
            tracing::warn!("camera stopped: {message}");
            if let Ok(mut slot) = shared.error.lock() {
              *slot = Some(message);
            }
            ctx.request_repaint();
          }
        })
        .ok()
    };
    Self { shared, stop, join }
  }

  /// Take the newest frame, if one arrived since the last call.
  pub fn take_frame(&self) -> Option<Frame> {
    self.shared.latest.lock().ok()?.take()
  }

  /// Diagnostic lines about how the capture mode was chosen.
  pub fn diagnostics(&self) -> Vec<String> {
    self.shared.diag.lock().map(|d| d.clone()).unwrap_or_default()
  }

  /// The error that stopped the capture, if any.
  pub fn error(&self) -> Option<String> {
    self.shared.error.lock().ok()?.clone()
  }
}

impl Drop for CameraStream {
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Relaxed);
    if let Some(join) = self.join.take() {
      let _ = join.join();
    }
  }
}
