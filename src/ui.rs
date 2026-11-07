use crate::app::{App, ListRow, Load};
use crate::args::Layout as DiffLayout;
use crate::diff::{FileDiff, FileStatus, Hunk, Line as DiffLine, LineKind, Section};
use crate::highlight::Styled;
use crate::split::pairs;
use crate::words::changed_words;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, Paragraph};
use std::ops::Range;

const ADDED_BG: Color = Color::Rgb(18, 52, 28);
const REMOVED_BG: Color = Color::Rgb(66, 24, 28);
const ADDED_WORD_BG: Color = Color::Rgb(30, 100, 52);
const REMOVED_WORD_BG: Color = Color::Rgb(128, 40, 46);

const HELP: &str = " j/k move · space/PgDn page · ctrl-d/u half page · g/G top/bottom · n/p file · s split/unified · e expand · v select lines · a stage hunk or selection · - stage file · q quit";

const CURRENT_HUNK_BG: Color = Color::Rgb(48, 54, 80);
const SELECTED_BG: Color = Color::Rgb(70, 70, 110);

enum Row<'a> {
  /// A hunk header, and whether the cursor is in that hunk.
  Header(&'a Hunk, bool),
  Line(&'a DiffLine, Option<&'a Styled>, &'a [Range<usize>], bool),
}

pub fn render(frame: &mut Frame, app: &mut App) {
  let [main, footer] =
    Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
  frame.render_widget(Paragraph::new(footer_line(app)), footer);

  if let Load::Failed(message) = &app.load {
    let text = Paragraph::new(format!("Error: {message}")).style(Style::new().fg(Color::Red));
    frame.render_widget(text, main);
    return;
  }
  if matches!(app.load, Load::Loading) {
    return message(frame, main, "Loading...");
  }
  if app.files.is_empty() {
    return message(frame, main, "No changes");
  }

  let list_width = (main.width / 3).clamp(20, 40).min(main.width);
  let [list, diff] =
    Layout::horizontal([Constraint::Length(list_width), Constraint::Min(1)]).areas(main);
  render_files(frame, list, app);
  render_diff(frame, diff, app);
}

/// The mode at the left, then the notice or the key help.
fn footer_line(app: &App) -> Line<'static> {
  let mode = Span::styled(
    format!(" {} ", app.mode_label()),
    Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED),
  );
  let rest = match &app.notice {
    Some(notice) => Span::styled(format!(" {notice}"), Style::new().fg(Color::Yellow)),
    None => Span::styled(HELP, Style::new().fg(Color::DarkGray)),
  };
  Line::from(vec![mode, rest])
}

fn message(frame: &mut Frame, area: Rect, text: &str) {
  let [_, middle, _] = Layout::vertical([
    Constraint::Fill(1),
    Constraint::Length(1),
    Constraint::Fill(1),
  ])
  .areas(area);
  frame.render_widget(Paragraph::new(text).alignment(Alignment::Center), middle);
}

fn status_color(status: FileStatus) -> Color {
  match status {
    FileStatus::Added => Color::Green,
    FileStatus::Deleted => Color::Red,
    FileStatus::Modified => Color::Yellow,
    FileStatus::Renamed | FileStatus::Copied => Color::Cyan,
    FileStatus::Conflicted => Color::Red,
  }
}

