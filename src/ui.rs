use crate::app::{App, Load};
use crate::diff::{FileDiff, FileStatus, Hunk, Line as DiffLine, LineKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, Paragraph};

const ADDED_BG: Color = Color::Rgb(18, 52, 28);
const REMOVED_BG: Color = Color::Rgb(66, 24, 28);

const HELP: &str =
    " j/k scroll  space/PgDn page  ctrl-d/u half page  g/G top/bottom  n/p file  q quit";

enum Row<'a> {
    Header(&'a Hunk),
    Line(&'a DiffLine),
}

pub fn render(frame: &mut Frame, app: &mut App) {
    let [main, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    frame.render_widget(
        Paragraph::new(HELP).style(Style::new().fg(Color::DarkGray)),
        footer,
    );

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
    }
}

fn render_files(frame: &mut Frame, area: Rect, app: &mut App) {
    let items: Vec<ListItem> = app
        .files
        .iter()
        .map(|f| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{} ", f.status.letter()),
                    Style::new().fg(status_color(f.status)),
                ),
                Span::raw(f.path().display().to_string()),
            ]))
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
    let file = &app.files[app.selected];

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
    let rows = file
        .hunks
        .iter()
        .flat_map(|h| std::iter::once(Row::Header(h)).chain(h.lines.iter().map(Row::Line)))
        .skip(app.scroll)
        .take(app.viewport)
        .map(|row| row_line(row, width, usize::from(inner.width)));
    frame.render_widget(Paragraph::new(rows.collect::<Vec<_>>()), inner);
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
        Row::Header(h) => Line::styled(
            format!(
                "@@ -{},{} +{},{} @@ {}",
                h.old_start,
                h.old_len,
                h.new_start,
                h.new_len,
                sanitize(&h.heading)
            ),
            Style::new().fg(Color::Cyan),
        ),
        Row::Line(l) => {
            let (marker, style) = match l.kind {
                LineKind::Context => (' ', Style::new()),
                LineKind::Added => ('+', Style::new().bg(ADDED_BG)),
                LineKind::Removed => ('-', Style::new().bg(REMOVED_BG)),
            };
            let no = |n: Option<u32>| n.map_or(String::new(), |n| n.to_string());
            let mut spans = vec![
                Span::styled(
                    format!("{:>width$} {:>width$} ", no(l.old_no), no(l.new_no)),
                    Style::new().fg(Color::DarkGray),
                ),
                Span::raw(format!("{marker}{}", sanitize(&l.content))),
            ];
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
    }
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
    use crate::diff::{FileStatus, parse};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
        rows.iter()
            .enumerate()
            .find(|(_, r)| r.contains(needle))
            .unwrap_or_else(|| panic!("no row contains {needle:?} in {rows:#?}"))
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
            rows.iter()
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
            rows.iter()
                .any(|r| r.contains("@@ -1,3 +1,3 @@ fn main() {"))
        );
        assert!(rows.iter().any(|r| r.contains("-old")));
        assert!(rows.iter().any(|r| r.contains("+new")));
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
        let mut app =
            app_with("diff --git a/e b/e\nnew file mode 100644\nindex 0000000..e69de29\n");
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
