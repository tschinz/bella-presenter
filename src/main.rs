//! bella-presenter — a fast native dual-screen PDF presenter (pympress-like).

use std::path::PathBuf;

use bella_presenter::app::PresenterApp;

fn main() {
  tracing_subscriber::fmt()
    .with_env_filter(
      // Quiet wgpu's very chatty per-frame INFO logging by default (GPUI draws with it on Linux).
      tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,wgpu=warn,wgpu_core=warn,wgpu_hal=warn,naga=warn".into()),
    )
    .init();

  // Optional PDF path as the first CLI argument.
  let initial: Option<PathBuf> = std::env::args().nth(1).map(PathBuf::from);

  // macOS hands "Open with" and double-clicked PDFs over as URLs, also at launch: queue them for
  // the presenter window. Presenter state lives in ~/.config/bella-presenter/state.json (or
  // %APPDATA%\bella-presenter on Windows).
  let (opened, opened_files) = futures::channel::mpsc::unbounded();
  let app = gpui_kit::application().with_assets(bella_presenter::style::Assets);
  app.on_open_urls(move |urls| {
    for path in urls.iter().filter_map(|url| bella_presenter::platform::path_from_url(url)) {
      let _ = opened.unbounded_send(path);
    }
  });
  app.run(move |cx| {
    gpui_kit::init(cx);
    bella_presenter::style::load_fonts(cx);
    PresenterApp::open_window(initial, opened_files, cx);
  });
}
