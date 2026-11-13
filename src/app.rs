//! Presenter dashboard.
//!
//! - **Header** (small, fixed): open, shortcuts, footer-font controls, layout flip,
//!   present/quit, timer controls.
//! - **Center**: current slide, next-slide preview, and notes in fixed, resizable layouts
//!   cycled by "Flip layout" (L).
//! - **Footer** (large, adjustable): slide number and elapsed time, centered.
//! - **Audience window**: a second OS window showing only the slide, on black.
//! - **Start screen**: a clickable list of recent files when nothing is open.
//!
//! All persistent state lives in `~/.config/bella-presenter/state.json` (see [`crate::config`]).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
use gpui_kit::component::link::Link;
use gpui_kit::component::popover::Popover;
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::slider::SliderEvent;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme, Disableable, Icon, Selectable, Sizable, VirtualListScrollHandle, h_flex, h_virtual_list, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
  Anchor, AnyElement, AnyWindowHandle, App, AppContext, Bounds, ClickEvent, ContentMask, Context, Corners, CursorStyle, DispatchPhase, Entity, ExternalPaths,
  FontWeight, Hitbox, HitboxBehavior, Hsla, Image, ImageFormat, InteractiveElement, IntoElement, KeystrokeEvent, Modifiers, MouseButton, MouseDownEvent,
  MouseMoveEvent, MouseUpEvent, ParentElement, PathBuilder, Pixels, Point, Render, RenderImage, Rgba, ScrollDelta, ScrollStrategy, ScrollWheelEvent,
  SharedString, Size, StatefulInteractiveElement, Styled, Subscription, Task, TitlebarOptions, TouchPhase, Window, WindowBounds, WindowOptions, canvas, div,
  img, point, px, rgb, size, uniform_list,
};
use notify::Watcher as _;

use crate::Waker;
use crate::camera::controller::CameraController;
use crate::config::{self, Geometry};
use crate::document::Document;
use crate::render::{self, LinkTarget};
use crate::screen;
use crate::session::{Session, format_elapsed};
use crate::style::Colors;
use crate::update::{self, Release};

/// Which piece of the presentation a panel shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PaneKind {
  Current,
  Next,
  Notes,
}

/// Which page region a cached picture holds.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Region {
  Slide,
  Notes,
}

/// Where a slide is drawn: the zoomable, annotatable current slide (presenter or audience), a
/// plain preview pane, or a thumbnail in the strip.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Surface {
  Presenter,
  Audience,
  Pane,
  Thumb,
}

/// What the audience sees in place of the current slide.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cover {
  Off,
  /// Solid black (the presenter keeps seeing the slide).
  Black,
  /// A white page to draw on, shown on both screens.
  White,
}

/// The `strokes` key of the whiteboard (no PDF page has this number).
const BOARD: usize = usize::MAX;

/// What holding the mouse over the current slide does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InputMode {
  /// Show a laser-pointer dot.
  Pointer,
  /// Draw freehand lines.
  Draw,
}

/// A freehand line: points normalized (0..1) within the slide, plus a width factor
/// (multiplied by the slide's on-screen height to get pixels).
struct Stroke {
  points: Vec<Point<f32>>,
  width_factor: f32,
}

const MIN_FOOTER_FONT: f32 = 12.0;
const MAX_FOOTER_FONT: f32 = 72.0;
const MIN_POINTER_SIZE: f32 = 0.25;
const MAX_POINTER_SIZE: f32 = 4.0;
const POINTER_STEP: f32 = 0.25;
const NUM_LAYOUTS: usize = 4;
const MAX_RECENT: usize = 15;
/// Resizable bounds for the thumbnail strip panel (total height, in points).
const MIN_STRIP_H: f32 = 80.0;
const MAX_STRIP_H: f32 = 420.0;
/// Height of the slide number below each thumbnail.
const THUMB_LABEL_H: f32 = 18.0;
const SAVE_THROTTLE: Duration = Duration::from_millis(1200);
const MIN_ZOOM: f32 = 1.0;
const MAX_ZOOM: f32 = 8.0;
/// Cap on how much extra render resolution zoom requests (keeps large renders in check).
const MAX_ZOOM_RENDER: f32 = 3.0;
/// Absolute cap on a rendered slide's pixel width (avoids huge PDFium renders when zoomed).
const MAX_RENDER_WIDTH: u32 = 5000;
const APP_NAME: &str = "bella-presenter";

pub struct PresenterApp {
  document: Option<Document>,
  session: Session,
  cache: TextureCache,
  /// Separate, small-picture cache for the thumbnail strip, so scrolling it never evicts
  /// the big current/next/notes pictures from `cache`.
  thumb_cache: TextureCache,
  /// Whether the bottom thumbnail strip is shown (toggle with T).
  show_thumbnails: bool,
  /// Height of the thumbnail strip panel (drag its top edge to resize); persisted.
  thumb_height: f32,
  /// The slide the strip last auto-scrolled to follow (so it only re-centres on change).
  thumb_follow_last: usize,
  thumb_scroll: VirtualListScrollHandle,
  /// Pictures to render ahead while idle (page, region, pixel size), and the task doing it.
  prefetch: Vec<(usize, Region, [u32; 2])>,
  prefetch_task: Option<Task<()>>,
  /// Last window title pushed to the OS (so it is only set on change).
  title: String,
  footer_font: f32,
  /// Laser-pointer dot size factor (1.0 = default).
  pointer_size: f32,
  /// RGB colour of the pointer dot and freehand drawings (alpha stays fixed); persisted.
  pointer_color: [u8; 3],
  color_picker: Entity<ColorPickerState>,
  /// UI theme preference (system / dark / light); persisted.
  theme: config::Theme,
  layout_index: usize,
  presenting: bool,
  /// Black screen or whiteboard instead of the slide (B / ,).
  cover: Cover,
  pub(crate) camera: CameraController,
  window_geometry: Option<Geometry>,
  recent: Vec<config::RecentEntry>,
  /// The presenter window (it moves to another window when the screens are swapped).
  presenter: AnyWindowHandle,
  /// The audience window while presenting.
  audience: Option<AnyWindowHandle>,
  /// When true, presenter and audience windows are swapped onto the opposite monitors
  /// (for when the OS reports the wrong primary display). Session-only.
  screens_swapped: bool,
  /// A screen swap while presenting, staged so each macOS window transition finishes first.
  swap_task: Option<Task<()>>,
  show_shortcuts: bool,
  show_about: bool,
  show_thirdparty: bool,
  show_release_notes: bool,
  /// The outcome of an update check started from the About card (up to date, or why it failed).
  update_note: Option<String>,
  /// A file that failed to open or reload, shown until dismissed.
  error: Option<String>,
  /// The update check at start and the offer it leads to.
  update: UpdateStep,
  /// A release version the user chose to skip (persisted).
  skipped_update: String,
  /// Pointer vs. drawing (toggle with P).
  mode: InputMode,
  /// Laser-pointer position, normalized (0..1) within the slide, while the mouse
  /// button is held over the current slide. `None` = no pointer shown.
  pointer_norm: Option<Point<f32>>,
  /// Freehand annotations per page (kept while navigating; cleared when the file closes).
  strokes: HashMap<usize, Vec<Stroke>>,
  /// Whether a stroke is currently being drawn.
  drawing: bool,
  /// The slide surface the left mouse button was pressed on, while it is held.
  pressed: Option<Surface>,
  /// Each page's slide links (looked up once), the surface with a link under the mouse, and where
  /// and when the current press began (a short press that stays put is a click).
  links: HashMap<usize, Rc<Vec<render::Link>>>,
  link_hover: Option<Surface>,
  press_start: Option<(Point<f32>, Instant)>,
  /// The previous mouse position of a held press (for Ctrl+drag panning).
  drag_last: Option<Point<f32>>,
  /// The slide number typed so far (go to it with Enter).
  goto_input: String,
  /// Accumulated mouse-wheel delta, for scroll-to-navigate.
  scroll_accum: f32,
  /// Whether the current trackpad swipe already turned a slide, and when the last scroll came.
  swipe_turned: bool,
  last_scroll: Instant,
  /// Zoom factor of the current slide (1.0 = fit); centre of the visible window (slide coords).
  zoom: f32,
  zoom_center: Point<f32>,
  /// The page the current zoom applies to (zoom resets when the page changes).
  zoom_page: usize,
  /// Where each window shows the current slide (for zoom-at-cursor).
  presenter_view: Option<SlideView>,
  audience_view: Option<SlideView>,
  /// Faint logo shown on the start screen and the full-color one of the About window.
  logo: (Arc<Image>, f32),
  about_logo: (Arc<Image>, f32),
  /// Reload the document when its file changes on disk (persisted).
  hot_reload: bool,
  /// Set by the file watcher thread when the open PDF changed; consumed on the UI thread.
  reload_flag: Arc<AtomicBool>,
  /// Keeps the OS file watcher alive; dropping it stops watching.
  watcher: Option<notify::RecommendedWatcher>,
  /// When a reload is due (debounce/retry after a file change); `None` when idle.
  reload_due: Option<Instant>,
  /// Consecutive failed reload attempts (the file may still be mid-write); bounded.
  reload_tries: u32,
  waker: Waker,
  /// The next timed wake-up (clock, timer, retries, throttled save).
  wake_at: Option<(Instant, Task<()>)>,
  /// Persistence bookkeeping.
  pub(crate) dirty: bool,
  last_save: Instant,
  _subscriptions: Vec<Subscription>,
  /// Subscriptions on the current presenter window.
  _window_subscriptions: Vec<Subscription>,
}

