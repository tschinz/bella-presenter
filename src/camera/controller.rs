//! UI-side camera state: owns the stream and texture, draws the overlay, and shows the
//! settings controls. Keeps `app.rs` free of camera details.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use egui::{Color32, Rect, TextureHandle, TextureOptions, pos2};

use super::overlay::place;
use super::{CameraInfo, CameraStream, list_available_cameras, request_permission};
use crate::config::{BackgroundMode, CameraSettings, CameraShape, Corner};

pub struct CameraController {
  pub settings: CameraSettings,
  stream: Option<CameraStream>,
  texture: Option<TextureHandle>,
  devices: Vec<CameraInfo>,
  /// Device the running stream was started with (to detect a change).
  running_device: Option<String>,
  stopping: Option<Receiver<()>>,
  stopping_since: Option<std::time::Instant>,
  discovery: Option<Receiver<anyhow::Result<Vec<CameraInfo>>>>,
  discovery_requested: bool,
  discovery_done: bool,
  last_error: Option<String>,
  diagnostics: Vec<String>,
  capture_failed: bool,
  permission_requested: bool,
  /// Set by `settings_ui` while the settings popover is open (the preview needs frames).
  preview_seen: bool,
  /// Shared with the capture thread; toggles segmentation without restarting the camera.
  background: Arc<AtomicU8>,
}

const CAMERA_STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const CAMERA_STOP_TIMEOUT: Duration = Duration::from_secs(2);

fn selected_available_device(current: Option<&str>, devices: &[CameraInfo]) -> Option<String> {
  current
    .filter(|id| devices.iter().any(|device| device.id == *id))
    .or_else(|| devices.first().map(|device| device.id.as_str()))
    .map(str::to_owned)
}

impl CameraController {
  pub fn new(settings: CameraSettings) -> Self {
    let background = Arc::new(AtomicU8::new(settings.background.to_u8()));
    Self {
      settings,
      stream: None,
      texture: None,
      devices: Vec::new(),
      running_device: None,
      stopping: None,
      stopping_since: None,
      discovery: None,
      discovery_requested: false,
      discovery_done: false,
      last_error: None,
      diagnostics: Vec::new(),
      capture_failed: false,
      permission_requested: false,
      preview_seen: false,
      background,
    }
  }

  /// Start, stop or restart the capture so it matches the settings, and upload any new frame.
  /// The camera only runs while enabled and either presenting or previewing in the settings
  /// popover, so an idle app keeps the device (and its LED) off. Call once per presenter frame.
  pub fn update(&mut self, ctx: &egui::Context, presenting: bool) {
    let wanted = self.settings.enabled && (presenting || self.preview_seen);
    self.preview_seen = false;
    self.background.store(self.settings.background.to_u8(), Ordering::Relaxed);
    self.poll_discovery(ctx);
    self.poll_stopping();
    if wanted && !self.discovery_done && self.discovery.is_none() {
      if !self.permission_requested {
        self.permission_requested = true;
        request_permission();
      }
      self.discovery_requested = true;
    }
    if self.discovery_requested && self.stopping.is_none() && self.stream.is_none() {
      self.begin_discovery(ctx);
    }
    if !wanted {
      if self.stream.is_some() {
        self.stop_stream();
        self.texture = None;
      }
      return;
    }
    if self.stream.is_some() && self.running_device != self.settings.device {
      self.stop_stream();
      self.capture_failed = false;
      self.texture = None;
    }
    if self.stream.as_ref().is_some_and(CameraStream::is_finished) {
      self.stop_stream();
      self.capture_failed = true;
      self.texture = None;
    }
    if self.stream.as_ref().is_some_and(|stream| stream.startup_timed_out(CAMERA_STARTUP_TIMEOUT)) {
      let unavailable = self.running_device.take();
      self.last_error = Some(format!(
        "camera did not deliver a frame; removed {} from the available list",
        unavailable.as_deref().unwrap_or("selected camera")
      ));
      self.stop_stream();
      self.devices.retain(|device| Some(device.id.as_str()) != unavailable.as_deref());
      self.settings.device = selected_available_device(None, &self.devices);
      self.capture_failed = self.settings.device.is_none();
      self.texture = None;
    }
    self.poll_stopping();
    if self.stopping.is_some() && !self.stop_timed_out() {
      ctx.request_repaint_after(std::time::Duration::from_millis(50));
      return;
    }
    if self.stop_timed_out() {
      self.stopping = None;
      self.stopping_since = None;
      self.last_error = Some("previous camera did not stop promptly; switching to the selected camera".to_owned());
    }
    if self.stream.is_none() {
      if self.devices.is_empty() || self.discovery.is_some() || self.discovery_requested {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        return;
      }
      if self.capture_failed {
        return;
      }
      let Some(device) = self.settings.device.clone() else {
        return;
      };
      self.running_device = Some(device.clone());
      self.stream = Some(CameraStream::start(Some(device), self.background.clone(), ctx.clone()));
    }
    if let Some(frame) = self.stream.as_ref().and_then(|s| s.take_frame()) {
      if frame.rgba.len() != frame.width as usize * frame.height as usize * 4 {
        return;
      }
      let image = egui::ColorImage::from_rgba_unmultiplied([frame.width as usize, frame.height as usize], &frame.rgba);
      match self.texture.as_mut() {
        Some(tex) => tex.set(image, TextureOptions::LINEAR),
        None => self.texture = Some(ctx.load_texture("camera", image, TextureOptions::LINEAR)),
      }
    } else if self.stream.is_some() {
      // A camera can enumerate and open but never deliver a frame (e.g. a closed-lid camera).
      // Keep polling so the startup timeout runs even when the UI otherwise has no repaint work.
      ctx.request_repaint_after(Duration::from_millis(100));
    }
  }

