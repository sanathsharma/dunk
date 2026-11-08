use crate::diff::{FileDiff, LineKind};
use crate::vcs::{Sides, VersionId};
use ratatui::style::{Color, Modifier, Style};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
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

/// Highlighted file versions kept for reuse across refreshes.
const CACHE_ENTRIES: usize = 256;

/// One line of text split into colored pieces, without its newline.
pub type Styled = Vec<(Style, String)>;

/// Colors for every line of a diff, indexed like `FileDiff::hunks[h].lines[l]`.
/// A line is `None` when its file version could not be highlighted.
#[derive(Debug, Default, PartialEq, Eq)]
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

/// A file version's highlighting, `None` when it could not be highlighted.
type Lines = Option<Arc<Vec<Styled>>>;

/// Highlighted versions keyed by version ID and path. The path is part of
/// the key because it picks the syntax. The oldest entry goes first when
/// the cache is full, where a lookup counts as a use.
#[derive(Default)]
struct Cache {
  entries: HashMap<(VersionId, PathBuf), Lines>,
  /// Keys from least to most recently used.
  order: VecDeque<(VersionId, PathBuf)>,
}

impl Cache {
  fn get(&mut self, key: &(VersionId, PathBuf)) -> Option<Lines> {
    let hit = self.entries.get(key)?.clone();
    self.touch(key);
    Some(hit)
  }

  fn insert(&mut self, key: (VersionId, PathBuf), lines: Lines) {
    if self.entries.insert(key.clone(), lines).is_some() {
      self.touch(&key);
      return;
    }
    self.order.push_back(key);
    if self.order.len() > CACHE_ENTRIES
      && let Some(oldest) = self.order.pop_front()
    {
      self.entries.remove(&oldest);
    }
  }

  fn touch(&mut self, key: &(VersionId, PathBuf)) {
    if let Some(at) = self.order.iter().position(|k| k == key)
      && let Some(key) = self.order.remove(at)
    {
      self.order.push_back(key);
    }
  }
}

