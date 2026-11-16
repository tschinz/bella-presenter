//! Headless smoke test for the render pipeline (no GUI): load PDFium, open the example deck,
//! detect its notes page, rasterize pages and read its media.
//!
//! NOTE: PDFium is single-threaded — its library init is not safe to call from
//! multiple threads at once. `cargo test` runs each `#[test]` on its own thread,
//! so all PDFium work lives in ONE test function here. If you add more PDFium
//! tests, keep them in this single function (or gate them behind a shared serial
//! lock); do not split them into separate `#[test]`s.

use std::path::Path;

use bella_presenter::document::{Document, NotesLayout};
use bella_presenter::media::{Format, Item, Source};
use bella_presenter::render::Region;

#[test]
fn render_pipeline_smoke() {
  // Skip when the example deck isn't present (e.g. a checkout without it).
  let path = Path::new("examples/showcase.pdf");
  if !path.exists() {
    eprintln!("skipping: examples/showcase.pdf not present");
    return;
  }
  let deck = Document::open(path).expect("open showcase");

  // One double-width page (slide + notes) makes it a notes deck; plain 16:9 pages stay whole.
  assert!(matches!(deck.notes_layout(), NotesLayout::Split(_)));
  assert_eq!(deck.slide_region(0), Region::FULL);
  assert!(deck.render_notes(0, [400, 225]).expect("notes ok").is_none());

  // A plain page renders at the requested size and is not a flat color.
  let img = deck.render_page(0, [800, 450]).expect("render page 0");
  assert_eq!((img.width(), img.height()), (800, 450));
  let first = img.as_raw().first().copied();
  assert!(
    img.as_raw().iter().any(|p| Some(*p) != first),
    "rendered page should not be a single flat color"
  );

  // Pixels come in GPUI's BGRA order: a part divider is filled crimson (#AA0159).
  let divider = (0..deck.page_count())
    .find(|p| {
      let img = deck.render_slide(*p, [32, 18]).expect("render");
      let [b, _, r, _] = img.get_pixel(1, 1).0;
      r > 150 && b < 110
    })
    .expect("a crimson divider page: red and blue swapped?");
  assert!(divider > 0);

  // The notes page: the slide half is ~16:9, and the notes half rendered on its own matches the
  // right half of the whole page.
  let notes = (0..deck.page_count()).find(|p| deck.notes_region(*p).is_some()).expect("a notes page");
  assert!((deck.slide_aspect(notes) - 1.78).abs() < 0.05, "slide aspect {}", deck.slide_aspect(notes));
  let slide = deck.render_slide(notes, [640, 360]).expect("render slide region");
  assert_eq!((slide.width(), slide.height()), (640, 360));
  let notes_img = deck.render_notes(notes, [640, 360]).expect("notes ok").expect("notes present");
  let whole = deck.render_page(notes, [1280, 360]).expect("render whole page");
  let right = image::imageops::crop_imm(&whole, 640, 0, 640, 360).to_image();
  let differing = notes_img
    .pixels()
    .zip(right.pixels())
    .filter(|(a, b)| a.0.iter().zip(b.0).any(|(x, y)| x.abs_diff(y) > 8))
    .count();
  assert!(differing < 640 * 360 / 100, "{differing} pixels differ");

  // Media come out of the manifest and attachments; one sits on the slide half of the notes page.
  assert!(deck.media_warnings().is_empty(), "{:?}", deck.media_warnings());
  assert_eq!(deck.slide_media(notes).len(), 1);
  let kind = |m: &Item| (m.format, matches!(m.source, Source::Embedded(_)));
  let on_page = |page: usize| deck.media().iter().filter(|m| m.page == page).map(kind).collect::<Vec<_>>();
  // The four-section slide: local GIF, web GIF, local MP4, web MP4.
  let four = deck.media().iter().map(|m| m.page).find(|p| on_page(*p).len() == 4).expect("four-media slide");
  assert_eq!(
    on_page(four),
    [(Format::Gif, true), (Format::Gif, false), (Format::Mp4, true), (Format::Mp4, false)]
  );

  // A build in two steps: the clip is hidden on step 1 (still in the manifest) and drawn on step 2,
  // so only step 2 plays it.
  assert_eq!(deck.media_hidden(), 1, "the hidden build step");
  let step2 = (1..deck.page_count())
    .find(|p| on_page(*p) == [(Format::Mp4, true)] && on_page(p - 1).is_empty() && deck.slide_aspect(p - 1) == deck.slide_aspect(*p))
    .expect("build step 2 with its clip, after step 1 without");
  assert!(step2 > four);
  // Videos play their sound by default; the showcase mutes all but the one on the sound slide.
  let videos: Vec<_> = deck.media().iter().filter(|m| kind(m) == (Format::Mp4, true)).collect();
  assert_eq!(videos.iter().filter(|m| !m.muted).count(), 1, "one video with sound");
  assert!(videos.iter().any(|m| m.page == four && m.muted), "the four-media video is muted");
  // Clips start by themselves unless they set `autoplay: false`; the showcase keeps the default.
  assert!(deck.media().iter().all(|m| m.autoplay));

  // The web GIF there has no poster, so it shows a black block: its edges are the media rectangle
  // (top-left origin, y down), so playing media covers the placeholder exactly.
  let (w, h) = (1000, 563);
  let (_, area, _) = deck.slide_media(four)[1];
  let img = deck.render_slide(four, [w, h]).expect("render media page");
  let black = |x: u32, y: u32| img.get_pixel(x, y).0[..3].iter().all(|c| *c < 30);
  let (cx, cy) = (((area.x + area.w / 2.0) * w as f32) as u32, ((area.y + area.h / 2.0) * h as f32) as u32);
  // The solid black run through the centre, along each axis.
  let left = (0..cx).rev().find(|x| !black(*x, cy)).map_or(0, |x| x + 1);
  let right = (cx..w).find(|x| !black(*x, cy)).unwrap_or(w);
  let top = (0..cy).rev().find(|y| !black(cx, *y)).map_or(0, |y| y + 1);
  let bottom = (cy..h).find(|y| !black(cx, *y)).unwrap_or(h);
  let near = |px: u32, at: f32| (px as f32 - at).abs() <= 1.5;
  assert!(
    near(left, area.x * w as f32) && near(right, (area.x + area.w) * w as f32),
    "{left}..{right} {area:?}"
  );
  assert!(
    near(top, area.y * h as f32) && near(bottom, (area.y + area.h) * h as f32),
    "{top}..{bottom} {area:?}"
  );
}
