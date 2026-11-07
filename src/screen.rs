//! Which monitor the presenter and the audience windows belong on, from GPUI's displays.

use std::rc::Rc;

use gpui_kit::{App, PlatformDisplay};

pub type Monitor = Rc<dyn PlatformDisplay>;

/// The primary monitor (where the presenter window belongs).
pub fn primary(cx: &App) -> Option<Monitor> {
  cx.primary_display().or_else(|| cx.displays().into_iter().next())
}

/// A secondary (non-primary) monitor, if one is attached (where the audience window belongs).
pub fn external(cx: &App) -> Option<Monitor> {
  let primary = primary(cx).map(|d| d.id());
  cx.displays().into_iter().find(|d| Some(d.id()) != primary)
}

/// The monitor the audience window should occupy. With two screens it is the external one
/// (or the primary when `swapped`); with a single screen it is the primary.
pub fn audience_monitor(swapped: bool, cx: &App) -> Option<Monitor> {
  match (primary(cx), external(cx)) {
    (Some(p), Some(e)) => Some(if swapped { p } else { e }),
    (p, e) => p.or(e),
  }
}

/// The monitor the presenter window should occupy (the one the audience is not on).
pub fn presenter_monitor(swapped: bool, cx: &App) -> Option<Monitor> {
  match (primary(cx), external(cx)) {
    (Some(p), Some(e)) => Some(if swapped { e } else { p }),
    (p, e) => p.or(e),
  }
}
