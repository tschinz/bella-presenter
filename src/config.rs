//! Persistent state, stored as JSON in a per-OS config directory.
//!
//! We deliberately use `~/.config/bella-presenter` on both macOS and Linux (rather than
//! macOS's `~/Library/Application Support`), and `%APPDATA%\bella-presenter` on Windows.
//! We manage this file ourselves so it always lives at this location and is written promptly.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The base configuration directory for the current OS.
fn config_base() -> PathBuf {
  if cfg!(windows) {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
  } else {
    std::env::var_os("XDG_CONFIG_HOME")
      .map(PathBuf::from)
      .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
      .unwrap_or_else(|| PathBuf::from(".config"))
  }
}

/// The `bella-presenter` configuration directory for the current OS.
pub fn config_dir() -> PathBuf {
  config_base().join("bella-presenter")
}

/// The pre-rename `presenter` configuration directory (only read, to migrate old state).
fn legacy_config_dir() -> PathBuf {
  config_base().join("presenter")
}

/// Full path to the persisted state file.
pub fn state_path() -> PathBuf {
  config_dir().join("state.json")
}

/// Copy `state.json` from the old `presenter` dir into the new one, once.
fn migrate_legacy_state() {
  let old = legacy_config_dir().join("state.json");
  let new = state_path();
  let Ok(bytes) = std::fs::read(&old) else { return };
  if let Some(dir) = new.parent()
    && std::fs::create_dir_all(dir).is_ok()
    && std::fs::write(&new, &bytes).is_ok()
  {
    tracing::info!("migrated state from {} to {}", old.display(), new.display());
  }
}

/// A window's position and size (logical pixels).
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct Geometry {
  pub x: f32,
  pub y: f32,
  pub w: f32,
  pub h: f32,
}

/// UI theme preference, persisted and applied to the component theme.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Theme {
  #[default]
  System,
  Dark,
  Light,
}

/// A recently opened document and the page last viewed in it (for resume).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct RecentEntry {
  pub path: PathBuf,
  pub page: usize,
}

/// Screen corner for the camera overlay.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Corner {
  TopLeft,
  TopRight,
  BottomLeft,
  #[default]
  BottomRight,
}

/// Outline of the camera overlay.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum CameraShape {
  #[default]
  Rect,
  Rounded,
  Circle,
}

/// What to do with the camera background.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BackgroundMode {
  #[default]
  Off,
  /// Cut out the person, transparent background.
  Remove,
  /// Keep the background but blur it.
  Blur,
}

impl BackgroundMode {
  pub fn to_u8(self) -> u8 {
    match self {
      Self::Off => 0,
      Self::Remove => 1,
      Self::Blur => 2,
    }
  }

  pub fn from_u8(v: u8) -> Self {
    match v {
      1 => Self::Remove,
      2 => Self::Blur,
      _ => Self::Off,
    }
  }
}

/// Webcam overlay settings (shown on the audience window only).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct CameraSettings {
  pub enabled: bool,
  /// Device name; `None` = first available camera.
  pub device: Option<String>,
  pub corner: Corner,
  /// Overlay width as a fraction of the audience window width.
  pub size: f32,
  /// Distance from the window edge as a fraction of the audience window width.
  pub margin: f32,
  pub mirror: bool,
  pub shape: CameraShape,
  /// Background handling (the shape setting is ignored while removing).
  pub background: BackgroundMode,
}

impl Default for CameraSettings {
  fn default() -> Self {
    Self {
      enabled: false,
      device: None,
      corner: Corner::default(),
      size: 0.2,
      margin: 0.015,
      mirror: true,
      shape: CameraShape::default(),
      background: BackgroundMode::Off,
    }
  }
}

/// Everything persisted between sessions.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct State {
  pub footer_font: f32,
  pub pointer_size: f32,
  pub layout_index: usize,
  pub window_geometry: Option<Geometry>,
  pub recent: Vec<RecentEntry>,
  pub show_thumbnails: bool,
  pub thumb_height: f32,
  /// RGB colour of the laser pointer and freehand drawings (alpha stays fixed).
  pub pointer_color: [u8; 3],
  pub theme: Theme,
  /// Reload the open PDF when its file changes on disk.
  pub hot_reload: bool,
  pub camera: CameraSettings,
  /// A release version the user chose to skip: not offered again at start.
  pub skipped_update: String,
}

impl Default for State {
  fn default() -> Self {
    Self {
      footer_font: 32.0,
      pointer_size: 1.0,
      layout_index: 0,
      window_geometry: None,
      recent: Vec::new(),
      show_thumbnails: true,
      thumb_height: 180.0,
      pointer_color: [230, 30, 30],
      theme: Theme::System,
      hot_reload: true,
      camera: CameraSettings::default(),
      skipped_update: String::new(),
    }
  }
}

impl State {
  /// Load the state file, or defaults if it is missing or unreadable.
  pub fn load() -> Self {
    if !state_path().exists() {
      migrate_legacy_state();
    }
    match std::fs::read_to_string(state_path()) {
      Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
        // Keep the unreadable file: the next save would otherwise overwrite the recent list for good.
        let backup = state_path().with_extension("json.bak");
        tracing::warn!("state file unreadable, using defaults (kept as {}): {e}", backup.display());
        let _ = std::fs::copy(state_path(), backup);
        State::default()
      }),
      Err(_) => State::default(),
    }
  }

  /// Write the state file, creating the config directory if needed.
  pub fn save(&self) {
    let path = state_path();
    if let Some(dir) = path.parent()
      && let Err(e) = std::fs::create_dir_all(dir)
    {
      tracing::warn!("could not create config dir {}: {e}", dir.display());
      return;
    }
    match serde_json::to_string_pretty(self) {
      Ok(json) => {
        // Write aside, then rename over the old file: a crash mid-write leaves the old state intact.
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, json).and_then(|()| std::fs::rename(&tmp, &path)) {
          tracing::warn!("could not write state {}: {e}", path.display());
        }
      }
      Err(e) => tracing::warn!("could not serialize state: {e}"),
    }
  }
}