fn render_files(frame: &mut Frame, area: Rect, app: &mut App) {
  let items: Vec<ListItem> = app
    .list_rows()
    .into_iter()
    .map(|row| match row {
      ListRow::Header(section, count) => {
        let name = match section {
          Section::Unstaged => "Unstaged",
          Section::Staged => "Staged",
        };
        ListItem::new(Line::styled(
          format!("{name} ({count})"),
          Style::new().add_modifier(Modifier::BOLD),
        ))
      }
      ListRow::Entry(index) => {
        let f = &app.files[index];
        ListItem::new(Line::from(vec![
          Span::styled(
            format!("{} ", f.status.letter()),
            Style::new().fg(status_color(f.status)),
          ),
          Span::raw(f.path().display().to_string()),
        ]))
      }
    })
    .collect();
  let list = List::new(items)
    .block(Block::bordered().title(format!(" Files ({}) ", app.files.len())))
    .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
  frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_diff(frame: &mut Frame, area: Rect, app: &mut App) {
  let file = &app.files[app.selected];
  let block = Block::bordered().title(title(file));
  let inner = block.inner(area);
  frame.render_widget(block, area);

  app.viewport = usize::from(inner.height);
  app.clamp_scroll();
  if app.words[app.selected].is_none() {
    let words = app.files[app.selected]
      .hunks
      .iter()
      .map(changed_words)
      .collect();
    app.words[app.selected] = Some(words);
  }
  let file = &app.files[app.selected];
  let words = app.words[app.selected].as_deref().unwrap_or_default();
  let colors = app.highlights.get(app.selected).and_then(Option::as_ref);

  if file.hunks.is_empty() {
    let text = if file.binary {
      "Binary file not shown"
    } else {
      "No textual changes (empty file or mode change)"
    };
    frame.render_widget(
      Paragraph::new(text).style(Style::new().fg(Color::DarkGray)),
      inner,
    );
    return;
  }

  let width = gutter_width(file);
  let current = app.cursor_hunk();
  let cursor = app.cursor.saturating_sub(app.scroll);
  let selection = app.selection();
  let mark = |i: usize, line: Line<'static>| {
    let selected = selection
      .as_ref()
      .is_some_and(|rows| rows.contains(&(app.scroll + i)));
    let line = if selected { select(line) } else { line };
    if i == cursor {
      line.patch_style(Modifier::REVERSED)
    } else {
      line
    }
  };
  let hunks = file.hunks.iter().enumerate();
  let line_of = |hi: usize, li: usize| {
    let h = &file.hunks[hi];
    let styled = colors.and_then(|c| c.hunks.get(hi)?.get(li)?.as_ref());
    let ranges = words
      .get(hi)
      .and_then(|h| h.get(li))
      .map_or(&[][..], Vec::as_slice);
    (&h.lines[li], styled, ranges)
  };

  match app.layout {
    DiffLayout::Unified => {
      let rows = hunks
        .flat_map(|(hi, h)| {
          let lines = (0..h.lines.len()).map(move |li| {
            let (l, styled, ranges) = line_of(hi, li);
            Row::Line(l, styled, ranges, current == Some(hi))
          });
          std::iter::once(Row::Header(h, current == Some(hi))).chain(lines)
        })
        .skip(app.scroll)
        .take(app.viewport)
        .map(|row| row_line(row, width, usize::from(inner.width)))
        .enumerate()
        .map(|(i, line)| mark(i, line))
        .collect::<Vec<_>>();
      frame.render_widget(Paragraph::new(rows), inner);
    }
    DiffLayout::Split => {
      let left_width = inner.width.saturating_sub(1) / 2;
      let right_width = inner.width.saturating_sub(1 + left_width);
      let [left_area, _, right_area] = Layout::horizontal([
        Constraint::Length(left_width),
        Constraint::Length(1),
        Constraint::Length(right_width),
      ])
      .areas(inner);
      let side = |index: Option<usize>, hi: usize, new: bool, fill: u16| match index {
        Some(li) => {
          let (l, styled, ranges) = line_of(hi, li);
          let number = if new { l.new_no } else { l.old_no };
          let gutter = format!(
            "{:>width$} ",
            number.map_or(String::new(), |n| n.to_string())
          );
          line_spans(
            l,
            styled,
            ranges,
            gutter,
            usize::from(fill),
            current == Some(hi),
          )
        }
        None => blank(fill),
      };
      let rows: Vec<(Line, Line)> = hunks
        .flat_map(|(hi, h)| {
          let header = Row::Header(h, current == Some(hi));
          let header = (
            row_line(header, width, left_width.into()),
            blank(right_width),
          );
          let pairs = pairs(h).into_iter().map(move |(l, r)| {
            (
              side(l, hi, false, left_width),
              side(r, hi, true, right_width),
            )
          });
          std::iter::once(header).chain(pairs)
        })
        .skip(app.scroll)
        .take(app.viewport)
        .enumerate()
        .map(|(i, (l, r))| (mark(i, l), mark(i, r)))
        .collect();
      let (left, right): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
      frame.render_widget(Paragraph::new(left), left_area);
      frame.render_widget(Paragraph::new(right), right_area);
    }
  }
}