pub struct Highlighter {
  syntaxes: SyntaxSet,
  theme: Theme,
  cache: Mutex<Cache>,
  runs: AtomicUsize,
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
    Ok(Self {
      syntaxes: two_face::syntax::extra_newlines(),
      theme: two_face::theme::extra().get(id).clone(),
      cache: Mutex::default(),
      runs: AtomicUsize::new(0),
    })
  }

  /// Highlights a whole file from the top, so multi-line strings and block
  /// comments carry across lines. Returns `None` for files that are too
  /// big, have very long lines, or have no known syntax.
  pub fn file(&self, path: &Path, bytes: &[u8]) -> Option<Vec<Styled>> {
    self.runs.fetch_add(1, Ordering::Relaxed);
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

  /// How many files `file` has highlighted so far.
  #[cfg(test)]
  pub(crate) fn runs(&self) -> usize {
    self.runs.load(Ordering::Relaxed)
  }

  /// Highlights both versions of a file and maps the colors onto its diff
  /// lines through their line numbers. A version with an ID is highlighted
  /// once and reused until the cache drops it.
  pub fn diff(&self, file: &FileDiff, sides: &Sides) -> FileHighlight {
    let old = self.version(
      file.old_path.as_deref(),
      sides.old.as_deref(),
      sides.old_id.as_ref(),
    );
    let new = self.version(
      file.new_path.as_deref(),
      sides.new.as_deref(),
      sides.new_id.as_ref(),
    );
    map_lines(
      file,
      old.as_deref().map(Vec::as_slice),
      new.as_deref().map(Vec::as_slice),
    )
  }

  fn version(&self, path: Option<&Path>, bytes: Option<&[u8]>, id: Option<&VersionId>) -> Lines {
    let (path, bytes) = path.zip(bytes)?;
    let Some(id) = id else {
      return self.file(path, bytes).map(Arc::new);
    };
    let key = (*id, path.to_path_buf());
    let hit = self.cache().get(&key);
    if let Some(hit) = hit {
      return hit;
    }
    let lines = self.file(path, bytes).map(Arc::new);
    self.cache().insert(key, lines.clone());
    lines
  }

  /// The cache stays usable after a panic elsewhere, since an entry is never
  /// left half written.
  fn cache(&self) -> MutexGuard<'_, Cache> {
    self.cache.lock().unwrap_or_else(PoisonError::into_inner)
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
const fn convert(style: SyntectStyle) -> Style {
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
    let h = highlighter().diff(&file, &sides(Some(old), Some(new)));
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
  fn expanded_lines_are_colored_from_the_whole_file() {
    let diff =
      "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -4 +4 @@\n-let x = 0;\n+let x = 1;\n";
    let file = parse(diff.as_bytes()).unwrap().remove(0);
    let old = b"/* start\nmiddle\nend */\nlet x = 0;\n";
    let new = b"/* start\nmiddle\nend */\nlet x = 1;\n";
    let (file, _) = file.widened(&[5], new);
    let h = highlighter().diff(&file, &sides(Some(old), Some(new)));
    let lines = &h.hunks[0];
    assert_eq!(lines.len(), 5);
    // "middle" is only a comment because the highlighter saw the line above.
    let comment = colors(lines[0].as_ref().unwrap());
    assert_eq!(colors(lines[1].as_ref().unwrap()), comment);
    assert_eq!(colors(lines[2].as_ref().unwrap()), comment);
    assert_ne!(colors(lines[4].as_ref().unwrap()), comment);
  }

  #[test]
  fn missing_versions_leave_lines_uncolored() {
    let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-a\n+b\n";
    let file = parse(diff.as_bytes()).unwrap().remove(0);
    let h = highlighter().diff(&file, &sides(None, Some(b"b\n")));
    assert!(h.hunks[0][0].is_none());
    assert!(h.hunks[0][1].is_some());
  }

  fn sides(old: Option<&[u8]>, new: Option<&[u8]>) -> Sides {
    Sides {
      old: old.map(<[u8]>::to_vec),
      new: new.map(<[u8]>::to_vec),
      ..Sides::default()
    }
  }

  fn one_line_file() -> FileDiff {
    let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-a\n+b\n";
    parse(diff.as_bytes()).unwrap().remove(0)
  }

  fn identified(old: &[u8], new: &[u8], old_id: u64, new_id: u64) -> Sides {
    Sides {
      old_id: Some(VersionId(old_id)),
      new_id: Some(VersionId(new_id)),
      ..sides(Some(old), Some(new))
    }
  }

  #[test]
  fn a_repeated_diff_reuses_highlighted_versions() {
    let h = highlighter();
    let file = one_line_file();
    let first = h.diff(&file, &identified(b"a\n", b"b\n", 1, 2));
    assert_eq!(h.runs(), 2);
    let again = h.diff(&file, &identified(b"a\n", b"b\n", 1, 2));
    assert_eq!(h.runs(), 2);
    assert_eq!(first, again);
    // A changed version is highlighted again, the unchanged one is not.
    h.diff(&file, &identified(b"a\n", b"c\n", 1, 3));
    assert_eq!(h.runs(), 3);
  }

  #[test]
  fn versions_without_an_id_are_never_cached() {
    let h = highlighter();
    let file = one_line_file();
    h.diff(&file, &sides(Some(b"a\n"), Some(b"b\n")));
    h.diff(&file, &sides(Some(b"a\n"), Some(b"b\n")));
    assert_eq!(h.runs(), 4);
  }

  #[test]
  fn the_cache_forgets_old_versions_past_its_limit() {
    let h = highlighter();
    let file = one_line_file();
    for i in 0..CACHE_ENTRIES + 10 {
      h.diff(&file, &identified(b"a\n", b"b\n", 0, 10 + i as u64));
    }
    let before = h.runs();
    // The first new version was evicted, the latest one is still held.
    h.diff(&file, &identified(b"a\n", b"b\n", 0, 10));
    assert_eq!(h.runs(), before + 1);
    let last = 10 + (CACHE_ENTRIES + 9) as u64;
    h.diff(&file, &identified(b"a\n", b"b\n", 0, last));
    assert_eq!(h.runs(), before + 1);
  }

  #[test]
  fn a_cache_hit_keeps_the_version_from_being_evicted() {
    let h = highlighter();
    let file = one_line_file();
    // The shared 0 version takes one slot, so this fills the cache exactly.
    for i in 0..CACHE_ENTRIES - 1 {
      h.diff(&file, &identified(b"a\n", b"b\n", 0, 10 + i as u64));
    }
    // Use the oldest version again, then overflow the cache.
    h.diff(&file, &identified(b"a\n", b"b\n", 0, 10));
    for i in CACHE_ENTRIES..CACHE_ENTRIES + 5 {
      h.diff(&file, &identified(b"a\n", b"b\n", 0, 10 + i as u64));
    }
    let before = h.runs();
    h.diff(&file, &identified(b"a\n", b"b\n", 0, 10));
    assert_eq!(h.runs(), before);
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