impl PresenterApp {
  /// Opens the presenter window on the primary monitor and wires the app around it: wake-ups from
  /// worker threads, files the OS hands over (`opened`), and quitting when the window closes.
  pub fn open_window(initial: Option<PathBuf>, mut opened: UnboundedReceiver<PathBuf>, cx: &mut App) {
    let st = config::State::load();
    let primary = screen::primary(cx);
    let size = st.window_geometry.map_or(size(px(1100.0), px(720.0)), |g| size(px(g.w), px(g.h)));
    // Restore the saved position only if it is on the primary monitor; otherwise center the window there.
    let bounds = match (st.window_geometry, &primary) {
      (Some(g), Some(p)) if p.bounds().contains(&point(px(g.x), px(g.y))) => Bounds::new(point(px(g.x), px(g.y)), size),
      _ => Bounds::centered(primary.as_ref().map(|d| d.id()), size, cx),
    };
    let options = window_options(APP_NAME, WindowBounds::Windowed(bounds), primary.map(|d| d.id()));
    let (waker, mut wakes) = Waker::new();
    let (frames, mut frame_wakes) = Waker::new();
    let opened_window = gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| Self::new(st, waker, frames, window, cx)));
    let (handle, app) = match opened_window {
      Ok(opened) => opened,
      Err(e) => {
        tracing::error!("cannot open the presenter window: {e:#}");
        cx.quit();
        return;
      }
    };
    app.update(cx, |app, cx| {
      app.presenter = handle;
      if let Some(path) = initial {
        app.open(&path, cx);
      }
    });
    let weak = app.downgrade();
    cx.spawn(async move |cx| {
      while wakes.next().await.is_some() {
        if weak.update(cx, |_, cx| cx.notify()).is_err() {
          break;
        }
      }
    })
    .detach();
    let weak = app.downgrade();
    cx.spawn(async move |cx| {
      while frame_wakes.next().await.is_some() {
        if weak.update(cx, |app, cx| app.camera_frame(cx)).is_err() {
          break;
        }
      }
    })
    .detach();
    let opener = app.clone();
    cx.spawn(async move |cx| {
      while let Some(path) = opened.next().await {
        opener.update(cx, |app, cx| app.open(&path, cx));
      }
    })
    .detach();
    // Closing the presenter window ends the app; the audience window only ends the presentation.
    cx.on_window_closed(move |cx, closed| {
      if app.read(cx).presenter.window_id() == closed {
        cx.quit();
      }
    })
    .detach();
  }

  fn new(st: config::State, waker: Waker, frames: Waker, window: &mut Window, cx: &mut Context<Self>) -> Self {
    // Prune recent entries whose file no longer exists on disk (moved/deleted).
    let mut recent = st.recent;
    let had = recent.len();
    recent.retain(|e| e.path.is_file());
    let pruned = recent.len() != had;

    let color_picker = cx.new(|cx| ColorPickerState::new(window, cx).default_value(color(st.pointer_color, 255)));
    let camera = CameraController::new(st.camera, waker.clone(), frames, cx);
    let this = cx.entity().downgrade();
    let subscriptions = vec![
      // Shortcuts work in every window: also over a fullscreen audience window on a single screen.
      cx.intercept_keystrokes(move |event, _, cx| {
        let _ = this.update(cx, |app, cx| app.on_keystroke(event, cx));
      }),
      cx.subscribe(&camera.size_slider, |app, _, event: &SliderEvent, cx| {
        if let SliderEvent::Change(value) = event {
          app.camera.settings.size = value.start();
          app.dirty = true;
          cx.notify();
        }
      }),
      cx.subscribe(&color_picker, |app, _, event: &ColorPickerEvent, cx| {
        if let ColorPickerEvent::Change(Some(c)) = event {
          let c = Rgba::from(*c);
          app.pointer_color = [c.r, c.g, c.b].map(|v| (v * 255.0).round() as u8);
          app.dirty = true;
          cx.notify();
        }
      }),
      cx.on_app_quit(|app, _| {
        app.save_now();
        async {}
      }),
    ];
    let mut app = Self {
      document: None,
      session: Session::new(0),
      cache: TextureCache::new(512 << 20),
      thumb_cache: TextureCache::new(64 << 20),
      show_thumbnails: st.show_thumbnails,
      thumb_height: st.thumb_height.clamp(MIN_STRIP_H, MAX_STRIP_H),
      thumb_follow_last: usize::MAX,
      thumb_scroll: VirtualListScrollHandle::new(),
      prefetch: Vec::new(),
      prefetch_task: None,
      title: String::new(),
      footer_font: st.footer_font,
      pointer_size: st.pointer_size.clamp(MIN_POINTER_SIZE, MAX_POINTER_SIZE),
      pointer_color: st.pointer_color,
      color_picker,
      theme: st.theme,
      layout_index: st.layout_index % NUM_LAYOUTS,
      presenting: false,
      cover: Cover::Off,
      camera,
      window_geometry: st.window_geometry,
      recent,
      presenter: window.window_handle(),
      audience: None,
      screens_swapped: false,
      swap_task: None,
      show_shortcuts: false,
      show_about: false,
      show_thirdparty: false,
      show_release_notes: false,
      update_note: None,
      error: None,
      update: UpdateStep::Checking(in_background(&waker, update::check), false),
      skipped_update: st.skipped_update,
      mode: InputMode::Pointer,
      pointer_norm: None,
      strokes: HashMap::new(),
      drawing: false,
      pressed: None,
      drag_last: None,
      links: HashMap::new(),
      link_hover: None,
      press_start: None,
      goto_input: String::new(),
      scroll_accum: 0.0,
      swipe_turned: false,
      last_scroll: Instant::now(),
      zoom: 1.0,
      zoom_center: SLIDE_CENTER,
      zoom_page: 0,
      presenter_view: None,
      audience_view: None,
      logo: logo(include_bytes!("../img/logo-gray.png")),
      about_logo: logo(include_bytes!("../img/logo.png")),
      hot_reload: st.hot_reload,
      reload_flag: Arc::new(AtomicBool::new(false)),
      watcher: None,
      reload_due: None,
      reload_tries: 0,
      waker,
      wake_at: None,
      dirty: pruned,
      last_save: Instant::now(),
      _subscriptions: subscriptions,
      _window_subscriptions: Vec::new(),
    };
    app.attach_window(window, cx);
    app.apply_theme(window, cx);
    // Persist the pruned list right away so the removals stick even if nothing else changes.
    if pruned {
      app.save_now();
    }
    app
  }

  /// Follow the presenter window's geometry (persisted) and the system appearance.
  fn attach_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    self.title.clear();
    self._window_subscriptions = vec![
      cx.observe_window_bounds(window, |app, window, _| app.capture_window_geometry(window)),
      cx.observe_window_appearance(window, |app, window, cx| {
        if app.theme == config::Theme::System {
          app.apply_theme(window, cx);
        }
      }),
    ];
  }

  fn apply_theme(&self, window: &mut Window, cx: &mut App) {
    crate::style::apply(self.theme, window, cx);
  }

  fn open(&mut self, path: &Path, cx: &mut Context<Self>) {
    // Absolute, so the watcher has a folder and the recent entry works from any working directory.
    let path = &std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    match Document::open(path) {
      Ok(doc) => {
        self.session = Session::new(doc.page_count());
        if let Some(page) = self.recent_page(path) {
          self.session.goto(page as isize);
        }
        self.document = Some(doc);
        self.cover = Cover::Off;
        self.reset_view();
        self.reload_due = None;
        self.error = None;
        self.update_watcher(); // point the file watcher at the new file
        self.record_recent(path);
        self.save_now(); // persist the new recent entry immediately
      }
      Err(e) => {
        tracing::error!("failed to open {}: {e:#}", path.display());
        self.error = Some(format!("Cannot open {}:\n{e:#}", path.display()));
      }
    }
    cx.notify();
  }

  /// Forget what belongs to the previous document or version: pictures, drawings, pointer and zoom.
  fn reset_view(&mut self) {
    self.prefetch.clear();
    self.links.clear();
    self.link_hover = None;
    self.cache.clear();
    self.thumb_cache.clear();
    self.thumb_follow_last = usize::MAX;
    self.strokes.clear();
    self.drawing = false;
    self.pressed = None;
    self.pointer_norm = None;
    self.reset_zoom();
    self.zoom_page = self.session.current();
  }

  fn recent_page(&self, path: &Path) -> Option<usize> {
    self.recent.iter().find(|e| e.path == path).map(|e| e.page)
  }

  /// Move `path` to the front of the recent list, keeping its last page.
  fn record_recent(&mut self, path: &Path) {
    let page = self.session.current();
    let path = path.to_path_buf();
    self.recent.retain(|e| e.path != path);
    self.recent.insert(0, config::RecentEntry { path, page });
    self.recent.truncate(MAX_RECENT);
    self.dirty = true;
  }

  /// Remove one file from the recent list (the × on the start screen), and persist it.
  fn remove_recent(&mut self, path: &Path) {
    let before = self.recent.len();
    self.recent.retain(|e| e.path != path);
    if self.recent.len() != before {
      self.dirty = true;
      self.save_now();
    }
  }

  /// Keep the current file's recent entry pointing at the current page.
  fn sync_recent_page(&mut self) {
    let Some(path) = self.document.as_ref().map(|d| d.path().to_path_buf()) else {
      return;
    };
    let page = self.session.current();
    if let Some(entry) = self.recent.iter_mut().find(|e| e.path == path)
      && entry.page != page
    {
      entry.page = page;
      self.dirty = true;
    }
  }

  fn snapshot(&self) -> config::State {
    config::State {
      footer_font: self.footer_font,
      pointer_size: self.pointer_size,
      layout_index: self.layout_index,
      window_geometry: self.window_geometry,
      recent: self.recent.clone(),
      show_thumbnails: self.show_thumbnails,
      thumb_height: self.thumb_height,
      pointer_color: self.pointer_color,
      theme: self.theme,
      hot_reload: self.hot_reload,
      camera: self.camera.settings.clone(),
      skipped_update: self.skipped_update.clone(),
    }
  }

  fn save_now(&mut self) {
    self.sync_recent_page();
    self.snapshot().save();
    self.last_save = Instant::now();
    self.dirty = false;
  }

  fn pick_file(&mut self, cx: &mut Context<Self>) {
    // The dialog runs its own event loop: show it outside any UI update.
    cx.spawn(async move |app, cx| {
      if let Some(path) = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).pick_file() {
        let _ = app.update(cx, |app, cx| app.open(&path, cx));
      }
    })
    .detach();
  }

  fn adjust_font(&mut self, delta: f32) {
    self.footer_font = (self.footer_font + delta).clamp(MIN_FOOTER_FONT, MAX_FOOTER_FONT);
    self.dirty = true;
  }

  fn adjust_pointer(&mut self, delta: f32) {
    self.pointer_size = (self.pointer_size + delta).clamp(MIN_POINTER_SIZE, MAX_POINTER_SIZE);
    self.dirty = true;
  }

  /// Toggle between laser-pointer and drawing.
  fn toggle_mode(&mut self) {
    self.mode = match self.mode {
      InputMode::Pointer => InputMode::Draw,
      InputMode::Draw => InputMode::Pointer,
    };
    self.drawing = false;
    self.pointer_norm = None;
  }

  /// Remove the freehand drawings on the current slide (or the whiteboard).
  fn clear_drawings(&mut self) {
    let ink = self.ink_page();
    self.strokes.remove(&ink);
    self.drawing = false;
  }

  /// Where drawings go now: the whiteboard while it is up, else the current page.
  fn ink_page(&self) -> usize {
    if self.cover == Cover::White { BOARD } else { self.session.current() }
  }

  /// Turn `cover` on, or off again if it is already up. The whiteboard starts in draw mode.
  fn toggle_cover(&mut self, cover: Cover) {
    self.cover = if self.cover == cover { Cover::Off } else { cover };
    self.drawing = false;
    if self.cover == Cover::White {
      self.mode = InputMode::Draw;
      self.pointer_norm = None;
    }
  }

  fn toggle_thumbnails(&mut self) {
    self.show_thumbnails = !self.show_thumbnails;
    self.dirty = true;
  }

  fn toggle_camera(&mut self) {
    self.camera.settings.enabled = !self.camera.settings.enabled;
    self.dirty = true;
  }

  fn reset_zoom(&mut self) {
    self.zoom = 1.0;
    self.zoom_center = SLIDE_CENTER;
  }

  /// Zoom is per-slide: reset it when the page changes.
  fn sync_zoom_page(&mut self) {
    let cur = self.session.current();
    if cur != self.zoom_page {
      self.reset_zoom();
      self.zoom_page = cur;
    }
  }

  /// Multiply the zoom by `factor`, keeping the slide point under `cursor` fixed
  /// (zoom-to-cursor). `view` is where the slide the cursor is over is shown (the presenter's
  /// or the audience's), used as the anchor. Falls back to zooming about the centre.
  fn apply_zoom(&mut self, factor: f32, cursor: Option<Point<f32>>, view: Option<SlideView>) {
    let new_z = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
    if (new_z - self.zoom).abs() < f32::EPSILON {
      return;
    }
    if let Some(view) = view {
      let (uv, rect) = view.zoomed(self.zoom, self.zoom_center);
      self.zoom_center = match cursor.filter(|c| rect.contains(c)) {
        Some(c) => view.center_keeping(new_z, screen_to_slide(rect, uv, c), c),
        None => uv.center(),
      };
    }
    self.zoom = new_z;
    if self.zoom <= MIN_ZOOM + f32::EPSILON {
      self.reset_zoom();
    }
  }

  fn flip_layout(&mut self) {
    self.layout_index = (self.layout_index + 1) % NUM_LAYOUTS;
    self.dirty = true;
  }

  /// Open the audience window fullscreen: on the second screen if present, otherwise on the
  /// current (primary) screen, covering the presenter. Shortcuts keep working over it.
  fn start_presentation(&mut self, cx: &mut Context<Self>) {
    if self.document.is_none() || self.presenting || self.audience.is_some() {
      return;
    }
    self.presenting = true;
    // The talk timer runs while presenting; starting/resuming here, pausing on quit.
    self.session.timer_mut().start();
    let monitor = screen::audience_monitor(self.screens_swapped, cx);
    let bounds = monitor
      .as_ref()
      .map_or_else(|| Bounds::centered(None, size(px(960.0), px(540.0)), cx), |m| m.bounds());
    let options = window_options("Presentation", WindowBounds::Fullscreen(bounds), monitor.map(|m| m.id()));
    let app = cx.entity();
    cx.defer(move |cx| {
      let opened = gpui_kit::open_window(options, cx, |window, cx| {
        let closing = app.clone();
        window.on_window_should_close(cx, move |window, cx| {
          let id = window.window_handle().window_id();
          closing.update(cx, |app, cx| {
            // The OS closes this window itself: only end the presentation it belongs to.
            if app.audience.is_some_and(|a| a.window_id() == id) {
              app.audience = None;
              app.stop_presentation(cx);
            }
          });
          true
        });
        cx.new(|cx| AudienceView {
          _changes: cx.observe(&app, |_, _, cx| cx.notify()),
          app: app.clone(),
        })
      });
      match opened {
        Ok((handle, _)) if app.read(cx).presenting => app.update(cx, |app, _| app.audience = Some(handle)),
        Ok((handle, _)) => {
          let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
        Err(e) => {
          tracing::error!("cannot open the audience window: {e:#}");
          app.update(cx, |app, cx| app.stop_presentation(cx));
        }
      }
    });
  }

  /// Quit the presentation (close the audience window) and pause the talk timer.
  fn stop_presentation(&mut self, cx: &mut Context<Self>) {
    self.presenting = false;
    // Cancel a pending swap step (swap_screens calls this first, then schedules its own).
    self.swap_task = None;
    self.session.timer_mut().pause();
    if let Some(handle) = self.audience.take() {
      cx.defer(move |cx| {
        let _ = handle.update(cx, |_, window, _| window.remove_window());
      });
    }
    cx.notify();
  }

  /// Swap which physical monitor shows the presenter vs. the audience window, for when the OS
  /// reports the wrong primary display. Moves the presenter window onto its new monitor and, if
  /// presenting, re-places the audience (fullscreen) on the other. Needs a second screen.
  fn swap_screens(&mut self, cx: &mut Context<Self>) {
    if self.swap_task.is_some() {
      return; // a swap is still under way
    }
    if screen::external(cx).is_none() {
      tracing::info!("swapping screens needs a second display");
      return;
    }
    self.screens_swapped = !self.screens_swapped;
    if self.presenting {
      // Stage it so each macOS window transition finishes before the next: quit the presentation
      // (closes the audience fullscreen Space), then move the presenter once that Space has closed,
      // then re-present on the swapped monitor. Doing these back-to-back races the Space transitions
      // and leaves a screen stuck black.
      self.stop_presentation(cx);
      self.swap_task = Some(cx.spawn(async move |app, cx| {
        cx.background_executor().timer(Duration::from_millis(700)).await;
        let _ = app.update(cx, |app, cx| app.move_presenter(cx));
        cx.background_executor().timer(Duration::from_millis(800)).await;
        let _ = app.update(cx, |app, cx| {
          // Dropping the task from within its last step is fine: it has nothing left to run.
          app.swap_task = None;
          app.start_presentation(cx);
        });
      }));
    } else {
      self.move_presenter(cx);
    }
  }

  /// Move the presenter onto its target monitor, maximised there: GPUI cannot move a window, so
  /// the presenter reopens there and the old window closes.
  fn move_presenter(&mut self, cx: &mut Context<Self>) {
    let Some(m) = screen::presenter_monitor(self.screens_swapped, cx) else {
      return;
    };
    let options = window_options(APP_NAME, WindowBounds::Maximized(m.bounds()), Some(m.id()));
    let (app, old) = (cx.entity(), self.presenter);
    cx.defer(move |cx| {
      let opened = gpui_kit::open_window(options, cx, |window, cx| {
        app.update(cx, |app, cx| app.attach_window(window, cx));
        app.clone()
      });
      match opened {
        Ok((handle, _)) => {
          app.update(cx, |app, _| app.presenter = handle);
          let _ = old.update(cx, |_, window, _| window.remove_window());
        }
        Err(e) => tracing::error!("cannot move the presenter window: {e:#}"),
      }
    });
  }

  /// Close the current document and return to the start screen.
  fn close_file(&mut self, cx: &mut Context<Self>) {
    self.save_now(); // persist last page before closing
    self.stop_presentation(cx);
    self.document = None;
    self.cover = Cover::Off;
    self.mode = InputMode::Pointer;
    self.session = Session::new(0);
    self.reset_view();
    self.reload_due = None;
    self.update_watcher(); // drop the file watcher (no document)
  }

  /// Rebuild the OS file watcher for the open document (or drop it). notify recommends watching
  /// the parent directory and filtering by filename, so editor/compiler rename-replace writes are
  /// still seen. The watcher thread sets `reload_flag` and wakes the UI on a matching change.
  fn update_watcher(&mut self) {
    self.watcher = None; // stop any previous watch
    if !self.hot_reload {
      return;
    }
    let Some(path) = self.document.as_ref().map(|d| d.path().to_path_buf()) else {
      return;
    };
    let (Some(dir), Some(name)) = (path.parent().map(Path::to_path_buf), path.file_name().map(std::ffi::OsString::from)) else {
      return;
    };
    let flag = self.reload_flag.clone();
    let waker = self.waker.clone();
    let handler = move |res: notify::Result<notify::Event>| {
      if let Ok(ev) = res
        && ev.paths.iter().any(|p| p.file_name() == Some(name.as_os_str()))
      {
        flag.store(true, Ordering::Relaxed);
        waker.wake();
      }
    };
    match notify::RecommendedWatcher::new(handler, notify::Config::default()) {
      Ok(mut w) => match w.watch(&dir, notify::RecursiveMode::NonRecursive) {
        Ok(()) => self.watcher = Some(w),
        Err(e) => tracing::warn!("hot reload: watching {} failed: {e}", dir.display()),
      },
      Err(e) => tracing::warn!("hot reload: watcher init failed: {e}"),
    }
  }

  /// Reload the open document from disk, preserving the current slide (clamped) and the timer.
  /// On failure (the file may still be mid-write) schedules a bounded retry.
  fn do_reload(&mut self, cx: &mut Context<Self>) {
    let Some(path) = self.document.as_ref().map(|d| d.path().to_path_buf()) else {
      return;
    };
    match Document::open(&path) {
      Ok(doc) => {
        self.session.set_page_count(doc.page_count());
        self.document = Some(doc);
        self.reset_view();
        self.reload_tries = 0;
        tracing::info!("reloaded {}", path.display());
      }
      Err(e) => {
        self.reload_tries += 1;
        if self.reload_tries < 12 {
          // File is likely still being written; retry shortly.
          self.reload_due = Some(Instant::now() + Duration::from_millis(400));
          self.wake_after(Duration::from_millis(400), cx);
        } else {
          tracing::warn!("hot reload failed after {} tries: {e:#}", self.reload_tries);
          self.error = Some(format!("Cannot reload {}, still showing the previous version:\n{e:#}", path.display()));
          self.reload_tries = 0;
        }
      }
    }
  }

  /// A camera frame arrived. Only the audience window shows the overlay, so only it redraws (which
  /// also takes the frame, see `tick`); the presenter window too while it previews the camera.
  fn camera_frame(&mut self, cx: &mut Context<Self>) {
    match self.audience {
      Some(audience) if !self.camera.previewing() => {
        let _ = audience.update(cx, |_, window, _| window.refresh());
      }
      _ => cx.notify(),
    }
  }

  /// Render again after `after`, unless an earlier wake-up is already scheduled.
  fn wake_after(&mut self, after: Duration, cx: &mut Context<Self>) {
    let at = Instant::now() + after;
    if self.wake_at.as_ref().is_some_and(|(t, _)| *t <= at) {
      return;
    }
    let task = cx.spawn(async move |app, cx| {
      cx.background_executor().timer(after).await;
      let _ = app.update(cx, |app, cx| {
        app.wake_at = None;
        cx.notify();
      });
    });
    self.wake_at = Some((at, task));
  }

  /// Keyboard shortcuts, for every window of the app (key events go to the focused window only,
  /// so they never trigger twice). Keys typed into a text field are left alone.
  fn on_keystroke(&mut self, event: &KeystrokeEvent, cx: &mut Context<Self>) {
    if event.context_stack.iter().any(|c| c.contains("Input")) {
      return;
    }
    let keystroke = &event.keystroke;
    if keystroke.modifiers.platform || keystroke.modifiers.control {
      if keystroke.modifiers.platform && keystroke.key == "q" {
        cx.quit();
      }
      return;
    }
    // Go to a slide: type its number, then Enter (Esc cancels, Backspace corrects).
    let key = keystroke.key.as_str();
    if key.len() == 1 && key.as_bytes()[0].is_ascii_digit() {
      if self.goto_input.len() < 5 {
        self.goto_input.push_str(key);
      }
      cx.stop_propagation();
      cx.notify();
      return;
    }
    if !self.goto_input.is_empty() {
      match key {
        "enter" => {
          if let Ok(n) = self.goto_input.parse::<isize>() {
            self.session.goto(n - 1);
          }
          self.goto_input.clear();
        }
        "backspace" => {
          self.goto_input.pop();
        }
        // Any other key drops the number; Esc does only that, not quit the presentation.
        "escape" => self.goto_input.clear(),
        _ => {
          self.goto_input.clear();
          return self.on_keystroke(event, cx);
        }
      }
      cx.stop_propagation();
      cx.notify();
      return;
    }
    match key {
      "right" | "space" | "pagedown" | "down" => self.session.next(),
      "left" | "pageup" | "up" => self.session.prev(),
      "home" => self.session.first(),
      "end" => self.session.last(),
      "r" => self.session.timer_mut().reset(),
      "k" => self.session.timer_mut().toggle(),
      "l" => self.flip_layout(),
      "+" | "=" => self.adjust_font(2.0),
      "-" => self.adjust_font(-2.0),
      "f5" => self.start_presentation(cx),
      "escape" => {
        // Not swallowed: Esc also closes an open popover.
        self.stop_presentation(cx);
        return;
      }
      "b" => self.toggle_cover(Cover::Black),
      "," => self.toggle_cover(Cover::White),
      "c" => self.toggle_camera(),
      "p" => self.toggle_mode(),
      "d" => self.clear_drawings(),
      "t" => self.toggle_thumbnails(),
      "s" => self.swap_screens(cx),
      // A stray W must not end a talk: while presenting, close with the header button.
      "w" if !self.presenting => self.close_file(cx),
      "o" => self.pick_file(cx),
      _ => return,
    }
    cx.stop_propagation();
    cx.notify();
  }

  /// Mouse-wheel navigation and Ctrl/Cmd+scroll zoom, for either window. `anchor` is where that
  /// window shows the current slide (the zoom-to-cursor anchor).
  ///
  /// Plain scroll navigates slides (down = next, up = previous). A real wheel reports
  /// discrete line notches — one slide each, by sign. Trackpad pixel scrolling accumulates
  /// against a threshold.
  fn handle_wheel(&mut self, event: &ScrollWheelEvent, anchor: Option<SlideView>, cx: &mut Context<Self>) {
    if self.document.is_none() {
      self.scroll_accum = 0.0;
      return;
    }
    const POINT_STEP: f32 = 40.0;
    let dy = match event.delta {
      ScrollDelta::Lines(d) => d.y,
      ScrollDelta::Pixels(d) => f32::from(d.y),
    };
    let lines = matches!(event.delta, ScrollDelta::Lines(_));
    if crate::platform::zoom_modifier(event.modifiers) {
      let factor = match (lines, dy) {
        (true, d) if d > 0.0 => 1.25,
        (true, d) if d < 0.0 => 0.8,
        (true, _) => 1.0,
        (false, d) => (d * 0.004).exp(),
      };
      self.apply_zoom(factor, Some(to_f32(event.position)), anchor);
    } else if lines {
      if dy < 0.0 {
        self.session.next();
      } else if dy > 0.0 {
        self.session.prev();
      }
    } else {
      // One slide per trackpad swipe, however long, so its momentum does not skip slides. A new
      // touch or a pause starts the next swipe.
      if event.touch_phase == TouchPhase::Started || self.last_scroll.elapsed() > Duration::from_millis(250) {
        self.scroll_accum = 0.0;
        self.swipe_turned = false;
      }
      self.last_scroll = Instant::now();
      if !self.swipe_turned {
        self.scroll_accum += dy;
        if self.scroll_accum <= -POINT_STEP {
          self.session.next();
          self.swipe_turned = true;
        } else if self.scroll_accum >= POINT_STEP {
          self.session.prev();
          self.swipe_turned = true;
        }
      }
    }
    cx.notify();
  }

  /// The presenter window's title carries the open file and its page count (kept out of the header).
  fn update_title(&mut self, window: &mut Window) {
    let want_title = match self.document.as_ref() {
      Some(doc) => format!(
        "{APP_NAME} - {} · {} pages",
        doc.path().file_name().unwrap_or_default().to_string_lossy(),
        doc.page_count()
      ),
      None => APP_NAME.to_owned(),
    };
    if want_title != self.title {
      window.set_window_title(&want_title);
      self.title = want_title;
    }
  }

  /// Timers, background results and reloads, checked whenever either window renders: macOS stops
  /// drawing a covered presenter window (a fullscreen audience on the same screen), and the
  /// camera, clock and reloads must keep going then.
  fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    self.sync_zoom_page();

    // Hot reload: reload when the watcher reported a change (debounced via a short due time +
    // bounded retry for mid-write).
    if self.reload_flag.swap(false, Ordering::Relaxed) {
      self.reload_due = Some(Instant::now());
      self.reload_tries = 0;
    }
    if let Some(t) = self.reload_due {
      let now = Instant::now();
      if now >= t {
        self.reload_due = None;
        self.do_reload(cx);
      } else {
        self.wake_after(t - now, cx);
      }
    }
    self.poll_update(cx);
    if let Some(after) = self.camera.update(self.presenting, window, cx) {
      self.wake_after(after, cx);
    }
    if self.document.is_some() {
      self.sync_recent_page();
      // Keep the wall clock (and running timer) current while idle: one redraw just after each
      // full second of the clock, the only time the footer changes.
      let into_second = u64::from(chrono::Local::now().timestamp_subsec_millis().min(999));
      self.wake_after(Duration::from_millis(1005 - into_second), cx);
    }
    // Throttled persistence; schedule a wake so a change flushes even while idle.
    if self.dirty && self.last_save.elapsed() >= SAVE_THROTTLE {
      self.save_now();
    }
    if self.dirty {
      self.wake_after(SAVE_THROTTLE + Duration::from_millis(100), cx);
    }
  }

  /// Picks up the background update check and install. Installing ends with a restart.
  fn poll_update(&mut self, cx: &mut Context<Self>) {
    match &self.update {
      // A check asked for in the About card also offers a skipped version and reports its outcome.
      &UpdateStep::Checking(ref rx, manual) => {
        let note = match rx.try_recv() {
          Ok(Ok(Some(release))) if manual || release.version != self.skipped_update => {
            self.update = UpdateStep::Offer(release);
            None
          }
          Ok(Ok(_)) => Some(format!("{APP_NAME} {} is the latest version.", env!("CARGO_PKG_VERSION"))),
          Ok(Err(error)) => {
            tracing::info!(%error, "update check failed");
            Some(format!("Update check failed: {error}"))
          }
          Err(mpsc::TryRecvError::Disconnected) => Some("The update check stopped.".to_owned()),
          Err(mpsc::TryRecvError::Empty) => return,
        };
        if !matches!(self.update, UpdateStep::Offer(_)) {
          self.update = UpdateStep::Idle;
        }
        self.update_note = note.filter(|_| manual);
      }
      UpdateStep::Installing(_, rx) => match rx.try_recv() {
        Ok(Ok(program)) => {
          self.save_now();
          crate::platform::relaunch(&program);
          self.update = UpdateStep::Idle;
          cx.quit();
        }
        Ok(Err(error)) => self.update = UpdateStep::Failed(error),
        Err(mpsc::TryRecvError::Disconnected) => self.update = UpdateStep::Failed("the installer stopped".to_string()),
        Err(mpsc::TryRecvError::Empty) => {}
      },
      UpdateStep::Idle | UpdateStep::Offer(_) | UpdateStep::Failed(_) => {}
    }
  }

  /// Track the presenter window's geometry for persistence.
  fn capture_window_geometry(&mut self, window: &Window) {
    let b = window.window_bounds().get_bounds();
    let g = Geometry {
      x: b.origin.x.into(),
      y: b.origin.y.into(),
      w: b.size.width.into(),
      h: b.size.height.into(),
    };
    if geom_changed(self.window_geometry, g) {
      self.window_geometry = Some(g);
      self.dirty = true;
    }
  }

  fn header(&mut self, cx: &mut Context<Self>) -> AnyElement {
    let c = Colors::current(cx);
    let separator = || div().w(px(1.0)).h(px(16.0)).mx_1().bg(c.border);
    // Icons only: the tooltip names the action and its key.
    let button = |id: &'static str, icon: Lucide, tooltip: &'static str, action: fn(&mut Self, &mut Context<Self>)| {
      Button::new(id)
        .icon(Icon::new(icon))
        .ghost()
        .xsmall()
        .tooltip(tooltip)
        .on_click(cx.listener(move |app, _: &ClickEvent, _, cx| {
          action(app, cx);
          cx.notify();
        }))
    };

    let mut left = h_flex().flex_1().flex_wrap().gap_1().items_center();
    // ── General ──
    left = left.child(button("open", Lucide::FolderOpen, "Open a PDF (O)", |app, cx| app.pick_file(cx)));
    if self.document.is_some() {
      left = left
        .child(button("close", Lucide::X, "Close the file (W)", |app, cx| app.close_file(cx)))
        .child(if self.presenting {
          button("quit", Lucide::CircleStop, "Quit the presentation (Esc)", |app, cx| app.stop_presentation(cx))
        } else {
          button("present", Lucide::Play, "Start the presentation (F5)", |app, cx| app.start_presentation(cx))
        })
        .child(
          button(
            "blank",
            Lucide::Square,
            if self.cover == Cover::Black {
              "Show the slide again (B)"
            } else {
              "Blank the audience screen (B)"
            },
            |app, _| app.toggle_cover(Cover::Black),
          )
          .selected(self.cover == Cover::Black),
        )
        .child(
          button(
            "board",
            Lucide::Presentation,
            if self.cover == Cover::White {
              "Show the slide again (,)"
            } else {
              "Show a whiteboard to draw on (,)"
            },
            |app, _| app.toggle_cover(Cover::White),
          )
          .selected(self.cover == Cover::White),
        )
        .child(button(
          "swap",
          Lucide::ArrowLeftRight,
          "Swap the presenter and audience screens (S)",
          |app, cx| app.swap_screens(cx),
        ))
        // ── Display ──
        .child(separator())
        .child(button("layout", Lucide::LayoutDashboard, "Cycle the panel layout (L)", |app, _| {
          app.flip_layout()
        }))
        .child(
          button("thumbs", Lucide::PanelBottom, "Show / hide the thumbnail strip (T)", |app, _| {
            app.toggle_thumbnails()
          })
          .selected(self.show_thumbnails),
        )
        // ── Drawing: toggle always; colour + delete appear only while in draw mode ──
        .child(separator())
        .child(match self.mode {
          InputMode::Pointer => button("mode", Lucide::Pencil, "Draw on the slide (P)", |app, _| app.toggle_mode()),
          InputMode::Draw => button("mode", Lucide::MousePointer, "Back to the laser pointer (P)", |app, _| app.toggle_mode()),
        });
      if self.mode == InputMode::Draw {
        left = left
          .child(
            div()
              .id("pointer-color")
              .tooltip(|window, cx| Tooltip::new("Pointer & drawing colour").build(window, cx))
              .child(ColorPicker::new(&self.color_picker).xsmall()),
          )
          .child(button("clear", Lucide::Eraser, "Delete the drawings on this slide (D)", |app, _| {
            app.clear_drawings()
          }));
      }
    }

    // Theme toggle: clicking cycles system -> dark -> light.
    let (theme_icon, theme_tooltip) = match self.theme {
      config::Theme::System => (Lucide::Laptop, "Theme: system (click to cycle)"),
      config::Theme::Dark => (Lucide::Moon, "Theme: dark (click to cycle)"),
      config::Theme::Light => (Lucide::Sun, "Theme: light (click to cycle)"),
    };
    // Settings popover (footer size, pointer size, hot reload, camera): first of the right-hand
    // icons, opening leftwards so it stays on screen.
    let settings = {
      let app = cx.entity();
      Popover::new("settings")
        .anchor(Anchor::TopRight)
        .trigger(
          Button::new("settings-button")
            .icon(Icon::new(Lucide::Settings))
            .ghost()
            .xsmall()
            .tooltip("Settings: footer, pointer, hot reload, camera"),
        )
        .content(move |_, _, cx| app.update(cx, |app, cx| app.settings_panel(cx)))
    };
    let right = h_flex()
      .flex_none()
      .gap_1()
      .child(settings)
      .child(
        Button::new("theme")
          .icon(Icon::new(theme_icon))
          .ghost()
          .xsmall()
          .tooltip(theme_tooltip)
          .on_click(cx.listener(|app, _: &ClickEvent, window, cx| {
            app.theme = match app.theme {
              config::Theme::System => config::Theme::Dark,
              config::Theme::Dark => config::Theme::Light,
              config::Theme::Light => config::Theme::System,
            };
            app.apply_theme(window, cx);
            app.dirty = true;
            cx.notify();
          })),
      )
      .child(button("shortcuts", Lucide::Keyboard, "Keyboard & mouse shortcuts", |app, _| {
        app.show_shortcuts = !app.show_shortcuts
      }))
      .child(button("about", Lucide::Info, "About bella-presenter", |app, _| {
        app.show_about = !app.show_about
      }));

    h_flex()
      .w_full()
      .p_1()
      .gap_1()
      .items_start()
      .bg(c.bar)
      .border_b_1()
      .border_color(c.border)
      .child(left)
      .child(right)
      .into_any_element()
  }

  /// The settings popover: footer size, pointer size, hot reload and the camera.
  fn settings_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
    let border = cx.theme().border;
    let step = |id: &'static str, label: Lucide, action: fn(&mut Self)| {
      Button::new(id)
        .icon(Icon::new(label))
        .xsmall()
        .on_click(cx.listener(move |app, _: &ClickEvent, _, cx| {
          action(app);
          cx.notify();
        }))
    };
    let row = |label: &'static str, minus, plus| h_flex().gap_2().child(div().w(px(100.0)).child(label)).child(minus).child(plus);
    // The same small text as the header: this menu is for setting up, not for the audience.
    v_flex()
      .text_xs()
      .gap_2()
      .min_w(px(150.0))
      .child(row(
        "Footer size",
        step("font-minus", Lucide::Minus, |app| app.adjust_font(-2.0)),
        step("font-plus", Lucide::Plus, |app| app.adjust_font(2.0)),
      ))
      .child(row(
        "Pointer size",
        step("pointer-minus", Lucide::Minus, |app| app.adjust_pointer(-POINTER_STEP)),
        step("pointer-plus", Lucide::Plus, |app| app.adjust_pointer(POINTER_STEP)),
      ))
      .child(div().h(px(1.0)).bg(border))
      .child(
        Checkbox::new("hot-reload")
          .label("Hot reload")
          .xsmall()
          .tooltip("Reload when the PDF file changes on disk")
          .checked(self.hot_reload)
          .on_click(cx.listener(|app, checked: &bool, _, cx| {
            app.hot_reload = *checked;
            app.update_watcher();
            app.dirty = true;
            cx.notify();
          })),
      )
      .child(div().h(px(1.0)).bg(border))
      .child(self.camera.settings_ui(cx))
      .into_any_element()
  }

  fn footer(&self, cx: &mut Context<Self>) -> AnyElement {
    let slide = if self.goto_input.is_empty() {
      format!("Slide {} / {}", self.session.current() + 1, self.session.page_count())
    } else {
      format!("Go to {}_ / {} (Enter)", self.goto_input, self.session.page_count())
    };
    let clock = chrono::Local::now().format("%H:%M:%S").to_string();
    let t = self.session.timer();
    // Whether it runs shows in the play/pause button next to it.
    let elapsed = format_elapsed(t.elapsed());
    let fs = px(self.footer_font);
    let icon = |name| Icon::new(name).size(fs * 0.85);
    let dot = || div().px(fs).child("·");
    let c = Colors::current(cx);
    // A timer control: an icon at footer size, highlighted under the mouse.
    let control = |id: &'static str, name, tooltip: &'static str, action: fn(&mut Self)| {
      div()
        .id(id)
        .p(fs * 0.15)
        .rounded(px(4.0))
        .cursor_pointer()
        .hover(|s| s.bg(c.row_alt))
        .tooltip(move |window, cx| Tooltip::new(tooltip).build(window, cx))
        .on_click(cx.listener(move |app, _: &ClickEvent, _, cx| {
          action(app);
          cx.notify();
        }))
        .child(icon(name))
    };
    let toggle = if t.is_running() {
      control("timer-toggle", Lucide::Pause, "Pause the timer (K)", |app| app.session.timer_mut().toggle())
    } else {
      control("timer-toggle", Lucide::Play, "Start / resume the timer (K)", |app| {
        app.session.timer_mut().toggle()
      })
    };
    h_flex()
      .w_full()
      .justify_center()
      .gap_2()
      .py(px(4.0))
      .bg(c.bar)
      .border_t_1()
      .border_color(c.border)
      .text_size(fs)
      .font_weight(FontWeight::BOLD)
      .child(slide)
      .child(dot())
      .child(icon(Lucide::Clock))
      .child(clock)
      .child(dot())
      .child(icon(Lucide::Timer))
      .child(elapsed)
      .child(toggle)
      .child(control("timer-reset", Lucide::RotateCcw, "Reset the timer (R)", |app| {
        app.session.timer_mut().reset()
      }))
      .into_any_element()
  }

  /// A PowerPoint-style strip of slide thumbnails along the bottom of the presenter view.
  /// Horizontally scrollable and virtualized — only the on-screen thumbnails are rendered,
  /// so it stays fast on 300+ page decks. Click a thumbnail to jump; the current slide is
  /// highlighted and the strip auto-follows it on navigation. Toggle with `T`.
  fn thumbnails(&mut self, cx: &mut Context<Self>) -> AnyElement {
    const GAP: f32 = 8.0;
    const PAD: f32 = 8.0;
    let (Some(doc), count) = (self.document.as_ref(), self.session.page_count()) else {
      return div().into_any_element();
    };
    // Size the thumbnails to the strip height, reserving room for the padding, the scrollbar and
    // the slide number below each one.
    let thumb_h = (self.thumb_height - PAD - 16.0 - THUMB_LABEL_H).max(36.0);
    let thumb_w = thumb_h * doc.slide_aspect(0).max(0.1);
    // Re-centre on the current slide only when it changes, so manual scrolling is preserved.
    let current = self.session.current();
    if current != self.thumb_follow_last {
      self.thumb_follow_last = current;
      self.thumb_scroll.scroll_to_item(current, ScrollStrategy::Center);
    }
    let sizes = Rc::new(vec![size(px(thumb_w + GAP), px(thumb_h + THUMB_LABEL_H)); count]);
    div()
      .size_full()
      .pt(px(PAD))
      .px(px(PAD))
      // The wheel over the strip scrolls it instead of changing slides.
      .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
      .child(
        h_virtual_list(cx.entity(), "thumbnails", sizes, move |app, range, _, cx| {
          range.map(|i| app.thumbnail(i, thumb_w, thumb_h, cx)).collect()
        })
        .track_scroll(&self.thumb_scroll)
        .size_full(),
      )
      .into_any_element()
  }

  fn thumbnail(&self, i: usize, thumb_w: f32, thumb_h: f32, cx: &mut Context<Self>) -> AnyElement {
    let c = Colors::current(cx);
    let (selected, muted, hover, weak) = (c.cursor, c.row_alt, c.border, c.muted);
    // Aspect-fit the slide within the fixed slot (letterbox if a page differs).
    let a = self.document.as_ref().map_or(1.0, |d| d.slide_aspect(i).max(0.1));
    let (w, h) = if thumb_h * a > thumb_w {
      (thumb_w, thumb_w / a)
    } else {
      (thumb_h * a, thumb_h)
    };
    let current = i == self.session.current();
    v_flex()
      .id(("thumb", i))
      .w(px(thumb_w))
      .items_center()
      .on_click(cx.listener(move |app, _: &ClickEvent, _, cx| {
        app.session.goto(i as isize);
        cx.notify();
      }))
      .child(
        div().w(px(thumb_w)).h(px(thumb_h)).flex().items_center().justify_center().child(
          div()
            .w(px(w))
            .h(px(h))
            .rounded(px(2.0))
            .bg(muted)
            .hover(|s| s.bg(hover))
            .when(current, |d| d.border(px(3.0)).border_color(selected))
            .child(self.slide(i, Region::Slide, a, Surface::Thumb, cx)),
        ),
      )
      // The slide number, centred below the picture (the current one in the accent colour).
      .child(
        div()
          .h(px(THUMB_LABEL_H))
          .text_xs()
          .text_color(if current { selected } else { weak })
          .when(current, |d| d.font_weight(FontWeight::BOLD))
          .child(format!("{}", i + 1)),
      )
      .into_any_element()
  }

  /// The screen shown when no document is open: a faint logo plus a recent-files list to click,
  /// or a prompt to open one.
  fn start_screen(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
    let theme = cx.theme();
    let (muted, hover, border) = (theme.muted_foreground, theme.list_hover, theme.border);
    let (logo, aspect) = self.logo.clone();
    let logo_w = 220.0_f32.min(f32::from(window.viewport_size().width) * 0.6);
    // The same small text as the header: this screen is for setting up, not for the audience.
    let top = v_flex()
      .items_center()
      .text_xs()
      .pt(px(20.0))
      .gap(px(8.0))
      .child(div().text_sm().font_weight(FontWeight::BOLD).child(APP_NAME))
      .child(img(logo).w(px(logo_w)).h(px(logo_w / aspect)).opacity(0.35))
      .child(
        Button::new("start-open")
          .icon(Icon::new(Lucide::FolderOpen))
          .label("Open a PDF… (O)")
          .xsmall()
          .on_click(cx.listener(|app, _: &ClickEvent, _, cx| app.pick_file(cx))),
      )
      .child(div().text_color(muted).child("… or drag & drop a PDF onto the window"));
    if self.recent.is_empty() {
      return top.child(div().pt_4().text_color(muted).child("No recent files yet.")).into_any_element();
    }

    // A vertical list (like Zed's recent projects): one row per file. Click the row to open it;
    // the icons open the containing folder and remove it from the list.
    let rows = self.recent.iter().enumerate().map(|(i, entry)| {
      let name = entry.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
      let folder = entry.path.parent().map(|p| p.display().to_string()).unwrap_or_default();
      let tip = format!("{name}\n{folder}");
      let (open, reveal, remove) = (entry.path.clone(), entry.path.clone(), entry.path.clone());
      h_flex()
        .id(("recent", i))
        .w_full()
        .h(px(28.0))
        .px(px(8.0))
        .gap_1()
        .rounded(px(5.0))
        .cursor_pointer()
        .hover(|s| s.bg(hover))
        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
        .on_click(cx.listener(move |app, _: &ClickEvent, _, cx| app.open(&open, cx)))
        .child(div().flex_1().min_w_0().truncate().child(name))
        .child(
          Button::new(("reveal", i))
            .icon(Icon::new(Lucide::FolderOpen))
            .ghost()
            .tooltip("Open containing folder")
            .xsmall()
            .on_click(move |_: &ClickEvent, _, cx| {
              cx.stop_propagation();
              crate::platform::reveal_in_file_manager(&reveal);
            }),
        )
        .child(
          Button::new(("remove", i))
            .icon(Icon::new(Lucide::X))
            .ghost()
            .tooltip("Remove from recent")
            .xsmall()
            .on_click(cx.listener(move |app, _: &ClickEvent, _, cx| {
              cx.stop_propagation();
              app.remove_recent(&remove);
              cx.notify();
            })),
        )
    });
    top
      .child(div().w_full().h(px(1.0)).mt_2().bg(border))
      .child(div().pt_1().text_sm().font_weight(FontWeight::BOLD).child("Recent files"))
      .child(
        div()
          .id("recent-list")
          .flex_1()
          .min_h_0()
          .w_full()
          .max_w(px(520.0))
          .overflow_y_scroll()
          .child(v_flex().children(rows)),
      )
      .size_full()
      .into_any_element()
  }

  /// Lay out the panels for the current arrangement. Dividers are draggable (sizes are
  /// remembered per arrangement); the arrangement is chosen by "Flip layout" (L). With no notes:
  /// just current + next (L toggles horizontal/vertical).
  fn layout(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
    let viewport = window.viewport_size();
    let (vw, vh) = (viewport.width, viewport.height);
    let has_notes = self.document.as_ref().is_some_and(Document::has_notes);
    let mut pane = |kind| resizable_panel().child(self.pane(kind, cx));
    // A nested group's own `.size()` pins its cross axis (a column's width, a row's height), so
    // it could shrink but never grow past it: the panel holding it carries the initial size.
    let side = |size, group| resizable_panel().size(size).child(group);
    if !has_notes {
      return if self.layout_index.is_multiple_of(2) {
        h_resizable("nn-h")
          .child(pane(PaneKind::Current))
          .child(pane(PaneKind::Next).size(vw * 0.5))
          .into_any_element()
      } else {
        v_resizable("nn-v")
          .child(pane(PaneKind::Current))
          .child(pane(PaneKind::Next).size(vh * 0.45))
          .into_any_element()
      };
    }
    match self.layout_index % NUM_LAYOUTS {
      0 => h_resizable("l0")
        .child(pane(PaneKind::Current))
        .child(side(vw * 0.34, v_resizable("l0-side").child(pane(PaneKind::Next)).child(pane(PaneKind::Notes))))
        .into_any_element(),
      1 => h_resizable("l1")
        .child(pane(PaneKind::Current))
        .child(side(vw * 0.34, v_resizable("l1-side").child(pane(PaneKind::Notes)).child(pane(PaneKind::Next))))
        .into_any_element(),
      2 => h_resizable("l2")
        .child(pane(PaneKind::Next).size(vw * 0.24))
        .child(pane(PaneKind::Current))
        .child(pane(PaneKind::Notes).size(vw * 0.28))
        .into_any_element(),
      _ => v_resizable("l3")
        .child(pane(PaneKind::Current))
        .child(side(vh * 0.32, h_resizable("l3-row").child(pane(PaneKind::Next)).child(pane(PaneKind::Notes))))
        .into_any_element(),
    }
  }

  /// One panel's content: a titled, aspect-fitted page region.
  fn pane(&self, kind: PaneKind, cx: &mut Context<Self>) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let title = match kind {
      PaneKind::Current => "Current",
      PaneKind::Next => "Next",
      PaneKind::Notes => "Notes",
    };
    let body = self.document.as_ref().map(|doc| {
      let current = self.session.current();
      let shown = match kind {
        PaneKind::Current => Some((current, Region::Slide, doc.slide_aspect(current), Surface::Presenter)),
        PaneKind::Next => self.session.next_page().map(|n| (n, Region::Slide, doc.slide_aspect(n), Surface::Pane)),
        PaneKind::Notes => doc.notes_aspect(current).map(|a| (current, Region::Notes, a, Surface::Pane)),
      };
      match shown {
        Some((page, region, aspect, surface)) => self.slide(page, region, aspect, surface, cx),
        None => div()
          .size_full()
          .flex()
          .items_center()
          .justify_center()
          .text_color(muted)
          .child(match kind {
            PaneKind::Next => "— end —",
            PaneKind::Notes => "No notes in this document.",
            PaneKind::Current => "",
          })
          .into_any_element(),
      }
    });
    v_flex()
      .size_full()
      .p_1()
      .child(div().text_sm().text_color(muted).child(title))
      .child(div().flex_1().min_h_0().children(body))
      .into_any_element()
  }

  /// A page region drawn aspect-fitted (letterboxed) into the space it gets; the current slide
  /// also zooms and takes the pointer and drawings.
  fn slide(&self, page: usize, region: Region, aspect: f32, surface: Surface, cx: &mut Context<Self>) -> AnyElement {
    let app = cx.entity();
    canvas(
      |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
      move |bounds, hitbox, window, cx| app.update(cx, |app, cx| app.paint_slide(bounds, hitbox, page, region, aspect, surface, window, cx)),
    )
    .size_full()
    .into_any_element()
  }

  #[expect(clippy::too_many_arguments, reason = "the canvas hands over its paint arguments")]
  fn paint_slide(
    &mut self,
    bounds: Bounds<Pixels>,
    hitbox: Hitbox,
    page: usize,
    region: Region,
    aspect: f32,
    surface: Surface,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    // Pictures evicted or cleared since the last paint leave the GPU now that a window is at hand.
    for old in self.cache.evicted.drain(..).chain(self.thumb_cache.evicted.drain(..)) {
      cx.drop_image(old, Some(window));
    }
    let Some(doc) = self.document.as_ref() else { return };
    let current = matches!(surface, Surface::Presenter | Surface::Audience);
    let (zoom, center) = if current { (self.zoom, self.zoom_center) } else { (1.0, SLIDE_CENTER) };
    let area = bounds.map(f32::from);
    let view = SlideView::new(area, aspect);
    let (uv, rect) = view.zoomed(zoom, center);
    // Rendered at extra resolution when zoomed, for crispness.
    let scale = window.scale_factor();
    let px_size = render_size(view.fit, scale * zoom.clamp(1.0, MAX_ZOOM_RENDER));
    // The whiteboard replaces the current slide on both screens, with its own drawings.
    let board = current && self.cover == Cover::White;
    let cache = if surface == Surface::Thumb { &mut self.thumb_cache } else { &mut self.cache };
    if board {
      window.paint_quad(gpui_kit::fill(rect.map(px), rgb(0xffffff)));
    } else if let Some(image) = cache.get_or_render(doc, page, region, px_size) {
      paint_cropped(window, rect, uv, 0.0, image);
    }
    // Get the following page ready at this size (unzoomed), so moving forward shows it at once.
    if surface != Surface::Thumb && page + 1 < doc.page_count() {
      self.want_prefetch((page + 1, region, render_size(view.fit, scale)), cx);
    }
    if surface == Surface::Audience {
      self.audience_view = Some(view);
      // Webcam overlay in a corner (audience window only).
      self.camera.paint_overlay(area, window);
    } else if surface == Surface::Presenter {
      self.presenter_view = Some(view);
    }
    if !current {
      return;
    }
    if self.link_hover == Some(surface) {
      window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
    }
    let page = if board { BOARD } else { page };
    self.listen_to_slide(surface, page, hitbox, rect, uv, window, cx);
    // The page's strokes, then the pointer dot (if any) on top, clipped to the slide.
    let color = self.pointer_color;
    window.with_content_mask(Some(ContentMask { bounds: rect.map(px) }), |window| {
      for s in self.strokes.get(&page).into_iter().flatten() {
        paint_stroke(window, rect, uv, s, color);
      }
      if let Some(n) = self.pointer_norm {
        paint_pointer_dot(window, rect, uv, n, self.pointer_size, color);
      }
    });
  }

  /// Mouse interaction on a current-slide surface shown at `rect` with visible window `uv`:
  /// laser pointer, freehand drawing, Ctrl/Cmd+drag panning (when zoomed), and following links on
  /// a click. The presenter's and the audience's slide drive the same state, so annotations and
  /// zoom mirror between them.
  #[expect(clippy::too_many_arguments, reason = "the paint hands over where and what the slide is")]
  fn listen_to_slide(&self, surface: Surface, page: usize, hitbox: Hitbox, rect: Bounds<f32>, uv: Bounds<f32>, window: &mut Window, cx: &mut Context<Self>) {
    let app = cx.entity().downgrade();
    window.on_mouse_event({
      let (app, hitbox) = (app.clone(), hitbox.clone());
      move |e: &MouseDownEvent, phase, window, cx| {
        let pos = to_f32(e.position);
        if phase == DispatchPhase::Bubble && e.button == MouseButton::Left && hitbox.is_hovered(window) && rect.contains(&pos) {
          let _ = app.update(cx, |app, cx| {
            app.pressed = Some(surface);
            app.press_start = Some((pos, Instant::now()));
            app.drag_last = None;
            app.slide_input(pos, e.modifiers, rect, uv);
            cx.notify();
          });
        }
      }
    });
    window.on_mouse_event({
      let app = app.clone();
      move |e: &MouseMoveEvent, phase, window, cx| {
        if phase == DispatchPhase::Bubble {
          let _ = app.update(cx, |app, cx| {
            if app.pressed.is_none() {
              // The hand cursor over a link (a click follows it in both modes).
              let pos = to_f32(e.position);
              let over = hitbox.is_hovered(window) && rect.contains(&pos) && app.link_at(page, screen_to_slide(rect, uv, pos)).is_some();
              if over != (app.link_hover == Some(surface)) {
                app.link_hover = over.then_some(surface);
                cx.notify();
              }
              return;
            }
            if app.pressed != Some(surface) {
              return;
            }
            // A release outside the window never arrives as a mouse-up: the button is up by now.
            if e.pressed_button == Some(MouseButton::Left) {
              app.slide_input(to_f32(e.position), e.modifiers, rect, uv);
            } else {
              app.release(surface);
            }
            cx.notify();
          });
        }
      }
    });
    window.on_mouse_event(move |e: &MouseUpEvent, phase, _, cx| {
      if phase == DispatchPhase::Bubble && e.button == MouseButton::Left {
        let _ = app.update(cx, |app, cx| {
          if app.pressed != Some(surface) {
            return;
          }
          let pos = to_f32(e.position);
          let click = app
            .press_start
            .take()
            .is_some_and(|(start, at)| at.elapsed() < Duration::from_millis(400) && (start.x - pos.x).hypot(start.y - pos.y) < 6.0);
          app.release(surface);
          if click
            && e.button == MouseButton::Left
            && !crate::platform::zoom_modifier(e.modifiers)
            && let Some(target) = app.link_at(page, screen_to_slide(rect, uv, pos))
          {
            app.follow(target, cx);
          }
          cx.notify();
        });
      }
    });
  }

  /// Where the link at slide point `n` of `page` leads, if one is there.
  fn link_at(&mut self, page: usize, n: Point<f32>) -> Option<LinkTarget> {
    let doc = self.document.as_ref().filter(|doc| page < doc.page_count())?;
    let links = self.links.entry(page).or_insert_with(|| Rc::new(doc.slide_links(page)));
    links.iter().find(|link| link.area.contains(n.x, n.y)).map(|link| link.target.clone())
  }

  fn follow(&mut self, target: LinkTarget, cx: &mut Context<Self>) {
    match target {
      LinkTarget::Page(page) => self.session.goto(page as isize),
      // Web and mail addresses only: a PDF can name any program or file, and the system would open it.
      LinkTarget::Uri(uri) if is_web_link(&uri) => cx.open_url(&uri),
      LinkTarget::Uri(uri) => tracing::warn!("not opening link {uri}: only http, https and mailto are followed"),
    }
  }

  /// Queue a picture to render once the current frame is on screen.
  fn want_prefetch(&mut self, item: (usize, Region, [u32; 2]), cx: &mut Context<Self>) {
    if !self.prefetch.contains(&item) {
            // In draw mode the press began a dot, but the click was meant for the link.
            if app.mode == InputMode::Draw
              && let Some(strokes) = app.strokes.get_mut(&page)
            {
              strokes.pop();
            }
      self.prefetch.push(item);
    }
    if self.prefetch_task.is_some() {
      return;
    }
    self.prefetch_task = Some(cx.spawn(async move |app, cx| {
      loop {
        // One picture per pause, so input between them is handled at once.
        cx.background_executor().timer(Duration::from_millis(30)).await;
        match app.update(cx, |app, _| app.prefetch_one()) {
          Ok(true) => {}
          Ok(false) | Err(_) => break,
        }
      }
      // Dropping the task from within its last step is fine: it has nothing left to run.
      let _ = app.update(cx, |app, _| app.prefetch_task = None);
    }));
  }

  /// Render the oldest queued picture still near the current slide. Returns whether more wait.
  fn prefetch_one(&mut self) -> bool {
    if self.prefetch.is_empty() {
      return false;
    }
    let (page, region, size) = self.prefetch.remove(0);
    let current = self.session.current();
    if let Some(doc) = self.document.as_ref()
      && (current..=current + 2).contains(&page)
      && page < doc.page_count()
    {
      self.cache.get_or_render(doc, page, region, size);
    }
    !self.prefetch.is_empty()
  }

  /// The left button let go of `surface`: hide the pointer and end the stroke.
  fn release(&mut self, surface: Surface) {
    if self.pressed == Some(surface) {
      self.pressed = None;
      self.pointer_norm = None;
      self.drawing = false;
    }
  }

  /// The held mouse at `pos`: pan with Ctrl/Cmd, else move the pointer or extend the stroke.
  fn slide_input(&mut self, pos: Point<f32>, modifiers: Modifiers, rect: Bounds<f32>, uv: Bounds<f32>) {
    if crate::platform::zoom_modifier(modifiers) {
      // Ctrl/Cmd/Option+drag pans (when zoomed); no pointer/drawing meanwhile.
      self.pointer_norm = None;
      self.drawing = false;
      if let Some(last) = self.drag_last
        && self.zoom > 1.0
      {
        let c = clamp_center(self.zoom_center, uv.size);
        let dx = (pos.x - last.x) / rect.size.width.max(1.0) * uv.size.width;
        let dy = (pos.y - last.y) / rect.size.height.max(1.0) * uv.size.height;
        self.zoom_center = clamp_center(point(c.x - dx, c.y - dy), uv.size);
      }
    } else {
      let n = screen_to_slide(rect, uv, pos);
      match self.mode {
        InputMode::Pointer => {
          self.drawing = false;
          self.pointer_norm = Some(n);
        }
        InputMode::Draw => {
          self.pointer_norm = None;
          let (width_factor, min_step) = (0.006 * self.pointer_size, 0.0015 / self.zoom);
          let strokes = self.strokes.entry(self.ink_page()).or_default();
          if !self.drawing {
            self.drawing = true;
            strokes.push(Stroke { points: vec![n], width_factor });
          } else if let Some(last) = strokes.last_mut()
            && last.points.last().is_none_or(|lp| (lp.x - n.x).abs() + (lp.y - n.y).abs() > min_step)
          {
            last.points.push(n);
          }
        }
      }
    }
    self.drag_last = Some(pos);
  }

  /// The audience window: only the slide (or the whiteboard), on black (or black alone while blanked).
  /// Double-click toggles fullscreen.
  fn render_audience(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
    self.tick(window, cx);
    let slide = self
      .document
      .as_ref()
      .filter(|_| self.cover != Cover::Black)
      .map(|doc| (self.session.current(), doc.slide_aspect(self.session.current())));
    div()
      .size_full()
      .bg(rgb(0x000000))
      .on_scroll_wheel(cx.listener(|app, e: &ScrollWheelEvent, _, cx| {
        let anchor = app.audience_view;
        app.handle_wheel(e, anchor, cx);
      }))
      .on_mouse_down(MouseButton::Left, |e, window, _| {
        if e.click_count == 2 {
          window.toggle_fullscreen();
        }
      })
      .children(slide.map(|(page, aspect)| self.slide(page, Region::Slide, aspect, Surface::Audience, cx)))
      .into_any_element()
  }

  /// A centered card above the window content with a title and, when `close` is given, a ✖.
  fn card(&self, id: &'static str, title: impl Into<SharedString>, close: Option<fn(&mut Self)>, body: impl IntoElement, cx: &mut Context<Self>) -> AnyElement {
    let theme = cx.theme();
    let card = v_flex()
      .id(id)
      .occlude()
      .max_w(px(760.0))
      .max_h_full()
      .p_3()
      .gap_2()
      .bg(theme.popover)
      .border_1()
      .border_color(theme.border)
      .rounded_lg()
      .shadow_lg()
      .child(
        h_flex()
          .justify_between()
          .gap_4()
          .child(div().text_lg().font_weight(FontWeight::BOLD).child(title.into()))
          .children(close.map(|close| {
            Button::new((id, 1_usize))
              .icon(Icon::new(Lucide::X))
              .ghost()
              .small()
              .on_click(cx.listener(move |app, _: &ClickEvent, _, cx| {
                close(app);
                cx.notify();
              }))
          })),
      )
      .child(body);
    div()
      .absolute()
      .inset_0()
      .p_4()
      .flex()
      .items_center()
      .justify_center()
      .child(card)
      .into_any_element()
  }

  fn shortcuts_card(&self, cx: &mut Context<Self>) -> AnyElement {
    let rows = [
      ("→ / Space / PgDn", "Next slide"),
      ("← / PgUp", "Previous slide"),
      ("Home / End", "First / last slide"),
      ("F5", "Start presentation"),
      ("Esc", "Quit presentation"),
      ("B", "Blank the audience screen (black)"),
      (",", "Whiteboard to draw on (both screens)"),
      ("C", "Toggle the webcam overlay"),
      ("W", "Close file, back to start screen"),
      ("R", "Reset the talk timer"),
      ("K", "Pause / resume the talk timer (also in the footer)"),
      ("Number + Enter", "Go to that slide"),
      ("Click a link", "Follow it"),
      ("L", "Flip layout (H/V with no notes; 4 presets with notes)"),
      ("T", "Toggle the slide thumbnail strip"),
      ("S", "Swap presenter / presentation screens"),
      ("+ / −", "Footer font larger / smaller"),
      ("O", "Open a PDF"),
      ("Hold mouse on current slide", "Laser pointer / draw (on the audience screen)"),
      ("P", "Toggle laser pointer / drawing"),
      ("D", "Delete the drawings on this slide"),
      (crate::platform::ZOOM_SCROLL, "Zoom the current slide at the cursor"),
      (crate::platform::ZOOM_DRAG, "Pan the zoomed slide"),
      ("Scroll", "Next / previous slide"),
      ("(audience) double-click", "Toggle fullscreen"),
    ];
    let body = v_flex().gap(px(3.0)).text_sm().children(
      rows
        .into_iter()
        .map(|(k, d)| h_flex().gap(px(20.0)).child(div().w(px(240.0)).font_weight(FontWeight::BOLD).child(k)).child(d)),
    );
    self.card("shortcuts", "Keyboard shortcuts", Some(|app| app.show_shortcuts = false), body, cx)
  }

  /// The About card: app info, logo, and the generated third-party license list.
  fn about_card(&self, cx: &mut Context<Self>) -> AnyElement {
    const THIRDPARTY: &str = include_str!("../assets/thirdparty.md");
    let theme = cx.theme();
    let mono = theme.mono_font_family.clone();
    let muted = theme.muted_foreground;
    let (logo, aspect) = self.about_logo.clone();
    let link = |id: &'static str, text: &str, url: &'static str| Link::new(id).href(url).child(text.to_owned());
    let body = v_flex()
      .gap_1()
      .w(px(700.0))
      .child(
        v_flex()
          .items_center()
          .gap_1()
          .child(img(logo).w(px(150.0)).h(px(150.0 / aspect)))
          .child(div().text_xl().font_weight(FontWeight::BOLD).child(APP_NAME))
          .child(env!("CARGO_PKG_DESCRIPTION"))
          .child(format!("Version {}", env!("CARGO_PKG_VERSION")))
          .child(self.update_check_button(cx))
          .children(self.update_note.clone().map(|note| div().text_sm().text_color(muted).child(note)))
          .child(link("author", "© 2026 tschinz", "https://github.com/tschinz"))
          .child(format!("License: {}", env!("CARGO_PKG_LICENSE")))
          .child(link("repository", env!("CARGO_PKG_REPOSITORY"), env!("CARGO_PKG_REPOSITORY")))
          // GitHub Sponsors button (styled like the GitHub one; opens the sponsors page).
          .child(
            div()
              .id("sponsor")
              .mt_2()
              .px_4()
              .py_1()
              .rounded_md()
              .bg(rgb(0xDB61A2))
              .text_color(rgb(0xFFFFFF))
              .font_weight(FontWeight::BOLD)
              .cursor_pointer()
              .tooltip(|window, cx| Tooltip::new("Support development on GitHub Sponsors").build(window, cx))
              .on_click(|_, _, cx| cx.open_url("https://github.com/sponsors/tschinz"))
              .flex()
              .items_center()
              .gap_2()
              .child(Icon::new(Lucide::Heart))
              .child("Sponsor"),
          ),
      )
      .child(
        div()
          .pt_2()
          .child("Renders PDFs with PDFium (BSD-3-Clause) and uses the Iosevka Nerd Font (SIL OFL 1.1), both bundled with the application."),
      )
      .child(
        Button::new("thirdparty")
          .icon(Icon::new(chevron(self.show_thirdparty)))
          .label("Third-party libraries")
          .ghost()
          .on_click(cx.listener(|app, _: &ClickEvent, _, cx| {
            app.show_thirdparty = !app.show_thirdparty;
            cx.notify();
          })),
      )
      .when(self.show_thirdparty, |body| {
        // Only the visible lines are laid out: the list is very long.
        let lines: Rc<Vec<&'static str>> = Rc::new(THIRDPARTY.lines().collect());
        body.child(
          uniform_list("thirdparty-lines", lines.len(), move |range, _, _| {
            range.map(|i| div().font_family(mono.clone()).text_sm().child(lines[i])).collect()
          })
          .h(px(320.0)),
        )
      });
    self.card("about", format!("About {APP_NAME}"), Some(|app| app.show_about = false), body, cx)
  }

  /// Checks for a newer release now; a running check or install disables it.
  fn update_check_button(&self, cx: &mut Context<Self>) -> AnyElement {
    let busy = matches!(self.update, UpdateStep::Checking(..) | UpdateStep::Installing(..));
    Button::new("check-update")
      .icon(Icon::new(Lucide::RefreshCw))
      .label(if busy { "Checking…" } else { "Check for updates" })
      .small()
      .disabled(busy)
      .on_click(cx.listener(|app, _: &ClickEvent, _, cx| {
        app.update_note = None;
        app.update = UpdateStep::Checking(in_background(&app.waker, update::check), true);
        cx.notify();
      }))
      .into_any_element()
  }

  /// Offers a newer release (never while presenting, so a talk is not interrupted).
  fn update_card(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
    if self.presenting {
      return None;
    }
    let card = match &self.update {
      UpdateStep::Offer(release) => {
        let body = v_flex()
          .gap_2()
          .child(format!("{APP_NAME} {} is available (you have {}).", release.version, env!("CARGO_PKG_VERSION")))
          .when(!release.notes.trim().is_empty(), |body| {
            body
              .child(
                Button::new("release-notes")
                  .icon(Icon::new(chevron(self.show_release_notes)))
                  .label("Release notes")
                  .ghost()
                  .on_click(cx.listener(|app, _: &ClickEvent, _, cx| {
                    app.show_release_notes = !app.show_release_notes;
                    cx.notify();
                  })),
              )
              .when(self.show_release_notes, |body| {
                body.child(div().id("notes").max_h(px(240.0)).overflow_y_scroll().child(release.notes.clone()))
              })
          })
          .child(
            h_flex()
              .gap_2()
              .child(
                Button::new("install")
                  .label("Install and restart")
                  .on_click(cx.listener(|app, _: &ClickEvent, _, cx| {
                    if let UpdateStep::Offer(release) = &app.update {
                      let r = release.clone();
                      let rx = in_background(&app.waker, move || update::install(&r));
                      app.update = UpdateStep::Installing(release.clone(), rx);
                      cx.notify();
                    }
                  })),
              )
              .child(
                Button::new("skip")
                  .label("Skip this version")
                  .on_click(cx.listener(|app, _: &ClickEvent, _, cx| {
                    if let UpdateStep::Offer(release) = &app.update {
                      app.skipped_update = release.version.clone();
                      app.dirty = true;
                    }
                    app.update = UpdateStep::Idle;
                    cx.notify();
                  })),
              )
              .child(Button::new("later").label("Later").on_click(cx.listener(|app, _: &ClickEvent, _, cx| {
                app.update = UpdateStep::Idle;
                cx.notify();
              }))),
          );
        self.card("update", "Update available", None, body, cx)
      }
      UpdateStep::Installing(release, _) => {
        let body = h_flex()
          .gap_2()
          .child(Spinner::new())
          .child(format!("Downloading and installing {APP_NAME} {}…", release.version));
        self.card("updating", "Updating", None, body, cx)
      }
      UpdateStep::Failed(error) => {
        let body = v_flex()
          .gap_2()
          .child(error.clone())
          .child(Button::new("update-close").label("Close").on_click(cx.listener(|app, _: &ClickEvent, _, cx| {
            app.update = UpdateStep::Idle;
            cx.notify();
          })));
        self.card("update-failed", "Update failed", None, body, cx)
      }
      UpdateStep::Idle | UpdateStep::Checking(..) => return None,
    };
    Some(card)
  }
}

