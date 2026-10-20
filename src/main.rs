//! bella-presenter — a fast native dual-screen PDF presenter (pympress-like).
//!
//! Milestone 1: open a PDF and page through it quickly in a single window.

use std::path::PathBuf;

use bella_presenter::app::PresenterApp;

fn main() -> eframe::Result<()> {
  tracing_subscriber::fmt()
    .with_env_filter(
      // Quiet wgpu's very chatty per-frame INFO logging by default.
      tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,wgpu=warn,wgpu_core=warn,wgpu_hal=warn,naga=warn".into()),
    )
    .init();

  // Optional PDF path as the first CLI argument.
  let initial: Option<PathBuf> = std::env::args().nth(1).map(PathBuf::from);

  // Restore the presenter window's size from our own state file, and place it on the
  // PRIMARY monitor (the presenter view always belongs there; the audience window goes
  // to the secondary screen at presentation time). State file lives in
  // ~/.config/bella-presenter/state.json (or %APPDATA%\bella-presenter on Windows).
  let saved = bella_presenter::config::State::load();
  let size = saved.window_geometry.map(|g| [g.w, g.h]).unwrap_or([1100.0, 720.0]);

  let mut viewport = egui::ViewportBuilder::default()
    .with_title("bella-presenter")
    .with_icon(bella_presenter::app_icon())
    .with_inner_size(size);

  if let Some(prim) = bella_presenter::screen::primary() {
    // Restore the saved position only if it is on the primary monitor; otherwise
    // center the window on the primary monitor.
    let pos = match saved.window_geometry {
      Some(g) if prim.contains(g.x, g.y) => [g.x, g.y],
      _ => [prim.x + (prim.w - size[0]).max(0.0) * 0.5, prim.y + (prim.h - size[1]).max(0.0) * 0.5],
    };
    viewport = viewport.with_position(pos);
  }

  // Use the wgpu backend (Metal / Vulkan) by default. The glow (OpenGL) backend has a
  // font-atlas panic ("Partial texture update is outside the bounds") when the atlas grows
  // while multiple viewports (presenter + audience) are open, which aborts the app mid-talk.
  // Escape hatch: set BP_RENDERER=glow to fall back if wgpu misbehaves on some machine.
  // RP_RENDERER is the pre-rename name, still honoured for existing setups.
  let renderer = match std::env::var("BP_RENDERER").or_else(|_| std::env::var("RP_RENDERER")).as_deref() {
    Ok("glow") => eframe::Renderer::Glow,
    _ => eframe::Renderer::Wgpu,
  };

  let options = eframe::NativeOptions {
    viewport,
    renderer,
    ..Default::default()
  };

  eframe::run_native(
    "bella-presenter",
    options,
    Box::new(move |cc| Ok(Box::new(PresenterApp::new(cc, initial.as_deref())))),
  )
}
