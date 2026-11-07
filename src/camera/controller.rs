//! UI-side camera state: owns the stream and the picture GPUI draws, paints the overlay, and
//! builds the settings controls. Keeps `app.rs` free of camera details.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::radio::Radio;
use gpui_kit::component::slider::{Slider, SliderState};
use gpui_kit::component::{ActiveTheme, Icon, Selectable, Sizable, h_flex, v_flex};
use gpui_kit::{
  AnyElement, App, AppContext, Bounds, ClickEvent, Context, Entity, IntoElement, ParentElement, RenderImage, Styled, Window, div, img, point, px, rgb, size,
};

use super::overlay::place;
use super::{CameraInfo, CameraStream, Frame, list_available_cameras, request_permission};
use crate::Waker;
use crate::app::PresenterApp;
use crate::config::{BackgroundMode, CameraSettings, CameraShape, Corner};

pub struct CameraController {
  pub settings: CameraSettings,
  stream: Option<CameraStream>,
  /// The newest frame, as GPUI draws it (mirrored when the settings ask for it).
  image: Option<Arc<RenderImage>>,
  devices: Vec<CameraInfo>,
  /// Device the running stream was started with (to detect a change).
  running_device: Option<String>,
  stopping: Option<Receiver<()>>,
  stopping_since: Option<Instant>,
  discovery: Option<Receiver<anyhow::Result<Vec<CameraInfo>>>>,
  discovery_requested: bool,
  discovery_done: bool,
  last_error: Option<String>,
  diagnostics: Vec<String>,
  capture_failed: bool,
  permission_requested: bool,
  /// Set by `settings_ui` while the settings popover is shown (the preview needs frames).
  preview_seen: bool,
  /// Whether the settings popover was shown on the previous frame.
  preview_open: bool,
  show_details: bool,
  /// Shared with the capture thread; toggles segmentation without restarting the camera.
  background: Arc<AtomicU8>,
  /// Shared with the capture thread, like `background`.
  mirror: Arc<AtomicBool>,
  waker: Waker,
  /// Wakes the UI for a new frame: only the audience window, which shows the overlay, redraws.
  frames: Waker,
  /// When the UI should look again (stopping, discovery or a first frame is pending).
  poll_after: Option<Duration>,
  pub size_slider: Entity<SliderState>,
}

const CAMERA_STARTUP_TIMEOUT: Duration = Duration::from_secs(8);
const CAMERA_STOP_TIMEOUT: Duration = Duration::from_secs(2);

fn selected_available_device(current: Option<&str>, devices: &[CameraInfo]) -> Option<String> {
  current
    .filter(|id| devices.iter().any(|device| device.id == *id))
    .or_else(|| devices.first().map(|device| device.id.as_str()))
    .map(str::to_owned)
}

/// A captured frame (already BGRA and mirrored by the capture thread) as GPUI draws it.
fn frame_image(frame: Frame) -> Option<Arc<RenderImage>> {
  let picture = image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba)?;
  Some(crate::app::render_image(picture))
}

impl CameraController {
  pub fn new(settings: CameraSettings, waker: Waker, frames: Waker, cx: &mut App) -> Self {
    let background = Arc::new(AtomicU8::new(settings.background.to_u8()));
    let mirror = Arc::new(AtomicBool::new(settings.mirror));
    let size_slider = cx.new(|_| SliderState::new().min(0.08).max(0.5).step(0.01).default_value(settings.size));
    Self {
      settings,
      stream: None,
      image: None,
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
      preview_open: false,
      show_details: false,
      background,
      mirror,
      waker,
      frames,
      poll_after: None,
      size_slider,
    }
  }

  /// Whether the settings popover shows the preview (then the presenter window needs the frames too).
  pub fn previewing(&self) -> bool {
    self.preview_open
  }

  fn poll_in(&mut self, after: Duration) {
    self.poll_after = Some(self.poll_after.map_or(after, |p| p.min(after)));
  }

  /// Replace the shown picture, taking the old one off the GPU.
  fn set_image(&mut self, image: Option<Arc<RenderImage>>, window: &mut Window, cx: &mut App) {
    if let Some(old) = std::mem::replace(&mut self.image, image) {
      cx.drop_image(old, Some(window));
    }
  }

  /// Start, stop or restart the capture so it matches the settings, and take any new frame.
  /// The camera only runs while enabled and either presenting or previewing in the settings
  /// popover, so an idle app keeps the device (and its LED) off. Call once per presenter frame;
  /// returns when to call again if nothing else wakes the UI.
  pub fn update(&mut self, presenting: bool, window: &mut Window, cx: &mut App) -> Option<Duration> {
    self.run(presenting, window, cx);
    self.poll_after.take()
  }

