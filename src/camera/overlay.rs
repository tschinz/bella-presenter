//! Placement math for the camera overlay.

use gpui_kit::{Bounds, point, size};

use crate::config::Corner;

/// Rectangle for the overlay inside `area`: width `fraction * area.size.width`, aspect-preserving
/// for a frame of `aspect` (width / height), inset by `margin * area.size.width` at `corner`.
pub fn place(area: Bounds<f32>, corner: Corner, fraction: f32, margin: f32, aspect: f32) -> Bounds<f32> {
  let aspect = if aspect.is_finite() && aspect > 0.0 { aspect } else { 16.0 / 9.0 };
  let w = (area.size.width * fraction).max(1.0);
  let h = w / aspect;
  let m = area.size.width * margin;
  let x = match corner {
    Corner::TopLeft | Corner::BottomLeft => area.origin.x + m,
    Corner::TopRight | Corner::BottomRight => area.right() - m - w,
  };
  let y = match corner {
    Corner::TopLeft | Corner::TopRight => area.origin.y + m,
    Corner::BottomLeft | Corner::BottomRight => area.bottom() - m - h,
  };
  Bounds::new(point(x, y), size(w, h))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn area() -> Bounds<f32> {
    Bounds::new(point(0.0, 0.0), size(1000.0, 500.0))
  }

  #[test]
  fn bottom_right_keeps_aspect_and_margin() {
    let r = place(area(), Corner::BottomRight, 0.2, 0.01, 2.0);
    assert_eq!(r.size.width, 200.0);
    assert_eq!(r.size.height, 100.0);
    assert_eq!(r.right(), 990.0);
    assert_eq!(r.bottom(), 490.0);
  }

  #[test]
  fn top_left_is_inset_by_margin() {
    let r = place(area(), Corner::TopLeft, 0.2, 0.01, 2.0);
    assert_eq!(r.origin, point(10.0, 10.0));
  }

  #[test]
  fn invalid_aspect_falls_back() {
    let r = place(area(), Corner::TopRight, 0.2, 0.0, 0.0);
    assert!(r.size.height > 0.0 && r.size.height.is_finite());
  }
}
