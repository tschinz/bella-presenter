//! The look of Bella Commander: its two grey palettes with a blue accent and its bundled Iosevka
//! font, with larger text, since a presenter often stands a step away from the screen.

use std::borrow::Cow;

use gpui_kit::component::{ActiveTheme, Theme, ThemeMode};
use gpui_kit::{App, Hsla, Window, px, rgb};

use crate::config;

/// Iosevka Nerd Font Mono (SIL OFL 1.1, see `assets/fonts/Iosevka-LICENSE.md` and
/// `NerdFonts-NOTICE.md`), Bella Commander's UI font, bundled unmodified.
const FONTS: [&[u8]; 2] = [
  include_bytes!("../assets/fonts/IosevkaNerdFontMono-Regular.ttf"),
  include_bytes!("../assets/fonts/IosevkaNerdFontMono-Bold.ttf"),
];
const FONT: &str = "Iosevka Nerd Font Mono";
/// Size of regular UI text; components and `rem` sizes scale with it.
const TEXT_SIZE: f32 = 18.0;

/// Registers the bundled fonts so they work without being installed.
pub fn load_fonts(cx: &mut App) {
  if let Err(e) = cx.text_system().add_fonts(FONTS.iter().map(|bytes| Cow::Borrowed(*bytes)).collect()) {
    tracing::error!("cannot load bundled fonts: {e:#}");
  }
}

/// Colors used by our views.
#[derive(Debug, Clone, Copy)]
pub struct Colors {
  pub background: Hsla,
  pub row_alt: Hsla,
  /// Header and footer.
  pub bar: Hsla,
  pub border: Hsla,
  pub text: Hsla,
  pub muted: Hsla,
  /// The accent: the current slide, selections, focus rings.
  pub cursor: Hsla,
  pub cursor_text: Hsla,
}

impl Colors {
  fn dark() -> Self {
    Self {
      background: rgb(0x282828).into(),
      row_alt: rgb(0x2f2f2f).into(),
      bar: rgb(0x323232).into(),
      border: rgb(0x1b1b1b).into(),
      text: rgb(0xe6e6e6).into(),
      muted: rgb(0x9a9a9a).into(),
      cursor: rgb(0x2d5ea6).into(),
      cursor_text: rgb(0xffffff).into(),
    }
  }

  fn light() -> Self {
    Self {
      background: rgb(0xffffff).into(),
      row_alt: rgb(0xf4f4f4).into(),
      bar: rgb(0xececec).into(),
      border: rgb(0xd4d4d4).into(),
      text: rgb(0x1e1e1e).into(),
      muted: rgb(0x6e6e6e).into(),
      cursor: rgb(0x2f6fd6).into(),
      cursor_text: rgb(0xffffff).into(),
    }
  }

  /// The palette for the current light or dark mode.
  pub fn current(cx: &App) -> Self {
    if cx.theme().is_dark() { Self::dark() } else { Self::light() }
  }
}

/// Switches to the light or dark mode `mode` asks for, then pushes our palette, font and text size
/// into the component theme so its widgets match.
pub fn apply(mode: config::Theme, window: &mut Window, cx: &mut App) {
  match mode {
    config::Theme::System => Theme::sync_system_appearance(Some(window), cx),
    config::Theme::Dark => Theme::change(ThemeMode::Dark, Some(window), cx),
    config::Theme::Light => Theme::change(ThemeMode::Light, Some(window), cx),
  }
  let c = Colors::current(cx);
  Theme::update(cx, |theme| {
    theme.font_family = FONT.into();
    theme.mono_font_family = FONT.into();
    theme.font_size = px(TEXT_SIZE);
    theme.mono_font_size = px(TEXT_SIZE);
    theme.colors.background = c.background;
    theme.colors.foreground = c.text;
    theme.colors.muted_foreground = c.muted;
    theme.colors.border = c.border;
    theme.colors.input = c.border;
    theme.colors.popover = c.bar;
    theme.colors.popover_foreground = c.text;
    theme.colors.list = c.background;
    theme.colors.list_even = c.row_alt;
    theme.colors.list_hover = c.row_alt;
    theme.colors.list_active = c.cursor;
    // Primary buttons (e.g. the active camera choice) in the accent blue, like the current slide.
    theme.colors.primary = c.cursor;
    theme.colors.primary_hover = Hsla {
      l: c.cursor.l + 0.06,
      ..c.cursor
    };
    theme.colors.primary_active = Hsla {
      l: c.cursor.l - 0.06,
      ..c.cursor
    };
    theme.colors.primary_foreground = c.cursor_text;
    theme.colors.accent = c.cursor;
    theme.colors.accent_foreground = c.cursor_text;
    theme.colors.selection = c.cursor;
    theme.colors.ring = c.cursor;
  });
  window.refresh();
}

gpui_kit::assets::icon_assets!(
  ExtraIcons,
  [
    X,
    CircleStop,
    Square,
    RotateCcw,
    ArrowLeftRight,
    Pencil,
    MousePointer,
    Eraser,
    Laptop,
    Keyboard,
    Clock,
    Timer,
    ArrowUpLeft,
    ArrowUpRight,
    ArrowDownLeft,
    ArrowDownRight
  ]
);

/// The Lucide icons the views draw beyond the component library's default set, then that set.
#[derive(Clone, Copy, Debug, Default)]
pub struct Assets;

impl gpui_kit::AssetSource for Assets {
  fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
    match ExtraIcons.load(path)? {
      Some(bytes) => Ok(Some(bytes)),
      None => gpui_kit::assets::Assets.load(path),
    }
  }

  fn list(&self, path: &str) -> gpui_kit::Result<Vec<gpui_kit::SharedString>> {
    let mut names = ExtraIcons.list(path)?;
    names.extend(gpui_kit::assets::Assets.list(path)?);
    names.sort();
    names.dedup();
    Ok(names)
  }
}

#[cfg(test)]
mod tests {
  use gpui_kit::AssetSource as _;

  use super::Assets;

  /// An icon missing from the embedded set draws nothing and leaves an empty gap.
  #[test]
  fn every_icon_the_views_draw_is_embedded() {
    let sources = [include_str!("app.rs"), include_str!("camera/controller.rs")].concat();
    let names = sources
      .split("Lucide::")
      .skip(1)
      .filter_map(|rest| rest.split(|c: char| !c.is_alphanumeric()).next());
    for name in names.filter(|n| !n.is_empty()) {
      // `RotateCcw` is `icons/rotate-ccw.svg`.
      let kebab: String = name
        .chars()
        .enumerate()
        .flat_map(|(i, c)| (i > 0 && c.is_uppercase()).then_some('-').into_iter().chain(c.to_lowercase()))
        .collect();
      let path = format!("icons/{kebab}.svg");
      assert!(matches!(Assets.load(&path), Ok(Some(_))), "{name} ({path}) is not embedded");
    }
  }
}
