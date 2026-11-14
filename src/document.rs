//! An open presentation document: the renderer plus derived metadata (page count,
//! sizes, detected notes layout).

use std::path::{Path, PathBuf};

use anyhow::Result;
use image::RgbaImage;

use crate::media;
use crate::render::pdfium::PdfiumRenderer;
use crate::render::{Link, PageRenderer, PageSize, Region};

/// Where the speaker notes live relative to the slide on each page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotesLayout {
  /// One slide per page, no notes.
  None,
  /// Page is a slide + notes side by side / stacked. The variant names the notes half.
  Split(NotesSide),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotesSide {
  Right,
  Left,
  Top,
  Bottom,
}

pub struct Document {
  path: PathBuf,
  renderer: Box<dyn PageRenderer>,
  notes: NotesLayout,
  /// Animated media attached by `bella-media.typ`, and why any item was skipped.
  media: Vec<media::Item>,
  media_warnings: Vec<String>,
}

impl Document {
  pub fn open(path: &Path) -> Result<Self> {
    let renderer = PdfiumRenderer::open(path)?;
    let notes = detect_notes_layout(&renderer);
    let sizes: Vec<_> = (0..renderer.page_count()).map(|p| renderer.page_size(p)).collect();
    let (media, media_warnings) = media::from_attachments(renderer.attachments(), &sizes);
    for w in &media_warnings {
      tracing::warn!("{w}");
    }
    tracing::info!(?notes, pages = renderer.page_count(), media = media.len(), "opened document");
    Ok(Self {
      path: path.to_path_buf(),
      renderer: Box::new(renderer),
      notes,
      media,
      media_warnings,
    })
  }

  pub fn path(&self) -> &Path {
    &self.path
  }

  pub fn page_count(&self) -> usize {
    self.renderer.page_count()
  }

  pub fn page_size(&self, page: usize) -> PageSize {
    self.renderer.page_size(page)
  }

  pub fn notes_layout(&self) -> NotesLayout {
    self.notes
  }

  /// Whether the document uses speaker notes at all (drives the presenter layout).
  pub fn has_notes(&self) -> bool {
    self.notes != NotesLayout::None
  }

  /// Whether *this specific page* carries the split. In a notes deck, most pages are
  /// double-width (slide + notes), but a page can be a plain single-width slide with no
  /// notes; such pages are shown whole rather than cropped to a half.
  fn page_is_split(&self, page: usize) -> bool {
    page_is_split_for(self.notes, self.page_size(page).aspect())
  }

  /// The slide sub-rectangle of a page (the part shown to the audience).
  pub fn slide_region(&self, page: usize) -> Region {
    match self.notes {
      NotesLayout::Split(side) if self.page_is_split(page) => slide_side(side),
      _ => Region::FULL,
    }
  }

  /// The notes sub-rectangle of a page, if that page has notes.
  pub fn notes_region(&self, page: usize) -> Option<Region> {
    match self.notes {
      NotesLayout::Split(side) if self.page_is_split(page) => Some(notes_side(side)),
      _ => None,
    }
  }

  /// Aspect (w/h) of the slide portion of a page.
  pub fn slide_aspect(&self, page: usize) -> f32 {
    region_aspect(self.page_size(page).aspect(), self.slide_region(page))
  }

  /// Aspect (w/h) of the notes portion of a page, if that page has notes.
  pub fn notes_aspect(&self, page: usize) -> Option<f32> {
    self.notes_region(page).map(|r| region_aspect(self.page_size(page).aspect(), r))
  }

  /// Render a full page at the given pixel size (BGRA, see [`PageRenderer::render`]).
  pub fn render_page(&self, page: usize, target_px: [u32; 2]) -> Result<RgbaImage> {
    self.renderer.render(page, Region::FULL, target_px)
  }

  /// The links on the slide part of `page`, with their areas relative to the slide.
  pub fn slide_links(&self, page: usize) -> Vec<Link> {
    let slide = self.slide_region(page);
    self
      .renderer
      .links(page)
      .into_iter()
      .filter_map(|link| {
        Some(Link {
          area: on_slide(slide, link.area)?,
          ..link
        })
      })
      .collect()
  }

  /// All media items of the deck (indexes are their ids).
  pub fn media(&self) -> &[media::Item] {
    &self.media
  }

  /// Why media items were skipped when the document was opened.
  pub fn media_warnings(&self) -> &[String] {
    &self.media_warnings
  }

  /// The media on the slide part of `page`: item index and area relative to the slide.
  pub fn slide_media(&self, page: usize) -> Vec<(usize, Region)> {
    let slide = self.slide_region(page);
    let on_page = self.media.iter().enumerate().filter(|(_, m)| m.page == page);
    on_page.filter_map(|(i, m)| Some((i, on_slide(slide, m.area)?))).collect()
  }

  /// Render just the slide portion of a page (the audience view).
  pub fn render_slide(&self, page: usize, target_px: [u32; 2]) -> Result<RgbaImage> {
    self.render_region(page, self.slide_region(page), target_px)
  }

