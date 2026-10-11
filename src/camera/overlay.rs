//! Placement math for the camera overlay.

use egui::{Pos2, Rect, Vec2};

use crate::config::Corner;

/// Rectangle for the overlay inside `area`: width `size * area.width()`, aspect-preserving
/// for a frame of `aspect` (width / height), inset by `margin * area.width()` at `corner`.
pub fn place(area: Rect, corner: Corner, size: f32, margin: f32, aspect: f32) -> Rect {
  let aspect = if aspect.is_finite() && aspect > 0.0 { aspect } else { 16.0 / 9.0 };
  let w = (area.width() * size).max(1.0);
  let h = w / aspect;
  let m = area.width() * margin;
  let x = match corner {
    Corner::TopLeft | Corner::BottomLeft => area.left() + m,
    Corner::TopRight | Corner::BottomRight => area.right() - m - w,
  };
  let y = match corner {
    Corner::TopLeft | Corner::TopRight => area.top() + m,
    Corner::BottomLeft | Corner::BottomRight => area.bottom() - m - h,
  };
  Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn area() -> Rect {
    Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 500.0))
  }

  #[test]
  fn bottom_right_keeps_aspect_and_margin() {
    let r = place(area(), Corner::BottomRight, 0.2, 0.01, 2.0);
    assert_eq!(r.width(), 200.0);
    assert_eq!(r.height(), 100.0);
    assert_eq!(r.right(), 990.0);
    assert_eq!(r.bottom(), 490.0);
  }

  #[test]
  fn top_left_is_inset_by_margin() {
    let r = place(area(), Corner::TopLeft, 0.2, 0.01, 2.0);
    assert_eq!(r.min, Pos2::new(10.0, 10.0));
  }

  #[test]
  fn invalid_aspect_falls_back() {
    let r = place(area(), Corner::TopRight, 0.2, 0.0, 0.0);
    assert!(r.height() > 0.0 && r.height().is_finite());
  }
}
