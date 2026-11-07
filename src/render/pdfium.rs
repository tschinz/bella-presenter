//! PDFium implementation of [`PageRenderer`]. This is the *only* module allowed to
//! name PDFium types.

use std::cell::OnceCell;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use image::RgbaImage;
use pdfium_render::prelude::*;

use super::{Link, LinkTarget, PageRenderer, PageSize, Region};

thread_local! {
    /// The PDFium library is initialized once per thread and intentionally leaked so
    /// documents can borrow it for `'static`. There is one presenter process, so this
    /// leaks a single small binding object for the life of the program.
    static PDFIUM: OnceCell<&'static Pdfium> = const { OnceCell::new() };
}

fn pdfium() -> Result<&'static Pdfium> {
  PDFIUM.with(|cell| {
    if let Some(p) = cell.get() {
      return Ok(*p);
    }
    let bindings = load_bindings()?;
    let leaked: &'static Pdfium = Box::leak(Box::new(Pdfium::new(bindings)));
    let _ = cell.set(leaked);
    Ok(leaked)
  })
}

/// Try the bundled PDFium next to the binary / repo, then the system library.
fn load_bindings() -> Result<Box<dyn PdfiumLibraryBindings>> {
  for dir in candidate_lib_dirs() {
    let path = Pdfium::pdfium_platform_library_name_at_path(&dir);
    if let Ok(b) = Pdfium::bind_to_library(&path) {
      tracing::info!("loaded PDFium from {}", path.display());
      return Ok(b);
    }
  }
  Pdfium::bind_to_system_library().map_err(|e| {
    anyhow!(
      "could not find a PDFium library (bundled or system): {e}. \
             Expected e.g. third_party/pdfium/lib/libpdfium.*"
    )
  })
}

/// Directories to probe for the PDFium dynamic library, most-specific first.
///
/// Covers three situations: an explicit override, a packaged bundle (macOS `.app`
/// `Contents/Resources`, Linux install prefixes, Windows next to the exe), and the dev
/// layout (`cargo run` from the repo root).
fn candidate_lib_dirs() -> Vec<PathBuf> {
  let mut dirs = Vec::new();

  // Explicit override wins.
  if let Ok(p) = std::env::var("PDFIUM_LIB_DIR") {
    dirs.push(PathBuf::from(p));
  }

  if let Ok(exe) = std::env::current_exe()
    && let Some(exe_dir) = exe.parent()
  {
    // Windows / AppImage / portable: next to the binary.
    dirs.push(exe_dir.to_path_buf());
    dirs.push(exe_dir.join("lib"));

    if let Some(parent) = exe_dir.parent() {
      // macOS .app: Contents/MacOS/<bin> -> Contents/Resources[/...]
      let resources = parent.join("Resources");
      dirs.push(resources.join("third_party/pdfium/lib"));
      dirs.push(resources.join("lib"));
      dirs.push(resources.clone());

      // Linux install prefix: <prefix>/bin/<bin> -> <prefix>/{lib,share}[/<pkg>][/...].
      // cargo-bundle's .deb preserves the resource's relative path, so the library lands
      // under a per-package dir (the package may be named after the crate or the bundle).
      for base in ["lib", "share"] {
        for pkg in ["bella-presenter", "rust-presenters", "presenters"] {
          dirs.push(parent.join(base).join(pkg).join("third_party/pdfium/lib"));
          dirs.push(parent.join(base).join(pkg));
        }
        dirs.push(parent.join(base));
      }
    }
  }

  // Common system locations (Linux/BSD).
  dirs.push(PathBuf::from("/usr/local/lib"));
  dirs.push(PathBuf::from("/usr/lib"));

  // Dev layout: cargo run from the repo root (bin/ is where the Windows DLL lands).
  dirs.push(PathBuf::from("third_party/pdfium/lib"));
  dirs.push(PathBuf::from("third_party/pdfium/bin"));
  dirs.push(PathBuf::from("third_party/pdfium"));
  dirs
}

pub struct PdfiumRenderer {
  document: PdfDocument<'static>,
  page_sizes: Vec<PageSize>,
}

