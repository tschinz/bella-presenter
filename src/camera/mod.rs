//! Webcam capture. Only `nokhwa.rs` names nokhwa types; everything else uses this API.
//!
//! A [`CameraStream`] owns one capture thread that keeps only the newest frame. The UI
//! polls [`CameraStream::take_frame`] and is woken through `egui::Context::request_repaint`.

pub mod controller;
mod nokhwa;
pub mod overlay;
pub mod processing;
mod segment;

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::Result;

use processing::FrameProcessor;

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

/// Enumerate devices and retain only cameras that open and deliver a frame.
pub fn list_available_cameras() -> Result<Vec<CameraInfo>> {
  nokhwa::list_available()
}

#[derive(Default)]
struct Shared {
  latest: Mutex<Option<Frame>>,
  error: Mutex<Option<String>>,
  finished: AtomicBool,
  first_frame: AtomicBool,
  /// Diagnostic lines about the chosen capture mode (shown in the settings popover).
  diag: Mutex<Vec<String>>,
}

/// A running capture. Dropping it stops the thread and releases the device.
pub struct CameraStream {
  shared: Arc<Shared>,
  stop: Arc<AtomicBool>,
  finished: Option<mpsc::Receiver<()>>,
  started_at: Instant,
}

impl CameraStream {
  /// Start capturing from the camera with the given id (`None` = first device).
  pub fn start(id: Option<String>, background: Arc<AtomicU8>, ctx: egui::Context) -> Self {
    let shared = Arc::new(Shared::default());
    let stop = Arc::new(AtomicBool::new(false));
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    let finished = finished_rx;
    let shared_for_thread = shared.clone();
    {
      let stop = stop.clone();
      let spawn = std::thread::Builder::new().name("camera".into()).spawn(move || {
        let shared = shared_for_thread;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
          let mut segmenter: Option<segment::Segmenter> = None;
          let mut segmenter_failed = false;
          let mut note = |line: String| {
            tracing::info!("camera: {line}");
            if let Ok(mut d) = shared.diag.lock() {
              d.push(line);
            }
            ctx.request_repaint();
          };
          nokhwa::capture(id.as_deref(), &stop, &mut note, |mut frame| {
            let mode = crate::config::BackgroundMode::from_u8(background.load(Ordering::Relaxed));
            if mode != crate::config::BackgroundMode::Off && !segmenter_failed {
              if segmenter.is_none() {
                match segment::Segmenter::new() {
                  Ok(s) => segmenter = Some(s),
                  Err(e) => {
                    tracing::warn!("background removal unavailable: {e:#}");
                    segmenter_failed = true;
                  }
                }
              }
              if let Some(s) = segmenter.as_mut() {
                frame = s.process(frame);
                if mode == crate::config::BackgroundMode::Blur {
                  processing::composite_blur(&mut frame);
                } else if mode == crate::config::BackgroundMode::Remove {
                  processing::crop_to_visible(&mut frame, 0.08);
                }
              }
            }
            if let Ok(mut slot) = shared.latest.lock() {
              *slot = Some(frame);
            }
            shared.first_frame.store(true, Ordering::Release);
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
        shared.finished.store(true, Ordering::Release);
        let _ = finished_tx.send(());
      });
      if let Err(error) = spawn {
        shared.finished.store(true, Ordering::Release);
        if let Ok(mut slot) = shared.error.lock() {
          *slot = Some(format!("starting camera thread: {error}"));
        }
      }
    }
    Self {
      shared,
      stop,
      finished: Some(finished),
      started_at: Instant::now(),
    }
  }

  /// Request capture shutdown and return immediately. The returned receiver becomes ready only
  /// after the capture worker has released the camera. Do not start another device before then.
  pub fn stop(mut self) -> mpsc::Receiver<()> {
    self.stop.store(true, Ordering::Relaxed);
    self.finished.take().expect("camera completion receiver is present until stop")
  }

  /// Whether the capture worker has ended (including after a device or backend error).
  pub fn is_finished(&self) -> bool {
    self.shared.finished.load(Ordering::Acquire)
  }

  /// Detect a worker that never delivered its first frame (for example, a closed-lid camera).
  pub fn startup_timed_out(&self, timeout: Duration) -> bool {
    !self.shared.first_frame.load(Ordering::Acquire) && self.started_at.elapsed() >= timeout
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
  }
}
