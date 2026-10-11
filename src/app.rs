//! Presenter dashboard.
//!
//! - **Header** (small, fixed): open, shortcuts, footer-font controls, layout flip,
//!   present/quit, timer controls, filename + page count.
//! - **Center**: current slide, next-slide preview, and notes in fixed, resizable layouts
//!   cycled by "Flip layout" (L).
//! - **Footer** (large, adjustable): slide number and elapsed time, centered.
//! - **Audience window**: a second OS window showing only the slide, on black.
//! - **Start screen**: a clickable list of recent files when nothing is open.
//!
//! All persistent state lives in `~/.config/presenter/state.json` (see [`crate::config`]).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use notify::Watcher as _;

use crate::config::{self, Geometry};
use crate::document::Document;
use crate::session::{Session, format_elapsed};

/// Which piece of the presentation a panel shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PaneKind {
  Current,
  Next,
  Notes,
}

/// Which page region a cached texture holds.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Region {
  Slide,
  Notes,
}

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
  points: Vec<egui::Pos2>,
  width_factor: f32,
}

/// Everything the current-slide pane needs to service pointer/drawing/zoom input.
struct SlideInput<'a> {
  mode: InputMode,
  /// Pointer/line size factor (from the header control).
  size: f32,
  /// Opaque base colour for the pointer dot and strokes (alpha applied when drawing).
  color: egui::Color32,
  pointer: &'a mut Option<egui::Pos2>,
  /// Strokes for the current page (drawn onto this slide).
  strokes: &'a mut Vec<Stroke>,
  /// Whether a stroke is currently being drawn (mouse held since press).
  drawing: &'a mut bool,
  /// Zoom factor (1.0 = whole slide) and the pan offset (top-left of the visible window,
  /// in slide-normalized coords). Panning updates the offset.
  zoom: f32,
  zoom_offset: &'a mut egui::Vec2,
  /// Records the current slide's on-screen rect (used for zoom-at-cursor next frame).
  rect_out: &'a mut Option<egui::Rect>,
}

const HEADER_FONT: f32 = 13.0;
const MIN_FOOTER_FONT: f32 = 12.0;
const MAX_FOOTER_FONT: f32 = 48.0;
const MIN_POINTER_SIZE: f32 = 0.25;
const MAX_POINTER_SIZE: f32 = 4.0;
const POINTER_STEP: f32 = 0.25;
const NUM_LAYOUTS: usize = 4;
const MAX_RECENT: usize = 15;
/// Resizable bounds for the thumbnail strip panel (total height, in points).
const MIN_STRIP_H: f32 = 70.0;
const MAX_STRIP_H: f32 = 420.0;
const SAVE_THROTTLE: Duration = Duration::from_millis(1200);
const MIN_ZOOM: f32 = 1.0;
const MAX_ZOOM: f32 = 8.0;
/// Cap on how much extra render resolution zoom requests (keeps large renders in check).
const MAX_ZOOM_RENDER: f32 = 3.0;
/// Absolute cap on a rendered slide's pixel width (avoids huge PDFium renders when zoomed).
const MAX_RENDER_WIDTH: u32 = 5000;

pub struct PresenterApp {
  document: Option<Document>,
  session: Session,
  cache: TextureCache,
  /// Separate, small-texture cache for the thumbnail strip, so scrolling it never evicts
  /// the big current/next/notes textures from `cache`.
  thumb_cache: TextureCache,
  /// Whether the bottom thumbnail strip is shown (toggle with T).
  show_thumbnails: bool,
  /// Height of the thumbnail strip panel (drag its top edge to resize); persisted.
  thumb_height: f32,
  /// The slide the strip last auto-scrolled to follow (so it only re-centres on change).
  thumb_follow_last: usize,
  /// The strip's on-screen rect (so the wheel scrolls the strip instead of changing slides).
  thumb_rect: Option<egui::Rect>,
  status: String,
  /// Last window title pushed to the OS (so we only send a viewport command on change).
  title: String,
  footer_font: f32,
  /// Laser-pointer dot size factor (1.0 = default).
  pointer_size: f32,
  /// RGB colour of the pointer dot and freehand drawings (alpha stays fixed); persisted.
  pointer_color: [u8; 3],
  /// UI theme preference (system / dark / light); persisted.
  theme: config::Theme,
  layout_index: usize,
  presenting: bool,
  /// When true, the audience window shows solid black instead of the slide.
  blanked: bool,
  camera: crate::camera::controller::CameraController,
  audience_fullscreen: bool,
  audience_geometry: Option<Geometry>,
  window_geometry: Option<Geometry>,
  recent: Vec<config::RecentEntry>,
  /// Set true when the audience window (re)opens, so we place it once and then let
  /// the user move it freely.
  audience_needs_place: bool,
  /// Pending request to fullscreen the audience window on the frame after placement
  /// (so OS fullscreen targets the monitor we just moved it to).
  audience_apply_fullscreen: bool,
  /// When true, presenter and audience windows are swapped onto the opposite monitors
  /// (for when the OS reports the wrong primary display). Session-only.
  screens_swapped: bool,
  /// A swap was requested (via S or the button); performed once in `update` with the main ctx.
  pending_swap: bool,
  /// Screen-swap staging (each phase waits for the previous macOS window transition to finish):
  /// move the presenter onto the target monitor at this instant (after the audience has closed).
  move_presenter_at: Option<Instant>,
  /// Re-present (reopen the audience on the swapped monitor) at this instant, after the presenter
  /// has settled on its new monitor.
  represent_at: Option<Instant>,
  show_shortcuts: bool,
  show_about: bool,
  /// Pointer vs. drawing (toggle with D).
  mode: InputMode,
  /// Laser-pointer position, normalized (0..1) within the slide, while the mouse
  /// button is held over the presenter's current slide. `None` = no pointer shown.
  pointer_norm: Option<egui::Pos2>,
  /// Freehand annotations per page (kept while navigating; cleared when the file closes).
  strokes: HashMap<usize, Vec<Stroke>>,
  /// Whether a stroke is currently being drawn.
  drawing: bool,
  /// Accumulated mouse-wheel delta, for scroll-to-navigate.
  scroll_accum: f32,
  /// Zoom factor of the current slide (1.0 = fit); pan offset within it (slide coords).
  zoom: f32,
  zoom_offset: egui::Vec2,
  /// The page the current zoom applies to (zoom resets when the page changes).
  zoom_page: usize,
  /// The current slide's on-screen rect in the presenter (for zoom-at-cursor).
  current_slide_rect: Option<egui::Rect>,
  /// Lazily-loaded faint logo shown on the start screen.
  logo: Option<egui::TextureHandle>,
  /// Lazily-loaded full-color logo for the About window.
  about_logo: Option<egui::TextureHandle>,
  /// Reload the document when its file changes on disk (persisted).
  hot_reload: bool,
  /// Set by the file watcher thread when the open PDF changed; consumed on the UI thread.
  reload_flag: Arc<AtomicBool>,
  /// Keeps the OS file watcher alive; dropping it stops watching.
  watcher: Option<notify::RecommendedWatcher>,
  /// Request to (re)build the watcher on the next frame (needs the egui context).
  rewatch: bool,
  /// When a reload is due (debounce/retry after a file change); `None` when idle.
  reload_due: Option<Instant>,
  /// Consecutive failed reload attempts (the file may still be mid-write); bounded.
  reload_tries: u32,
  /// Persistence bookkeeping.
  dirty: bool,
  last_save: Instant,
}

impl PresenterApp {
  pub fn new(_cc: &eframe::CreationContext<'_>, initial: Option<&Path>) -> Self {
    let st = config::State::load();
    // Prune recent entries whose file no longer exists on disk (moved/deleted).
    let mut recent = st.recent;
    let had = recent.len();
    recent.retain(|e| e.path.is_file());
    let pruned = recent.len() != had;
    let mut app = Self {
      document: None,
      session: Session::new(0),
      cache: TextureCache::new(48),
      thumb_cache: TextureCache::new(128),
      show_thumbnails: st.show_thumbnails,
      thumb_height: st.thumb_height.clamp(MIN_STRIP_H, MAX_STRIP_H),
      thumb_follow_last: usize::MAX,
      thumb_rect: None,
      status: "Open a PDF to begin (O).".to_owned(),
      title: String::new(),
      footer_font: st.footer_font,
      pointer_size: st.pointer_size.clamp(MIN_POINTER_SIZE, MAX_POINTER_SIZE),
      pointer_color: st.pointer_color,
      theme: st.theme,
      layout_index: st.layout_index % NUM_LAYOUTS,
      presenting: false,
      blanked: false,
      camera: crate::camera::controller::CameraController::new(st.camera.clone()),
      audience_fullscreen: st.audience_fullscreen,
      audience_geometry: st.audience_geometry,
      window_geometry: st.window_geometry,
      recent,
      audience_needs_place: false,
      audience_apply_fullscreen: false,
      screens_swapped: false,
      pending_swap: false,
      move_presenter_at: None,
      represent_at: None,
      show_shortcuts: false,
      show_about: false,
      mode: InputMode::Pointer,
      pointer_norm: None,
      strokes: HashMap::new(),
      drawing: false,
      scroll_accum: 0.0,
      zoom: 1.0,
      zoom_offset: egui::Vec2::ZERO,
      zoom_page: 0,
      current_slide_rect: None,
      logo: None,
      about_logo: None,
      hot_reload: st.hot_reload,
      reload_flag: Arc::new(AtomicBool::new(false)),
      watcher: None,
      rewatch: false,
      reload_due: None,
      reload_tries: 0,
      dirty: pruned,
      last_save: Instant::now(),
    };
    // Persist the pruned list right away so the removals stick even if nothing else changes.
    if pruned {
      app.save_now();
    }
    if let Some(path) = initial {
      app.open(path);
    }
    app
  }

