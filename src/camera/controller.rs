//! UI-side camera state: owns the stream and texture, draws the overlay, and shows the
//! settings controls. Keeps `app.rs` free of camera details.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use egui::{Color32, Rect, TextureHandle, TextureOptions, pos2};

use super::overlay::place;
use super::{CameraInfo, CameraStream, list_cameras, request_permission};
use crate::config::{BackgroundMode, CameraSettings, CameraShape, Corner};

pub struct CameraController {
  pub settings: CameraSettings,
  stream: Option<CameraStream>,
  texture: Option<TextureHandle>,
  devices: Vec<CameraInfo>,
  /// Device the running stream was started with (to detect a change).
  running_device: Option<String>,
  permission_requested: bool,
  /// Set by `settings_ui` while the settings popover is open (the preview needs frames).
  preview_seen: bool,
  /// Shared with the capture thread; toggles segmentation without restarting the camera.
  background: Arc<AtomicU8>,
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
    if !wanted {
      if self.stream.is_some() {
        self.stream = None;
        self.texture = None;
      }
      return;
    }
    if self.stream.is_some() && self.running_device != self.settings.device {
      self.stream = None;
    }
    if self.stream.is_none() {
      if !self.permission_requested {
        self.permission_requested = true;
        request_permission();
      }
      self.running_device = self.settings.device.clone();
      self.stream = Some(CameraStream::start(self.settings.device.clone(), self.background.clone(), ctx.clone()));
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
    }
  }

  /// Drop a failed stream so the next frame starts a fresh one.
  fn retry(&mut self) {
    self.stream = None;
  }

  fn error(&self) -> Option<String> {
    self.stream.as_ref().and_then(|s| s.error())
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
    if self.settings.enabled && self.stream.is_none() {
      ui.ctx().request_repaint(); // let `update` start the preview stream
    }
    if self.settings.enabled && self.devices.is_empty() {
      self.refresh_devices();
    }
    if ui.checkbox(&mut self.settings.enabled, "Camera (C)").changed() {
      changed = true;
      if self.settings.enabled {
        self.refresh_devices();
      }
    }
    if !self.settings.enabled {
      return changed;
    }

    // Inline list (a combo box's dropdown would count as a click outside the popover).
    ui.horizontal(|ui| {
      ui.label("Source");
      if ui.small_button("⟳").on_hover_text("Rescan cameras").clicked() {
        self.refresh_devices();
      }
    });
    let selected = self.settings.device.clone().or_else(|| self.devices.first().map(|d| d.id.clone()));
    for d in &self.devices {
      let is_selected = selected.as_deref() == Some(d.id.as_str());
      if ui.radio(is_selected, &d.name).clicked() && !is_selected {
        self.settings.device = Some(d.id.clone());
        changed = true;
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
        self.refresh_devices();
        self.retry();
      }
    } else if self.devices.is_empty() {
      ui.colored_label(Color32::YELLOW, "No camera found (check access in system settings)");
    }
    if let Some(stream) = self.stream.as_ref() {
      let lines = stream.diagnostics();
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

  fn refresh_devices(&mut self) {
    match list_cameras() {
      Ok(list) => self.devices = list,
      Err(e) => tracing::warn!("camera enumeration failed: {e:#}"),
    }
  }
}
