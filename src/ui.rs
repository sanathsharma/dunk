use crate::app::{App, ListRow, Load, Mode};
use crate::args::{Border, Layout as DiffLayout};
use crate::diff::{FileDiff, FileStatus, Hunk, Line as DiffLine, LineKind, Section};
use crate::highlight::Styled;
use crate::palette::Palette;
use crate::split::pairs;
use crate::words::changed_words;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, Paragraph};
use std::ops::Range;

const HELP: &str = " j/k move · n/p file · [/] hunk · a stage · ? help · q quit";

/// Every key, grouped, for the help menu.
const HELP_MENU: &[(&str, &str)] = &[
  ("j/k, up/down", "move the cursor"),
  ("space/PgDn, PgUp", "page down, up"),
  ("ctrl-d/u", "half page down, up"),
  ("g/G, Home/End", "top, bottom"),
  ("]/[", "next, previous hunk"),
  ("n/p, }/{, Tab/Shift-Tab", "next, previous file"),
  ("s", "split or unified layout"),
  ("+/-", "show 3 more, fewer unchanged lines around the hunk"),
  ("=", "back to the default context"),
  ("v", "select lines (esc or v leaves)"),
  ("a", "stage or unstage hunk or selection"),
  ("f", "stage or unstage file"),
  ("?", "open this help"),
  ("q, esc", "quit"),
];

impl Row<'_> {
  /// Whether the row is in the hunk under the cursor.
  fn active(&self) -> bool {
    match self {
      Row::Header(_, active) | Row::Line(_, _, _, active) => *active,
    }
  }
}