impl PdfiumRenderer {
  pub fn open(path: &Path) -> Result<Self> {
    let pdfium = pdfium()?;
    // Read the whole file into memory and hand ownership to PDFium instead of keeping the file
    // open for the document's lifetime. This releases the OS handle immediately, so another
    // program recompiling the deck (Typst/LaTeX) can overwrite the PDF - on Windows an open
    // handle would otherwise lock it - and PDFium then works from a stable in-memory snapshot.
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let document = pdfium
      .load_pdf_from_byte_vec(bytes, None)
      .with_context(|| format!("opening {}", path.display()))?;

    // Cache page sizes up front: cheap, and it drives layout/notes detection.
    let page_sizes = document
      .pages()
      .iter()
      .map(|p| PageSize {
        width: p.width().value,
        height: p.height().value,
      })
      .collect();

    Ok(Self { document, page_sizes })
  }
}

impl PageRenderer for PdfiumRenderer {
  fn page_count(&self) -> usize {
    self.page_sizes.len()
  }

  fn page_size(&self, page: usize) -> PageSize {
    self.page_sizes.get(page).copied().unwrap_or(PageSize { width: 1.0, height: 1.0 })
  }

  fn render(&self, page: usize, region: Region, target_px: [u32; 2]) -> Result<RgbaImage> {
    let [w, h] = [target_px[0].max(1), target_px[1].max(1)];
    let index: u16 = page.try_into().map_err(|_| anyhow!("page index {page} out of range"))?;
    let pdf_page = self.document.pages().get(index).with_context(|| format!("getting page {page}"))?;

    // The whole page at the scale that makes `region` land on `target_px`.
    let full = [(w as f32 / region.w.max(1e-3)).round(), (h as f32 / region.h.max(1e-3)).round()];
    // pdfium-render asks for RGBA by default; keep PDFium's native BGRA, which GPUI draws as is.
    let mut config = PdfRenderConfig::new()
      .set_reverse_byte_order(false)
      .set_target_width(full[0] as i32)
      .set_target_height(full[1] as i32);
    if !region.is_full() {
      // Only the region: shift the page so it starts at the bitmap's corner, and draw nothing else.
      let size = self.page_size(page);
      config = config
        .translate(PdfPoints::new(-region.x * size.width), PdfPoints::new(-region.y * size.height))
        .with_context(|| format!("placing page {page}"))?
        .clip(0, 0, w as i32, h as i32);
    }
    let mut bitmap =
      PdfBitmap::empty(w as i32, h as i32, PdfBitmapFormat::BGRA, self.document.bindings()).with_context(|| format!("allocating {w}x{h} for page {page}"))?;
    pdf_page
      .render_into_bitmap_with_config(&mut bitmap, &config)
      .with_context(|| format!("rendering page {page}"))?;
    // BGRA as it comes: GPUI draws BGRA, so no channel swap on either side.
    RgbaImage::from_raw(w, h, bitmap.as_raw_bytes()).with_context(|| format!("page {page}: unexpected bitmap layout"))
  }

  fn links(&self, page: usize) -> Vec<Link> {
    let Ok(index) = u16::try_from(page) else { return Vec::new() };
    let Ok(pdf_page) = self.document.pages().get(index) else {
      return Vec::new();
    };
    let size = self.page_size(page);
    let links = pdf_page.links();
    links
      .iter()
      .filter_map(|link| {
        let target = match (link.destination(), link.action()) {
          (Some(dest), _) => LinkTarget::Page(dest.page_index().ok()?.into()),
          (None, Some(action)) => match (action.as_local_destination_action(), action.as_uri_action()) {
            (Some(local), _) => LinkTarget::Page(local.destination().ok()?.page_index().ok()?.into()),
            (None, Some(uri)) => LinkTarget::Uri(uri.uri().ok()?),
            (None, None) => return None,
          },
          (None, None) => return None,
        };
        // PDF rectangles count up from the bottom; ours from the top. (Assumes the media box starts at 0.)
        let r = link.rect().ok()?;
        let area = Region {
          x: r.left().value / size.width,
          y: 1.0 - r.top().value / size.height,
          w: (r.right().value - r.left().value) / size.width,
          h: (r.top().value - r.bottom().value) / size.height,
        };
        Some(Link { area, target })
      })
      .collect()
  }
}