  fn open(&mut self, path: &Path) {
    match Document::open(path) {
      Ok(doc) => {
        self.status = format!("notes: {:?}", doc.notes_layout());
        self.session = Session::new(doc.page_count());
        if let Some(page) = self.recent_page(path) {
          self.session.goto(page as isize);
        }
        self.document = Some(doc);
        self.cache.clear();
        self.thumb_cache.clear();
        self.reload_due = None;
        self.rewatch = true; // (re)point the file watcher at the new file
        self.record_recent(path);
        self.save_now(); // persist the new recent entry immediately
      }
      Err(e) => {
        self.status = format!("Failed to open {}: {e:#}", path.display());
        tracing::error!("{e:#}");
      }
    }
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
      audience_fullscreen: self.audience_fullscreen,
      audience_geometry: self.audience_geometry,
      window_geometry: self.window_geometry,
      recent: self.recent.clone(),
      show_thumbnails: self.show_thumbnails,
      thumb_height: self.thumb_height,
      pointer_color: self.pointer_color,
      theme: self.theme,
      hot_reload: self.hot_reload,
      camera: self.camera.settings.clone(),
    }
  }

  fn save_now(&mut self) {
    self.sync_recent_page();
    self.snapshot().save();
    self.last_save = Instant::now();
    self.dirty = false;
  }

  fn pick_file(&mut self) {
    if let Some(path) = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).pick_file() {
      self.open(&path);
    }
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

  /// Remove all freehand drawings.
  fn clear_drawings(&mut self) {
    self.strokes.clear();
    self.drawing = false;
  }

  /// The visible window of the current slide, in slide-normalized coords.
  fn zoom_uv(&self) -> egui::Rect {
    zoom_uv_from(self.zoom, self.zoom_offset)
  }

  fn reset_zoom(&mut self) {
    self.zoom = 1.0;
    self.zoom_offset = egui::Vec2::ZERO;
  }

  /// Multiply the zoom by `factor`, keeping the slide point under `cursor` fixed
  /// (zoom-to-cursor). `rect` is the on-screen rect of the slide the cursor is over (the
  /// presenter's or the audience's), used as the anchor. Falls back to zooming about the centre.
  fn apply_zoom(&mut self, factor: f32, cursor: Option<egui::Pos2>, rect: Option<egui::Rect>) {
    let new_z = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
    if (new_z - self.zoom).abs() < f32::EPSILON {
      return;
    }
    let uv = self.zoom_uv();
    // Anchor: the slide point that should stay under the same screen spot.
    let (anchor, f) = match (rect, cursor) {
      (Some(rect), Some(c)) if rect.contains(c) => {
        let f = egui::vec2((c.x - rect.left()) / rect.width().max(1.0), (c.y - rect.top()) / rect.height().max(1.0));
        (egui::pos2(uv.min.x + f.x * uv.width(), uv.min.y + f.y * uv.height()), f)
      }
      _ => (uv.center(), egui::vec2(0.5, 0.5)),
    };
    let s = 1.0 / new_z;
    let mut off = egui::vec2(anchor.x - f.x * s, anchor.y - f.y * s);
    clamp_offset(&mut off, s);
    self.zoom = new_z;
    self.zoom_offset = off;
    if self.zoom <= MIN_ZOOM + f32::EPSILON {
      self.reset_zoom();
    }
  }

  fn flip_layout(&mut self) {
    self.layout_index = (self.layout_index + 1) % NUM_LAYOUTS;
    self.dirty = true;
  }

  fn start_presentation(&mut self) {
    if self.document.is_some() {
      self.presenting = true;
      self.audience_needs_place = true;
      // The talk timer runs while presenting; starting/resuming here, pausing on quit.
      self.session.timer_mut().start();
      // Fullscreen the audience window either way: on the second screen if present, otherwise
      // on the current (primary) screen, covering the presenter. Placement (which monitor)
      // happens next frame in `show_audience`, then fullscreen is applied. Shortcuts keep
      // working over the fullscreen audience because `handle_shortcuts` also runs for it.
      self.audience_apply_fullscreen = true;
      self.audience_fullscreen = true;
    }
  }

  /// Quit the presentation (close the audience window) and pause the talk timer.
  fn stop_presentation(&mut self) {
    self.presenting = false;
    // Cancel any pending swap steps. (swap_screens calls this first, then schedules the move, so
    // its own sequence survives; a user-initiated quit cancels an in-flight swap.)
    self.move_presenter_at = None;
    self.represent_at = None;
    self.session.timer_mut().pause();
  }

  /// Swap which physical monitor shows the presenter vs. the audience window, for when the OS
  /// reports the wrong primary display. Moves the presenter window onto its new monitor and, if
  /// presenting, re-places the audience (fullscreen) on the other. Needs a second screen.
  /// Call with the main (presenter) context.
  fn swap_screens(&mut self, ctx: &egui::Context) {
    if crate::screen::external().is_none() {
      self.status = "Swapping screens needs a second display.".to_owned();
      return;
    }
    self.screens_swapped = !self.screens_swapped;
    if self.presenting {
      // Stage it so each macOS window transition finishes before the next: quit the presentation
      // (closes the audience fullscreen Space), then move the presenter once that Space has closed,
      // then re-present on the swapped monitor. Doing these back-to-back races the Space transitions
      // and leaves a screen stuck black.
      self.stop_presentation();
      self.move_presenter_at = Some(Instant::now() + Duration::from_millis(700));
    } else {
      // Not presenting: a single viewport, so move immediately (the stable path).
      self.move_presenter(ctx);
    }
  }

  /// Move the presenter window onto its target monitor and maximise it to fill that screen.
  fn move_presenter(&mut self, ctx: &egui::Context) {
    if let Some(m) = crate::screen::presenter_monitor(self.screens_swapped) {
      ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(false));
      ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(m.x + 20.0, m.y + 20.0)));
      ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
    }
  }

  /// Close the current document and return to the start screen.
  fn close_file(&mut self) {
    self.save_now(); // persist last page before closing
    self.document = None;
    self.presenting = false;
    self.blanked = false;
    self.pointer_norm = None;
    self.strokes.clear();
    self.drawing = false;
    self.mode = InputMode::Pointer;
    self.session = Session::new(0);
    self.cache.clear();
    self.reload_due = None;
    self.rewatch = true; // drop the file watcher (no document)
    self.status = "Open a PDF to begin (O).".to_owned();
  }

  /// Rebuild the OS file watcher for the open document (or drop it). notify recommends watching
  /// the parent directory and filtering by filename, so editor/compiler rename-replace writes are
  /// still seen. The watcher thread sets `reload_flag` and wakes the UI on a matching change.
  fn update_watcher(&mut self, ctx: &egui::Context) {
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
    let repaint = ctx.clone();
    let handler = move |res: notify::Result<notify::Event>| {
      if let Ok(ev) = res
        && ev.paths.iter().any(|p| p.file_name() == Some(name.as_os_str()))
      {
        flag.store(true, Ordering::Relaxed);
        repaint.request_repaint();
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
  fn do_reload(&mut self, ctx: &egui::Context) {
    let Some(path) = self.document.as_ref().map(|d| d.path().to_path_buf()) else {
      return;
    };
    match Document::open(&path) {
      Ok(doc) => {
        self.session.set_page_count(doc.page_count());
        self.document = Some(doc);
        self.cache.clear();
        self.thumb_cache.clear();
        self.reset_zoom();
        self.strokes.clear();
        self.drawing = false;
        self.reload_tries = 0;
        self.status = format!("Reloaded {}", path.file_name().unwrap_or_default().to_string_lossy());
      }
      Err(e) => {
        self.reload_tries += 1;
        if self.reload_tries < 12 {
          // File is likely still being written; retry shortly.
          self.reload_due = Some(Instant::now() + Duration::from_millis(400));
          ctx.request_repaint_after(Duration::from_millis(400));
        } else {
          tracing::warn!("hot reload failed after {} tries: {e:#}", self.reload_tries);
          self.status = "Reload failed (file unreadable)".to_owned();
          self.reload_tries = 0;
        }
      }
    }
  }

  /// Open the first PDF dropped onto the window (drag & drop).
  fn handle_dropped_files(&mut self, ctx: &egui::Context) {
    let dropped = ctx.input(|i| {
      i.raw
        .dropped_files
        .iter()
        .filter_map(|f| f.path.clone())
        .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")))
    });
    if let Some(path) = dropped {
      self.open(&path);
    }
  }

  /// Keyboard shortcuts for one window's input context. Runs for the presenter window and,
  /// while presenting, also for the audience window — so navigation and controls work no
  /// matter which window has focus (e.g. when the audience is fullscreen on a single screen,
  /// covering the presenter). A key event only lands in the focused viewport, so running this
  /// for both windows never double-triggers.
  fn handle_shortcuts(&mut self, ctx: &egui::Context) {
    let mut open = false;
    let mut font_delta = 0.0;
    let mut present = false;
    let mut close = false;
    ctx.input(|i| {
      use egui::Key;
      if i.key_pressed(Key::ArrowRight) || i.key_pressed(Key::Space) || i.key_pressed(Key::PageDown) || i.key_pressed(Key::ArrowDown) {
        self.session.next();
      }
      if i.key_pressed(Key::ArrowLeft) || i.key_pressed(Key::PageUp) || i.key_pressed(Key::ArrowUp) {
        self.session.prev();
      }
      if i.key_pressed(Key::Home) {
        self.session.first();
      }
      if i.key_pressed(Key::End) {
        self.session.last();
      }
      if i.key_pressed(Key::R) {
        self.session.timer_mut().reset();
      }
      if i.key_pressed(Key::L) {
        self.flip_layout();
      }
      if i.key_pressed(Key::Plus) || i.key_pressed(Key::Equals) {
        font_delta += 2.0;
      }
      if i.key_pressed(Key::Minus) {
        font_delta -= 2.0;
      }
      if i.key_pressed(Key::F5) {
        present = true;
      }
      if i.key_pressed(Key::Escape) {
        self.stop_presentation();
      }
      if i.key_pressed(Key::B) {
        self.blanked = !self.blanked;
      }
      if i.key_pressed(Key::C) && !i.modifiers.command {
        self.camera.settings.enabled = !self.camera.settings.enabled;
        self.dirty = true;
      }
      if i.key_pressed(Key::P) {
        self.toggle_mode();
      }
      if i.key_pressed(Key::D) {
        self.clear_drawings();
      }
      if i.key_pressed(Key::T) {
        self.show_thumbnails = !self.show_thumbnails;
        self.dirty = true;
      }
      if i.key_pressed(Key::S) {
        self.pending_swap = true;
      }
      close = i.key_pressed(Key::W);
      open = i.key_pressed(Key::O);
    });
    if font_delta != 0.0 {
      self.adjust_font(font_delta);
    }
    if present {
      self.start_presentation();
    }
    if close {
      self.close_file();
    }
    if open {
      self.pick_file();
    }
  }

  /// Mouse-wheel navigation and Ctrl/Cmd+scroll zoom for one window's input context.
  /// `anchor` is the on-screen rect of that window's current slide (the zoom-to-cursor
  /// anchor). Shared by the presenter and the audience windows so both react to the wheel.
  ///
  /// Plain scroll navigates slides (down = next, up = previous). A real wheel reports
  /// discrete Line notches — one slide each, by sign. Trackpad pixel scrolling (Point)
  /// accumulates against a threshold.
  fn handle_wheel(&mut self, ctx: &egui::Context, anchor: Option<egui::Rect>, strip: Option<egui::Rect>) {
    if self.document.is_none() {
      self.scroll_accum = 0.0;
      return;
    }
    const POINT_STEP: f32 = 40.0;
    ctx.input(|i| {
      let zoom_mod = i.modifiers.command || i.modifiers.ctrl;
      let cursor = i.pointer.hover_pos();
      // Over the thumbnail strip, let it scroll horizontally instead of changing slides.
      if let (Some(s), Some(c)) = (strip, cursor)
        && s.contains(c)
      {
        return;
      }
      for event in &i.events {
        if let egui::Event::MouseWheel { unit, delta, .. } = event {
          if zoom_mod {
            let factor = match unit {
              egui::MouseWheelUnit::Line | egui::MouseWheelUnit::Page => {
                if delta.y > 0.0 {
                  1.25
                } else if delta.y < 0.0 {
                  0.8
                } else {
                  1.0
                }
              }
              egui::MouseWheelUnit::Point => (delta.y * 0.004).exp(),
            };
            self.apply_zoom(factor, cursor, anchor);
            continue;
          }
          match unit {
            egui::MouseWheelUnit::Line | egui::MouseWheelUnit::Page => {
              if delta.y < 0.0 {
                self.session.next();
              } else if delta.y > 0.0 {
                self.session.prev();
              }
            }
            egui::MouseWheelUnit::Point => {
              self.scroll_accum += delta.y;
              while self.scroll_accum <= -POINT_STEP {
                self.session.next();
                self.scroll_accum += POINT_STEP;
              }
              while self.scroll_accum >= POINT_STEP {
                self.session.prev();
                self.scroll_accum -= POINT_STEP;
              }
            }
          }
        }
      }
    });
  }

  fn header(&mut self, ctx: &egui::Context) {
    egui::TopBottomPanel::top("header").show(ctx, |ui| {
      let small = |s: &str| egui::RichText::new(s).size(HEADER_FONT);
      let doc_open = self.document.is_some();

      // Width needed by the fixed right-aligned controls (theme, shortcuts, about), so the
      // wrapping left controls are constrained to the remaining space and never run under them.
      let text_w = |ui: &egui::Ui, s: &str| {
        ui.painter()
          .layout_no_wrap(s.to_owned(), egui::FontId::proportional(HEADER_FONT), egui::Color32::WHITE)
          .size()
          .x
      };
      let per_btn = ui.spacing().button_padding.x * 2.0 + ui.spacing().item_spacing.x;
      let right_w = text_w(ui, "About") + text_w(ui, "Shortcuts") + text_w(ui, "💻") + 3.0 * per_btn + 8.0;

      ui.horizontal(|ui| {
        let left_w = (ui.available_width() - right_w).max(160.0);
        ui.scope(|ui| {
          ui.set_min_width(left_w);
          ui.set_max_width(left_w);
          ui.horizontal_wrapped(|ui| {
            // ── General ──
            if ui.button(small("📂 Open (O)")).clicked() {
              self.pick_file();
            }
            if doc_open {
              if ui.button(small("✖ Close (W)")).clicked() {
                self.close_file();
              }
              if self.presenting {
                if ui.button(small("⏹ Quit (Esc)")).clicked() {
                  self.stop_presentation();
                }
              } else if ui.button(small("▶ Present (F5)")).clicked() {
                self.start_presentation();
              }
              let blank_label = if self.blanked { "⬛ Blanked (B)" } else { "⬛ Blank (B)" };
              if ui.button(small(blank_label)).clicked() {
                self.blanked = !self.blanked;
              }
              if ui.button(small("↺ Reset time (R)")).clicked() {
                self.session.timer_mut().reset();
              }
              if ui.button(small("🖥 Swap (S)")).on_hover_text("Swap presenter / presentation screens").clicked() {
                self.pending_swap = true;
              }

              // ── Settings popover: footer size + pointer size (stays open while stepping) ──
              ui.separator();
              let gear = ui.button(small("⚙")).on_hover_text("Footer & pointer size");
              egui::Popup::menu(&gear)
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                .show(|ui| {
                  ui.set_min_width(150.0);
                  ui.horizontal(|ui| {
                    ui.label("Footer size");
                    if ui.button(" − ").clicked() {
                      self.adjust_font(-2.0);
                    }
                    if ui.button(" + ").clicked() {
                      self.adjust_font(2.0);
                    }
                  });
                  ui.horizontal(|ui| {
                    ui.label("Pointer size");
                    if ui.button(" − ").clicked() {
                      self.adjust_pointer(-POINTER_STEP);
                    }
                    if ui.button(" + ").clicked() {
                      self.adjust_pointer(POINTER_STEP);
                    }
                  });
                  ui.separator();
                  if ui
                    .checkbox(&mut self.hot_reload, "Hot reload")
                    .on_hover_text("Reload when the PDF file changes on disk")
                    .changed()
                  {
                    self.rewatch = true;
                    self.dirty = true;
                  }
                  ui.separator();
                  if self.camera.settings_ui(ui) {
                    self.dirty = true;
                  }
                });

              // ── Display ──
              if ui.button(small("🔀 Layout (L)")).on_hover_text("Cycle panel arrangements").clicked() {
                self.flip_layout();
              }
              if ui.button(small("🎞 Thumbnails (T)")).on_hover_text("Toggle the slide thumbnail strip").clicked() {
                self.show_thumbnails = !self.show_thumbnails;
                self.dirty = true;
              }

              // ── Drawing: toggle always; colour + delete appear only while in draw mode ──
              ui.separator();
              let mode_label = match self.mode {
                InputMode::Pointer => "✏ Draw (P)",
                InputMode::Draw => "🔴 Pointer (P)",
              };
              if ui.button(small(mode_label)).on_hover_text("Toggle laser pointer / drawing").clicked() {
                self.toggle_mode();
              }
              if self.mode == InputMode::Draw {
                // Pointer/drawing colour (RGB only, transparency fixed).
                if color_dot_picker(ui, &mut self.pointer_color) {
                  self.dirty = true;
                }
                if ui.button(small("🗑 Delete (D)")).on_hover_text("Clear all drawings").clicked() {
                  self.clear_drawings();
                }
              }
            }
          });
        });

        // Fixed right-aligned controls.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
          if ui.button(small("About")).on_hover_text("About this application").clicked() {
            self.show_about = !self.show_about;
          }
          if ui.button(small("Shortcuts")).on_hover_text("Show keyboard & mouse shortcuts").clicked() {
            self.show_shortcuts = !self.show_shortcuts;
          }
          // Theme toggle (left of Shortcuts): clicking cycles system -> dark -> light.
          let (theme_icon, theme_name, next_theme) = match self.theme {
            config::Theme::System => ("💻", "system", config::Theme::Dark),
            config::Theme::Dark => ("🌙", "dark", config::Theme::Light),
            config::Theme::Light => ("☀", "light", config::Theme::System),
          };
          if ui
            .button(small(theme_icon))
            .on_hover_text(format!("Theme: {theme_name} (click to cycle)"))
            .clicked()
          {
            self.theme = next_theme;
            self.dirty = true;
          }
        });
      });
    });
  }

  fn footer(&mut self, ctx: &egui::Context) {
    if self.document.is_none() {
      return;
    }
    let fs = self.footer_font;
    let slide = format!("Slide {} / {}", self.session.current() + 1, self.session.page_count());
    let clock = chrono::Local::now().format("%H:%M:%S").to_string();
    let t = self.session.timer();
    let elapsed = format!("⏱ {}{}", format_elapsed(t.elapsed()), if t.is_running() { "" } else { "  (paused)" });

    egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
      ui.add_space(3.0);
      ui.vertical_centered(|ui| {
        ui.label(
          egui::RichText::new(format!("{slide}      ·      🕑 {clock}      ·      {elapsed}"))
            .size(fs)
            .strong(),
        );
      });
      ui.add_space(3.0);
    });
    // Keep the wall-clock (and running timer) display current while idle.
    ctx.request_repaint_after(Duration::from_secs(1));
  }

  /// A PowerPoint-style strip of slide thumbnails along the bottom of the presenter view.
  /// Horizontally scrollable and virtualized — only the on-screen thumbnails are rendered,
  /// so it stays fast on 300+ page decks. Click a thumbnail to jump; the current slide is
  /// highlighted and the strip auto-follows it on navigation. Toggle with `T`.
  fn thumbnails(&mut self, ctx: &egui::Context) {
    let Self {
      document,
      session,
      thumb_cache,
      show_thumbnails,
      thumb_height,
      thumb_follow_last,
      thumb_rect,
      dirty,
      ..
    } = self;
    if !*show_thumbnails {
      *thumb_rect = None;
      return;
    }
    let Some(doc) = document.as_ref() else {
      *thumb_rect = None;
      return;
    };
    let count = session.page_count();
    if count == 0 {
      *thumb_rect = None;
      return;
    }
    let current = session.current();
    let aspect0 = doc.slide_aspect(0).max(0.1);

    const GAP: f32 = 8.0;
    const PAD: f32 = 8.0;

    // Re-centre on the current slide only when it changes, so manual scrolling is preserved.
    let follow = current != *thumb_follow_last;
    *thumb_follow_last = current;

    let mut goto: Option<usize> = None;

    // The panel is resizable (drag its top edge); thumbnails scale to fill its height.
    let panel = egui::TopBottomPanel::bottom("thumbnails")
      .resizable(true)
      .default_height(*thumb_height)
      .height_range(MIN_STRIP_H..=MAX_STRIP_H)
      .show(ctx, |ui| {
        // Size the thumbnails to the current strip height, reserving room for the padding
        // above and the horizontal scrollbar below.
        let thumb_h = (ui.available_height() - PAD - 16.0).max(36.0);
        let thumb_w = thumb_h * aspect0;
        let stride = thumb_w + GAP;
        ui.add_space(PAD);
        egui::ScrollArea::horizontal().show_viewport(ui, |ui, viewport| {
          let content_w = 2.0 * PAD + count as f32 * stride - GAP;
          ui.set_width(content_w);
          ui.set_height(thumb_h);
          let origin = ui.min_rect().min;
          let ppp = ctx.pixels_per_point();

          let slot = |i: usize| egui::Rect::from_min_size(origin + egui::vec2(PAD + i as f32 * stride, 0.0), egui::vec2(thumb_w, thumb_h));

          // Only the thumbnails inside the scroll viewport are laid out and rendered.
          let first = (((viewport.min.x - PAD) / stride).floor() as isize).max(0) as usize;
          let last = ((((viewport.max.x - PAD) / stride).ceil() as isize).max(0) as usize).min(count);

          for i in first..last {
            let rect = slot(i);
            // Aspect-fit the slide within the fixed slot (letterbox if a page differs).
            let a = doc.slide_aspect(i).max(0.1);
            let (mut w, mut h) = (thumb_h * a, thumb_h);
            if w > thumb_w {
              w = thumb_w;
              h = thumb_w / a;
            }
            let img_rect = egui::Rect::from_center_size(rect.center(), egui::vec2(w, h));
            let px = [(w * ppp).round().max(1.0) as u32, (h * ppp).round().max(1.0) as u32];

            let resp = ui.interact(rect, ui.id().with(("thumb", i)), egui::Sense::click());
            let bg = if resp.hovered() {
              ui.visuals().widgets.hovered.weak_bg_fill
            } else {
              ui.visuals().extreme_bg_color
            };
            ui.painter().rect_filled(img_rect, 2.0, bg);
            if let Some(tex) = thumb_cache.get_or_render(ctx, doc, i, Region::Slide, px) {
              egui::Image::new(egui::load::SizedTexture::new(tex.id(), egui::vec2(w, h))).paint_at(ui, img_rect);
            }
            if i == current {
              ui.painter().rect_stroke(
                img_rect.expand(1.5),
                2.0,
                egui::Stroke::new(2.5_f32, ui.visuals().selection.bg_fill),
                egui::StrokeKind::Inside,
              );
            }
            // Slide number, bottom-left of the slot.
            ui.painter().text(
              rect.left_bottom() + egui::vec2(2.0, -1.0),
              egui::Align2::LEFT_BOTTOM,
              format!("{}", i + 1),
              egui::FontId::proportional(10.0),
              ui.visuals().weak_text_color(),
            );
            if resp.clicked() {
              goto = Some(i);
            }
          }

          // Keep the current slide visible when it changes.
          if follow {
            ui.scroll_to_rect(slot(current), Some(egui::Align::Center));
          }
        });
        // Claim the rest of the panel's height so egui stores the dragged size (otherwise the
        // content rect is shorter than the panel and the strip collapses back when dragged up).
        ui.add_space(ui.available_height().max(0.0));
      });
    *thumb_rect = Some(panel.response.rect);

    // Remember the (possibly dragged) strip height for next session.
    let h = panel.response.rect.height();
    if (h - *thumb_height).abs() > 0.5 {
      *thumb_height = h;
      *dirty = true;
    }

    if let Some(i) = goto {
      session.goto(i as isize);
    }
  }

  fn shortcuts_window(&mut self, ctx: &egui::Context) {
    egui::Window::new("Keyboard shortcuts")
      .open(&mut self.show_shortcuts)
      .collapsible(false)
      .resizable(false)
      .show(ctx, |ui| {
        let rows = [
          ("➡ / Space / PgDn", "Next slide"),
          ("⬅ / PgUp", "Previous slide"),
          ("Home / End", "First / last slide"),
          ("F5", "Start presentation"),
          ("Esc", "Quit presentation"),
          ("B", "Blank the audience screen (black)"),
          ("C", "Toggle the webcam overlay"),
          ("W", "Close file, back to start screen"),
          ("R", "Reset the talk timer"),
          ("L", "Flip layout (H/V with no notes; 4 presets with notes)"),
          ("T", "Toggle the slide thumbnail strip"),
          ("S", "Swap presenter / presentation screens"),
          ("+ / −", "Footer font larger / smaller"),
          ("O", "Open a PDF"),
          ("Hold mouse on current slide", "Laser pointer / draw (on the audience screen)"),
          ("P", "Toggle laser pointer / drawing"),
          ("D", "Delete all drawings"),
          ("Ctrl + scroll", "Zoom the current slide at the cursor"),
          ("Ctrl + drag", "Pan the zoomed slide"),
          ("Scroll", "Next / previous slide"),
          ("(audience) double-click", "Toggle fullscreen"),
        ];
        egui::Grid::new("shortcuts-grid").num_columns(2).spacing([24.0, 6.0]).show(ui, |ui| {
          for (k, d) in rows {
            ui.strong(k);
            ui.label(d);
            ui.end_row();
          }
        });
      });
  }

  /// Lazily load the faint start-screen logo texture.
  fn logo_texture(&mut self, ctx: &egui::Context) -> egui::TextureHandle {
    self
      .logo
      .get_or_insert_with(|| {
        let bytes = include_bytes!("../img/logo-gray.png");
        let color = match image::load_from_memory(bytes) {
          Ok(img) => {
            let rgba = img.into_rgba8();
            let (w, h) = rgba.dimensions();
            egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw())
          }
          Err(_) => egui::ColorImage::filled([1, 1], egui::Color32::TRANSPARENT),
        };
        ctx.load_texture("start-logo", color, egui::TextureOptions::LINEAR)
      })
      .clone()
  }

  /// Lazily load the full-color logo used in the About window.
  fn about_logo_texture(&mut self, ctx: &egui::Context) -> egui::TextureHandle {
    self
      .about_logo
      .get_or_insert_with(|| {
        let bytes = include_bytes!("../img/logo.png");
        let color = match image::load_from_memory(bytes) {
          Ok(img) => {
            let rgba = img.into_rgba8();
            let (w, h) = rgba.dimensions();
            egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw())
          }
          Err(_) => egui::ColorImage::filled([1, 1], egui::Color32::TRANSPARENT),
        };
        ctx.load_texture("about-logo", color, egui::TextureOptions::LINEAR)
      })
      .clone()
  }

  /// The About window: app info, logo, and the generated third-party license list.
  fn about_window(&mut self, ctx: &egui::Context) {
    if !self.show_about {
      return;
    }
    const THIRDPARTY: &str = include_str!("../assets/thirdparty.md");
    let logo = self.about_logo_texture(ctx);
    let mut open = self.show_about;
    egui::Window::new("About presenters")
      .open(&mut open)
      .collapsible(false)
      .resizable(true)
      .default_size([560.0, 520.0])
      .show(ctx, |ui| {
        ui.vertical_centered(|ui| {
          let [lw, lh] = logo.size();
          let aspect = lw as f32 / lh.max(1) as f32;
          let w = 150.0_f32;
          ui.add(egui::Image::new(egui::load::SizedTexture::new(logo.id(), egui::vec2(w, w / aspect))));
          ui.heading("presenters");
          ui.label(env!("CARGO_PKG_DESCRIPTION"));
          ui.add_space(4.0);
          ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
          ui.hyperlink_to("© 2026 tschinz", "https://github.com/tschinz");
          ui.label(format!("License: {}", env!("CARGO_PKG_LICENSE")));
          ui.hyperlink(env!("CARGO_PKG_REPOSITORY"));
          ui.add_space(8.0);
          // GitHub Sponsors button (styled like the GitHub one; opens the sponsors page).
          let sponsor = egui::Button::new(egui::RichText::new("❤ Sponsor").color(egui::Color32::WHITE).strong())
            .fill(egui::Color32::from_rgb(0xDB, 0x61, 0xA2))
            .min_size(egui::vec2(120.0, 28.0));
          if ui.add(sponsor).on_hover_text("Support development on GitHub Sponsors").clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab("https://github.com/sponsors/tschinz"));
          }
        });
        ui.add_space(6.0);
        ui.label("Renders PDFs with PDFium (BSD-3-Clause), bundled with the application.");
        ui.separator();

        egui::CollapsingHeader::new("Third-party libraries").default_open(false).show(ui, |ui| {
          // Render only the visible lines: laying out the whole (very long) text as one
          // galley overflows the font atlas and panics epaint.
          let lines: Vec<&str> = THIRDPARTY.lines().collect();
          let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
          egui::ScrollArea::vertical()
            .max_height(280.0)
            .auto_shrink([false, false])
            .show_rows(ui, row_height, lines.len(), |ui, range| {
              for line in &lines[range] {
                ui.monospace(*line);
              }
            });
        });
      });
    self.show_about = open;
  }

  /// The screen shown when no document is open: a faint logo watermark plus a
  /// recent-files list to click, or a prompt to open one.
  fn start_screen(&mut self, ctx: &egui::Context) {
    let logo = self.logo_texture(ctx);
    let mut to_open: Option<PathBuf> = None;
    let mut to_remove: Option<PathBuf> = None;
    let mut to_reveal: Option<PathBuf> = None;
    egui::CentralPanel::default().show(ctx, |ui| {
      ui.add_space(28.0);
      ui.vertical_centered(|ui| {
        ui.heading("presenters");
        ui.add_space(10.0);
        // Faint gray logo, just below the title.
        let [lw, lh] = logo.size();
        let aspect = lw as f32 / lh.max(1) as f32;
        let w = 220.0_f32.min(ui.available_width() * 0.6);
        let h = w / aspect;
        ui.add(egui::Image::new(egui::load::SizedTexture::new(logo.id(), egui::vec2(w, h))).tint(egui::Color32::from_white_alpha(90)));
        ui.add_space(12.0);
        if ui.button("📂 Open a PDF… (O)").clicked() {
          self.pick_file();
        }
        ui.add_space(4.0);
        ui.weak("… or drag & drop a PDF onto the window");
      });
      ui.add_space(16.0);

      if self.recent.is_empty() {
        ui.vertical_centered(|ui| ui.weak("No recent files yet."));
        return;
      }

      ui.separator();
      ui.add_space(8.0);
      ui.vertical_centered(|ui| ui.strong(egui::RichText::new("Recent files").size(16.0)));
      ui.add_space(10.0);

      // A vertical list (like Zed's recent projects): one row per file. Click the row to
      // open it; hovering reveals two icons — open the containing folder, and remove.
      const ROW_H: f32 = 32.0;
      const ICON: f32 = 22.0;
      const PAD: f32 = 10.0;
      egui::ScrollArea::vertical().show(ui, |ui| {
        let list_w = ui.available_width().min(560.0);
        // Geometric hover (pointer inside the row rect), so moving onto an action icon — which
        // sits on top of the row — doesn't steal the row's hover and make the icons flicker.
        let pointer = ui.input(|i| i.pointer.hover_pos());
        ui.vertical_centered(|ui| {
          for entry in &self.recent {
            let name = entry.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            let folder = entry.path.parent().map(|p| p.display().to_string()).unwrap_or_default();

            let (rect, row) = ui.allocate_exact_size(egui::vec2(list_w, ROW_H), egui::Sense::click());
            // Row highlight on hover (geometric, so it doesn't flicker over the icons).
            if pointer.is_some_and(|p| rect.contains(p)) {
              ui.painter().rect_filled(rect, 5.0, ui.visuals().widgets.hovered.weak_bg_fill);
            }

            // Right-aligned action icons, always shown: remove, then open-folder to its left.
            let x_rect = egui::Rect::from_center_size(egui::pos2(rect.right() - PAD - ICON * 0.5, rect.center().y), egui::vec2(ICON, ICON));
            let folder_rect = x_rect.translate(egui::vec2(-(ICON + 2.0), 0.0));
            let folder_resp = ui
              .put(folder_rect, egui::Button::new(egui::RichText::new("📂").size(13.0)).frame(false))
              .on_hover_text("Open containing folder");
            let x_resp = ui
              .put(x_rect, egui::Button::new(egui::RichText::new("✖").size(12.0)).frame(false))
              .on_hover_text("Remove from recent");

            // Filename, left-aligned, elided only if it would reach the icons.
            let text_left = rect.left() + PAD;
            let text_right = rect.right() - PAD - 2.0 * (ICON + 2.0);
            let label = elide_middle(ui, &name, 15.0, (text_right - text_left).max(20.0));
            ui.painter().text(
              egui::pos2(text_left, rect.center().y),
              egui::Align2::LEFT_CENTER,
              label,
              egui::FontId::proportional(15.0),
              ui.visuals().text_color(),
            );
            row.clone().on_hover_text(format!("{name}\n{folder}"));

            // Icon clicks take priority over the row's open-click.
            if x_resp.clicked() {
              to_remove = Some(entry.path.clone());
            } else if folder_resp.clicked() {
              to_reveal = Some(entry.path.clone());
            } else if row.clicked() {
              to_open = Some(entry.path.clone());
            }
          }
        });
      });
    });
    if let Some(path) = to_reveal {
      crate::platform::reveal_in_file_manager(&path);
    }
    if let Some(path) = to_remove {
      self.remove_recent(&path);
    }
    if let Some(path) = to_open {
      self.open(&path);
    }
  }

  /// Track the presenter window's geometry for persistence.
  fn capture_window_geometry(&mut self, ctx: &egui::Context) {
    let (pos, size) = ctx.input(|i| {
      let vp = i.viewport();
      (vp.outer_rect.map(|r| r.min), vp.inner_rect.map(|r| r.size()))
    });
    if let (Some(p), Some(s)) = (pos, size) {
      let g = Geometry {
        x: p.x,
        y: p.y,
        w: s.x,
        h: s.y,
      };
      if geom_changed(self.window_geometry, g) {
        self.window_geometry = Some(g);
        self.dirty = true;
      }
    }
  }

  /// Lay out the panels for the current arrangement. Dividers are draggable (sizes are
  /// remembered by egui); the arrangement is chosen by "Flip layout" (L). With no notes:
  /// just current + next (L toggles horizontal/vertical).
  fn layout(&mut self, ctx: &egui::Context) {
    let Self {
      document,
      session,
      cache,
      layout_index,
      pointer_norm,
      pointer_size,
      pointer_color,
      mode,
      strokes,
      drawing,
      zoom,
      zoom_offset,
      current_slide_rect,
      ..
    } = self;
    let doc = document.as_ref();
    let screen = ctx.content_rect();
    let has_notes = doc.map(|d| d.has_notes()).unwrap_or(false);
    // Input for the current-slide pane (pointer/drawing/zoom). Consumed once by the Current pane.
    let page = session.current();
    let mut input = Some(SlideInput {
      mode: *mode,
      size: *pointer_size,
      color: egui::Color32::from_rgb(pointer_color[0], pointer_color[1], pointer_color[2]),
      pointer: pointer_norm,
      strokes: strokes.entry(page).or_default(),
      drawing,
      zoom: *zoom,
      zoom_offset,
      rect_out: current_slide_rect,
    });

    if !has_notes {
      if *layout_index % 2 == 0 {
        egui::SidePanel::right("nn-next-h")
          .resizable(true)
          .default_width(screen.width() * 0.5)
          .show(ctx, |ui| {
            render_pane(ui, ctx, cache, doc, session, PaneKind::Next, None);
          });
        egui::CentralPanel::default().show(ctx, |ui| {
          render_pane(ui, ctx, cache, doc, session, PaneKind::Current, input.take());
        });
      } else {
        egui::TopBottomPanel::bottom("nn-next-v")
          .resizable(true)
          .default_height(screen.height() * 0.45)
          .show(ctx, |ui| {
            render_pane(ui, ctx, cache, doc, session, PaneKind::Next, None);
          });
        egui::CentralPanel::default().show(ctx, |ui| {
          render_pane(ui, ctx, cache, doc, session, PaneKind::Current, input.take());
        });
      }
      return;
    }

    match *layout_index % NUM_LAYOUTS {
      0 => {
        egui::SidePanel::right("side")
          .resizable(true)
          .default_width(screen.width() * 0.34)
          .show(ctx, |ui| {
            egui::TopBottomPanel::top("side-top")
              .resizable(true)
              .default_height(ui.available_height() * 0.5)
              .show_inside(ui, |ui| {
                render_pane(ui, ctx, cache, doc, session, PaneKind::Next, None);
              });
            egui::CentralPanel::default().show_inside(ui, |ui| {
              render_pane(ui, ctx, cache, doc, session, PaneKind::Notes, None);
            });
          });
        egui::CentralPanel::default().show(ctx, |ui| {
          render_pane(ui, ctx, cache, doc, session, PaneKind::Current, input.take());
        });
      }
      1 => {
        egui::SidePanel::right("side")
          .resizable(true)
          .default_width(screen.width() * 0.34)
          .show(ctx, |ui| {
            egui::TopBottomPanel::top("side-top")
              .resizable(true)
              .default_height(ui.available_height() * 0.5)
              .show_inside(ui, |ui| {
                render_pane(ui, ctx, cache, doc, session, PaneKind::Notes, None);
              });
            egui::CentralPanel::default().show_inside(ui, |ui| {
              render_pane(ui, ctx, cache, doc, session, PaneKind::Next, None);
            });
          });
        egui::CentralPanel::default().show(ctx, |ui| {
          render_pane(ui, ctx, cache, doc, session, PaneKind::Current, input.take());
        });
      }
      2 => {
        egui::SidePanel::left("col-next")
          .resizable(true)
          .default_width(screen.width() * 0.24)
          .show(ctx, |ui| {
            render_pane(ui, ctx, cache, doc, session, PaneKind::Next, None);
          });
        egui::SidePanel::right("col-notes")
          .resizable(true)
          .default_width(screen.width() * 0.28)
          .show(ctx, |ui| {
            render_pane(ui, ctx, cache, doc, session, PaneKind::Notes, None);
          });
        egui::CentralPanel::default().show(ctx, |ui| {
          render_pane(ui, ctx, cache, doc, session, PaneKind::Current, input.take());
        });
      }
      _ => {
        egui::TopBottomPanel::bottom("row")
          .resizable(true)
          .default_height(screen.height() * 0.32)
          .show(ctx, |ui| {
            egui::SidePanel::left("row-next")
              .resizable(true)
              .default_width(ui.available_width() * 0.5)
              .show_inside(ui, |ui| {
                render_pane(ui, ctx, cache, doc, session, PaneKind::Next, None);
              });
            egui::CentralPanel::default().show_inside(ui, |ui| {
              render_pane(ui, ctx, cache, doc, session, PaneKind::Notes, None);
            });
          });
        egui::CentralPanel::default().show(ctx, |ui| {
          render_pane(ui, ctx, cache, doc, session, PaneKind::Current, input.take());
        });
      }
    }
  }
}

