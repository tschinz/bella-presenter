//! bella-presenter library crate: the app modules, shared by the binary and tests.

pub mod app;
pub mod camera;
pub mod config;
pub mod document;
pub mod platform;
pub mod render;
pub mod screen;
pub mod session;
pub mod style;
pub mod update;

use futures::channel::mpsc;

/// Wakes the UI from another thread when a camera frame, a file change or a background result is ready.
#[derive(Clone)]
pub struct Waker(mpsc::UnboundedSender<()>);

impl Waker {
  /// A waker and the stream of its wake-ups, for the UI to wait on.
  pub fn new() -> (Self, mpsc::UnboundedReceiver<()>) {
    let (tx, rx) = mpsc::unbounded();
    (Self(tx), rx)
  }

  pub fn wake(&self) {
    let _ = self.0.unbounded_send(());
  }
}

/// The application icon (embedded PNG), for the window icon where the platform takes one (X11).
pub fn app_icon() -> Option<std::sync::Arc<image::RgbaImage>> {
  match image::load_from_memory(include_bytes!("../img/icon-256.png")) {
    Ok(img) => Some(std::sync::Arc::new(img.into_rgba8())),
    Err(e) => {
      tracing::warn!("failed to decode app icon: {e}");
      None
    }
  }
}