impl Render for PresenterApp {
  fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    self.update_title(window);
    self.tick(window, cx);
    let theme = cx.theme();
    let (background, foreground) = (theme.background, theme.foreground);
    let body = if self.document.is_none() {
      self.start_screen(window, cx)
    } else {
      v_resizable("body")
        .child(resizable_panel().child(self.layout(window, cx)))
        .child(
          resizable_panel()
            .visible(self.show_thumbnails)
            .size(px(self.thumb_height))
            .size_range(px(MIN_STRIP_H)..px(MAX_STRIP_H))
            .child(self.thumbnails(cx)),
        )
        .on_resize(cx.listener(|app, state: &Entity<ResizableState>, _, cx| {
          // Remember the (possibly dragged) strip height for next session.
          if let Some(h) = state.read(cx).sizes().get(1).map(|h| f32::from(*h))
            && h > 0.0
            && (h - app.thumb_height).abs() > 0.5
          {
            app.thumb_height = h.clamp(MIN_STRIP_H, MAX_STRIP_H);
            app.dirty = true;
            cx.notify();
          }
        }))
        .into_any_element()
    };
    let header = self.header(cx);
    let footer = self.document.is_some().then(|| self.footer(cx));
    let shortcuts = self.show_shortcuts.then(|| self.shortcuts_card(cx));
    let about = self.show_about.then(|| self.about_card(cx));
    let update = self.update_card(cx);
    let error = self.error.clone().map(|e| self.card("error", "Error", Some(|app| app.error = None), e, cx));
    v_flex()
      .relative()
      .size_full()
      .bg(background)
      .text_color(foreground)
      .on_scroll_wheel(cx.listener(|app, e: &ScrollWheelEvent, _, cx| {
        let anchor = app.presenter_view;
        app.handle_wheel(e, anchor, cx);
      }))
      // Open the first PDF dropped onto the window.
      .on_drop(cx.listener(|app, paths: &ExternalPaths, _, cx| {
        if let Some(path) = paths.paths().iter().find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf"))) {
          app.open(path, cx);
        }
      }))
      .child(header)
      .child(div().flex_1().min_h_0().child(body))
      .children(footer)
      .children(shortcuts)
      .children(about)
      .children(update)
      .children(error)
  }
}