impl eframe::App for PresenterApp {
  fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
    ctx.set_theme(theme_pref(self.theme));
    self.camera.update(ctx, self.presenting);

    // Window title carries the open file and its page count (kept out of the header).
    let want_title = match self.document.as_ref() {
      Some(doc) => format!(
        "presenters - {} · {} pages",
        doc.path().file_name().unwrap_or_default().to_string_lossy(),
        doc.page_count()
      ),
      None => "presenters".to_owned(),
    };
    if want_title != self.title {
      self.title.clone_from(&want_title);
      ctx.send_viewport_cmd(egui::ViewportCommand::Title(want_title));
    }

    self.handle_shortcuts(ctx);
    // Swap requests (S or the button, from either window) run here with the main context, which
    // owns the presenter window.
    if self.pending_swap {
      self.pending_swap = false;
      self.swap_screens(ctx);
    }
    // Staged screen swap: move the presenter once the audience has closed, then re-present on the
    // swapped monitor once the presenter has settled. Spacing the steps avoids racing macOS window
    // transitions (which left a screen stuck black).
    let now = Instant::now();
    if let Some(t) = self.move_presenter_at {
      if now >= t {
        self.move_presenter_at = None;
        self.move_presenter(ctx);
        self.represent_at = Some(now + Duration::from_millis(800));
      } else {
        ctx.request_repaint_after(t - now);
      }
    }
    if let Some(t) = self.represent_at {
      if now >= t {
        self.represent_at = None;
        self.start_presentation();
      } else {
        ctx.request_repaint_after(t - now);
      }
    }