/// An empty column of a split row. It holds spaces so that a selection
/// highlight spans it.
fn blank(width: u16) -> Line<'static> {
  Line::raw(" ".repeat(width.into()))
}

/// Paints the selection background over a line, replacing the colors of its
/// added or removed background.
fn select(mut line: Line<'static>) -> Line<'static> {
  line.style = line.style.bg(SELECTED_BG);
  for span in &mut line.spans {
    span.style = span.style.bg(SELECTED_BG);
  }
  line
}

fn title(file: &FileDiff) -> String {
  let counts = format!(
    "+{} -{}",
    file.count(LineKind::Added),
    file.count(LineKind::Removed)
  );
  match (&file.old_path, file.status) {
    (Some(from), FileStatus::Renamed | FileStatus::Copied) => {
      format!(
        " {} -> {} ({counts}) ",
        from.display(),
        file.path().display()
      )
    }
    _ => format!(" {} ({counts}) ", file.path().display()),
  }
}

/// Digits needed for the largest line number in either version of the file.
fn gutter_width(file: &FileDiff) -> usize {
  let last = file
    .hunks
    .iter()
    .map(|h| (h.old_start + h.old_len).max(h.new_start + h.new_len))
    .max()
    .unwrap_or(0);
  last.to_string().len().max(3)
}

/// `fill` pads changed lines to the pane width so their background spans it.
fn row_line(row: Row, width: usize, fill: usize) -> Line<'static> {
  match row {
    Row::Header(h, current) => Line::styled(
      format!(
        "{:<fill$}",
        format!(
          "@@ -{},{} +{},{} @@ {}",
          h.old_start,
          h.old_len,
          h.new_start,
          h.new_len,
          sanitize(&h.heading)
        )
      ),
      if current {
        Style::new()
          .fg(Color::Cyan)
          .bg(CURRENT_HUNK_BG)
          .add_modifier(Modifier::BOLD)
      } else {
        Style::new().fg(Color::Cyan)
      },
    ),
    Row::Line(l, styled, changed, current) => {
      let no = |n: Option<u32>| n.map_or(String::new(), |n| n.to_string());
      let gutter = format!("{:>width$} {:>width$} ", no(l.old_no), no(l.new_no));
      line_spans(l, styled, changed, gutter, fill, current)
    }
  }
}

/// One diff line: gutter, marker, then the code with changed words
/// emphasized.
fn line_spans(
  l: &DiffLine,
  styled: Option<&Styled>,
  changed: &[Range<usize>],
  gutter: String,
  fill: usize,
  current: bool,
) -> Line<'static> {
  let (marker, style, word_bg) = match l.kind {
    LineKind::Context => (' ', Style::new(), None),
    LineKind::Added => ('+', Style::new().bg(ADDED_BG), Some(ADDED_WORD_BG)),
    LineKind::Removed => ('-', Style::new().bg(REMOVED_BG), Some(REMOVED_WORD_BG)),
  };
  let gutter_color = if current {
    Color::Gray
  } else {
    Color::DarkGray
  };
  let mut spans = vec![
    Span::styled(gutter, Style::new().fg(gutter_color)),
    Span::raw(marker.to_string()),
  ];
  let plain;
  let pieces = match styled {
    Some(pieces) => pieces,
    None => {
      plain = vec![(
        Style::new(),
        String::from_utf8_lossy(&l.content).into_owned(),
      )];
      &plain
    }
  };
  spans.extend(
    emphasize(pieces, changed, word_bg)
      .into_iter()
      .map(|(style, text)| Span::styled(sanitize(text.as_bytes()), style)),
  );
  if l.no_newline {
    spans.push(Span::styled(
      " [no newline at end of file]",
      Style::new().fg(Color::DarkGray),
    ));
  }
  let used: usize = spans.iter().map(Span::width).sum();
  spans.push(Span::raw(" ".repeat(fill.saturating_sub(used))));
  Line::from(spans).style(style)
}