enum Row<'a> {
  /// A hunk header, and whether the cursor is in that hunk.
  Header(&'a Hunk, bool),
  Line(&'a DiffLine, Option<&'a Styled>, &'a [Range<usize>], bool),
}

pub fn render(frame: &mut Frame, app: &mut App) {
  let area = frame.area();
  frame.buffer_mut().set_style(area, app.palette.normal);
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
  if app.help {
    render_help(frame, main, app);
  }
}

fn render_help(frame: &mut Frame, area: Rect, app: &App) {
  let key_width = HELP_MENU.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
  let lines: Vec<Line> = HELP_MENU
    .iter()
    .map(|(key, what)| Line::from(format!(" {key:<key_width$}  {what}")))
    .collect();
  let width = (key_width + 40).min(area.width as usize) as u16;
  let height = (lines.len() as u16 + 2).min(area.height);
  let [_, row, _] = Layout::vertical([
    Constraint::Fill(1),
    Constraint::Length(height),
    Constraint::Fill(1),
  ])
  .areas(area);
  let [_, popup, _] = Layout::horizontal([
    Constraint::Fill(1),
    Constraint::Length(width),
    Constraint::Fill(1),
  ])
  .areas(row);
  frame.render_widget(Clear, popup);
  let block = Block::bordered()
    .border_type(border_type(app.border))
    .border_style(app.palette.border)
    .title_style(app.palette.border)
    .title(" Help (esc or q closes) ");
  frame.render_widget(
    Paragraph::new(lines).block(block).style(app.palette.normal),
    popup,
  );
}

/// The mode at the left, then the notice or the key help.
fn footer_line(app: &App) -> Line<'static> {
  let style = match app.mode {
    Mode::Normal => app.palette.mode_normal,
    Mode::Visual => app.palette.mode_visual,
  };
  let mode = Span::styled(format!(" {} ", app.mode_label()), style);
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

fn border_type(border: Border) -> BorderType {
  match border {
    Border::Plain => BorderType::Plain,
    Border::Rounded => BorderType::Rounded,
  }
}

fn render_files(frame: &mut Frame, area: Rect, app: &mut App) {
  let palette = app.palette;
  let border_type = border_type(app.border);
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
          palette.section_label,
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
    .block(
      Block::bordered()
        .border_type(border_type)
        .border_style(palette.border)
        .title_style(palette.border)
        .title(format!(" Files ({}) ", app.files.len())),
    )
    .highlight_style(palette.cursor_line);
  frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_diff(frame: &mut Frame, area: Rect, app: &mut App) {
  let file = &app.files[app.selected];
  let border = app.palette.border.patch(app.palette.diff_border);
  let block = Block::bordered()
    .border_type(border_type(app.border))
    .border_style(border)
    .title_style(border)
    .title(title(file));
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
  let palette = app.palette;
  let mark = |i: usize, line: Line<'static>| {
    let selected = selection
      .as_ref()
      .is_some_and(|rows| rows.contains(&(app.scroll + i)));
    let line = if selected {
      overlay(line, palette.visual)
    } else {
      line
    };
    if i == cursor {
      overlay(line, palette.cursor_line)
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
      let [bar_area, text] =
        Layout::horizontal([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
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
        .map(|row| {
          let active = row.active();
          (active, row_line(row, &palette, width, text.width.into()))
        })
        .enumerate()
        .map(|(i, (active, line))| (active, mark(i, line)))
        .collect::<Vec<_>>();
      let (active, rows): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
      frame.render_widget(Paragraph::new(bar(&active, &palette)), bar_area);
      frame.render_widget(Paragraph::new(rows), text);
    }
    DiffLayout::Split => {
      let left_width = inner.width.saturating_sub(2) / 2;
      let right_width = inner.width.saturating_sub(2 + left_width);
      let [edge_area, left_area, gap_area, right_area] = Layout::horizontal([
        Constraint::Length(1),
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
            &palette,
            styled,
            ranges,
            gutter,
            usize::from(fill),
            current == Some(hi),
          )
        }
        None => blank(fill),
      };
      let rows: Vec<(bool, Line, Line)> = hunks
        .flat_map(|(hi, h)| {
          let active = current == Some(hi);
          let header = Row::Header(h, active);
          let header = (
            active,
            row_line(header, &palette, width, left_width.into()),
            blank(right_width),
          );
          let pairs = pairs(h).into_iter().map(move |(l, r)| {
            (
              active,
              side(l, hi, false, left_width),
              side(r, hi, true, right_width),
            )
          });
          std::iter::once(header).chain(pairs)
        })
        .skip(app.scroll)
        .take(app.viewport)
        .enumerate()
        .map(|(i, (active, l, r))| (active, mark(i, l), mark(i, r)))
        .collect();
      let active: Vec<bool> = rows.iter().map(|(a, _, _)| *a).collect();
      let left: Vec<Line> = rows.iter().map(|(_, l, _)| l.clone()).collect();
      let right: Vec<Line> = rows.into_iter().map(|(_, _, r)| r).collect();
      frame.render_widget(Paragraph::new(bar(&active, &palette)), edge_area);
      frame.render_widget(Paragraph::new(bar(&active, &palette)), gap_area);
      frame.render_widget(Paragraph::new(left), left_area);
      frame.render_widget(Paragraph::new(right), right_area);
    }
  }
}

/// The left half of a cell, which `DiffHunkBar` colors with its foreground.
const BAR: &str = "\u{258C}";

/// A one cell wide column beside the diff. It holds the `DiffHunkBar` half
/// block on the rows of the hunk under the cursor and is transparent on the
/// others.
fn bar(active: &[bool], palette: &Palette) -> Vec<Line<'static>> {
  active
    .iter()
    .map(|&on| {
      if on {
        Line::styled(BAR, palette.diff_hunk_bar)
      } else {
        Line::raw(" ")
      }
    })
    .collect()
}

/// An empty column of a split row. It holds spaces so that a selection
/// highlight spans it.
fn blank(width: u16) -> Line<'static> {
  Line::raw(" ".repeat(width.into()))
}

/// Lays a group such as `Visual` over a line and its spans. It overrides only
/// what it sets, so the line keeps its other colors.
fn overlay(mut line: Line<'static>, group: Style) -> Line<'static> {
  line.style = line.style.patch(group);
  for span in &mut line.spans {
    span.style = span.style.patch(group);
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
fn row_line(row: Row, palette: &Palette, width: usize, fill: usize) -> Line<'static> {
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
        palette.diff_hunk_header.patch(palette.diff_current_hunk)
      } else {
        palette.diff_hunk_header
      },
    ),
    Row::Line(l, styled, changed, current) => {
      let no = |n: Option<u32>| n.map_or(String::new(), |n| n.to_string());
      let gutter = format!("{:>width$} {:>width$} ", no(l.old_no), no(l.new_no));
      line_spans(l, palette, styled, changed, gutter, fill, current)
    }
  }
}