    // Hot reload: (re)build the watcher when the document/toggle changed, then reload when the
    // watcher reported a change (debounced via a short due time + bounded retry for mid-write).
    if self.rewatch {
      self.rewatch = false;
      self.update_watcher(ctx);
    }
    if self.reload_flag.swap(false, Ordering::Relaxed) {
      self.reload_due = Some(Instant::now());
      self.reload_tries = 0;
    }
    if let Some(t) = self.reload_due {
      if Instant::now() >= t {
        self.reload_due = None;
        self.do_reload(ctx);
      } else {
        ctx.request_repaint_after(t - Instant::now());
      }
    }
    // Mouse-wheel navigation / zoom on the presenter window, anchored at its current slide.
    let anchor = self.current_slide_rect;
    let strip = self.thumb_rect;
    self.handle_wheel(ctx, anchor, strip);
    self.handle_dropped_files(ctx);

    // Zoom is per-slide: reset it when the page changes.
    let cur = self.session.current();
    if cur != self.zoom_page {
      self.reset_zoom();
      self.zoom_page = cur;
    }

    if self.session.timer().is_running() {
      ctx.request_repaint_after(Duration::from_millis(250));
    }

    self.header(ctx);
    self.footer(ctx);
    self.thumbnails(ctx);
    self.shortcuts_window(ctx);
    self.about_window(ctx);

