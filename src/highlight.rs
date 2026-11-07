use crate::diff::{FileDiff, LineKind};
use ratatui::style::{Color, Modifier, Style};
use std::path::Path;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style as SyntectStyle, Theme};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use two_face::theme::{EmbeddedLazyThemeSet, EmbeddedThemeName};

pub const DEFAULT_THEME: &str = "base16-ocean.dark";

/// Files larger than this are drawn uncolored.
const MAX_FILE_BYTES: usize = 1024 * 1024;
/// A file with a longer line is drawn uncolored (minified or generated code).
const MAX_LINE_BYTES: usize = 10_000;

/// One line of text split into colored pieces, without its newline.
pub type Styled = Vec<(Style, String)>;

/// Colors for every line of a diff, indexed like `FileDiff::hunks[h].lines[l]`.
/// A line is `None` when its file version could not be highlighted.
#[derive(Debug, Default, PartialEq)]
pub struct FileHighlight {
  pub hunks: Vec<Vec<Option<Styled>>>,
}

/// Checks `name` against the two-face themes, ignoring case, and returns the
/// canonical spelling.
pub fn resolve_theme(name: &str) -> Result<String, String> {
  let names = EmbeddedLazyThemeSet::theme_names();
  names
    .iter()
    .map(|n| n.as_name())
    .find(|n| n.eq_ignore_ascii_case(name))
    .map(str::to_string)
    .ok_or_else(|| {
      let list: Vec<_> = names.iter().map(|n| n.as_name()).collect();
      format!(
        "unknown theme '{name}', expected one of: {}",
        list.join(", ")
      )
    })
}

pub struct Highlighter {
  syntaxes: SyntaxSet,
  theme: Theme,
}

impl Highlighter {
  /// `theme` must be a name accepted by `resolve_theme`.
  pub fn new(theme: &str) -> Result<Self, String> {
    let canonical = resolve_theme(theme)?;
    let id = EmbeddedLazyThemeSet::theme_names()
      .iter()
      .find(|n| n.as_name() == canonical)
      .copied()
      .unwrap_or(EmbeddedThemeName::Base16OceanDark);
    Ok(Highlighter {
      syntaxes: two_face::syntax::extra_newlines(),
      theme: two_face::theme::extra().get(id).clone(),
    })
  }

  /// Highlights a whole file from the top, so multi-line strings and block
  /// comments carry across lines. Returns `None` for files that are too
  /// big, have very long lines, or have no known syntax.
  pub fn file(&self, path: &Path, bytes: &[u8]) -> Option<Vec<Styled>> {
    if bytes.len() > MAX_FILE_BYTES
      || bytes
        .split(|&b| b == b'\n')
        .any(|l| l.len() > MAX_LINE_BYTES)
    {
      return None;
    }
    let syntax = self.syntax_for(path)?;
    let text = String::from_utf8_lossy(bytes);
    let mut lines = HighlightLines::new(syntax, &self.theme);
    let mut out = Vec::new();
    for line in LinesWithEndings::from(&text) {
      let pieces = lines.highlight_line(line, &self.syntaxes).ok()?;
      out.push(
        pieces
          .into_iter()
          .map(|(style, piece)| {
            (
              convert(style),
              piece.trim_end_matches(['\n', '\r']).to_string(),
            )
          })
          .filter(|(_, piece)| !piece.is_empty())
          .collect(),
      );
    }
    Some(out)
  }

  fn syntax_for(&self, path: &Path) -> Option<&syntect::parsing::SyntaxReference> {
    let name = path.file_name()?.to_str()?;
    self.syntaxes.find_syntax_by_extension(name).or_else(|| {
      self
        .syntaxes
        .find_syntax_by_extension(path.extension()?.to_str()?)
    })
  }

  /// Highlights both versions of a file and maps the colors onto its diff
  /// lines through their line numbers.
  pub fn diff(&self, file: &FileDiff, old: Option<&[u8]>, new: Option<&[u8]>) -> FileHighlight {
    let old = old
      .zip(file.old_path.as_deref())
      .and_then(|(bytes, path)| self.file(path, bytes));
    let new = new
      .zip(file.new_path.as_deref())
      .and_then(|(bytes, path)| self.file(path, bytes));
    map_lines(file, old.as_deref(), new.as_deref())
  }
}

fn map_lines(file: &FileDiff, old: Option<&[Styled]>, new: Option<&[Styled]>) -> FileHighlight {
  let pick = |side: Option<&[Styled]>, no: Option<u32>| -> Option<Styled> {
    let index = usize::try_from(no?).ok()?.checked_sub(1)?;
    side?.get(index).cloned()
  };
  let hunks = file
    .hunks
    .iter()
    .map(|h| {
      h.lines
        .iter()
        .map(|l| match l.kind {
          LineKind::Removed => pick(old, l.old_no),
          LineKind::Added | LineKind::Context => pick(new, l.new_no),
        })
        .collect()
    })
    .collect();
  FileHighlight { hunks }
}