/// The audience window's view: draws through the presenter app and redraws when it changes.
struct AudienceView {
  app: Entity<PresenterApp>,
  _changes: Subscription,
}

impl Render for AudienceView {
  fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    self.app.update(cx, |app, cx| app.render_audience(window, cx))
  }
}

/// Options for a window titled `title` at `bounds`, on the display `display`.
fn window_options(title: &str, bounds: WindowBounds, display: Option<gpui_kit::DisplayId>) -> WindowOptions {
  WindowOptions {
    window_bounds: Some(bounds),
    display_id: display,
    titlebar: Some(TitlebarOptions {
      title: Some(title.to_owned().into()),
      ..Default::default()
    }),
    icon: crate::app_icon(),
    app_id: Some(APP_NAME.to_owned()),
    ..Default::default()
  }
}

/// An embedded PNG logo and its width / height.
fn logo(bytes: &'static [u8]) -> (Arc<Image>, f32) {
  let aspect = image::ImageReader::new(std::io::Cursor::new(bytes))
    .with_guessed_format()
    .ok()
    .and_then(|r| r.into_dimensions().ok())
    .map_or(1.0, |(w, h)| w as f32 / h.max(1) as f32);
  (Arc::new(Image::from_bytes(ImageFormat::Png, bytes.to_vec())), aspect)
}