    if self.document.is_none() {
      self.start_screen(ctx);
    } else {
      self.layout(ctx);
      self.sync_recent_page();

      if let Some(n) = self.session.next_page()
        && let Some(doc) = self.document.as_ref()
      {
        self.cache.get_or_render(ctx, doc, n, Region::Slide, [1280, 720]);
      }

      if self.presenting {
        self.show_audience(ctx);
      }
    }

    self.capture_window_geometry(ctx);

    // Throttled persistence; schedule a wake so a change flushes even while idle.
    if self.dirty && self.last_save.elapsed() >= SAVE_THROTTLE {
      self.save_now();
    }
    if self.dirty {
      ctx.request_repaint_after(SAVE_THROTTLE + Duration::from_millis(100));
    }
  }

  fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
    self.save_now();
  }
}

impl PresenterApp {
  /// Draw the current slide on the audience window (unless blanked) and service the same
  /// mouse interaction as the presenter's current-slide pane — laser pointer, freehand
  /// drawing, and Ctrl/Cmd+drag panning — all against the shared state, so annotations and
  /// zoom mirror between the two windows. Returns the on-screen slide rect (for wheel
  /// zoom-to-cursor) and whether the slide was double-clicked (for fullscreen toggle).
  fn audience_slide(&mut self, ui: &mut egui::Ui, vctx: &egui::Context) -> (Option<egui::Rect>, bool) {
    if self.blanked {
      return (None, false);
    }
    let Some(doc) = self.document.as_ref() else {
      return (None, false);
    };
    let page = self.session.current();
    let aspect = doc.slide_aspect(page);
    let uv = self.zoom_uv();
    let res = self.zoom.clamp(1.0, MAX_ZOOM_RENDER);
    let Some(rect) = draw_slide_region(ui, vctx, &mut self.cache, doc, page, Region::Slide, aspect, uv, res) else {
      return (None, false);
    };
    let mut rect_sink = None;
    let mut inp = SlideInput {
      mode: self.mode,
      size: self.pointer_size,
      color: egui::Color32::from_rgb(self.pointer_color[0], self.pointer_color[1], self.pointer_color[2]),
      pointer: &mut self.pointer_norm,
      strokes: self.strokes.entry(page).or_default(),
      drawing: &mut self.drawing,
      zoom: self.zoom,
      zoom_offset: &mut self.zoom_offset,
      rect_out: &mut rect_sink,
    };
    let resp = process_slide_input(ui, rect, uv, &mut inp);
    (Some(rect), resp.double_clicked())
  }