/// Themes such as `ansi` and `base16` encode terminal palette colors in the
/// alpha channel: 0 means the palette index is in red, 1 means the default.
fn convert(style: SyntectStyle) -> Style {
  let fg = style.foreground;
  let color = match fg.a {
    0 => Some(Color::Indexed(fg.r)),
    1 => None,
    _ => Some(Color::Rgb(fg.r, fg.g, fg.b)),
  };
  let mut out = Style::new();
  if let Some(color) = color {
    out = out.fg(color);
  }
  if style.font_style.contains(FontStyle::BOLD) {
    out = out.add_modifier(Modifier::BOLD);
  }
  if style.font_style.contains(FontStyle::ITALIC) {
    out = out.add_modifier(Modifier::ITALIC);
  }
  if style.font_style.contains(FontStyle::UNDERLINE) {
    out = out.add_modifier(Modifier::UNDERLINED);
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::diff::parse;

  fn highlighter() -> Highlighter {
    Highlighter::new(DEFAULT_THEME).unwrap()
  }

  /// The distinct colors on a line, in order.
  fn colors(line: &Styled) -> Vec<Option<Color>> {
    let mut out: Vec<_> = line.iter().map(|(s, _)| s.fg).collect();
    out.dedup();
    out
  }

  #[test]
  fn theme_names_are_validated_ignoring_case() {
    assert_eq!(resolve_theme("nord").unwrap(), "Nord");
    assert_eq!(resolve_theme("Dracula").unwrap(), "Dracula");
    let err = resolve_theme("nope").unwrap_err();
    assert!(err.contains("unknown theme 'nope'") && err.contains("Nord"));
  }

  #[test]
  fn block_comments_and_strings_span_lines() {
    let h = highlighter();
    let lines = h
      .file(Path::new("a.rs"), b"/* one\ntwo\nthree */\nfn main() {}\n")
      .unwrap();
    assert_eq!(lines.len(), 4);
    let comment = colors(&lines[0]);
    assert_eq!(colors(&lines[1]), comment);
    assert_eq!(colors(&lines[2]), comment);
    assert_ne!(colors(&lines[3]), comment);

    let lines = h.file(Path::new("a.rs"), b"let s = \"a\nb\";\n").unwrap();
    let piece = |l: usize, t: &str| lines[l].iter().find(|(_, p)| p == t).unwrap().0.fg;
    assert_eq!(piece(0, "a"), piece(1, "b"));
    assert_ne!(piece(1, "b"), piece(1, ";"));
  }

  #[test]
  fn pieces_exclude_newlines() {
    let lines = highlighter()
      .file(Path::new("a.rs"), b"fn a() {}\r\n")
      .unwrap();
    assert_eq!(
      lines[0].iter().map(|(_, t)| t.as_str()).collect::<String>(),
      "fn a() {}"
    );
  }

  #[test]
  fn skips_large_files_long_lines_and_unknown_syntax() {
    let h = highlighter();
    assert!(
      h.file(Path::new("a.rs"), &vec![b'a'; MAX_FILE_BYTES + 1])
        .is_none()
    );
    let mut long = vec![b'a'; MAX_LINE_BYTES + 1];
    long.push(b'\n');
    assert!(h.file(Path::new("a.rs"), &long).is_none());
    assert!(h.file(Path::new("a.zzzunknown"), b"x\n").is_none());
    assert!(h.file(Path::new("a.rs"), b"fn a() {}\n").is_some());
  }

  #[test]
  fn finds_syntax_by_file_name() {
    let h = highlighter();
    assert!(
      h.file(Path::new("sub/Makefile"), b"all:\n\techo hi\n")
        .is_some()
    );
  }

  #[test]
  fn maps_old_and_new_versions_onto_diff_lines() {
    let diff = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -2,3 +2,3 @@
 two
-/* gone
+let x = 1;
 */
";
    let file = parse(diff.as_bytes()).unwrap().remove(0);
    let old = b"one\ntwo\n/* gone\n*/\n";
    let new = b"one\ntwo\nlet x = 1;\n*/\n";
    let h = highlighter().diff(&file, Some(old), Some(new));
    let lines = &h.hunks[0];
    let text = |l: &Option<Styled>| {
      l.as_ref()
        .unwrap()
        .iter()
        .map(|(_, t)| t.as_str())
        .collect::<String>()
    };
    assert_eq!(text(&lines[0]), "two");
    // The removed line is a comment only in the old file.
    assert_eq!(text(&lines[1]), "/* gone");
    assert_eq!(text(&lines[2]), "let x = 1;");
    // The closing line is a comment in the old file but code in the new.
    assert_eq!(text(&lines[3]), "*/");
  }

  #[test]
  fn missing_versions_leave_lines_uncolored() {
    let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-a\n+b\n";
    let file = parse(diff.as_bytes()).unwrap().remove(0);
    let h = highlighter().diff(&file, None, Some(b"b\n"));
    assert!(h.hunks[0][0].is_none());
    assert!(h.hunks[0][1].is_some());
  }

  #[test]
  fn palette_themes_map_to_indexed_colors() {
    let style = convert(SyntectStyle {
      foreground: syntect::highlighting::Color {
        r: 4,
        g: 0,
        b: 0,
        a: 0,
      },
      background: syntect::highlighting::Color::BLACK,
      font_style: FontStyle::BOLD,
    });
    assert_eq!(style.fg, Some(Color::Indexed(4)));
    assert!(style.add_modifier.contains(Modifier::BOLD));
  }
}
