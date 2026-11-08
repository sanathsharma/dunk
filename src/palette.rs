use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

/// The style of every highlight group. Each starts as its ANSI default, so
/// the terminal's own palette picks the colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
  pub normal: Style,
  pub border: Style,
  pub diff_border: Style,
  pub cursor_line: Style,
  pub section_label: Style,
  pub mode_normal: Style,
  pub mode_visual: Style,
  pub diff_add: Style,
  pub diff_delete: Style,
  pub diff_add_text: Style,
  pub diff_delete_text: Style,
  pub diff_hunk_header: Style,
  pub diff_current_hunk: Style,
  pub diff_hunk_bar: Style,
  pub diff_hunk_bar_staged: Style,
  pub visual: Style,
}

impl Default for Palette {
  fn default() -> Self {
    let mode = Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED);
    Self {
      normal: Style::new(),
      border: Style::new(),
      diff_border: Style::new(),
      cursor_line: Style::new().add_modifier(Modifier::REVERSED),
      section_label: Style::new().add_modifier(Modifier::BOLD),
      mode_normal: mode,
      mode_visual: mode,
      diff_add: Style::new().bg(Color::Green),
      diff_delete: Style::new().bg(Color::Red),
      diff_add_text: Style::new().bg(Color::LightGreen),
      diff_delete_text: Style::new().bg(Color::LightRed),
      diff_hunk_header: Style::new().fg(Color::Cyan),
      diff_current_hunk: Style::new()
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD),
      diff_hunk_bar: Style::new().fg(Color::White),
      diff_hunk_bar_staged: Style::new().fg(Color::Blue),
      visual: Style::new().bg(Color::Blue),
    }
  }
}

/// The `[highlights]` table of the config file. A group that is set replaces
/// the whole default style of that group.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Highlights {
  #[serde(rename = "Normal")]
  normal: Option<Spec>,
  #[serde(rename = "Border")]
  border: Option<Spec>,
  #[serde(rename = "DiffBorder")]
  diff_border: Option<Spec>,
  #[serde(rename = "CursorLine")]
  cursor_line: Option<Spec>,
  #[serde(rename = "SectionLabel")]
  section_label: Option<Spec>,
  #[serde(rename = "ModeNormal")]
  mode_normal: Option<Spec>,
  #[serde(rename = "ModeVisual")]
  mode_visual: Option<Spec>,
  #[serde(rename = "DiffAdd")]
  diff_add: Option<Spec>,
  #[serde(rename = "DiffDelete")]
  diff_delete: Option<Spec>,
  #[serde(rename = "DiffAddText")]
  diff_add_text: Option<Spec>,
  #[serde(rename = "DiffDeleteText")]
  diff_delete_text: Option<Spec>,
  #[serde(rename = "DiffHunkHeader")]
  diff_hunk_header: Option<Spec>,
  #[serde(rename = "DiffCurrentHunk")]
  diff_current_hunk: Option<Spec>,
  #[serde(rename = "DiffHunkBar")]
  diff_hunk_bar: Option<Spec>,
  #[serde(rename = "DiffHunkBarStaged")]
  diff_hunk_bar_staged: Option<Spec>,
  #[serde(rename = "Visual")]
  visual: Option<Spec>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
#[allow(
  clippy::struct_excessive_bools,
  reason = "one flag per style key in the file"
)]
struct Spec {
  fg: Option<Value>,
  bg: Option<Value>,
  #[serde(default)]
  bold: bool,
  #[serde(default)]
  italic: bool,
  #[serde(default)]
  underline: bool,
  #[serde(default)]
  reverse: bool,
}

/// A color as written in the file: a palette index, or a name or `#rrggbb`.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(untagged)]
enum Value {
  Index(i64),
  Text(String),
}

impl Value {
  fn color(&self) -> Result<Color, String> {
    match self {
      Self::Index(n) => u8::try_from(*n)
        .map(Color::Indexed)
        .map_err(|_| format!("palette index {n} is not between 0 and 255")),
      Self::Text(text) => parse_color(text),
    }
  }
}

fn parse_color(text: &str) -> Result<Color, String> {
  if let Some(hex) = text.strip_prefix('#') {
    let digits = (hex.len() == 6 && hex.is_ascii())
      .then(|| u32::from_str_radix(hex, 16).ok())
      .flatten();
    return digits.map_or_else(
      || Err(format!("unknown color '{text}', expected #rrggbb")),
      |n| {
        let [_, r, g, b] = n.to_be_bytes();
        Ok(Color::Rgb(r, g, b))
      },
    );
  }
  Ok(match text.to_ascii_lowercase().as_str() {
    "black" => Color::Black,
    "red" => Color::Red,
    "green" => Color::Green,
    "yellow" => Color::Yellow,
    "blue" => Color::Blue,
    "magenta" => Color::Magenta,
    "cyan" => Color::Cyan,
    "white" => Color::Gray,
    "bright-black" => Color::DarkGray,
    "bright-red" => Color::LightRed,
    "bright-green" => Color::LightGreen,
    "bright-yellow" => Color::LightYellow,
    "bright-blue" => Color::LightBlue,
    "bright-magenta" => Color::LightMagenta,
    "bright-cyan" => Color::LightCyan,
    "bright-white" => Color::White,
    _ => {
      return Err(format!(
        "unknown color '{text}', expected an ANSI name such as red or bright-red, \
         a palette index from 0 to 255, or #rrggbb"
      ));
    }
  })
}