/// True if `new` differs from `old` by more than a pixel (or `old` is unset).
fn geom_changed(old: Option<Geometry>, new: Geometry) -> bool {
  match old {
    None => true,
    Some(o) => (o.x - new.x).abs() > 1.0 || (o.y - new.y).abs() > 1.0 || (o.w - new.w).abs() > 1.0 || (o.h - new.h).abs() > 1.0,
  }
}

/// `rgb` with transparency `alpha`, as GPUI colours.
fn color(rgb: [u8; 3], alpha: u8) -> Hsla {
  let [r, g, b] = rgb.map(|v| v as f32 / 255.0);
  Rgba {
    r,
    g,
    b,
    a: alpha as f32 / 255.0,
  }
  .into()
}

/// The arrow of a section that opens and closes.
fn chevron(open: bool) -> Lucide {
  if open { Lucide::ChevronDown } else { Lucide::ChevronRight }
}

fn to_f32(p: Point<Pixels>) -> Point<f32> {
  p.map(f32::from)
}

/// Where the update offer stands (see [`PresenterApp::update_card`]).
enum UpdateStep {
  Idle,
  /// The check's result, and whether the user asked for it (in the About card).
  Checking(mpsc::Receiver<Result<Option<Release>, String>>, bool),
  Offer(Release),
  /// Downloading and installing; the result is the program to restart.
  Installing(Release, mpsc::Receiver<Result<PathBuf, String>>),
  Failed(String),
}