  /// The audience window: only the slide, on black. Double-click toggles fullscreen;
  /// Esc or the close button quits. Geometry is captured for persistence.
  fn show_audience(&mut self, ctx: &egui::Context) {
    let vid = egui::ViewportId::from_hash_of("audience-window");

    let mut builder = egui::ViewportBuilder::default().with_title("Presentation").with_icon(crate::app_icon());
    if self.audience_needs_place {
      // Place the window this frame on its target monitor (honouring the screen swap), then
      // fullscreen it next frame so the OS fullscreens on that monitor and not on primary.
      if let Some(m) = crate::screen::audience_monitor(self.screens_swapped) {
        builder = builder.with_position([m.x, m.y]).with_inner_size([m.w.max(640.0), m.h.max(360.0)]);
      } else if let Some(g) = self.audience_geometry {
        builder = builder.with_position([g.x, g.y]).with_inner_size([g.w, g.h]);
      } else {
        builder = builder.with_inner_size([960.0, 540.0]);
      }
    }

    let mut quit = false;
    let mut toggle_fullscreen = false;

    ctx.show_viewport_immediate(vid, builder, |vctx, _class| {
      egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(egui::Color32::BLACK))
        .show(vctx, |ui| {
          // Double-click the black margins to toggle fullscreen (the slide area is handled
          // by `audience_slide`, whose interaction sits on top of this one).
          let resp = ui.interact(ui.max_rect(), ui.id().with("audience-surface"), egui::Sense::click());
          if resp.double_clicked() {
            toggle_fullscreen = true;
          }
          // Draw the slide (unless blanked) and let the same mouse features as the presenter
          // window drive it: pointer, drawing, and Ctrl+drag pan.
          let (slide_rect, slide_double) = self.audience_slide(ui, vctx);
          if slide_double {
            toggle_fullscreen = true;
          }
          // Webcam overlay in a corner (audience window only; hidden while blanked).
          if !self.blanked {
            self.camera.draw_overlay(ui, ui.max_rect());
          }
          // Scroll to navigate and Ctrl/Cmd+scroll to zoom, anchored at this window's slide.
          self.handle_wheel(vctx, slide_rect, None);
        });

      // All keyboard shortcuts also work while the audience window has focus — essential on a
      // single screen, where it is fullscreen over the presenter and holds the keyboard focus.
      self.handle_shortcuts(vctx);

      let (pos, size) = vctx.input(|i| {
        let vp = i.viewport();
        (vp.outer_rect.map(|r| r.min), vp.inner_rect.map(|r| r.size()))
      });
      if !self.audience_fullscreen
        && let (Some(p), Some(s)) = (pos, size)
      {
        let g = Geometry {
          x: p.x,
          y: p.y,
          w: s.x,
          h: s.y,
        };
        if geom_changed(self.audience_geometry, g) {
          self.audience_geometry = Some(g);
          self.dirty = true;
        }
      }

      if vctx.input(|i| i.viewport().close_requested() || i.key_pressed(egui::Key::Escape)) {
        quit = true;
      }
    });