/// Splits `pieces` at the edges of `changed` (byte offsets into the joined
/// text) and gives the changed parts the `bg` background.
fn emphasize(
  pieces: &[(Style, String)],
  changed: &[Range<usize>],
  bg: Option<Color>,
) -> Vec<(Style, String)> {
  let Some(bg) = bg.filter(|_| !changed.is_empty()) else {
    return pieces.to_vec();
  };
  let mut out = Vec::new();
  let mut at = 0;
  for (style, text) in pieces {
    let end = at + text.len();
    let mut cut = vec![0, text.len()];
    for r in changed {
      for edge in [r.start, r.end] {
        if edge > at && edge < end {
          cut.push(edge - at);
        }
      }
    }
    cut.sort_unstable();
    cut.dedup();
    for w in cut.windows(2) {
      let (from, to) = (at + w[0], at + w[1]);
      let hit = changed.iter().any(|r| r.start <= from && to <= r.end);
      let style = if hit { style.bg(bg) } else { *style };
      out.push((style, text[w[0]..w[1]].to_string()));
    }
    at = end;
  }
  out
}

/// Makes raw file bytes safe to draw: invalid UTF-8 and control characters
/// would otherwise corrupt the screen.
fn sanitize(bytes: &[u8]) -> String {
  let mut out = String::with_capacity(bytes.len());
  for c in String::from_utf8_lossy(bytes).chars() {
    match c {
      '\t' => out.push_str("    "),
      '\r' => {}
      c if c.is_control() => out.push('\u{FFFD}'),
      c => out.push(c),
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::diff::{FileStatus, Section, parse};
  use ratatui::Terminal;
  use ratatui::backend::TestBackend;
  use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

  const DIFF: &str = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,3 @@ fn main() {
 keep
-old
+new
 tail
diff --git a/b.txt b/b.txt
new file mode 100644
--- /dev/null
+++ b/b.txt
@@ -0,0 +1 @@
+hello\tworld
";

  fn app_with(diff: &str) -> App {
    let mut app = App::default();
    app.on_loaded(Ok(parse(diff.as_bytes()).unwrap()));
    app
  }

  fn draw(app: &mut App, width: u16, height: u16) -> (Terminal<TestBackend>, Vec<String>) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| render(f, app)).unwrap();
    let rows = terminal
      .backend()
      .to_string()
      .lines()
      .map(str::to_string)
      .collect();
    (terminal, rows)
  }

  fn row_with<'a>(rows: &'a [String], needle: &str) -> (usize, &'a String) {
    rows
      .iter()
      .enumerate()
      .find(|(_, r)| r.contains(needle))
      .unwrap_or_else(|| panic!("no row contains {needle:?} in {rows:#?}"))
  }

  #[test]
  fn the_footer_shows_the_mode_at_the_left_and_notices_beside_it() {
    let mut app = app_with(DIFF);
    let (_, rows) = draw(&mut app, 100, 8);
    assert!(rows[7].starts_with("\" NORMAL  j/k move"), "{}", rows[7]);
    app.notice = Some("No unchanged lines to show here".into());
    let (_, rows) = draw(&mut app, 100, 8);
    assert!(
      rows[7].starts_with("\" NORMAL  No unchanged lines to show here"),
      "{}",
      rows[7]
    );
  }

  #[test]
  fn the_hunk_under_the_cursor_is_highlighted_and_the_cursor_line_reversed() {
    let mut app =
      app_with("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-a\n+b\n@@ -9 +9 @@\n-c\n+d\n");
    let (terminal, rows) = draw(&mut app, 60, 12);
    let buffer = terminal.backend().buffer();
    let bg = |rows: &[String], needle: &str| {
      let (y, row) = row_with(rows, needle);
      let x = row.find(needle).unwrap() as u16;
      buffer[(x, y as u16)].bg
    };
    assert_eq!(bg(&rows, "@@ -1,1"), CURRENT_HUNK_BG);
    assert_ne!(bg(&rows, "@@ -9,1"), CURRENT_HUNK_BG);
    let (y, _) = row_with(&rows, "@@ -1,1");
    assert!(buffer[(5, y as u16)].modifier.contains(Modifier::REVERSED));

    for _ in 0..3 {
      app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    }
    let (terminal, rows) = draw(&mut app, 60, 12);
    let buffer = terminal.backend().buffer();
    let (y, row) = row_with(&rows, "@@ -9,1");
    assert_eq!(
      buffer[(row.find("@@").unwrap() as u16, y as u16)].bg,
      CURRENT_HUNK_BG
    );
    let (y, _) = row_with(&rows, "@@ -1,1");
    assert_ne!(buffer[(5, y as u16)].bg, CURRENT_HUNK_BG);
  }

  #[test]
  fn visual_mode_shows_the_line_count_and_highlights_the_selection() {
    let mut app = app_with(DIFF);
    let press = |app: &mut App, keys: &str| {
      for c in keys.chars() {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
      }
    };
    // The cursor is on `-old`; the selection takes in `+new` too.
    press(&mut app, "jjvj");
    let (terminal, rows) = draw(&mut app, 100, 8);
    assert!(rows[7].contains(" VISUAL 2 lines "), "{}", rows[7]);
    let buffer = terminal.backend().buffer();
    let bg = |needle: &str| {
      let (y, row) = row_with(&rows, needle);
      let x = row[..row.find(needle).unwrap()].chars().count() as u16;
      buffer[(x, y as u16)].bg
    };
    assert_eq!(bg("-old"), SELECTED_BG);
    assert_eq!(bg("+new"), SELECTED_BG);
    assert_ne!(bg("keep"), SELECTED_BG);
    assert_ne!(bg("tail"), SELECTED_BG);

    press(&mut app, "k");
    let (_, rows) = draw(&mut app, 100, 8);
    assert!(rows[7].contains(" VISUAL 1 line "), "{}", rows[7]);
  }

  #[test]
  fn shows_a_loading_state_until_a_result_arrives() {
    let (_, rows) = draw(&mut App::default(), 40, 5);
    assert!(rows.iter().any(|r| r.contains("Loading...")));
  }

  #[test]
  fn shows_the_error_when_loading_fails() {
    let mut app = App::default();
    app.on_loaded(Err(crate::vcs::Error::Git("not a git repository".into())));
    let (_, rows) = draw(&mut app, 60, 5);
    assert!(
      rows
        .iter()
        .any(|r| r.contains("Error: not a git repository"))
    );
  }

  #[test]
  fn shows_a_message_when_there_are_no_changes() {
    let mut app = App::default();
    app.on_loaded(Ok(Vec::new()));
    let (_, rows) = draw(&mut app, 40, 5);
    assert!(rows.iter().any(|r| r.contains("No changes")));
  }

  #[test]
  fn lists_files_and_draws_the_selected_diff() {
    let mut app = app_with(DIFF);
    let (_, rows) = draw(&mut app, 80, 14);
    assert!(rows.iter().any(|r| r.contains("Files (2)")));
    assert!(rows.iter().any(|r| r.contains("M src/a.rs")));
    assert!(rows.iter().any(|r| r.contains("A b.txt")));
    assert!(rows.iter().any(|r| r.contains("src/a.rs (+1 -1)")));
    assert!(
      rows
        .iter()
        .any(|r| r.contains("@@ -1,3 +1,3 @@ fn main() {"))
    );
    assert!(rows.iter().any(|r| r.contains("-old")));
    assert!(rows.iter().any(|r| r.contains("+new")));
  }

  fn sectioned_app(sections: &[Section]) -> App {
    let mut files = parse(DIFF.as_bytes()).unwrap();
    files.truncate(1);
    let entries = sections
      .iter()
      .map(|&s| FileDiff {
        section: Some(s),
        ..files[0].clone()
      })
      .collect();
    let mut app = App::default();
    app.on_loaded(Ok(entries));
    app
  }

  #[test]
  fn sections_get_headers_with_counts_and_each_entry_its_own_row() {
    let mut app = sectioned_app(&[Section::Unstaged, Section::Unstaged, Section::Staged]);
    let (_, rows) = draw(&mut app, 80, 14);
    let at = |needle: &str| row_with(&rows, needle).0;
    assert!(at("Unstaged (2)") < at("Staged (1)"));
    let entries: Vec<_> = rows
      .iter()
      .enumerate()
      .filter(|(_, r)| r.contains("M src/a.rs"))
      .map(|(y, _)| y)
      .collect();
    assert_eq!(entries.len(), 3);
    assert!(at("Unstaged (2)") < entries[0] && entries[1] < at("Staged (1)"));
    assert!(at("Staged (1)") < entries[2]);
  }

  #[test]
  fn an_empty_section_has_no_header() {
    let (_, rows) = draw(&mut sectioned_app(&[Section::Staged]), 80, 14);
    assert!(rows.iter().any(|r| r.contains("Staged (1)")));
    assert!(!rows.iter().any(|r| r.contains("Unstaged")));
    let (_, rows) = draw(&mut sectioned_app(&[Section::Unstaged]), 80, 14);
    assert!(rows.iter().any(|r| r.contains("Unstaged (1)")));
    assert!(!rows.iter().any(|r| r.contains(" Staged")));
  }

  #[test]
  fn a_revision_lists_files_flat_without_headers() {
    let (_, rows) = draw(&mut app_with(DIFF), 80, 14);
    assert!(!rows.iter().any(|r| r.contains("Staged (")));
  }

  #[test]
  fn the_highlighted_row_is_the_entry_not_the_header() {
    let mut app = sectioned_app(&[Section::Unstaged, Section::Staged]);
    app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    let (terminal, rows) = draw(&mut app, 80, 14);
    let buffer = terminal.backend().buffer();
    let (y, row) = row_with(&rows, "Staged (1)");
    let x = row.find("Staged").unwrap() as u16;
    assert!(!buffer[(x, y as u16)].modifier.contains(Modifier::REVERSED));
    let x = rows[y + 1].find("src/a.rs").unwrap() as u16;
    assert!(
      buffer[(x, y as u16 + 1)]
        .modifier
        .contains(Modifier::REVERSED)
    );
  }

  #[test]
  fn added_and_removed_lines_get_background_colors() {
    let mut app = app_with(DIFF);
    let (terminal, rows) = draw(&mut app, 80, 14);
    let buffer = terminal.backend().buffer();
    let last_col = 80 - 2;
    let (y, _) = row_with(&rows, "+new");
    assert_eq!(buffer[(last_col, y as u16)].bg, ADDED_BG);
    let (y, _) = row_with(&rows, "-old");
    assert_eq!(buffer[(last_col, y as u16)].bg, REMOVED_BG);
    let (y, _) = row_with(&rows, " keep");
    assert_ne!(buffer[(last_col, y as u16)].bg, ADDED_BG);
  }

  #[test]
  fn draws_uncolored_until_highlights_arrive_then_colors_the_code() {
    use crate::highlight::FileHighlight;
    let mut app = app_with(DIFF);
    let (terminal, rows) = draw(&mut app, 80, 14);
    let (y, row) = row_with(&rows, "+new");
    let x = row[..row.find("+new").unwrap()].chars().count() as u16 + 1;
    assert_eq!(terminal.backend().buffer()[(x, y as u16)].fg, Color::Reset);

    let red = Style::new().fg(Color::Rgb(200, 1, 2));
    let h = FileHighlight {
      hunks: vec![vec![None, None, Some(vec![(red, "new".to_string())]), None]],
    };
    app.on_highlighted(0, h);
    let (terminal, rows) = draw(&mut app, 80, 14);
    let (y, row) = row_with(&rows, "+new");
    let x = row[..row.find("+new").unwrap()].chars().count() as u16 + 1;
    let cell = &terminal.backend().buffer()[(x, y as u16)];
    assert_eq!(cell.fg, Color::Rgb(200, 1, 2));
    assert_eq!(cell.bg, ADDED_WORD_BG);
    assert!(rows.iter().any(|r| r.contains("-old")));
  }

  #[test]
  fn changed_words_get_a_stronger_background_than_the_line() {
    let mut app = app_with(DIFF);
    let (terminal, rows) = draw(&mut app, 80, 14);
    let buffer = terminal.backend().buffer();
    for (needle, line_bg, word_bg) in [
      ("+new", ADDED_BG, ADDED_WORD_BG),
      ("-old", REMOVED_BG, REMOVED_WORD_BG),
    ] {
      let (y, row) = row_with(&rows, needle);
      let x = row[..row.find(needle).unwrap()].chars().count() as u16 + 1;
      assert_eq!(buffer[(x, y as u16)].bg, word_bg);
      assert_eq!(buffer[(79 - 1, y as u16)].bg, line_bg);
    }
  }

  #[test]
  #[allow(clippy::single_range_in_vec_init)]
  fn emphasis_keeps_syntax_colors_and_splits_pieces() {
    let red = Style::new().fg(Color::Red);
    let pieces = vec![(red, "ab".to_string()), (Style::new(), "cd".to_string())];
    let out = emphasize(&pieces, &[1..3], Some(Color::Blue));
    let texts: Vec<_> = out.iter().map(|(_, t)| t.as_str()).collect();
    assert_eq!(texts, ["a", "b", "c", "d"]);
    assert_eq!(out[0].0, red);
    assert_eq!(out[1].0, red.bg(Color::Blue));
    assert_eq!(out[2].0, Style::new().bg(Color::Blue));
    assert_eq!(out[3].0, Style::new());
  }

  #[test]
  fn unpaired_lines_get_no_word_emphasis() {
    let mut app = app_with(DIFF);
    app.on_key(ratatui::crossterm::event::KeyEvent::from(
      ratatui::crossterm::event::KeyCode::Tab,
    ));
    let (terminal, rows) = draw(&mut app, 80, 14);
    let (y, row) = row_with(&rows, "+hello");
    let x = row[..row.find("+hello").unwrap()].chars().count() as u16 + 1;
    assert_eq!(terminal.backend().buffer()[(x, y as u16)].bg, ADDED_BG);
  }

  const SPLIT_DIFF: &str = "\
diff --git a/a.rs b/a.rs
--- a/a.rs
+++ b/a.rs
@@ -1,4 +1,3 @@
 keep
-old1
-old2
+new1
 tail
";

  fn split_app() -> App {
    let mut app = app_with(SPLIT_DIFF);
    app.layout = DiffLayout::Split;
    app
  }

  #[test]
  fn split_layout_aligns_old_and_new_columns() {
    let mut app = split_app();
    let (_, rows) = draw(&mut app, 81, 12);
    let (_, keep) = row_with(&rows, "keep");
    assert!(keep.matches("keep").count() == 2);
    let (_, pair) = row_with(&rows, "-old1");
    assert!(pair.contains("+new1"));
    assert!(pair.find("-old1") < pair.find("+new1"));
    let (_, surplus) = row_with(&rows, "-old2");
    assert!(!surplus.contains('+'));
    let (_, tail) = row_with(&rows, "tail");
    assert_eq!(tail.matches("tail").count(), 2);
  }

  #[test]
  fn split_layout_highlights_whole_rows_of_the_selection() {
    let mut app = split_app();
    // The cursor goes to the row of `-old2`, which has no partner on the right.
    for c in "jjjv".chars() {
      app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    let (terminal, rows) = draw(&mut app, 81, 12);
    assert!(rows[11].contains(" VISUAL 1 line "), "{}", rows[11]);
    let buffer = terminal.backend().buffer();
    let (y, row) = row_with(&rows, "-old2");
    let left = row[..row.find("-old2").unwrap()].chars().count() as u16;
    // The left line and the blank right column are both highlighted.
    for x in [left, 79] {
      assert_eq!(buffer[(x, y as u16)].bg, SELECTED_BG, "column {x}");
    }
    let (y, _) = row_with(&rows, "-old1");
    for x in [left, 79] {
      assert_ne!(buffer[(x, y as u16)].bg, SELECTED_BG, "column {x}");
    }
  }

  #[test]
  fn split_layout_shows_one_line_number_per_column() {
    let mut app = split_app();
    let (_, rows) = draw(&mut app, 81, 12);
    let (_, pair) = row_with(&rows, "-old1");
    // old line 2 on the left, new line 2 on the right
    assert_eq!(pair.matches('2').count(), 2, "{pair}");
    let (_, surplus) = row_with(&rows, "-old2");
    assert_eq!(surplus.matches('3').count(), 1, "{surplus}");
  }

  #[test]
  fn split_layout_keeps_each_column_inside_its_half() {
    let mut app = app_with(
      "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n+b\n",
    );
    app.layout = DiffLayout::Split;
    let (_, rows) = draw(&mut app, 81, 8);
    let (_, row) = row_with(&rows, "+b");
    assert!(row.contains("+b"));
  }

  #[test]
  fn split_layout_colors_each_column_by_kind() {
    let mut app = split_app();
    let (terminal, rows) = draw(&mut app, 81, 12);
    let buffer = terminal.backend().buffer();
    let (y, row) = row_with(&rows, "-old1");
    let new_x = row[..row.find("+new1").unwrap()].chars().count() as u16 - 1;
    let old_x = row[..row.find("-old1").unwrap()].chars().count() as u16 - 1;
    assert_eq!(buffer[(old_x, y as u16)].bg, REMOVED_BG);
    assert_eq!(buffer[(new_x, y as u16)].bg, ADDED_BG);
    let (y, _) = row_with(&rows, "-old2");
    // The blank right column carries no background.
    assert_ne!(buffer[(new_x, y as u16)].bg, ADDED_BG);
  }

  #[test]
  fn split_layout_survives_tiny_terminals() {
    let mut app = split_app();
    for (w, h) in [(1, 1), (10, 2), (30, 3), (5, 40)] {
      draw(&mut app, w, h);
    }
  }

  #[test]
  fn switching_files_changes_the_diff_pane() {
    let mut app = app_with(DIFF);
    app.on_key(ratatui::crossterm::event::KeyEvent::from(
      ratatui::crossterm::event::KeyCode::Tab,
    ));
    let (_, rows) = draw(&mut app, 80, 14);
    assert!(rows.iter().any(|r| r.contains("b.txt (+1 -0)")));
    assert!(rows.iter().any(|r| r.contains("+hello    world")));
    assert!(!rows.iter().any(|r| r.contains("-old")));
  }

  #[test]
  fn scrolling_hides_earlier_rows() {
    let mut app = app_with(DIFF);
    // 6 rows in the first file; a 4 row pane (height 6 minus borders and footer).
    let (_, rows) = draw(&mut app, 80, 7);
    assert_eq!(app.viewport, 4);
    assert!(rows.iter().any(|r| r.contains("@@ -1,3")));
    app.on_key(ratatui::crossterm::event::KeyEvent::from(
      ratatui::crossterm::event::KeyCode::Char('G'),
    ));
    let (_, rows) = draw(&mut app, 80, 7);
    assert!(!rows.iter().any(|r| r.contains("@@ -1,3")));
    assert!(rows.iter().any(|r| r.contains(" tail")));
  }

  #[test]
  fn explains_files_without_hunks() {
    let mut app = app_with(
      "diff --git a/i.png b/i.png\nindex 1..2 100644\nBinary files a/i.png and b/i.png differ\n",
    );
    let (_, rows) = draw(&mut app, 60, 8);
    assert!(rows.iter().any(|r| r.contains("Binary file not shown")));
    let mut app = app_with("diff --git a/e b/e\nnew file mode 100644\nindex 0000000..e69de29\n");
    assert_eq!(app.files[0].status, FileStatus::Added);
    let (_, rows) = draw(&mut app, 60, 8);
    assert!(rows.iter().any(|r| r.contains("No textual changes")));
  }

  #[test]
  fn survives_tiny_terminals() {
    let mut app = app_with(DIFF);
    for (w, h) in [(1, 1), (10, 2), (30, 3), (5, 40)] {
      draw(&mut app, w, h);
    }
  }

  #[test]
  fn sanitize_handles_tabs_controls_and_invalid_utf8() {
    assert_eq!(sanitize(b"a\tb\r"), "a    b");
    assert_eq!(sanitize(b"x\x1b[31m"), "x\u{FFFD}[31m");
    assert_eq!(sanitize(b"\xff"), "\u{FFFD}");
  }
}
