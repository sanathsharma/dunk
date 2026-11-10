use crate::app::{App, ListRow, Load, Mode};
use crate::args::{Border, Layout as DiffLayout};
use crate::diff::{FileDiff, FileStatus, Hunk, Line as DiffLine, LineKind, Section};
use crate::highlight::{FileHighlight, Styled};
use crate::palette::Palette;
use crate::restore::Prompt;
use crate::split::pairs;
use crate::vcs::Kind;
use crate::words::changed_words;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, Paragraph};
use std::borrow::Cow;
use std::ops::{Range, RangeInclusive};

const HELP: &str = " j/k move · n/p file · [/] hunk · a stage · ? help · q quit";

/// Every key, grouped, for the help menu.
const HELP_MENU: &[(&str, &str)] = &[
  ("j/k, up/down", "move the cursor"),
  ("space/PgDn, PgUp", "page down, up"),
  ("ctrl-d/u", "half page down, up"),
  ("g/G, Home/End", "top, bottom"),
  ("h/l, left/right", "pan the code left, right"),
  ("]/[", "next, previous hunk"),
  ("n/p, }/{, Tab/Shift-Tab", "next, previous file"),
  ("s", "split or unified layout"),
  ("+/-", "show 3 more, fewer unchanged lines around the hunk"),
  ("=", "back to the default context"),
  ("v", "select lines (esc or v leaves)"),
  ("e", "open the cursor's line in $VISUAL or $EDITOR"),
  ("a", "stage or unstage hunk or selection"),
  ("f", "stage or unstage file"),
  (
    "A",
    "stage all files, or unstage all when none is left to stage",
  ),
  (
    "r",
    "restore the file: discard its unstaged changes, after typing restore",
  ),
  ("x", "nuke: every file back to HEAD, after typing nuke"),
  (",/.", "first, last file"),
  ("?", "open this help"),
  ("q, esc", "quit"),
];

impl Row<'_> {
  /// Whether the row is in the hunk under the cursor.
  const fn active(&self) -> bool {
    match self {
      Row::Header(_, active) | Row::Line(_, _, _, active) => *active,
      Row::Separator => false,
    }
  }
}

/// What a row of the split layout holds.
enum Slot {
  Separator,
  Header,
  Pair((Option<usize>, Option<usize>)),
}