    // Staged placement: frame 1 positions the window on the target monitor; frame 2
    // fullscreens it there (so OS fullscreen picks the monitor we moved it to).
    if self.audience_needs_place {
      self.audience_needs_place = false;
      if self.audience_apply_fullscreen {
        ctx.request_repaint(); // ensure a second frame runs to apply fullscreen
      }
    } else if self.audience_apply_fullscreen {
      self.audience_apply_fullscreen = false;
      self.audience_fullscreen = true;
      ctx.send_viewport_cmd_to(vid, egui::ViewportCommand::Fullscreen(true));
    }

    if toggle_fullscreen {
      self.audience_fullscreen = !self.audience_fullscreen;
      self.dirty = true;
      ctx.send_viewport_cmd_to(vid, egui::ViewportCommand::Fullscreen(self.audience_fullscreen));
    }
    if quit {
      self.stop_presentation();
    }
  }
}

/// True if `new` differs from `old` by more than a pixel (or `old` is unset).
fn geom_changed(old: Option<Geometry>, new: Geometry) -> bool {
  match old {
    None => true,
    Some(o) => (o.x - new.x).abs() > 1.0 || (o.y - new.y).abs() > 1.0 || (o.w - new.w).abs() > 1.0 || (o.h - new.h).abs() > 1.0,
  }
}

/// Render one panel's content (a titled, aspect-fitted page region).
///
/// When `input` is `Some` (the current-slide pane), the slide becomes a laser-pointer /
/// drawing surface: while the mouse is held over it, it either tracks the pointer (a dot)
/// or extends a freehand stroke, depending on the mode. Existing strokes for the page are
/// always drawn on top of the slide.
fn render_pane(
  ui: &mut egui::Ui,
  ctx: &egui::Context,
  cache: &mut TextureCache,
  doc: Option<&Document>,
  session: &Session,
  kind: PaneKind,
  input: Option<SlideInput<'_>>,
) {
  ui.small(match kind {
    PaneKind::Current => "Current",
    PaneKind::Next => "Next",
    PaneKind::Notes => "Notes",
  });
  let Some(doc) = doc else { return };
  let current = session.current();

  let (page, region, aspect) = match kind {
    PaneKind::Current => (Some(current), Region::Slide, Some(doc.slide_aspect(current))),
    PaneKind::Next => match session.next_page() {
      Some(n) => (Some(n), Region::Slide, Some(doc.slide_aspect(n))),
      None => (None, Region::Slide, None),
    },
    PaneKind::Notes => match doc.notes_aspect(current) {
      Some(a) => (Some(current), Region::Notes, Some(a)),
      None => (None, Region::Notes, None),
    },
  };

  match (page, aspect) {
    (Some(p), Some(a)) => {
      let (uv, res) = match &input {
        Some(inp) => (zoom_uv_from(inp.zoom, *inp.zoom_offset), inp.zoom.clamp(1.0, MAX_ZOOM_RENDER)),
        None => (full_uv(), 1.0),
      };
      let rect = draw_slide_region(ui, ctx, cache, doc, p, region, a, uv, res);
      if let (Some(mut inp), Some(rect)) = (input, rect) {
        *inp.rect_out = Some(rect);
        let _ = process_slide_input(ui, rect, uv, &mut inp);
      }
    }
    _ => {
      let msg = match kind {
        PaneKind::Next => "— end —",
        PaneKind::Notes => "No notes in this document.",
        PaneKind::Current => "",
      };
      ui.centered_and_justified(|ui| {
        ui.weak(msg);
      });
    }
  }
}

/// Mouse interaction for a slide surface shown at `rect` with visible window `uv`:
/// laser pointer, freehand drawing, and Ctrl/Cmd+drag panning (when zoomed). Then draws
/// the page's strokes and the pointer dot on top.
///
/// Shared by the presenter's current-slide pane and the audience window, both of which
/// drive the same shared state (`inp.pointer`, `inp.strokes`, `inp.zoom_offset`). To avoid
/// the two windows fighting over the pointer each frame, a window only *updates* the
/// pointer/drawing state while the OS pointer is inside that window (or a drag started
/// there); at most one window holds the pointer at a time. The strokes and dot are always
/// drawn, so annotations mirror on both.
fn process_slide_input(ui: &mut egui::Ui, rect: egui::Rect, uv: egui::Rect, inp: &mut SlideInput<'_>) -> egui::Response {
  let ctrl = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);
  let resp = ui.interact(rect, ui.id().with("slide-input"), egui::Sense::click_and_drag());
  let held = resp.is_pointer_button_down_on();
  // Only this window manages the pointer while it actually has the cursor (or is mid-drag),
  // so the other window's per-frame pass doesn't clobber it.
  let engaged = ui.input(|i| i.pointer.hover_pos().is_some()) || resp.dragged();

  if engaged {
    if ctrl {
      // Ctrl+drag pans (when zoomed); no pointer/drawing meanwhile.
      *inp.pointer = None;
      *inp.drawing = false;
      if resp.dragged() && inp.zoom > 1.0 {
        let d = resp.drag_delta();
        let s = 1.0 / inp.zoom;
        let mut off = *inp.zoom_offset - egui::vec2(d.x / rect.width().max(1.0) * s, d.y / rect.height().max(1.0) * s);
        clamp_offset(&mut off, s);
        *inp.zoom_offset = off;
      }
    } else {
      match inp.mode {
        InputMode::Pointer => {
          *inp.pointer = None;
          *inp.drawing = false;
          if held && let Some(pos) = resp.interact_pointer_pos() {
            *inp.pointer = Some(screen_to_slide(rect, uv, pos));
          }
        }
        InputMode::Draw => {
          *inp.pointer = None;
          if held && let Some(pos) = resp.interact_pointer_pos() {
            let n = screen_to_slide(rect, uv, pos);
            if !*inp.drawing {
              *inp.drawing = true;
              inp.strokes.push(Stroke {
                points: vec![n],
                width_factor: 0.006 * inp.size,
              });
            } else if let Some(last) = inp.strokes.last_mut()
              && last.points.last().is_none_or(|lp| (lp.x - n.x).abs() + (lp.y - n.y).abs() > 0.0015 / inp.zoom)
            {
              last.points.push(n);
            }
          } else {
            *inp.drawing = false;
          }
        }
      }
    }
  }

  // Draw the page's strokes, then the pointer dot (if any) on top — clipped to the slide.
  let painter = ui.painter_at(rect);
  draw_strokes(&painter, rect, uv, inp.strokes, inp.color);
  if let Some(n) = *inp.pointer {
    draw_pointer_dot(&painter, rect, uv, n, inp.size, inp.color);
  }
  resp
}

/// A small round colour swatch that opens an RGB colour picker popup on click (alpha stays
/// fixed). Returns true when the colour changed. A compact alternative to egui's wide
/// `color_edit_button_srgb` rectangle.
fn color_dot_picker(ui: &mut egui::Ui, rgb: &mut [u8; 3]) -> bool {
  let d = ui.spacing().interact_size.y.clamp(16.0, 22.0);
  let (rect, resp) = ui.allocate_exact_size(egui::vec2(d, d), egui::Sense::click());
  let resp = resp.on_hover_text("Pointer & drawing colour");

  // The dot: a filled circle in the current colour with a subtle rim.
  let color = egui::Color32::from_rgb(rgb[0], rgb[1], rgb[2]);
  let r = d * 0.38;
  let rim = ui.visuals().widgets.inactive.fg_stroke.color;
  ui.painter().circle_filled(rect.center(), r, color);
  ui.painter().circle_stroke(rect.center(), r, egui::Stroke::new(1.0_f32, rim));

  // Clicking the dot toggles a colour-picker popup (stays open while editing).
  let mut changed = false;
  egui::Popup::menu(&resp)
    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
    .show(|ui| {
      let mut c = color;
      if egui::color_picker::color_picker_color32(ui, &mut c, egui::color_picker::Alpha::Opaque) {
        let [cr, cg, cb, _] = c.to_array();
        *rgb = [cr, cg, cb];
        changed = true;
      }
    });
  changed
}