/// Runs `work` on its own thread and wakes the UI when its result is ready.
fn in_background<T: Send + 'static>(waker: &Waker, work: impl FnOnce() -> T + Send + 'static) -> mpsc::Receiver<T> {
  let (tx, rx) = mpsc::channel();
  let waker = waker.clone();
  std::thread::spawn(move || {
    let _ = tx.send(work());
    waker.wake();
  });
  rx
}

/// The middle of a slide (slide-normalized coords): the zoom centre when not zoomed.
const SLIDE_CENTER: Point<f32> = Point { x: 0.5, y: 0.5 };

/// Where a zoomable slide is shown: the area it may fill and its fitted size there at zoom 1.
#[derive(Clone, Copy, Debug)]
struct SlideView {
  area: Bounds<f32>,
  fit: Size<f32>,
}

impl SlideView {
  /// The slide of width/height `aspect` fitted (letterboxed) into `area`.
  fn new(area: Bounds<f32>, aspect: f32) -> Self {
    let (mut w, mut h) = (area.size.width, area.size.width / aspect);
    if h > area.size.height {
      h = area.size.height;
      w = area.size.height * aspect;
    }
    Self { area, fit: size(w, h) }
  }

  /// The visible slide window (slide-normalized) and its on-screen rect at `zoom` around
  /// `center`. Zooming grows the slide past its fitted size until it fills the whole area,
  /// so a portrait page also widens into its side bars instead of staying letterboxed.
  fn zoomed(&self, zoom: f32, center: Point<f32>) -> (Bounds<f32>, Bounds<f32>) {
    let z = zoom.max(1.0);
    let scaled = size(self.fit.width * z, self.fit.height * z);
    let shown = size(scaled.width.min(self.area.size.width), scaled.height.min(self.area.size.height));
    let window = size(shown.width / scaled.width.max(1e-6), shown.height / scaled.height.max(1e-6));
    let uv = Bounds::centered_at(clamp_center(center, window), window);
    (uv, Bounds::centered_at(self.area.center(), shown))
  }