  /// Render just the notes portion of a page, if that page has notes.
  pub fn render_notes(&self, page: usize, target_px: [u32; 2]) -> Result<Option<RgbaImage>> {
    match self.notes_region(page) {
      Some(region) => Ok(Some(self.render_region(page, region, target_px)?)),
      None => Ok(None),
    }
  }

  /// Render a normalized sub-rectangle of a page at `target_px`, at full resolution.
  fn render_region(&self, page: usize, region: Region, target_px: [u32; 2]) -> Result<RgbaImage> {
    self.renderer.render(page, region, target_px)
  }
}

/// `area` of a page relative to the `slide` part of it, if its centre lies on the slide.
fn on_slide(slide: Region, area: Region) -> Option<Region> {
  slide.contains(area.x + area.w / 2.0, area.y + area.h / 2.0).then(|| Region {
    x: (area.x - slide.x) / slide.w,
    y: (area.y - slide.y) / slide.h,
    w: area.w / slide.w,
    h: area.h / slide.h,
  })
}

fn region_aspect(page_aspect: f32, r: Region) -> f32 {
  if r.h > 0.0 { page_aspect * (r.w / r.h) } else { page_aspect }
}

/// The slide occupies the half opposite the notes.
fn slide_side(notes: NotesSide) -> Region {
  match notes {
    NotesSide::Right => Region {
      x: 0.0,
      y: 0.0,
      w: 0.5,
      h: 1.0,
    },
    NotesSide::Left => Region {
      x: 0.5,
      y: 0.0,
      w: 0.5,
      h: 1.0,
    },
    NotesSide::Bottom => Region {
      x: 0.0,
      y: 0.0,
      w: 1.0,
      h: 0.5,
    },
    NotesSide::Top => Region {
      x: 0.0,
      y: 0.5,
      w: 1.0,
      h: 0.5,
    },
  }
}

fn notes_side(notes: NotesSide) -> Region {
  match notes {
    NotesSide::Right => Region {
      x: 0.5,
      y: 0.0,
      w: 0.5,
      h: 1.0,
    },
    NotesSide::Left => Region {
      x: 0.0,
      y: 0.0,
      w: 0.5,
      h: 1.0,
    },
    NotesSide::Bottom => Region {
      x: 0.0,
      y: 0.5,
      w: 1.0,
      h: 0.5,
    },
    NotesSide::Top => Region {
      x: 0.0,
      y: 0.0,
      w: 1.0,
      h: 0.5,
    },
  }
}

/// A page whose aspect is at least this wide is a doubled (slide + notes) page.
/// The widest normal slide is 16:9 ≈ 1.78, so ~2.4 cleanly separates the two.
const SPLIT_ASPECT: f32 = 2.4;

/// Whether a page with the given aspect carries a notes split, for the deck's notes style.
fn page_is_split_for(notes: NotesLayout, aspect: f32) -> bool {
  match notes {
    NotesLayout::None => false,
    NotesLayout::Split(NotesSide::Right | NotesSide::Left) => aspect >= SPLIT_ASPECT,
    NotesLayout::Split(NotesSide::Top | NotesSide::Bottom) => aspect <= 1.0 / SPLIT_ASPECT,
  }
}

/// Detect whether a deck uses Beamer/Typst "notes on second screen=right" split pages.
///
/// The deck is treated as a notes deck if *any* page is roughly double-width — decks can
/// mix double-width (slide + notes) pages with plain single-width slides, so a single
/// full-width page in the middle must not switch the whole document out of notes mode.
/// The per-page split is then decided individually (see `page_is_split`).
fn detect_notes_layout(r: &impl PageRenderer) -> NotesLayout {
  let uses_notes = (0..r.page_count()).any(|p| r.page_size(p).aspect() >= SPLIT_ASPECT);
  if uses_notes {
    NotesLayout::Split(NotesSide::Right)
  } else {
    NotesLayout::None
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn split_is_per_page() {
    let notes = NotesLayout::Split(NotesSide::Right);
    // A double-width page (slide + notes) is split; a plain 16:9 or 4:3 page is not,
    // so a single-width slide in the middle of a notes deck is shown whole.
    assert!(page_is_split_for(notes, 3.56)); // 2× 16:9
    assert!(page_is_split_for(notes, 2.66)); // 2× 4:3
    assert!(!page_is_split_for(notes, 1.78)); // plain 16:9
    assert!(!page_is_split_for(notes, 1.33)); // plain 4:3
    assert!(!page_is_split_for(NotesLayout::None, 3.56));
  }

  #[test]
  fn maps_page_areas_onto_the_slide_half() {
    // A media rectangle on the left (slide) half of a slide + notes page.
    let area = Region {
      x: 0.1,
      y: 0.2,
      w: 0.2,
      h: 0.5,
    };
    let on = on_slide(slide_side(NotesSide::Right), area).expect("on the slide");
    assert_eq!(
      on,
      Region {
        x: 0.2,
        y: 0.2,
        w: 0.4,
        h: 0.5
      }
    );
    assert_eq!(on_slide(Region::FULL, area), Some(area));
    assert_eq!(on_slide(slide_side(NotesSide::Left), area), None, "on the notes half");
  }
}