/// Truncate `text` with a middle "…" so it fits within `max_w` points at `size`
/// (proportional font). Keeps the start and the end (e.g. the file extension).
fn elide_middle(ui: &egui::Ui, text: &str, size: f32, max_w: f32) -> String {
  let fid = egui::FontId::proportional(size);
  let width = |s: &str| ui.painter().layout_no_wrap(s.to_owned(), fid.clone(), egui::Color32::WHITE).size().x;
  if width(text) <= max_w {
    return text.to_owned();
  }
  let chars: Vec<char> = text.chars().collect();
  let n = chars.len();
  let mut keep = n.saturating_sub(1);
  while keep > 1 {
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let candidate: String = chars[..head].iter().collect::<String>() + "…" + &chars[n - tail..].iter().collect::<String>();
    if width(&candidate) <= max_w {
      return candidate;
    }
    keep -= 1;
  }
  "…".to_owned()
}

/// Map our persisted theme preference to egui's.
fn theme_pref(t: config::Theme) -> egui::ThemePreference {
  match t {
    config::Theme::System => egui::ThemePreference::System,
    config::Theme::Dark => egui::ThemePreference::Dark,
    config::Theme::Light => egui::ThemePreference::Light,
  }
}

/// The whole-slide UV window (no zoom).
fn full_uv() -> egui::Rect {
  egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0))
}

/// The visible slide window for a zoom factor and pan offset (slide-normalized coords).
fn zoom_uv_from(zoom: f32, offset: egui::Vec2) -> egui::Rect {
  let s = 1.0 / zoom.max(1.0);
  egui::Rect::from_min_size(egui::pos2(offset.x, offset.y), egui::vec2(s, s))
}

/// Clamp a pan offset so the visible window (size `s`) stays within the slide.
fn clamp_offset(off: &mut egui::Vec2, s: f32) {
  let max = (1.0 - s).max(0.0);
  off.x = off.x.clamp(0.0, max);
  off.y = off.y.clamp(0.0, max);
}

/// Map a slide-normalized point to screen, given the visible `uv` window shown in `rect`.
fn slide_to_screen(rect: egui::Rect, uv: egui::Rect, n: egui::Pos2) -> egui::Pos2 {
  let fx = (n.x - uv.min.x) / uv.width().max(1e-6);
  let fy = (n.y - uv.min.y) / uv.height().max(1e-6);
  egui::pos2(rect.left() + fx * rect.width(), rect.top() + fy * rect.height())
}

/// Inverse of [`slide_to_screen`].
fn screen_to_slide(rect: egui::Rect, uv: egui::Rect, p: egui::Pos2) -> egui::Pos2 {
  let fx = ((p.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
  let fy = ((p.y - rect.top()) / rect.height().max(1.0)).clamp(0.0, 1.0);
  egui::pos2(uv.min.x + fx * uv.width(), uv.min.y + fy * uv.height())
}

/// Draw the laser-pointer dot at a slide-normalized position within the visible `uv` window.
fn draw_pointer_dot(painter: &egui::Painter, rect: egui::Rect, uv: egui::Rect, norm: egui::Pos2, size: f32, color: egui::Color32) {
  let center = slide_to_screen(rect, uv, norm);
  let r = (rect.height() * 0.012 * size).max(3.0);
  let [cr, cg, cb, _] = color.to_array();
  // Semi-transparent (alpha fixed): a faint halo, a translucent core, and a darker rim.
  let dark = |c: u8| ((c as f32) * 0.6) as u8;
  painter.circle_filled(center, r * 2.2, egui::Color32::from_rgba_unmultiplied(cr, cg, cb, 40));
  painter.circle_filled(center, r, egui::Color32::from_rgba_unmultiplied(cr, cg, cb, 140));
  painter.circle_stroke(
    center,
    r,
    egui::Stroke::new(1.5_f32, egui::Color32::from_rgba_unmultiplied(dark(cr), dark(cg), dark(cb), 140)),
  );
}

/// Draw freehand strokes onto `rect`, mapped through the visible `uv` window (so they
/// track the slide under zoom/pan).
fn draw_strokes(painter: &egui::Painter, rect: egui::Rect, uv: egui::Rect, strokes: &[Stroke], color: egui::Color32) {
  // Semi-transparent (alpha fixed), matching the pointer dot's core.
  let [cr, cg, cb, _] = color.to_array();
  let color = egui::Color32::from_rgba_unmultiplied(cr, cg, cb, 140);
  for s in strokes {
    // Line width scales with zoom (thinner window -> thicker on screen).
    let w = (s.width_factor * rect.height() / uv.height().max(1e-6)).max(1.0);
    match s.points.as_slice() {
      [] => {}
      [p] => {
        painter.circle_filled(slide_to_screen(rect, uv, *p), (w * 0.5).max(1.0), color);
      }
      _ => {
        let pts: Vec<egui::Pos2> = s.points.iter().map(|p| slide_to_screen(rect, uv, *p)).collect();
        painter.add(egui::Shape::line(pts, egui::Stroke::new(w, color)));
      }
    }
  }
}

/// Draw an aspect-fitted page region into the available area (letterboxed), showing only
/// the `uv` sub-window (for zoom) and rendering at `res_scale`× resolution for crispness.
/// Returns the on-screen rect the image occupies.
#[allow(clippy::too_many_arguments)]
fn draw_slide_region(
  ui: &mut egui::Ui,
  ctx: &egui::Context,
  cache: &mut TextureCache,
  doc: &Document,
  page: usize,
  region: Region,
  aspect: f32,
  uv: egui::Rect,
  res_scale: f32,
) -> Option<egui::Rect> {
  let full = ui.available_rect_before_wrap();
  // Claim the whole available area up front. A resizable panel stores the height (or width)
  // of its content's actual rect; if we only claimed the letterboxed image below, a vertical
  // split's top pane would store the image bottom instead of the panel bottom and collapse to
  // its minimum every frame ("jumps back to the top"). Allocating `full` pins the stored size
  // to what the user dragged. Sense::hover doesn't eat clicks meant for the slide interaction.
  ui.allocate_rect(full, egui::Sense::hover());
  let ppp = ctx.pixels_per_point();

  let (mut w, mut h) = (full.width(), full.width() / aspect);
  if h > full.height() {
    h = full.height();
    w = full.height() * aspect;
  }
  let img_rect = egui::Rect::from_center_size(full.center(), egui::vec2(w, h));

  let mut pw = (w * ppp * res_scale).round().max(1.0) as u32;
  let mut ph = (h * ppp * res_scale).round().max(1.0) as u32;
  if pw > MAX_RENDER_WIDTH {
    ph = ((ph as f32) * (MAX_RENDER_WIDTH as f32 / pw as f32)).round().max(1.0) as u32;
    pw = MAX_RENDER_WIDTH;
  }

  if let Some(tex) = cache.get_or_render(ctx, doc, page, region, [pw, ph]) {
    egui::Image::new(egui::load::SizedTexture::new(tex.id(), egui::vec2(w, h)))
      .uv(uv)
      .paint_at(ui, img_rect);
    Some(img_rect)
  } else {
    egui::Spinner::new().paint_at(ui, img_rect);
    None
  }
}

type TexKey = (usize, Region, u32);

/// Bounded **LRU** texture cache keyed by (page, region, pixel width).
///
/// LRU matters for correctness, not just efficiency: eviction drops a `TextureHandle`,
/// which frees the GPU texture. If a texture still referenced by the current frame's paint
/// commands were freed, wgpu aborts ("texture has been destroyed"). By moving a key to the
/// back of `order` on every access, eviction only ever removes the least-recently-used
/// texture — never one drawn this frame (a frame touches far fewer textures than `capacity`).
struct TextureCache {
  map: HashMap<TexKey, egui::TextureHandle>,
  order: VecDeque<TexKey>,
  capacity: usize,
}

impl TextureCache {
  fn new(capacity: usize) -> Self {
    Self {
      map: HashMap::new(),
      order: VecDeque::new(),
      capacity,
    }
  }

  fn clear(&mut self) {
    self.map.clear();
    self.order.clear();
  }

  /// Mark `key` as most-recently-used.
  fn touch(&mut self, key: TexKey) {
    if let Some(pos) = self.order.iter().position(|k| *k == key) {
      self.order.remove(pos);
    }
    self.order.push_back(key);
  }

  fn get_or_render(&mut self, ctx: &egui::Context, doc: &Document, page: usize, region: Region, target_px: [u32; 2]) -> Option<egui::TextureHandle> {
    let key = (page, region, target_px[0]);
    if let Some(tex) = self.map.get(&key).cloned() {
      self.touch(key);
      return Some(tex);
    }

    let img = match region {
      Region::Slide => doc.render_slide(page, target_px),
      Region::Notes => match doc.render_notes(page, target_px) {
        Ok(Some(img)) => Ok(img),
        Ok(None) => return None,
        Err(e) => Err(e),
      },
    };
    let img = match img {
      Ok(img) => img,
      Err(e) => {
        tracing::error!("render {region:?} page {page}: {e:#}");
        return None;
      }
    };

    let size = [img.width() as usize, img.height() as usize];
    let color = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    let tex = ctx.load_texture(format!("{region:?}-{page}-{}", target_px[0]), color, egui::TextureOptions::LINEAR);

    self.map.insert(key, tex.clone());
    self.order.push_back(key);
    while self.order.len() > self.capacity {
      if let Some(old) = self.order.pop_front() {
        self.map.remove(&old);
      }
    }
    Some(tex)
  }
}
