//! bella-presenter library crate: the app modules, shared by the binary and tests.

pub mod app;
pub mod camera;
pub mod config;
pub mod document;
pub mod platform;
pub mod render;
pub mod screen;
pub mod session;

/// The application icon (embedded PNG), for use as a window icon.
pub fn app_icon() -> egui::IconData {
  let bytes = include_bytes!("../img/icon-256.png");
  match image::load_from_memory(bytes) {
    Ok(img) => {
      let rgba = img.into_rgba8();
      let (width, height) = rgba.dimensions();
      egui::IconData {
        rgba: rgba.into_raw(),
        width,
        height,
      }
    }
    Err(e) => {
      tracing::warn!("failed to decode app icon: {e}");
      egui::IconData::default()
    }
  }
}
