//! Rendering abstraction.
//!
//! Everything above this module talks to PDFs through the [`PageRenderer`] trait,
//! never through PDFium types directly. Swapping the backend (MuPDF, a pure-Rust
//! renderer, ...) means adding a new impl here and nothing else.

pub mod pdfium;

use image::RgbaImage;

/// A page's intrinsic size, in PDF points (1/72 inch).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageSize {
  pub width: f32,
  pub height: f32,
}

impl PageSize {
  /// width / height. Guards against degenerate pages.
  pub fn aspect(&self) -> f32 {
    if self.height > 0.0 { self.width / self.height } else { 1.0 }
  }
}

/// Normalized sub-rectangle of a page (0..1 in both axes, origin top left).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Region {
  pub x: f32,
  pub y: f32,
  pub w: f32,
  pub h: f32,
}

impl Region {
  pub const FULL: Region = Region {
    x: 0.0,
    y: 0.0,
    w: 1.0,
    h: 1.0,
  };

  pub fn is_full(&self) -> bool {
    self.x <= f32::EPSILON && self.y <= f32::EPSILON && self.w >= 1.0 - f32::EPSILON && self.h >= 1.0 - f32::EPSILON
  }

  pub fn contains(&self, x: f32, y: f32) -> bool {
    x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
  }
}

/// Where a link on a page leads.
#[derive(Clone, Debug, PartialEq)]
pub enum LinkTarget {
  /// A page of the same document (zero-based).
  Page(usize),
  /// A web or mail address.
  Uri(String),
}

/// A clickable area of a page.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
  /// Where on the page, normalized like [`Region`].
  pub area: Region,
  pub target: LinkTarget,
}

/// Opens one document and rasterizes its pages to bitmaps.
///
/// Implementations own the document for their lifetime; callers address pages by
/// zero-based index and ask for a pixel size (already aspect-correct — the caller
/// decides the fit).
pub trait PageRenderer {
  fn page_count(&self) -> usize;

  /// Intrinsic size of a page, in points.
  fn page_size(&self, page: usize) -> PageSize;

  /// Rasterize `region` of `page` at `target_px` = [width, height] pixels. The pixels are in
  /// **BGRA** order (what PDFium produces and GPUI draws), although the buffer type says RGBA.
  fn render(&self, page: usize, region: Region, target_px: [u32; 2]) -> anyhow::Result<RgbaImage>;

  /// The links on `page`.
  fn links(&self, page: usize) -> Vec<Link>;
}
