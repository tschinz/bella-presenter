//! Headless smoke test for the render pipeline (no GUI): load PDFium, open the
//! example decks, detect notes layout, and rasterize a page.
//!
//! NOTE: PDFium is single-threaded — its library init is not safe to call from
//! multiple threads at once. `cargo test` runs each `#[test]` on its own thread,
//! so all PDFium work lives in ONE test function here. If you add more PDFium
//! tests, keep them in this single function (or gate them behind a shared serial
//! lock); do not split them into separate `#[test]`s.

use std::path::Path;

use bella_presenter::document::{Document, NotesLayout};

#[test]
fn render_pipeline_smoke() {
  // Skip when the sample decks aren't present (e.g. a checkout without them).
  let plain_path = Path::new("examples/04-aprog-ptr-en.pdf");
  let notes_path = Path::new("examples/04-aprog-ptr-notes-en.pdf");
  if !plain_path.exists() || !notes_path.exists() {
    eprintln!("skipping: example PDFs not present");
    return;
  }

  // Plain deck: no notes, renders to the requested pixel size, not a flat color.
  let plain = Document::open(plain_path).expect("open plain deck");
  assert_eq!(plain.page_count(), 32);
  assert_eq!(plain.notes_layout(), NotesLayout::None);

  let img = plain.render_page(0, [800, 450]).expect("render page 0");
  assert_eq!(img.width(), 800);
  assert!(img.height() > 0);
  let first = img.as_raw().first().copied();
  assert!(
    img.as_raw().iter().any(|p| Some(*p) != first),
    "rendered page should not be a single flat color"
  );

  // Plain deck has no notes region.
  assert!(plain.render_notes(0, [400, 300]).expect("notes ok").is_none());

  // Notes deck (double-width page) is detected as split notes.
  let notes = Document::open(notes_path).expect("open notes deck");
  assert_eq!(notes.page_count(), 32);
  assert!(matches!(notes.notes_layout(), NotesLayout::Split(_)));

  // The slide half of a doubled page is ~16:9 (about half the full page's ~3.56).
  assert!((notes.slide_aspect(0) - 1.78).abs() < 0.2, "slide aspect {}", notes.slide_aspect(0));

  // Rendering the slide region at 640x360 yields exactly that, and the notes region exists.
  let slide = notes.render_slide(0, [640, 360]).expect("render slide region");
  assert_eq!((slide.width(), slide.height()), (640, 360));
  let notes_img = notes.render_notes(0, [640, 360]).expect("notes ok").expect("notes present");
  assert_eq!((notes_img.width(), notes_img.height()), (640, 360));
}