#[derive(Clone, Copy)]
enum Row<'a> {
  /// A hunk header, and whether the cursor is in that hunk.
  Header(&'a Hunk, bool),
  /// The rule between two hunks.
  Separator,
  Line(&'a DiffLine, Option<&'a Styled>, &'a [Range<usize>], bool),
}

pub fn render(frame: &mut Frame, app: &mut App) {
  let area = frame.area();
  frame.buffer_mut().set_style(area, app.palette.normal);
  let [main, footer] =
    Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
  frame.render_widget(Paragraph::new(footer_line(app)), footer);
  render_main(frame, main, app);
  // The prompt takes every key, so it is drawn whatever is behind it.
  if app.prompt.is_some() {
    render_prompt(frame, main, app);
  }
}

fn render_main(frame: &mut Frame, main: Rect, app: &mut App) {
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
  let widest = lines.iter().map(Line::width).max().unwrap_or(0);
  // The widest line, a space after it and the two borders.
  let width = widest
    .saturating_add(3)
    .max(" Help (esc or q closes) ".len().saturating_add(2))
    .min(usize::from(area.width));
  let width = u16::try_from(width).unwrap_or(u16::MAX);
  let height = u16::try_from(lines.len())
    .unwrap_or(u16::MAX)
    .saturating_add(2)
    .min(area.height);
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

/// The lines of the confirmation: what is lost per file, the warning and the
/// input. At most `room` rows are left for files, with the last one saying
/// how many more there are when they do not all fit.
fn prompt_lines(prompt: &Prompt, room: usize) -> Vec<Line<'static>> {
  let entries = &prompt.request().entries;
  let (shown, hidden) = if entries.len() <= room {
    (entries.len(), 0)
  } else {
    let shown = room.saturating_sub(1);
    (shown, entries.len().saturating_sub(shown))
  };
  let mut lines = Vec::new();
  if prompt.narrowed() {
    lines.push(Line::from(
      " The view is narrowed, but a nuke covers every file.",
    ));
    lines.push(Line::from(""));
  }
  for entry in entries.iter().take(shown) {
    lines.push(Line::from(format!(
      " {}  ({})",
      entry.file.path().display(),
      entry.loss.label()
    )));
  }
  if hidden > 0 {
    lines.push(Line::from(format!(" … and {hidden} more")));
  }
  lines.push(Line::from(""));
  lines.push(Line::from(" This cannot be undone."));
  lines.push(Line::from(vec![
    Span::raw(" Type "),
    Span::styled(prompt.word(), Style::new().add_modifier(Modifier::BOLD)),
    Span::raw(" to confirm, esc to cancel."),
  ]));
  lines.push(Line::from(format!(" > {}", prompt.input())));
  lines
}

/// Rows of the confirmation that are not files: the warning, the instruction,
/// the input, the blank line before them and the narrowed note with its blank.
const fn prompt_fixed_rows(narrowed: bool) -> usize {
  if narrowed { 6 } else { 4 }
}

fn render_prompt(frame: &mut Frame, area: Rect, app: &App) {
  let Some(prompt) = &app.prompt else {
    return;
  };
  let count = prompt.request().entries.len();
  let verb = match prompt.request().kind {
    Kind::Restore => "Restore",
    Kind::Nuke => "Nuke",
  };
  let noun = if count == 1 { "file" } else { "files" };
  let title = format!(" {verb} {count} {noun} ");
  // Two rows go to the borders.
  let room = usize::from(area.height)
    .saturating_sub(2)
    .saturating_sub(prompt_fixed_rows(prompt.narrowed()));
  let lines = prompt_lines(prompt, room);
  let widest = lines.iter().map(Line::width).max().unwrap_or(0);
  let width = widest
    .saturating_add(3)
    .max(title.chars().count().saturating_add(2))
    .min(usize::from(area.width));
  let width = u16::try_from(width).unwrap_or(u16::MAX);
  let height = u16::try_from(lines.len())
    .unwrap_or(u16::MAX)
    .saturating_add(2)
    .min(area.height);
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
    .title(title);
  frame.render_widget(
    Paragraph::new(lines).block(block).style(app.palette.normal),
    popup,
  );
}

/// The mode at the left, then the notice or the key help.
fn footer_line(app: &App) -> Line<'static> {
  let style = match app.mode {
    _ if app.prompt.is_some() => app.palette.mode_restore,
    Mode::Normal => app.palette.mode_normal,
    Mode::Visual => app.palette.mode_visual,
  };
  let mode = Span::styled(format!(" {} ", app.mode_label()), style);
  let rest = app.notice.as_ref().map_or_else(
    || Span::styled(HELP, Style::new().fg(Color::DarkGray)),
    |notice| Span::styled(format!(" {notice}"), Style::new().fg(Color::Yellow)),
  );
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

const fn status_color(status: FileStatus) -> Color {
  match status {
    FileStatus::Added => Color::Green,
    FileStatus::Deleted | FileStatus::Conflicted => Color::Red,
    FileStatus::Modified => Color::Yellow,
    FileStatus::Renamed | FileStatus::Copied => Color::Cyan,
  }
}

const fn border_type(border: Border) -> BorderType {
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
      ListRow::Entry(index) => app.files.get(index).map_or_else(
        || ListItem::new(""),
        |f| {
          ListItem::new(Line::from(vec![
            Span::styled(
              format!("{} ", f.status.letter()),
              Style::new().fg(status_color(f.status)),
            ),
            Span::raw(f.path().display().to_string()),
          ]))
        },
      ),
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

/// What `render_diff` needs to draw the rows of one file, gathered once.
struct DiffView<'a> {
  file: &'a FileDiff,
  words: &'a [Vec<Vec<Range<usize>>>],
  colors: Option<&'a FileHighlight>,
  selection: Option<RangeInclusive<usize>>,
  palette: Palette,
  bar_style: Style,
  scroll: usize,
  viewport: usize,
  pan: usize,
  /// Digits of the widest line number.
  gutter: usize,
  /// The hunk under the cursor.
  current: Option<usize>,
  /// The cursor's row within the viewport.
  cursor: usize,
}

impl<'a> DiffView<'a> {
  /// Lays the selection over `line`, drawn at viewport row `i`. `line_spans`
  /// lays the cursor style under the changed words itself, and blank split
  /// cells take it in `side`, so only the selection goes on here.
  fn mark(&self, i: usize, line: Line<'static>) -> Line<'static> {
    let selected = self
      .selection
      .as_ref()
      .is_some_and(|rows| rows.contains(&self.scroll.saturating_add(i)));
    if selected {
      overlay(line, self.palette.visual)
    } else {
      line
    }
  }

  /// The colors and the changed word ranges of line `li` of hunk `hi`.
  fn extras(&self, hi: usize, li: usize) -> (Option<&'a Styled>, &'a [Range<usize>]) {
    let styled = self.colors.and_then(|c| c.hunks.get(hi)?.get(li)?.as_ref());
    let ranges = self
      .words
      .get(hi)
      .and_then(|h| h.get(li))
      .map_or(&[][..], Vec::as_slice);
    (styled, ranges)
  }

  fn unified(&self, frame: &mut Frame, inner: Rect) {
    let [bar_area, text] =
      Layout::horizontal([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    let (active, rows): (Vec<_>, Vec<_>) = self
      .file
      .hunks
      .iter()
      .enumerate()
      .flat_map(|(hi, h)| {
        let lines = h.lines.iter().enumerate().map(move |(li, l)| {
          let (styled, ranges) = self.extras(hi, li);
          Row::Line(l, styled, ranges, self.current == Some(hi))
        });
        let separator = (hi > 0).then_some(Row::Separator);
        separator
          .into_iter()
          .chain(std::iter::once(Row::Header(h, self.current == Some(hi))))
          .chain(lines)
      })
      .skip(self.scroll)
      .take(self.viewport)
      .enumerate()
      .map(|(i, row)| {
        let active = row.active();
        let at_cursor = (i == self.cursor).then_some(self.palette.cursor_line);
        let line = row_line(
          row,
          &self.palette,
          self.gutter,
          text.width.into(),
          self.pan,
          at_cursor,
        );
        (active, self.mark(i, line))
      })
      .unzip();
    frame.render_widget(Paragraph::new(bar(&active, self.bar_style)), bar_area);
    frame.render_widget(Paragraph::new(rows), text);
  }

  /// One side of a split row: line `index` of hunk `h`, or a blank cell.
  fn side(
    &self,
    (hi, h): (usize, &Hunk),
    index: Option<usize>,
    new: bool,
    fill: u16,
    at_cursor: bool,
  ) -> Line<'static> {
    let Some((li, l)) = index.and_then(|li| Some((li, h.lines.get(li)?))) else {
      let cell = blank(fill);
      return if at_cursor {
        overlay(cell, self.palette.cursor_line)
      } else {
        cell
      };
    };
    let (styled, ranges) = self.extras(hi, li);
    let number = if new { l.new_no } else { l.old_no };
    let gutter = format!(
      "{:>width$} ",
      number.map_or_else(String::new, |n| n.to_string()),
      width = self.gutter
    );
    line_spans(
      l,
      &self.palette,
      styled,
      ranges,
      gutter,
      usize::from(fill),
      self.pan,
      self.current == Some(hi),
      at_cursor.then_some(self.palette.cursor_line),
    )
  }

  fn split(&self, frame: &mut Frame, inner: Rect) {
    let left_width = inner.width.saturating_sub(2) / 2;
    let right_width = inner.width.saturating_sub(2).saturating_sub(left_width);
    let [edge_area, left_area, gap_area, right_area] = Layout::horizontal([
      Constraint::Length(1),
      Constraint::Length(left_width),
      Constraint::Length(1),
      Constraint::Length(right_width),
    ])
    .areas(inner);
    let mut gap = Vec::new();
    let (active, (left, right)): (Vec<bool>, (Vec<Line>, Vec<Line>)) = self
      .file
      .hunks
      .iter()
      .enumerate()
      .flat_map(|(hi, h)| {
        let separator = (hi > 0).then_some((hi, h, Slot::Separator));
        let header = std::iter::once((hi, h, Slot::Header));
        let pairs = pairs(h).into_iter().map(move |p| (hi, h, Slot::Pair(p)));
        separator.into_iter().chain(header).chain(pairs)
      })
      .skip(self.scroll)
      .take(self.viewport)
      .enumerate()
      .map(|(i, (hi, h, slot))| {
        let active = self.current == Some(hi);
        let on = i == self.cursor;
        gap.push(if matches!(slot, Slot::Separator) {
          Line::styled(RULE, self.palette.diff_hunk_separator)
        } else if active {
          Line::styled(BAR, self.bar_style)
        } else {
          Line::raw(" ")
        });
        match slot {
          Slot::Separator => (
            false,
            (
              self.mark(
                i,
                row_line(
                  Row::Separator,
                  &self.palette,
                  self.gutter,
                  left_width.into(),
                  self.pan,
                  None,
                ),
              ),
              self.mark(
                i,
                row_line(
                  Row::Separator,
                  &self.palette,
                  self.gutter,
                  right_width.into(),
                  self.pan,
                  None,
                ),
              ),
            ),
          ),
          Slot::Header => {
            let header = Row::Header(h, active);
            let left = row_line(
              header,
              &self.palette,
              self.gutter,
              left_width.into(),
              self.pan,
              None,
            );
            (
              active,
              (self.mark(i, left), self.mark(i, blank(right_width))),
            )
          }
          Slot::Pair((l, r)) => (
            active,
            (
              self.mark(i, self.side((hi, h), l, false, left_width, on)),
              self.mark(i, self.side((hi, h), r, true, right_width, on)),
            ),
          ),
        }
      })
      .unzip();
    frame.render_widget(Paragraph::new(bar(&active, self.bar_style)), edge_area);
    frame.render_widget(Paragraph::new(gap), gap_area);
    frame.render_widget(Paragraph::new(left), left_area);
    frame.render_widget(Paragraph::new(right), right_area);
  }
}

fn render_diff(frame: &mut Frame, area: Rect, app: &mut App) {
  let Some(file) = app.files.get(app.selected) else {
    return;
  };
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
  if app.words.get(app.selected).is_some_and(Option::is_none) {
    let words = app
      .files
      .get(app.selected)
      .map(|f| f.hunks.iter().map(changed_words).collect());
    if let Some(slot) = app.words.get_mut(app.selected) {
      *slot = words;
    }
  }
  let Some(file) = app.files.get(app.selected) else {
    return;
  };
  let words = app
    .words
    .get(app.selected)
    .and_then(|w| w.as_deref())
    .unwrap_or_default();
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

  let widest = file
    .hunks
    .iter()
    .flat_map(|h| &h.lines)
    .map(|l| Span::raw(sanitize(&l.content)).width())
    .max()
    .unwrap_or(0);
  app.pan = app.pan.min(widest.saturating_sub(1));
  let palette = app.palette;
  let view = DiffView {
    file,
    words,
    colors,
    selection: app.selection(),
    palette,
    bar_style: if file.section == Some(Section::Unstaged) {
      palette.diff_hunk_bar
    } else {
      palette.diff_hunk_bar_staged
    },
    scroll: app.scroll,
    viewport: app.viewport,
    pan: app.pan,
    gutter: gutter_width(file),
    current: app.cursor_hunk(),
    cursor: app.cursor.saturating_sub(app.scroll),
  };
  match app.layout {
    DiffLayout::Unified => view.unified(frame, inner),
    DiffLayout::Split => view.split(frame, inner),
  }
}

/// One cell of the rule between hunks.
const RULE: &str = "\u{2500}";

/// The left half of a cell, which `DiffHunkBar` or `DiffHunkBarStaged` colors
/// with its foreground.
const BAR: &str = "\u{258C}";

/// A one cell wide column beside the diff. It holds a half block
/// in `style` on the rows of the hunk under the cursor and is transparent on
/// the others. `DiffHunkBar` styles an unstaged hunk, `DiffHunkBarStaged` a
/// staged one or one in a revision.
fn bar(active: &[bool], style: Style) -> Vec<Line<'static>> {
  active
    .iter()
    .map(|&on| {
      if on {
        Line::styled(BAR, style)
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
    .map(|h| {
      h.old_start
        .saturating_add(h.old_len)
        .max(h.new_start.saturating_add(h.new_len))
    })
    .max()
    .unwrap_or(0);
  last.to_string().len().max(3)
}

/// `fill` pads changed lines to the pane width so their background spans it.
/// `pan` is how many columns the code of a diff line shifts left. The header
/// does not move. `cursor` is the style to lay under the changed words of a
/// diff line.
fn row_line(
  row: Row,
  palette: &Palette,
  width: usize,
  fill: usize,
  pan: usize,
  cursor: Option<Style>,
) -> Line<'static> {
  match row {
    Row::Separator => Line::styled(RULE.repeat(fill), palette.diff_hunk_separator),
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
      line_spans(
        l, palette, styled, changed, gutter, fill, pan, current, cursor,
      )
    }
  }
}

/// One diff line: gutter, marker, then the code with changed words
/// emphasized. The `cursor` style covers all but the changed words, which keep
/// their own background so the cursor line does not flatten them. The first
/// `pan` columns of the code are cut off, and the gutter and marker stay.
#[allow(clippy::too_many_arguments)]
fn line_spans(
  l: &DiffLine,
  palette: &Palette,
  styled: Option<&Styled>,
  changed: &[Range<usize>],
  gutter: String,
  fill: usize,
  pan: usize,
  current: bool,
  cursor: Option<Style>,
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
  let under = cursor.unwrap_or_default();
  let mut spans = vec![
    Span::styled(gutter, Style::new().fg(gutter_color).patch(under)),
    Span::styled(marker.to_string(), under),
  ];
  let pieces: Cow<Styled> = styled.map_or_else(
    || {
      Cow::Owned(vec![(
        Style::new(),
        String::from_utf8_lossy(&l.content).into_owned(),
      )])
    },
    Cow::Borrowed,
  );
  let mut code: Vec<(Style, String)> = emphasize(&pieces, changed, word_bg, under)
    .into_iter()
    .map(|(style, text)| (style, sanitize(text.as_bytes())))
    .collect();
  if l.no_newline {
    code.push((
      Style::new().fg(Color::DarkGray).patch(under),
      " [no newline at end of file]".to_string(),
    ));
  }
  spans.extend(
    skip_columns(code, pan)
      .into_iter()
      .map(|(style, text)| Span::styled(text, style)),
  );
  let used: usize = spans.iter().map(Span::width).sum();
  spans.push(Span::styled(" ".repeat(fill.saturating_sub(used)), under));
  Line::from(spans).style(style)
}

/// Drops the first `columns` display columns of `pieces`. A wide character
/// that straddles the cut is dropped whole.
fn skip_columns(pieces: Vec<(Style, String)>, columns: usize) -> Vec<(Style, String)> {
  let mut skipped = 0_usize;
  let mut out = Vec::with_capacity(pieces.len());
  for (style, text) in pieces {
    if skipped >= columns {
      out.push((style, text));
      continue;
    }
    let mut kept = String::new();
    for c in text.chars() {
      if skipped >= columns {
        kept.push(c);
      } else {
        skipped = skipped.saturating_add(Span::raw(c.encode_utf8(&mut [0; 4]).to_string()).width());
      }
    }
    if !kept.is_empty() {
      out.push((style, kept));
    }
  }
  out
}

/// Splits `pieces` at the edges of `changed` (byte offsets into the joined
/// text), lays the `emphasis` style over the changed parts and the `rest`
/// style over everything else.
fn emphasize(
  pieces: &[(Style, String)],
  changed: &[Range<usize>],
  emphasis: Option<Style>,
  rest: Style,
) -> Vec<(Style, String)> {
  let Some(emphasis) = emphasis.filter(|_| !changed.is_empty()) else {
    return pieces
      .iter()
      .map(|(s, t)| (s.patch(rest), t.clone()))
      .collect();
  };
  let mut out = Vec::new();
  let mut at = 0_usize;
  for (style, text) in pieces {
    let end = at.saturating_add(text.len());
    let mut cut = vec![0, text.len()];
    for r in changed {
      for edge in [r.start, r.end] {
        if edge > at && edge < end {
          cut.push(edge.saturating_sub(at));
        }
      }
    }
    cut.sort_unstable();
    cut.dedup();
    for edges in cut.windows(2) {
      let &[start, stop] = edges else {
        continue;
      };
      let (from, to) = (at.saturating_add(start), at.saturating_add(stop));
      let hit = changed.iter().any(|r| r.start <= from && to <= r.end);
      let style = if hit {
        style.patch(emphasis)
      } else {
        style.patch(rest)
      };
      out.push((style, text.get(start..stop).unwrap_or_default().to_string()));
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

  fn col(n: usize) -> u16 {
    u16::try_from(n).unwrap()
  }

  /// The column where `needle` starts in `row`, counting characters.
  fn col_of(row: &str, needle: &str) -> u16 {
    let at = row.find(needle).unwrap();
    col(row.get(..at).unwrap().chars().count())
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
  fn the_help_menu_lists_restore_and_nuke() {
    let mut app = app_with(DIFF);
    app.on_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
    let (_, rows) = draw(&mut app, 110, 30);
    let (_, restore) = row_with(&rows, "discard its unstaged changes");
    assert!(restore.contains(" r "), "{restore}");
    let (_, nuke) = row_with(&rows, "back to HEAD");
    assert!(nuke.contains(" x "), "{nuke}");
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
      let x = col(row.find(needle).unwrap());
      buffer[(x, col(y))].bg
    };
    let current_bg = Palette::default().diff_current_hunk.bg.unwrap();
    assert_eq!(bg(&rows, "@@ -1,1"), current_bg);
    assert_ne!(bg(&rows, "@@ -9,1"), current_bg);
    let (y, _) = row_with(&rows, "@@ -1,1");
    assert!(buffer[(5, col(y))].modifier.contains(Modifier::REVERSED));

    for _ in 0..3 {
      app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    }
    let (terminal, rows) = draw(&mut app, 60, 12);
    let buffer = terminal.backend().buffer();
    let (y, row) = row_with(&rows, "@@ -9,1");
    assert_eq!(
      buffer[(col(row.find("@@").unwrap()), col(y))].bg,
      current_bg
    );
    let (y, _) = row_with(&rows, "@@ -1,1");
    assert_ne!(buffer[(5, col(y))].bg, current_bg);
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
    press(&mut app, "jvj");
    let (terminal, rows) = draw(&mut app, 100, 8);
    assert!(rows[7].contains(" VISUAL 2 lines "), "{}", rows[7]);
    let buffer = terminal.backend().buffer();
    let bg = |needle: &str| {
      let (y, row) = row_with(&rows, needle);
      let x = col_of(row, needle);
      buffer[(x, col(y))].bg
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

  /// An app listing one entry per `(name, section)`, all with the diff of
  /// the first file of `DIFF`.
  fn named_app(entries: &[(&str, Section)]) -> App {
    let template = parse(DIFF.as_bytes()).unwrap().remove(0);
    let files = entries
      .iter()
      .map(|&(name, section)| FileDiff {
        old_path: Some(name.into()),
        new_path: Some(name.into()),
        section: Some(section),
        ..template.clone()
      })
      .collect();
    let mut app = App::default();
    app.on_loaded(Ok(files));
    app
  }

  fn press(app: &mut App, keys: &str) {
    for c in keys.chars() {
      app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
  }

  #[test]
  fn the_restore_prompt_names_the_file_what_it_loses_and_the_word_to_type() {
    let mut app = named_app(&[("src/lib.rs", Section::Unstaged)]);
    press(&mut app, "r");
    let (_, rows) = draw(&mut app, 100, 24);
    row_with(&rows, "Restore 1 file");
    assert!(
      rows
        .iter()
        .any(|r| r.contains("src/lib.rs") && r.contains("(unstaged)")),
      "{rows:#?}"
    );
    row_with(&rows, "This cannot be undone");
    row_with(&rows, "Type restore to confirm");
    assert!(rows[23].starts_with("\" RESTORE "), "{}", rows[23]);
  }

  #[test]
  fn the_nuke_prompt_lists_every_file_sorted_with_its_loss() {
    let mut app = named_app(&[("b.rs", Section::Staged), ("a.rs", Section::Unstaged)]);
    press(&mut app, "x");
    let (_, rows) = draw(&mut app, 100, 24);
    row_with(&rows, "Nuke 2 files");
    row_with(&rows, "Type nuke to confirm");
    let at = |name: &str, label: &str| {
      rows
        .iter()
        .position(|r| r.contains(name) && r.contains(label))
        .unwrap_or_else(|| panic!("no row with {name} and {label} in {rows:#?}"))
    };
    assert!(at("a.rs", "(unstaged)") < at("b.rs", "(staged)"));
  }

  #[test]
  fn only_the_word_to_type_is_bold() {
    for (keys, word) in [("r", "restore"), ("x", "nuke")] {
      let mut app = named_app(&[("a.rs", Section::Unstaged)]);
      press(&mut app, keys);
      let (terminal, rows) = draw(&mut app, 100, 24);
      let (y, row) = row_with(&rows, &format!("Type {word} to confirm"));
      // The test backend puts a quote before each row.
      let start = col_of(row, word) - 1;
      let buffer = terminal.backend().buffer();
      let bold = |x: u16| buffer[(x, col(y))].modifier.contains(Modifier::BOLD);
      let len = col(word.chars().count());
      for x in start..start + len {
        assert!(bold(x), "{word}: column {x} is not bold in {row}");
      }
      assert!(!bold(start - 1), "the space before {word} is bold");
      assert!(!bold(start + len), "the space after {word} is bold");
      assert!(!bold(start - 5), "'Type' is bold");
    }
  }

  #[test]
  fn typed_input_shows_after_the_prompt_arrow() {
    let mut app = named_app(&[("a.rs", Section::Unstaged)]);
    // The first `r` opens the prompt, the rest is typed into it.
    press(&mut app, "rrest");
    let (_, rows) = draw(&mut app, 100, 24);
    assert!(rows.iter().any(|r| r.contains("> rest")), "{rows:#?}");
  }

  #[test]
  fn a_long_list_is_cut_to_the_room_and_the_input_stays_in_view() {
    let names: Vec<String> = (1..=10).map(|n| format!("f{n:02}.rs")).collect();
    let entries: Vec<(&str, Section)> = names
      .iter()
      .map(|n| (n.as_str(), Section::Unstaged))
      .collect();
    let mut app = named_app(&entries);
    press(&mut app, "x");
    // 15 rows less the footer, two borders and four fixed rows leave eight
    // for files.
    let (_, rows) = draw(&mut app, 100, 15);
    row_with(&rows, "Nuke 10 files");
    row_with(&rows, "… and 3 more");
    let listed = rows.iter().filter(|r| r.contains("(unstaged)")).count();
    assert_eq!(listed, 7, "{rows:#?}");
    row_with(&rows, "> ");
  }

  #[test]
  fn every_file_is_listed_when_the_room_is_exactly_enough() {
    let names: Vec<String> = (1..=8).map(|n| format!("f{n:02}.rs")).collect();
    let entries: Vec<(&str, Section)> = names
      .iter()
      .map(|n| (n.as_str(), Section::Unstaged))
      .collect();
    let mut app = named_app(&entries);
    press(&mut app, "x");
    let (_, rows) = draw(&mut app, 100, 15);
    assert!(!rows.iter().any(|r| r.contains("more")), "{rows:#?}");
    assert_eq!(rows.iter().filter(|r| r.contains("(unstaged)")).count(), 8);
  }

  #[test]
  fn a_narrowed_view_says_the_nuke_covers_more_and_still_fits() {
    let mut app = named_app(&[("a.rs", Section::Staged)]);
    let request = crate::restore::nuke_all(&app.files).unwrap();
    app.prompt = Some(Prompt::new(request).narrowed_view());
    let (_, rows) = draw(&mut app, 100, 12);
    row_with(&rows, "The view is narrowed");
    row_with(&rows, "> ");
  }

  #[test]
  fn the_prompt_is_drawn_even_when_the_changeset_became_empty() {
    let mut app = named_app(&[("a.rs", Section::Unstaged)]);
    press(&mut app, "r");
    app.on_loaded(Ok(Vec::new()));
    assert!(app.files.is_empty());
    let (_, rows) = draw(&mut app, 100, 24);
    row_with(&rows, "Restore 1 file");
  }

  #[test]
  fn the_prompt_survives_a_tiny_terminal() {
    let names: Vec<String> = (1..=5).map(|n| format!("f{n}.rs")).collect();
    let entries: Vec<(&str, Section)> = names
      .iter()
      .map(|n| (n.as_str(), Section::Unstaged))
      .collect();
    for (width, height) in [(30, 6), (12, 3), (5, 2), (1, 1)] {
      let mut app = named_app(&entries);
      press(&mut app, "x");
      draw(&mut app, width, height);
    }
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
    let x = col(row.find("Staged").unwrap());
    assert!(!buffer[(x, col(y))].modifier.contains(Modifier::REVERSED));
    let x = col(rows[y + 1].find("src/a.rs").unwrap());
    assert!(
      buffer[(x, col(y) + 1)]
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
    assert_eq!(buffer[(last_col, col(y))].bg, ADDED_BG);
    let (y, _) = row_with(&rows, "-old");
    assert_eq!(buffer[(last_col, col(y))].bg, REMOVED_BG);
    let (y, _) = row_with(&rows, " keep");
    assert_ne!(buffer[(last_col, col(y))].bg, ADDED_BG);
  }

  #[test]
  fn draws_uncolored_until_highlights_arrive_then_colors_the_code() {
    use crate::highlight::FileHighlight;
    let mut app = app_with(DIFF);
    let (terminal, rows) = draw(&mut app, 80, 14);
    let (y, row) = row_with(&rows, "+new");
    let x = col_of(row, "+new") + 1;
    assert_eq!(terminal.backend().buffer()[(x, col(y))].fg, Color::Reset);

    let red = Style::new().fg(Color::Rgb(200, 1, 2));
    let h = FileHighlight {
      hunks: vec![vec![None, None, Some(vec![(red, "new".to_string())]), None]],
    };
    app.on_highlighted(0, h);
    let (terminal, rows) = draw(&mut app, 80, 14);
    let (y, row) = row_with(&rows, "+new");
    let x = col_of(row, "+new") + 1;
    let cell = &terminal.backend().buffer()[(x, col(y))];
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
      let x = col_of(row, needle) + 1;
      assert_eq!(buffer[(x, col(y))].bg, word_bg);
      assert_eq!(buffer[(79 - 1, col(y))].bg, line_bg);
    }
  }

  #[test]
  fn changed_words_keep_their_background_under_the_cursor_in_both_layouts() {
    let p = Palette::default();
    for split in [false, true] {
      let mut app = app_with(DIFF);
      if split {
        app.layout = DiffLayout::Split;
      }
      app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
      let (terminal, rows) = draw(&mut app, 80, 14);
      let buffer = terminal.backend().buffer();
      let (y, row) = row_with(&rows, "old");
      let x = col_of(row, "old");
      let (word, rest) = (&buffer[(x, col(y))], &buffer[(x + 10, col(y))]);
      assert_eq!(word.bg, p.diff_delete_text.bg.unwrap(), "split={split}");
      assert!(!word.modifier.contains(Modifier::REVERSED), "split={split}");
      assert!(rest.modifier.contains(Modifier::REVERSED), "split={split}");
    }
  }

  #[test]
  #[allow(clippy::single_range_in_vec_init)]
  fn emphasis_keeps_syntax_colors_and_splits_pieces() {
    let red = Style::new().fg(Color::Red);
    let pieces = vec![(red, "ab".to_string()), (Style::new(), "cd".to_string())];
    let out = emphasize(
      &pieces,
      &[1..3],
      Some(Style::new().bg(Color::Blue)),
      Style::new(),
    );
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
      let x = col_of(row, needle)
        .checked_add_signed(i16::try_from(dx - 1).unwrap())
        .unwrap();
      terminal.backend().buffer()[(x, col(y))].clone()
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
    let at = |x: u16, y: usize| terminal.backend().buffer()[(x, col(y))].clone();

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
    let (y, _) = row_with(&rows, "keep");
    assert_eq!(at(list_width + 2, y).bg, Color::Rgb(6, 6, 6));

    app.on_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
    let (terminal, _) = draw(&mut app, 100, 14);
    assert_eq!(terminal.backend().buffer()[(1, 13)].fg, Color::Rgb(8, 8, 8));
  }

  #[test]
  fn the_footer_label_takes_the_restore_group_while_a_prompt_is_open() {
    let mut app = named_app(&[("a.rs", Section::Unstaged)]);
    app.palette.mode_normal = Style::new().fg(Color::Rgb(7, 7, 7));
    app.palette.mode_restore = Style::new().fg(Color::Rgb(9, 9, 9));
    let (terminal, _) = draw(&mut app, 100, 14);
    assert_eq!(terminal.backend().buffer()[(1, 13)].fg, Color::Rgb(7, 7, 7));
    press(&mut app, "r");
    let (terminal, rows) = draw(&mut app, 100, 14);
    assert!(rows[13].starts_with("\" RESTORE "), "{}", rows[13]);
    assert_eq!(terminal.backend().buffer()[(1, 13)].fg, Color::Rgb(9, 9, 9));
    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let (terminal, _) = draw(&mut app, 100, 14);
    assert_eq!(terminal.backend().buffer()[(1, 13)].fg, Color::Rgb(7, 7, 7));
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
    let bar = Palette::default().diff_hunk_bar_staged.fg.unwrap();
    // The list is 20 wide at this size, so the diff text starts at column 21.
    let (edge, gap) = (21, 21 + 1 + (38 - 2) / 2);
    // The bar is a half block, so its color is the foreground of the cell,
    // and a row without the bar is a blank cell.
    let bg_at = |app: &mut App, needle: &str, x: u16| {
      let (terminal, rows) = draw(app, 60, 12);
      let (y, _) = row_with(&rows, needle);
      let cell = &terminal.backend().buffer()[(x, col(y))];
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
  fn the_hunk_bar_is_white_when_unstaged_and_blue_when_staged() {
    let palette = Palette::default();
    let bar_color = |section: Section| {
      let mut app = sectioned_app(&[section]);
      let (terminal, rows) = draw(&mut app, 60, 12);
      let (y, _) = row_with(&rows, "@@");
      let cell = &terminal.backend().buffer()[(21, col(y))];
      assert_eq!(cell.symbol(), BAR);
      cell.fg
    };

    assert_eq!(
      bar_color(Section::Unstaged),
      palette.diff_hunk_bar.fg.unwrap()
    );
    assert_eq!(
      bar_color(Section::Staged),
      palette.diff_hunk_bar_staged.fg.unwrap()
    );
    assert_ne!(bar_color(Section::Unstaged), bar_color(Section::Staged));
  }

  #[test]
  fn section_labels_take_the_section_label_group() {
    let mut app = sectioned_app(&[Section::Unstaged, Section::Staged]);
    app.palette.section_label = Style::new().fg(Color::Rgb(5, 5, 5));
    let (terminal, rows) = draw(&mut app, 80, 14);
    let (y, row) = row_with(&rows, "Unstaged");
    let x = col_of(row, "Unstaged") - 1;
    assert_eq!(
      terminal.backend().buffer()[(x, col(y))].fg,
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
    let x = col_of(row, "+hello") + 1;
    assert_eq!(terminal.backend().buffer()[(x, col(y))].bg, ADDED_BG);
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
    assert_eq!(keep.matches("keep").count(), 2);
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
    for c in "jjv".chars() {
      app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }
    let (terminal, rows) = draw(&mut app, 81, 12);
    assert!(rows[11].contains(" VISUAL 1 line "), "{}", rows[11]);
    let buffer = terminal.backend().buffer();
    let (y, row) = row_with(&rows, "-old2");
    let left = col_of(row, "-old2");
    // The left line and the blank right column are both highlighted.
    for x in [left, 79] {
      assert_eq!(buffer[(x, col(y))].bg, SELECTED_BG, "column {x}");
    }
    let (y, _) = row_with(&rows, "-old1");
    for x in [left, 79] {
      assert_ne!(buffer[(x, col(y))].bg, SELECTED_BG, "column {x}");
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

  fn pan_app(layout: DiffLayout) -> App {
    let mut app = app_with(
      "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-0123456789abcdef\n+ABCDEFGHIJKLMNOP\n",
    );
    app.layout = layout;
    app
  }

  #[test]
  fn panning_shifts_the_code_and_keeps_the_gutter_marker_and_header() {
    for layout in [DiffLayout::Unified, DiffLayout::Split] {
      let mut app = pan_app(layout);
      app.pan = 4;
      let (_, rows) = draw(&mut app, 81, 8);
      let (_, removed) = row_with(&rows, "456789abcdef");
      assert!(!removed.contains("0123"), "{removed}");
      assert!(removed.contains("-456789abcdef"), "{removed}");
      let (_, header) = row_with(&rows, "@@ -1,1 +1,1 @@");
      assert!(header.contains("@@ -1,1 +1,1 @@"), "{header}");
    }
  }

  #[test]
  fn the_pan_stops_at_the_widest_line() {
    let mut app = pan_app(DiffLayout::Unified);
    app.pan = 500;
    let (_, rows) = draw(&mut app, 81, 8);
    assert_eq!(app.pan, 15);
    row_with(&rows, "P");
  }

  #[test]
  fn split_layout_colors_each_column_by_kind() {
    let mut app = split_app();
    let (terminal, rows) = draw(&mut app, 81, 12);
    let buffer = terminal.backend().buffer();
    let (y, row) = row_with(&rows, "-old1");
    let new_x = col_of(row, "+new1") - 1;
    let old_x = col_of(row, "-old1") - 1;
    assert_eq!(buffer[(old_x, col(y))].bg, REMOVED_BG);
    assert_eq!(buffer[(new_x, col(y))].bg, ADDED_BG);
    let (y, _) = row_with(&rows, "-old2");
    // The blank right column carries no background.
    assert_ne!(buffer[(new_x, col(y))].bg, ADDED_BG);
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