impl Spec {
  fn style(&self, group: &str) -> Result<Style, String> {
    let mut style = Style::new();
    for (key, value) in [("fg", &self.fg), ("bg", &self.bg)] {
      let Some(value) = value else { continue };
      let color = value
        .color()
        .map_err(|e| format!("highlights.{group}.{key}: {e}"))?;
      style = if key == "fg" {
        style.fg(color)
      } else {
        style.bg(color)
      };
    }
    for (on, modifier) in [
      (self.bold, Modifier::BOLD),
      (self.italic, Modifier::ITALIC),
      (self.underline, Modifier::UNDERLINED),
      (self.reverse, Modifier::REVERSED),
    ] {
      if on {
        style = style.add_modifier(modifier);
      }
    }
    Ok(style)
  }
}

impl Highlights {
  /// The default palette with the configured groups replaced.
  pub fn palette(&self) -> Result<Palette, String> {
    let mut palette = Palette::default();
    let groups = [
      ("Normal", &self.normal, &mut palette.normal),
      ("Border", &self.border, &mut palette.border),
      ("DiffBorder", &self.diff_border, &mut palette.diff_border),
      ("CursorLine", &self.cursor_line, &mut palette.cursor_line),
      (
        "SectionLabel",
        &self.section_label,
        &mut palette.section_label,
      ),
      ("ModeNormal", &self.mode_normal, &mut palette.mode_normal),
      ("ModeVisual", &self.mode_visual, &mut palette.mode_visual),
      ("DiffAdd", &self.diff_add, &mut palette.diff_add),
      ("DiffDelete", &self.diff_delete, &mut palette.diff_delete),
      (
        "DiffAddText",
        &self.diff_add_text,
        &mut palette.diff_add_text,
      ),
      (
        "DiffDeleteText",
        &self.diff_delete_text,
        &mut palette.diff_delete_text,
      ),
      (
        "DiffHunkHeader",
        &self.diff_hunk_header,
        &mut palette.diff_hunk_header,
      ),
      (
        "DiffCurrentHunk",
        &self.diff_current_hunk,
        &mut palette.diff_current_hunk,
      ),
      (
        "DiffHunkBar",
        &self.diff_hunk_bar,
        &mut palette.diff_hunk_bar,
      ),
      (
        "DiffHunkBarStaged",
        &self.diff_hunk_bar_staged,
        &mut palette.diff_hunk_bar_staged,
      ),
      ("Visual", &self.visual, &mut palette.visual),
    ];
    for (name, spec, slot) in groups {
      if let Some(spec) = spec {
        *slot = spec.style(name)?;
      }
    }
    Ok(palette)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn palette(text: &str) -> Result<Palette, String> {
    let highlights: Highlights = toml::from_str(text).map_err(|e| e.to_string())?;
    highlights.palette()
  }

  #[test]
  fn no_groups_leaves_the_ansi_defaults() {
    assert_eq!(palette("").unwrap(), Palette::default());
  }

  #[test]
  fn a_set_group_replaces_the_whole_default_and_the_rest_stay() {
    let p = palette("DiffCurrentHunk = { fg = \"red\" }").unwrap();
    assert_eq!(p.diff_current_hunk, Style::new().fg(Color::Red));
    assert_eq!(p.diff_add, Palette::default().diff_add);
  }

  #[test]
  fn colors_come_as_names_indexes_or_hex() {
    let p = palette(
      "DiffAdd = { fg = \"bright-red\", bg = \"#3a1418\", bold = true, underline = true }\n\
       DiffDelete = { bg = 42 }\n\
       Visual = { fg = \"WHITE\", reverse = true, italic = true }",
    )
    .unwrap();
    assert_eq!(
      p.diff_add,
      Style::new()
        .fg(Color::LightRed)
        .bg(Color::Rgb(0x3a, 0x14, 0x18))
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    );
    assert_eq!(p.diff_delete, Style::new().bg(Color::Indexed(42)));
    assert_eq!(
      p.visual,
      Style::new()
        .fg(Color::Gray)
        .add_modifier(Modifier::REVERSED | Modifier::ITALIC)
    );
  }

  #[test]
  fn a_bad_color_names_the_group_and_key() {
    let err = palette("DiffAdd = { bg = \"grn\" }").unwrap_err();
    assert!(
      err.starts_with("highlights.DiffAdd.bg: unknown color 'grn'"),
      "{err}"
    );
    let err = palette("Visual = { fg = 300 }").unwrap_err();
    assert!(
      err.contains("highlights.Visual.fg: palette index 300"),
      "{err}"
    );
    let err = palette("Visual = { fg = \"#12345\" }").unwrap_err();
    assert!(err.contains("expected #rrggbb"), "{err}");
  }

  #[test]
  fn an_unknown_group_lists_the_valid_ones() {
    let err = palette("DiffAdds = { bg = \"red\" }").unwrap_err();
    assert!(err.contains("DiffAdds"), "{err}");
    for group in [
      "Normal",
      "Border",
      "DiffBorder",
      "CursorLine",
      "SectionLabel",
      "ModeNormal",
      "ModeVisual",
      "DiffAdd",
      "DiffDelete",
      "DiffAddText",
      "DiffDeleteText",
      "DiffHunkHeader",
      "DiffCurrentHunk",
      "DiffHunkBar",
      "DiffHunkBarStaged",
      "Visual",
    ] {
      assert!(err.contains(group), "{group} missing from {err}");
    }
  }

  #[test]
  fn an_unknown_style_key_is_an_error() {
    let err = palette("DiffAdd = { background = \"red\" }").unwrap_err();
    assert!(err.contains("background"), "{err}");
  }
}