  /// The zoom centre that keeps slide point `anchor` under screen point `cursor` at `zoom`.
  fn center_keeping(&self, zoom: f32, anchor: Point<f32>, cursor: Point<f32>) -> Point<f32> {
    let (uv, rect) = self.zoomed(zoom, anchor);
    let min_x = anchor.x - (cursor.x - rect.origin.x) / (self.fit.width * zoom).max(1e-6);
    let min_y = anchor.y - (cursor.y - rect.origin.y) / (self.fit.height * zoom).max(1e-6);
    clamp_center(point(min_x + uv.size.width * 0.5, min_y + uv.size.height * 0.5), uv.size)
  }
}

/// Clamp a zoom centre so the visible window of `size` stays within the slide.
fn clamp_center(c: Point<f32>, size: Size<f32>) -> Point<f32> {
  let (hx, hy) = (size.width * 0.5, size.height * 0.5);
  point(c.x.clamp(hx, (1.0 - hx).max(hx)), c.y.clamp(hy, (1.0 - hy).max(hy)))
}

/// Map a slide-normalized point to screen, given the visible `uv` window shown in `rect`.
fn slide_to_screen(rect: Bounds<f32>, uv: Bounds<f32>, n: Point<f32>) -> Point<f32> {
  let fx = (n.x - uv.origin.x) / uv.size.width.max(1e-6);
  let fy = (n.y - uv.origin.y) / uv.size.height.max(1e-6);
  point(rect.origin.x + fx * rect.size.width, rect.origin.y + fy * rect.size.height)
}