  fn run(&mut self, presenting: bool, window: &mut Window, cx: &mut App) {
    self.preview_open = self.preview_seen;
    let wanted = self.settings.enabled && (presenting || self.preview_seen);
    self.preview_seen = false;
    self.background.store(self.settings.background.to_u8(), Ordering::Relaxed);
    self.mirror.store(self.settings.mirror, Ordering::Relaxed);
    self.poll_discovery();
    self.poll_stopping();
    if wanted && !self.discovery_done && self.discovery.is_none() {
      if !self.permission_requested {
        self.permission_requested = true;
        request_permission();
      }
      self.discovery_requested = true;
    }
    if self.discovery_requested && self.stopping.is_none() && self.stream.is_none() {
      self.begin_discovery();
    }
    if !wanted {
      if self.stream.is_some() {
        self.stop_stream();
        self.set_image(None, window, cx);
      }
      return;
    }
    if self.stream.is_some() && self.running_device != self.settings.device {
      self.stop_stream();
      self.capture_failed = false;
      self.set_image(None, window, cx);
    }
    if self.stream.as_ref().is_some_and(CameraStream::is_finished) {
      self.stop_stream();
      self.capture_failed = true;
      self.set_image(None, window, cx);
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
      self.set_image(None, window, cx);
    }
    self.poll_stopping();
    if self.stopping.is_some() && !self.stop_timed_out() {
      self.poll_in(Duration::from_millis(50));
      return;
    }
    if self.stop_timed_out() {
      self.stopping = None;
      self.stopping_since = None;
      self.last_error = Some("previous camera did not stop promptly; switching to the selected camera".to_owned());
    }
    if self.stream.is_none() {
      if self.devices.is_empty() || self.discovery.is_some() || self.discovery_requested {
        self.poll_in(Duration::from_millis(100));
        return;
      }
      if self.capture_failed {
        return;
      }
      let Some(device) = self.settings.device.clone() else {
        return;
      };
      self.running_device = Some(device.clone());
      self.stream = Some(CameraStream::start(
        Some(device),
        self.background.clone(),
        self.mirror.clone(),
        self.frames.clone(),
      ));
    }
    if let Some(frame) = self.stream.as_ref().and_then(|s| s.take_frame()) {
      if let Some(image) = frame_image(frame) {
        self.set_image(Some(image), window, cx);
      }
    } else if self.stream.is_some() {
      // A camera can enumerate and open but never deliver a frame (e.g. a closed-lid camera).
      // Keep polling so the startup timeout runs even when the UI otherwise has no work.
      self.poll_in(Duration::from_millis(100));
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

  /// Paint the overlay inside `area` (the audience window's full rect).
  pub fn paint_overlay(&self, area: Bounds<f32>, window: &mut Window) {
    let (true, Some(image)) = (self.settings.enabled, self.image.as_ref()) else {
      return;
    };
    let s = &self.settings;
    let frame = image.size(0);
    let frame_aspect = frame.width.0 as f32 / frame.height.0.max(1) as f32;
    let shape = if s.background == BackgroundMode::Remove { CameraShape::Rect } else { s.shape };
    let circle = shape == CameraShape::Circle;
    let rect = place(area, s.corner, s.size, s.margin, if circle { 1.0 } else { frame_aspect });

    // A circle shows the centred square of the frame.
    let uv = if !circle {
      Bounds::new(point(0.0, 0.0), size(1.0, 1.0))
    } else if frame_aspect > 1.0 {
      Bounds::centered_at(point(0.5, 0.5), size(1.0 / frame_aspect, 1.0))
    } else {
      Bounds::centered_at(point(0.5, 0.5), size(1.0, frame_aspect))
    };
    let radius = match shape {
      CameraShape::Rect => 0.0,
      CameraShape::Rounded => rect.size.height.min(rect.size.width) * 0.12,
      CameraShape::Circle => rect.size.width / 2.0,
    };
    crate::app::paint_cropped(window, rect, uv, radius, image.clone());
  }

  /// Settings controls, shown in the presenter's settings popover.
  pub fn settings_ui(&mut self, cx: &mut Context<PresenterApp>) -> AnyElement {
    self.preview_seen = true;
    if self.settings.enabled && self.devices.is_empty() && self.discovery.is_none() && !self.discovery_done {
      self.refresh_devices();
    }
    // The popover just opened: let `update` start the preview stream.
    if self.settings.enabled && self.stream.is_none() && !self.preview_open {
      self.waker.wake();
    }
    let enabled = Checkbox::new("camera-enabled")
      .label("Camera (C)")
      .xsmall()
      .checked(self.settings.enabled)
      .on_click(cx.listener(|app, checked: &bool, _, cx| {
        app.camera.set_enabled(*checked);
        app.dirty = true;
        cx.notify();
      }));
    if !self.settings.enabled {
      return v_flex().gap_2().child(enabled).into_any_element();
    }
    let theme = cx.theme();
    let (danger, warning, muted) = (theme.danger, theme.warning, theme.muted_foreground);
    let note = |text: &str| div().text_color(muted).child(text.to_owned());

    let mut col = v_flex().gap_2().w(px(320.)).child(enabled).child(
      h_flex().gap_2().child("Source").child(
        Button::new("camera-rescan")
          .icon(Icon::new(Lucide::RefreshCw))
          .xsmall()
          .tooltip("Rescan cameras")
          .on_click(cx.listener(|app, _: &ClickEvent, _, cx| {
            app.camera.refresh_devices();
            cx.notify();
          })),
      ),
    );
    if self.discovery.is_some() {
      col = col.child(note("Searching for available cameras…"));
    } else if self.discovery_requested || self.stopping.is_some() {
      col = col.child(note("Updating camera list…"));
    }
    let selected = self.settings.device.clone().or_else(|| self.devices.first().map(|d| d.id.clone()));
    for (i, d) in self.devices.iter().enumerate() {
      let is_selected = selected.as_deref() == Some(d.id.as_str());
      let id = d.id.clone();
      col = col.child(
        Radio::new(("camera-device", i))
          .label(d.name.clone())
          .xsmall()
          .checked(is_selected)
          .on_click(cx.listener(move |app, _: &bool, window, cx| {
            if app.camera.settings.device.as_deref() != Some(id.as_str()) {
              let camera = &mut app.camera;
              camera.settings.device = Some(id.clone());
              camera.last_error = None;
              camera.capture_failed = false;
              camera.running_device = None;
              camera.set_image(None, window, cx);
              app.dirty = true;
              cx.notify();
            }
          })),
      );
    }
    if self.image.is_none() {
      if let Some(error) = self.error() {
        col = col.child(div().text_color(danger).child(format!("Camera preview unavailable: {error}")));
      } else if self.discovery.is_some() || self.discovery_requested || self.stopping.is_some() {
        col = col.child(note("Preparing camera preview…"));
      } else if !self.capture_failed {
        col = col.child(note("Starting camera preview…"));
      }
    }

    let corners = [
      (Corner::TopLeft, "Top left", Some(Lucide::ArrowUpLeft)),
      (Corner::TopRight, "Top right", Some(Lucide::ArrowUpRight)),
      (Corner::BottomLeft, "Bottom left", Some(Lucide::ArrowDownLeft)),
      (Corner::BottomRight, "Bottom right", Some(Lucide::ArrowDownRight)),
    ];
    let shapes = [
      (CameraShape::Rect, "Rect", None),
      (CameraShape::Rounded, "Rounded", None),
      (CameraShape::Circle, "Circle", None),
    ];
    let backgrounds = [
      (BackgroundMode::Off, "Keep", None),
      (BackgroundMode::Remove, "Remove", None),
      (BackgroundMode::Blur, "Blur", None),
    ];
    col = col
      .child(choice_row("Corner", &corners, self.settings.corner, cx, |s, v| s.corner = v))
      .child(choice_row("Shape", &shapes, self.settings.shape, cx, |s, v| s.shape = v))
      .child(h_flex().gap_2().child(Slider::new(&self.size_slider).flex_1()).child("Size"))
      .child(
        Checkbox::new("camera-mirror")
          .label("Mirror")
          .xsmall()
          .checked(self.settings.mirror)
          .on_click(cx.listener(|app, checked: &bool, _, cx| {
            app.camera.settings.mirror = *checked;
            app.dirty = true;
            cx.notify();
          })),
      )
      .child(choice_row("Background", &backgrounds, self.settings.background, cx, |s, v| s.background = v));

    if let Some(err) = self.error() {
      col = col
        .child(div().text_color(danger).child(format!("Camera error: {err}")))
        .child(
          Button::new("camera-retry")
            .label("Retry")
            .xsmall()
            .on_click(cx.listener(|app, _: &ClickEvent, _, cx| {
              app.camera.refresh_devices();
              app.camera.retry();
              cx.notify();
            })),
        );
    } else if self.devices.is_empty() && self.discovery.is_none() {
      col = col.child(div().text_color(warning).child("No available cameras (check access or connection)"));
    } else if self.settings.device.is_none() {
      col = col.child(div().text_color(danger).child("No selected camera is available"));
    }
    if self.stopping.is_some() {
      col = col.child(note(if self.stop_timed_out() {
        "Previous camera is unresponsive; continuing with the selected camera…"
      } else {
        "Waiting for the previous camera to stop…"
      }));
    }
    let lines = self.stream.as_ref().map(CameraStream::diagnostics).unwrap_or_else(|| self.diagnostics.clone());
    if !lines.is_empty() {
      col = col.child(
        Button::new("camera-details")
          .icon(Icon::new(if self.show_details { Lucide::ChevronDown } else { Lucide::ChevronRight }))
          .label("Capture details")
          .ghost()
          .xsmall()
          .on_click(cx.listener(|app, _: &ClickEvent, _, cx| {
            app.camera.show_details = !app.camera.show_details;
            cx.notify();
          })),
      );
      if self.show_details {
        col = col.children(lines.iter().map(|line| note(line)));
      }
    }
    // Small self-preview, so the presenter can check the framing.
    if let Some(image) = self.image.clone() {
      let frame = image.size(0);
      let (tw, th) = (frame.width.0, frame.height.0);
      let w = 240.0;
      let h = w * th as f32 / tw.max(1) as f32;
      // Dark backdrop so a removed background is visible as transparency.
      col = col
        .child(note(&format!("Capture: {tw}x{th}")))
        .child(div().w(px(w)).h(px(h)).bg(rgb(0x282828)).child(img(image).size_full()));
    }
    col.into_any_element()
  }

  fn set_enabled(&mut self, enabled: bool) {
    self.settings.enabled = enabled;
    if enabled {
      if !self.permission_requested {
        self.permission_requested = true;
        request_permission();
      }
      self.refresh_devices();
    }
  }

  fn refresh_devices(&mut self) {
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
      self.poll_in(Duration::from_millis(50));
      return;
    }
    self.begin_discovery();
  }

  fn begin_discovery(&mut self) {
    if self.discovery.is_some() || self.stream.is_some() || self.stopping.is_some() {
      return;
    }
    let (tx, rx) = mpsc::sync_channel(1);
    self.discovery = Some(rx);
    self.discovery_requested = false;
    let waker = self.waker.clone();
    let spawn = std::thread::Builder::new().name("camera-discovery".into()).spawn(move || {
      let result = list_available_cameras();
      let _ = tx.send(result);
      waker.wake();
    });
    if let Err(error) = spawn {
      self.discovery = None;
      self.discovery_done = true;
      self.discovery_requested = false;
      self.last_error = Some(format!("starting camera discovery: {error}"));
    }
  }

  fn poll_discovery(&mut self) {
    let Some(receiver) = self.discovery.as_ref() else { return };
    match receiver.try_recv() {
      Ok(Ok(devices)) => {
        self.devices = devices;
        self.discovery_done = true;
        self.discovery = None;
        self.settings.device = selected_available_device(self.settings.device.as_deref(), &self.devices);
      }
      Ok(Err(error)) => {
        self.last_error = Some(format!("camera discovery failed: {error:#}"));
        self.discovery_done = true;
        self.discovery = None;
      }
      Err(mpsc::TryRecvError::Disconnected) => {
        self.last_error = Some("camera discovery worker stopped unexpectedly".to_owned());
        self.discovery_done = true;
        self.discovery = None;
      }
      Err(mpsc::TryRecvError::Empty) => self.poll_in(Duration::from_millis(100)),
    }
  }

  fn stop_stream(&mut self) {
    if let Some(stream) = self.stream.take() {
      if let Some(error) = stream.error() {
        self.last_error = Some(error);
      }
      self.diagnostics = stream.diagnostics();
      self.stopping = Some(stream.stop());
      self.stopping_since = Some(Instant::now());
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

/// A label and a row of buttons, one selected, that set one camera setting. A choice with an icon
/// shows the icon and names itself in the tooltip.
fn choice_row<T: Copy + PartialEq + 'static>(
  label: &'static str,
  choices: &[(T, &'static str, Option<Lucide>)],
  current: T,
  cx: &mut Context<PresenterApp>,
  set: fn(&mut CameraSettings, T),
) -> impl IntoElement {
  h_flex()
    .gap_1()
    .child(div().mr_1().child(label))
    .children(choices.iter().enumerate().map(|(i, &(value, text, icon))| {
      let button = Button::new((label, i));
      match icon {
        Some(icon) => button.icon(Icon::new(icon)).tooltip(text),
        None => button.label(text),
      }
      .xsmall()
      .selected(value == current)
      .on_click(cx.listener(move |app, _: &ClickEvent, _, cx| {
        set(&mut app.camera.settings, value);
        app.dirty = true;
        cx.notify();
      }))
    }))
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