/// One diff line: gutter, marker, then the code with changed words
/// emphasized.
fn line_spans(
  l: &DiffLine,
  palette: &Palette,
  styled: Option<&Styled>,
  changed: &[Range<usize>],
  gutter: String,
  fill: usize,
  current: bool,
) -> Line<'static> {
  let (marker, style, word_bg) = match l.kind {
    LineKind::Context => (' ', Style::new(), None),
    LineKind::Added => ('+', palette.diff_add, Some(palette.diff_add_text)),
    LineKind::Removed => ('-', palette.diff_delete, Some(palette.diff_delete_text)),
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
/// text) and lays the `emphasis` style over the changed parts.
fn emphasize(
  pieces: &[(Style, String)],
  changed: &[Range<usize>],
  emphasis: Option<Style>,
) -> Vec<(Style, String)> {
  let Some(emphasis) = emphasis.filter(|_| !changed.is_empty()) else {
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
      let style = if hit { style.patch(emphasis) } else { *style };
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
  use ratatui::style::Modifier;
  const ADDED_BG: Color = Color::Green;
  const REMOVED_BG: Color = Color::Red;
  const SELECTED_BG: Color = Color::Blue;
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
  fn the_help_menu_lists_the_keys_over_the_diff() {
    let mut app = app_with(DIFF);
    app.on_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
    let (_, rows) = draw(&mut app, 100, 24);
    row_with(&rows, "Help (esc or q closes)");
    row_with(&rows, "next, previous hunk");
    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let (_, rows) = draw(&mut app, 100, 24);
    assert!(!rows.iter().any(|r| r.contains("Help (esc or q closes)")));
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
    let current_bg = Palette::default().diff_current_hunk.bg.unwrap();
    assert_eq!(bg(&rows, "@@ -1,1"), current_bg);
    assert_ne!(bg(&rows, "@@ -9,1"), current_bg);
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
      current_bg
    );
    let (y, _) = row_with(&rows, "@@ -1,1");
    assert_ne!(buffer[(5, y as u16)].bg, current_bg);
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
    assert_eq!(cell.bg, Palette::default().diff_add_text.bg.unwrap());
    assert!(rows.iter().any(|r| r.contains("-old")));
  }

  #[test]
  fn changed_words_get_a_stronger_background_than_the_line() {
    let mut app = app_with(DIFF);
    let (terminal, rows) = draw(&mut app, 80, 14);
    let buffer = terminal.backend().buffer();
    let p = Palette::default();
    for (needle, line, word) in [
      ("+new", p.diff_add, p.diff_add_text),
      ("-old", p.diff_delete, p.diff_delete_text),
    ] {
      let (line_bg, word_bg) = (line.bg.unwrap(), word.bg.unwrap());
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
    let out = emphasize(&pieces, &[1..3], Some(Style::new().bg(Color::Blue)));
    let texts: Vec<_> = out.iter().map(|(_, t)| t.as_str()).collect();
    assert_eq!(texts, ["a", "b", "c", "d"]);
    assert_eq!(out[0].0, red);
    assert_eq!(out[1].0, red.bg(Color::Blue));
    assert_eq!(out[2].0, Style::new().bg(Color::Blue));
    assert_eq!(out[3].0, Style::new());
  }

  #[test]
  fn a_custom_palette_colors_the_diff_and_visual_keeps_the_line_foreground() {
    let mut app = app_with(DIFF);
    app.palette.diff_add = Style::new().fg(Color::Magenta).bg(Color::Rgb(1, 2, 3));
    app.palette.diff_add_text = Style::new().bg(Color::Rgb(4, 5, 6));
    app.palette.visual = Style::new().bg(Color::Rgb(7, 8, 9));
    let cell = |app: &mut App, needle: &str, dx: isize| {
      let (terminal, rows) = draw(app, 80, 14);
      let (y, row) = row_with(&rows, needle);
      // The first row character is a quote, so the cell is one to the left.
      let x = row[..row.find(needle).unwrap()].chars().count() as isize - 1 + dx;
      terminal.backend().buffer()[(x as u16, y as u16)].clone()
    };
    // The marker takes the line style, the changed word the emphasis on top.
    assert_eq!(cell(&mut app, "+new", 0).bg, Color::Rgb(1, 2, 3));
    assert_eq!(cell(&mut app, "+new", 1).bg, Color::Rgb(4, 5, 6));
    assert_eq!(cell(&mut app, "+new", 1).fg, Color::Magenta);
    for c in "jjvj".chars() {
      app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    let selected = cell(&mut app, "+new", 1);
    assert_eq!(selected.bg, Color::Rgb(7, 8, 9));
    assert_eq!(selected.fg, Color::Magenta);
  }

  #[test]
  fn chrome_groups_color_the_background_borders_labels_cursor_and_mode() {
    let mut app = app_with(DIFF);
    app.palette.normal = Style::new().fg(Color::Rgb(1, 1, 1)).bg(Color::Rgb(2, 2, 2));
    app.palette.border = Style::new().fg(Color::Rgb(3, 3, 3));
    app.palette.diff_border = Style::new().fg(Color::Rgb(4, 4, 4));
    app.palette.section_label = Style::new().fg(Color::Rgb(5, 5, 5));
    app.palette.cursor_line = Style::new().bg(Color::Rgb(6, 6, 6));
    app.palette.mode_normal = Style::new().fg(Color::Rgb(7, 7, 7));
    app.palette.mode_visual = Style::new().fg(Color::Rgb(8, 8, 8));
    let (terminal, rows) = draw(&mut app, 100, 14);
    let at = |x: u16, y: usize| terminal.backend().buffer()[(x, y as u16)].clone();

    // The list border is `Border`, the diff border is `Border` under `DiffBorder`.
    assert_eq!(at(0, 0).fg, Color::Rgb(3, 3, 3));
    let list_width = (100u16 / 3).clamp(20, 40);
    assert_eq!(at(list_width, 0).fg, Color::Rgb(4, 4, 4));
    // Cells nothing else drew take `Normal`.
    assert_eq!(at(99, 12).bg, Color::Rgb(2, 2, 2));
    assert_eq!(at(5, 10).fg, Color::Rgb(1, 1, 1));
    // The footer mode label.
    assert_eq!(at(1, 13).fg, Color::Rgb(7, 7, 7));
    // The diff cursor.
    let (y, _) = row_with(&rows, "@@ -1,3");
    assert_eq!(at(list_width + 2, y).bg, Color::Rgb(6, 6, 6));

    app.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
    let (terminal, _) = draw(&mut app, 100, 14);
    assert_eq!(terminal.backend().buffer()[(1, 13)].fg, Color::Rgb(8, 8, 8));
  }

  #[test]
  fn rounded_borders_use_rounded_corners_and_plain_is_the_default() {
    let mut app = app_with(DIFF);
    let (_, rows) = draw(&mut app, 100, 14);
    assert!(rows[0].starts_with("\"┌"), "{}", rows[0]);
    app.border = Border::Rounded;
    let (_, rows) = draw(&mut app, 100, 14);
    assert!(rows[0].starts_with("\"╭"), "{}", rows[0]);
    assert!(rows[0].contains("╮╭"), "{}", rows[0]);
    assert!(rows[12].contains("╯╰"), "{}", rows[12]);
  }

  #[test]
  fn the_current_hunk_fills_a_bar_beside_the_diff_and_between_split_columns() {
    let mut app =
      app_with("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-a\n+b\n@@ -9 +9 @@\n-c\n+d\n");
    let bar = Palette::default().diff_hunk_bar.fg.unwrap();
    // The list is 20 wide at this size, so the diff text starts at column 21.
    let (edge, gap) = (21, 21 + 1 + (38 - 2) / 2);
    // The bar is a half block, so its color is the foreground of the cell,
    // and a row without the bar is a blank cell.
    let bg_at = |app: &mut App, needle: &str, x: u16| {
      let (terminal, rows) = draw(app, 60, 12);
      let (y, _) = row_with(&rows, needle);
      let cell = &terminal.backend().buffer()[(x, y as u16)];
      if cell.symbol() == BAR {
        cell.fg
      } else {
        assert_eq!(cell.symbol(), " ");
        Color::Reset
      }
    };

    assert_eq!(bg_at(&mut app, "@@ -1,1", edge), bar);
    assert_eq!(bg_at(&mut app, "-a", edge), bar);
    assert_ne!(bg_at(&mut app, "@@ -9,1", edge), bar);
    assert_ne!(bg_at(&mut app, "-c", edge), bar);

    app.on_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
    for x in [edge, gap] {
      assert_eq!(bg_at(&mut app, "@@ -1,1", x), bar, "column {x}");
      assert_eq!(bg_at(&mut app, "-a", x), bar, "column {x}");
      assert_ne!(bg_at(&mut app, "@@ -9,1", x), bar, "column {x}");
    }
    for c in "jjj".chars() {
      app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    assert_eq!(bg_at(&mut app, "@@ -9,1", gap), bar);
    assert_ne!(bg_at(&mut app, "@@ -1,1", gap), bar);
  }

  #[test]
  fn section_labels_take_the_section_label_group() {
    let mut app = sectioned_app(&[Section::Unstaged, Section::Staged]);
    app.palette.section_label = Style::new().fg(Color::Rgb(5, 5, 5));
    let (terminal, rows) = draw(&mut app, 80, 14);
    let (y, row) = row_with(&rows, "Unstaged");
    let x = row[..row.find("Unstaged").unwrap()].chars().count() - 1;
    assert_eq!(
      terminal.backend().buffer()[(x as u16, y as u16)].fg,
      Color::Rgb(5, 5, 5)
    );
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