/// Inverse of [`slide_to_screen`].
fn screen_to_slide(rect: Bounds<f32>, uv: Bounds<f32>, p: Point<f32>) -> Point<f32> {
  let fx = ((p.x - rect.origin.x) / rect.size.width.max(1.0)).clamp(0.0, 1.0);
  let fy = ((p.y - rect.origin.y) / rect.size.height.max(1.0)).clamp(0.0, 1.0);
  point(uv.origin.x + fx * uv.size.width, uv.origin.y + fy * uv.size.height)
}

/// Paint `image` so that its slide-normalized window `uv` fills `rect`, with rounded corners.
pub(crate) fn paint_cropped(window: &mut Window, rect: Bounds<f32>, uv: Bounds<f32>, radius: f32, image: Arc<RenderImage>) {
  let (w, h) = (rect.size.width / uv.size.width.max(1e-6), rect.size.height / uv.size.height.max(1e-6));
  let full = Bounds::new(point(rect.origin.x - uv.origin.x * w, rect.origin.y - uv.origin.y * h), size(w, h));
  if let Err(e) = window.paint_image(rect.map(px), full.map(px), Corners::all(px(radius)), image, 0, false) {
    tracing::warn!("painting a picture failed: {e:#}");
  }
}

/// A picture for GPUI, whose pixels are already in its BGRA order.
pub(crate) fn render_image(bgra: image::RgbaImage) -> Arc<RenderImage> {
  Arc::new(RenderImage::new(vec![image::Frame::new(bgra)]))
}

/// Whether `uri` is a web or mail address, the only links followed.
fn is_web_link(uri: &str) -> bool {
  let lower = uri.trim_start().to_ascii_lowercase();
  ["https://", "http://", "mailto:"].iter().any(|scheme| lower.starts_with(scheme))
}

/// The pixel size to render a picture shown `fit` big at `ppp` pixels per point. The width is
/// rounded up to steps about 9% apart, so resizing a panel or zooming re-renders only now and then
/// instead of on every frame.
fn render_size(fit: Size<f32>, ppp: f32) -> [u32; 2] {
  let exact = (fit.width * ppp).max(1.0);
  let w = 2f32.powf((exact.log2() * 8.0).ceil() / 8.0).round().clamp(1.0, MAX_RENDER_WIDTH as f32);
  let h = (w * fit.height / fit.width.max(1e-3)).round().max(1.0);
  [w as u32, h as u32]
}

/// A filled circle, optionally with a thin rim.
fn paint_circle(window: &mut Window, center: Point<f32>, r: f32, fill: Hsla, rim: Option<Hsla>) {
  let quad = gpui_kit::fill(Bounds::centered_at(center, size(2.0 * r, 2.0 * r)).map(px), fill).corner_radii(px(r));
  window.paint_quad(match rim {
    Some(rim) => quad.border_widths(px(1.5)).border_color(rim),
    None => quad,
  });
}

/// Draw the laser-pointer dot at a slide-normalized position within the visible `uv` window.
fn paint_pointer_dot(window: &mut Window, rect: Bounds<f32>, uv: Bounds<f32>, norm: Point<f32>, size: f32, rgb: [u8; 3]) {
  let center = slide_to_screen(rect, uv, norm);
  let r = (rect.size.height * 0.012 * size).max(3.0);
  // Semi-transparent (alpha fixed): a faint halo, a translucent core, and a darker rim.
  let dark = rgb.map(|c| ((c as f32) * 0.6) as u8);
  paint_circle(window, center, r * 2.2, color(rgb, 40), None);
  paint_circle(window, center, r, color(rgb, 140), Some(color(dark, 140)));
}

/// Draw a freehand stroke onto `rect`, mapped through the visible `uv` window (so it tracks
/// the slide under zoom/pan).
fn paint_stroke(window: &mut Window, rect: Bounds<f32>, uv: Bounds<f32>, stroke: &Stroke, rgb: [u8; 3]) {
  // Semi-transparent (alpha fixed), matching the pointer dot's core.
  let fill = color(rgb, 140);
  // Line width scales with zoom (thinner window -> thicker on screen).
  let w = (stroke.width_factor * rect.size.height / uv.size.height.max(1e-6)).max(1.0);
  let mut points = stroke.points.iter().map(|p| slide_to_screen(rect, uv, *p).map(px));
  match (points.next(), stroke.points.len()) {
    (None, _) => {}
    (Some(p), 1) => paint_circle(window, p.map(f32::from), (w * 0.5).max(1.0), fill, None),
    (Some(first), _) => {
      let mut path = PathBuilder::stroke(px(w));
      path.move_to(first);
      points.for_each(|p| path.line_to(p));
      match path.build() {
        Ok(path) => window.paint_path(path, fill),
        Err(e) => tracing::warn!("drawing a stroke failed: {e}"),
      }
    }
  }
}

type TexKey = (usize, Region, u32);

/// **LRU** cache of rendered pictures keyed by (page, region, pixel width), bounded by memory.
///
/// Evicted (and cleared) pictures wait in `evicted` until a paint takes them off the GPU. LRU
/// means eviction only ever removes the least-recently-used picture, never one drawn this frame
/// (a frame uses far less than the budget).
struct TextureCache {
  map: HashMap<TexKey, (Arc<RenderImage>, usize)>,
  order: VecDeque<TexKey>,
  /// Bytes held, and the most to hold.
  bytes: usize,
  budget: usize,
  evicted: Vec<Arc<RenderImage>>,
}

impl TextureCache {
  fn new(budget: usize) -> Self {
    Self {
      map: HashMap::new(),
      order: VecDeque::new(),
      bytes: 0,
      budget,
      evicted: Vec::new(),
    }
  }

  fn clear(&mut self) {
    self.evicted.extend(self.map.drain().map(|(_, (image, _))| image));
    self.order.clear();
    self.bytes = 0;
  }

  /// Mark `key` as most-recently-used.
  fn touch(&mut self, key: TexKey) {
    if let Some(pos) = self.order.iter().position(|k| *k == key) {
      self.order.remove(pos);
    }
    self.order.push_back(key);
  }

  fn get_or_render(&mut self, doc: &Document, page: usize, region: Region, target_px: [u32; 2]) -> Option<Arc<RenderImage>> {
    let key = (page, region, target_px[0]);
    if let Some((image, _)) = self.map.get(&key).cloned() {
      self.touch(key);
      return Some(image);
    }

    let picture = match region {
      Region::Slide => doc.render_slide(page, target_px).map(Some),
      Region::Notes => doc.render_notes(page, target_px),
    };
    let picture = match picture {
      Ok(picture) => picture?,
      Err(e) => {
        tracing::error!("render {region:?} page {page}: {e:#}");
        return None;
      }
    };
    let size = picture.as_raw().len();
    let image = render_image(picture);

    self.map.insert(key, (image.clone(), size));
    self.order.push_back(key);
    self.bytes += size;
    // Keep at least the picture just made, however large.
    while self.bytes > self.budget && self.order.len() > 1 {
      if let Some((old, old_size)) = self.order.pop_front().and_then(|old| self.map.remove(&old)) {
        self.bytes -= old_size;
        self.evicted.push(old);
      }
    }
    Some(image)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn wide_area() -> Bounds<f32> {
    Bounds::new(point(0.0, 0.0), size(1600.0, 900.0))
  }

  #[test]
  fn a_zoomed_portrait_page_widens_into_the_side_bars() {
    let view = SlideView::new(wide_area(), 0.5);
    assert_eq!(view.zoomed(1.0, SLIDE_CENTER).1.size.width, 450.0);
    let (uv, rect) = view.zoomed(4.0, SLIDE_CENTER);
    assert_eq!(rect, wide_area(), "fills the whole window");
    assert!((uv.size.width - 1600.0 / 1800.0).abs() < 1e-5 && (uv.size.height - 0.25).abs() < 1e-5, "{uv:?}");
  }

  #[test]
  fn zooming_at_the_cursor_keeps_the_point_under_it() {
    let view = SlideView::new(wide_area(), 0.5);
    let cursor = point(820.0, 300.0);
    let (uv, rect) = view.zoomed(1.0, SLIDE_CENTER);
    let anchor = screen_to_slide(rect, uv, cursor);
    let center = view.center_keeping(4.0, anchor, cursor);
    let (uv, rect) = view.zoomed(4.0, center);
    let moved = slide_to_screen(rect, uv, anchor);
    assert!((moved.x - cursor.x).hypot(moved.y - cursor.y) < 0.01, "{moved:?}");
  }

  #[test]
  fn render_sizes_round_up_in_few_steps_and_keep_the_aspect() {
    let mut sizes = std::collections::BTreeSet::new();
    for w in 1000..1500 {
      let fit = size(w as f32, w as f32 * 9.0 / 16.0);
      let [pw, ph] = render_size(fit, 1.0);
      assert!(pw as f32 >= fit.width && (pw as f32) < fit.width * 1.1, "{w} -> {pw}");
      assert!((pw as f32 / ph as f32 - 16.0 / 9.0).abs() < 0.01, "{pw}x{ph}");
      sizes.insert(pw);
    }
    assert!(sizes.len() <= 6, "{sizes:?}");
    assert_eq!(render_size(size(9000.0, 5000.0), 2.0)[0], MAX_RENDER_WIDTH);
  }

  #[test]
  fn only_web_and_mail_links_are_followed() {
    assert!(is_web_link("https://typst.app") && is_web_link("HTTP://x") && is_web_link("mailto:a@b.ch"));
    assert!(!is_web_link("file:///etc/passwd") && !is_web_link("javascript:alert(1)") && !is_web_link("../talk.pdf"));
  }

  #[test]
  fn the_visible_window_stays_on_the_slide() {
    let (uv, _) = SlideView::new(wide_area(), 16.0 / 9.0).zoomed(2.0, point(0.99, 0.0));
    assert!(uv.origin.x >= 0.0 && uv.origin.y >= 0.0 && uv.right() <= 1.0 && uv.bottom() <= 1.0, "{uv:?}");
  }
}