  /// Drop a failed stream so the next frame starts a fresh one.
  fn retry(&mut self) {
    self.last_error = None;
    self.capture_failed = false;
    if self.stream.is_some() {
      self.stop_stream();
    }
  }

  fn error(&self) -> Option<String> {
    self.stream.as_ref().and_then(|s| s.error()).or_else(|| self.last_error.clone())
  }

  /// Draw the overlay inside `area` (the audience window's full rect).
  pub fn draw_overlay(&self, ui: &egui::Ui, area: Rect) {
    if !self.settings.enabled {
      return;
    }
    if let Some(tex) = self.texture.as_ref() {
      self.paint(ui, tex, area, self.settings.size);
    }
  }

  fn paint(&self, ui: &egui::Ui, tex: &TextureHandle, area: Rect, size: f32) {
    let s = &self.settings;
    let [tw, th] = tex.size();
    let frame_aspect = tw as f32 / th.max(1) as f32;
    let shape = if s.background == BackgroundMode::Remove { CameraShape::Rect } else { s.shape };
    let circle = shape == CameraShape::Circle;
    let aspect = if circle { 1.0 } else { frame_aspect };
    let rect = place(area, s.corner, size, s.margin, aspect);

    // UV: crop to a centred square for a circle; flip horizontally when mirrored.
    let (mut u0, mut u1) = (0.0_f32, 1.0_f32);
    if circle && frame_aspect > 1.0 {
      let half = 0.5 / frame_aspect;
      u0 = 0.5 - half;
      u1 = 0.5 + half;
    }
    let (mut v0, mut v1) = (0.0_f32, 1.0_f32);
    if circle && frame_aspect < 1.0 {
      let half = 0.5 * frame_aspect;
      v0 = 0.5 - half;
      v1 = 0.5 + half;
    }
    if s.mirror {
      std::mem::swap(&mut u0, &mut u1);
    }
    let uv = Rect::from_min_max(pos2(u0, v0), pos2(u1, v1));

    let radius = match shape {
      CameraShape::Rect => 0.0,
      CameraShape::Rounded => rect.height().min(rect.width()) * 0.12,
      CameraShape::Circle => rect.width() / 2.0,
    }
    .min(255.0);
    ui.painter()
      .add(egui::epaint::RectShape::filled(rect, radius, Color32::WHITE).with_texture(tex.id(), uv));
  }

  /// Settings controls. Returns true if anything changed (so the caller persists it).
  pub fn settings_ui(&mut self, ui: &mut egui::Ui) -> bool {
    let mut changed = false;
    self.preview_seen = true;
    self.poll_discovery(ui.ctx());
    if self.settings.enabled && self.stream.is_none() {
      ui.ctx().request_repaint(); // let `update` start the preview stream
    }
    if self.settings.enabled && self.devices.is_empty() && self.discovery.is_none() && !self.discovery_done {
      self.refresh_devices(ui.ctx());
    }
    if ui.checkbox(&mut self.settings.enabled, "Camera (C)").changed() {
      changed = true;
      if self.settings.enabled {
        if !self.permission_requested {
          self.permission_requested = true;
          request_permission();
        }
        self.refresh_devices(ui.ctx());
      }
    }
    if !self.settings.enabled {
      return changed;
    }
    // Inline list (a combo box's dropdown would count as a click outside the popover).
    ui.horizontal(|ui| {
      ui.label("Source");
      if ui.small_button("⟳").on_hover_text("Rescan cameras").clicked() {
        self.refresh_devices(ui.ctx());
      }
    });
    if self.discovery.is_some() {
      ui.small("Searching for available cameras…");
    } else if self.discovery_requested || self.stopping.is_some() {
      ui.small("Updating camera list…");
    }
    let selected = self.settings.device.clone().or_else(|| self.devices.first().map(|d| d.id.clone()));
    for d in &self.devices {
      let is_selected = selected.as_deref() == Some(d.id.as_str());
      if ui.radio(is_selected, &d.name).clicked() && !is_selected {
        self.settings.device = Some(d.id.clone());
        self.last_error = None;
        self.capture_failed = false;
        self.running_device = None;
        self.texture = None;
        changed = true;
      }
    }
    if self.settings.enabled && self.texture.is_none() {
      if let Some(error) = self.error() {
        ui.colored_label(Color32::LIGHT_RED, format!("Camera preview unavailable: {error}"));
      } else if self.discovery.is_some() || self.discovery_requested || self.stopping.is_some() {
        ui.small("Preparing camera preview…");
      } else if !self.capture_failed {
        ui.small("Starting camera preview…");
      }
    }

    ui.horizontal(|ui| {
      ui.label("Corner");
      for (corner, label) in [
        (Corner::TopLeft, "↖"),
        (Corner::TopRight, "↗"),
        (Corner::BottomLeft, "↙"),
        (Corner::BottomRight, "↘"),
      ] {
        changed |= ui.selectable_value(&mut self.settings.corner, corner, label).changed();
      }
    });
    ui.horizontal(|ui| {
      ui.label("Shape");
      for (shape, label) in [(CameraShape::Rect, "Rect"), (CameraShape::Rounded, "Rounded"), (CameraShape::Circle, "Circle")] {
        changed |= ui.selectable_value(&mut self.settings.shape, shape, label).changed();
      }
    });
    changed |= ui.add(egui::Slider::new(&mut self.settings.size, 0.08..=0.5).text("Size")).changed();
    changed |= ui.checkbox(&mut self.settings.mirror, "Mirror").changed();
    ui.horizontal(|ui| {
      ui.label("Background");
      for (mode, label) in [
        (BackgroundMode::Off, "Keep"),
        (BackgroundMode::Remove, "Remove"),
        (BackgroundMode::Blur, "Blur"),
      ] {
        changed |= ui.selectable_value(&mut self.settings.background, mode, label).changed();
      }
    });

    if let Some(err) = self.error() {
      ui.colored_label(Color32::LIGHT_RED, format!("Camera error: {err}"));
      if ui.button("Retry").clicked() {
        self.refresh_devices(ui.ctx());
        self.retry();
      }
    } else if self.devices.is_empty() && self.discovery.is_none() {
      ui.colored_label(Color32::YELLOW, "No available cameras (check access or connection)");
    } else if self.settings.enabled && self.settings.device.is_none() {
      ui.colored_label(Color32::LIGHT_RED, "No selected camera is available");
    }
    if self.stopping.is_some() {
      ui.small(if self.stop_timed_out() {
        "Previous camera is unresponsive; continuing with the selected camera…"
      } else {
        "Waiting for the previous camera to stop…"
      });
    }
    {
      let lines = self.stream.as_ref().map(CameraStream::diagnostics).unwrap_or_else(|| self.diagnostics.clone());
      if !lines.is_empty() {
        ui.collapsing("Capture details", |ui| {
          ui.set_max_width(320.0);
          for line in lines {
            ui.small(line);
          }
        });
      }
    }
    // Small self-preview, so the presenter can check the framing.
    if let Some(tex) = self.texture.as_ref() {
      let [tw, th] = tex.size();
      ui.small(format!("Capture: {tw}x{th}"));
      let w = 200.0;
      let h = w * th as f32 / tw.max(1) as f32;
      let (rect, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
      let uv = if self.settings.mirror {
        Rect::from_min_max(pos2(1.0, 0.0), pos2(0.0, 1.0))
      } else {
        Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))
      };
      // Dark backdrop so a removed background is visible as transparency.
      ui.painter().rect_filled(rect, 0.0, Color32::from_gray(40));
      ui.painter().image(tex.id(), rect, uv, Color32::WHITE);
    }
    changed
  }

  fn refresh_devices(&mut self, ctx: &egui::Context) {
    if self.discovery.is_some() || self.discovery_requested {
      return;
    }
    self.discovery_requested = true;
    self.discovery_done = false;
    if self.stream.is_some() {
      self.stop_stream();
    }
    self.poll_stopping();
    if self.stopping.is_some() {
      ctx.request_repaint_after(std::time::Duration::from_millis(50));
      return;
    }
    self.begin_discovery(ctx);
  }

  fn begin_discovery(&mut self, ctx: &egui::Context) {
    if self.discovery.is_some() || self.stream.is_some() || self.stopping.is_some() {
      return;
    }
    let (tx, rx) = mpsc::sync_channel(1);
    self.discovery = Some(rx);
    self.discovery_requested = false;
    let ctx = ctx.clone();
    let spawn = std::thread::Builder::new().name("camera-discovery".into()).spawn(move || {
      let result = list_available_cameras();
      let _ = tx.send(result);
      ctx.request_repaint();
    });
    if let Err(error) = spawn {
      self.discovery = None;
      self.discovery_done = true;
      self.discovery_requested = false;
      self.last_error = Some(format!("starting camera discovery: {error}"));
    }
  }

  fn poll_discovery(&mut self, ctx: &egui::Context) {
    let Some(receiver) = self.discovery.as_ref() else { return };
    match receiver.try_recv() {
      Ok(Ok(devices)) => {
        self.devices = devices;
        self.discovery_done = true;
        self.discovery = None;
        self.settings.device = selected_available_device(self.settings.device.as_deref(), &self.devices);
        ctx.request_repaint();
      }
      Ok(Err(error)) => {
        self.last_error = Some(format!("camera discovery failed: {error:#}"));
        self.discovery_done = true;
        self.discovery = None;
        ctx.request_repaint();
      }
      Err(mpsc::TryRecvError::Disconnected) => {
        self.last_error = Some("camera discovery worker stopped unexpectedly".to_owned());
        self.discovery_done = true;
        self.discovery = None;
      }
      Err(mpsc::TryRecvError::Empty) => ctx.request_repaint_after(std::time::Duration::from_millis(100)),
    }
  }

  fn stop_stream(&mut self) {
    if let Some(stream) = self.stream.take() {
      if let Some(error) = stream.error() {
        self.last_error = Some(error);
      }
      self.diagnostics = stream.diagnostics();
      self.stopping = Some(stream.stop());
      self.stopping_since = Some(std::time::Instant::now());
    }
  }

  fn poll_stopping(&mut self) {
    let Some(receiver) = self.stopping.as_ref() else { return };
    if matches!(receiver.try_recv(), Ok(()) | Err(mpsc::TryRecvError::Disconnected)) {
      self.stopping = None;
      self.stopping_since = None;
    }
  }

  fn stop_timed_out(&self) -> bool {
    self.stopping_since.is_some_and(|since| since.elapsed() >= CAMERA_STOP_TIMEOUT)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn selects_first_available_when_no_saved_device() {
    let devices = vec![
      CameraInfo {
        id: "external-id".to_owned(),
        name: "External camera".to_owned(),
      },
      CameraInfo {
        id: "phone-id".to_owned(),
        name: "Phone camera".to_owned(),
      },
    ];
    assert_eq!(selected_available_device(None, &devices), Some("external-id".to_owned()));
  }

  #[test]
  fn replaces_saved_camera_that_is_no_longer_available() {
    let devices = vec![CameraInfo {
      id: "external-id".to_owned(),
      name: "External camera".to_owned(),
    }];
    assert_eq!(
      selected_available_device(Some("closed-laptop-camera"), &devices),
      Some("external-id".to_owned())
    );
  }

  #[test]
  fn preserves_saved_available_camera() {
    let devices = vec![
      CameraInfo {
        id: "external-id".to_owned(),
        name: "External camera".to_owned(),
      },
      CameraInfo {
        id: "phone-id".to_owned(),
        name: "Phone camera".to_owned(),
      },
    ];
    assert_eq!(selected_available_device(Some("phone-id"), &devices), Some("phone-id".to_owned()));
  }
}
