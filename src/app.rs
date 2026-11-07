use crate::args::Layout;
use crate::diff::{FileDiff, Hunk};
use crate::highlight::FileHighlight;
use crate::highlight::Highlighter;
use crate::split;
use crate::ui;
use crate::vcs::{self, DiffRequest, Vcs};
use crate::watch::Watch;
use crate::worker::Worker;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::ListState;
use std::io;
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::thread;

pub enum Event {
  Key(KeyEvent),
  Resize,
  /// A load finished. The number says which load, so a slow older one cannot
  /// overwrite a newer result.
  Loaded(u64, Result<Vec<FileDiff>, vcs::Error>),
  /// The watcher saw the repository change.
  Changed,
  /// A highlight for a file of the load with this number.
  Highlighted(u64, usize, FileHighlight),
  InputFailed(io::Error),
}

pub enum Load {
  Loading,
  Ready,
  Failed(String),
}

pub struct App {
  pub load: Load,
  pub files: Arc<Vec<FileDiff>>,
  /// Colors per file, parallel to `files`. `None` until the worker delivers.
  pub highlights: Vec<Option<FileHighlight>>,
  /// Changed word ranges per file, hunk and line, filled when a file is first
  /// drawn. See `words::changed_words`.
  pub words: Vec<Option<Vec<Vec<Vec<Range<usize>>>>>>,
  pub layout: Layout,
  pub selected: usize,
  pub list_state: ListState,
  /// Index of the first visible row of the selected file's diff.
  pub scroll: usize,
  /// Height of the diff pane in rows, as of the last render.
  pub viewport: usize,
  pub quit: bool,
}

impl Default for App {
  fn default() -> Self {
    App {
      load: Load::Loading,
      files: Arc::new(Vec::new()),
      highlights: Vec::new(),
      words: Vec::new(),
      layout: Layout::Unified,
      selected: 0,
      list_state: ListState::default(),
      scroll: 0,
      viewport: 0,
      quit: false,
    }
  }
}

/// Rows a hunk occupies: its header plus one row per line (unified) or per
/// split pair.
fn hunk_rows(hunk: &Hunk, layout: Layout) -> usize {
  1 + match layout {
    Layout::Unified => hunk.lines.len(),
    Layout::Split => split::pairs(hunk).len(),
  }
}

/// Rows a file's diff occupies in `layout`.
pub fn row_count(file: &FileDiff, layout: Layout) -> usize {
  file.hunks.iter().map(|h| hunk_rows(h, layout)).sum()
}

/// Finds the hunk and line shown at `row`. The line is `None` on a header.
fn locate(file: &FileDiff, layout: Layout, mut row: usize) -> Option<(usize, Option<usize>)> {
  for (hi, hunk) in file.hunks.iter().enumerate() {
    let rows = hunk_rows(hunk, layout);
    if row >= rows {
      row -= rows;
      continue;
    }
    let Some(offset) = row.checked_sub(1) else {
      return Some((hi, None));
    };
    let line = match layout {
      Layout::Unified => offset,
      Layout::Split => {
        let (left, right) = split::pairs(hunk)[offset];
        left.or(right).expect("a pair has at least one side")
      }
    };
    return Some((hi, Some(line)));
  }
  None
}

/// The row where a hunk, or a line within it, is drawn in `layout`.
fn row_of(file: &FileDiff, layout: Layout, hunk: usize, line: Option<usize>) -> usize {
  let before: usize = file.hunks[..hunk]
    .iter()
    .map(|h| hunk_rows(h, layout))
    .sum();
  let Some(line) = line else { return before };
  let offset = match layout {
    Layout::Unified => line,
    Layout::Split => split::pairs(&file.hunks[hunk])
      .iter()
      .position(|&(l, r)| l == Some(line) || r == Some(line))
      .unwrap_or(0),
  };
  before + 1 + offset
}

impl App {
  /// Takes a freshly loaded changeset. Returns false when nothing changed,
  /// so callers can keep the highlights they already have. After a reload
  /// the same file stays selected, at the same scroll position, when it is
  /// still in the changeset; otherwise the selection moves to the nearest
  /// index.
  pub fn on_loaded(&mut self, result: Result<Vec<FileDiff>, vcs::Error>) -> bool {
    let files = match result {
      Ok(files) => files,
      Err(e) => {
        self.load = Load::Failed(e.to_string());
        return false;
      }
    };
    self.load = Load::Ready;
    if *self.files == files {
      return false;
    }
    let kept = self
      .files
      .get(self.selected)
      .and_then(|old| files.iter().position(|f| f.path() == old.path()));
    let scroll = if kept.is_some() { self.scroll } else { 0 };
    self.highlights = files.iter().map(|_| None).collect();
    self.words = files.iter().map(|_| None).collect();
    let index = kept.unwrap_or(self.selected.min(files.len().saturating_sub(1)));
    self.files = Arc::new(files);
    self.select(index);
    self.scroll = scroll;
    self.clamp_scroll();
    true
  }

  pub fn on_highlighted(&mut self, index: usize, highlight: FileHighlight) {
    if let Some(slot) = self.highlights.get_mut(index) {
      *slot = Some(highlight);
    }
  }

  pub fn on_key(&mut self, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
      return;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let page = self.viewport.max(1);
    match key.code {
      KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
      KeyCode::Char('c') if ctrl => self.quit = true,
      KeyCode::Char('j') | KeyCode::Down => self.scroll_by(1),
      KeyCode::Char('k') | KeyCode::Up => self.scroll_by(-1),
      KeyCode::Char('d') if ctrl => self.scroll_by(page as isize / 2),
      KeyCode::Char('u') if ctrl => self.scroll_by(-(page as isize / 2)),
      KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_by(page as isize),
      KeyCode::PageUp => self.scroll_by(-(page as isize)),
      KeyCode::Char('g') | KeyCode::Home => self.scroll = 0,
      KeyCode::Char('G') | KeyCode::End => self.scroll = self.max_scroll(),
      KeyCode::Char('s') => self.toggle_layout(),
      KeyCode::Tab | KeyCode::Char('n') => self.select_by(1),
      KeyCode::BackTab | KeyCode::Char('p') => self.select_by(-1),
      _ => {}
    }
  }

  pub fn max_scroll(&self) -> usize {
    let rows = self
      .files
      .get(self.selected)
      .map_or(0, |f| row_count(f, self.layout));
    rows.saturating_sub(self.viewport)
  }

  pub fn clamp_scroll(&mut self) {
    self.scroll = self.scroll.min(self.max_scroll());
  }

  /// Switches between unified and split, keeping the hunk and line at the top
  /// of the pane in view.
  fn toggle_layout(&mut self) {
    let from = self.layout;
    let to = match from {
      Layout::Unified => Layout::Split,
      Layout::Split => Layout::Unified,
    };
    if let Some(file) = self.files.get(self.selected)
      && let Some((hunk, line)) = locate(file, from, self.scroll)
    {
      self.scroll = row_of(file, to, hunk, line);
    }
    self.layout = to;
    self.clamp_scroll();
  }

  fn scroll_by(&mut self, delta: isize) {
    self.scroll = self.scroll.saturating_add_signed(delta);
    self.clamp_scroll();
  }

  fn select(&mut self, index: usize) {
    self.selected = index;
    self.scroll = 0;
    self
      .list_state
      .select((!self.files.is_empty()).then_some(index));
  }

  fn select_by(&mut self, delta: isize) {
    if self.files.is_empty() {
      return;
    }
    let last = self.files.len() - 1;
    self.select(self.selected.saturating_add_signed(delta).min(last));
  }
}

/// Runs git off the UI thread and reports the result over the channel.
fn spawn_load(
  vcs: Arc<impl Vcs + Send + Sync + 'static>,
  request: DiffRequest,
  id: u64,
  tx: Sender<Event>,
) {
  thread::spawn(move || {
    let _ = tx.send(Event::Loaded(id, vcs.diff(&request)));
  });
}

fn spawn_input(tx: Sender<Event>) {
  thread::spawn(move || {
    loop {
      let event = match event::read() {
        Ok(event::Event::Key(key)) => Event::Key(key),
        Ok(event::Event::Resize(..)) => Event::Resize,
        Ok(_) => continue,
        Err(e) => {
          let _ = tx.send(Event::InputFailed(e));
          break;
        }
      };
      if tx.send(event).is_err() {
        break;
      }
    }
  });
}

/// `theme` turns syntax highlighting on; `None` leaves diffs uncolored.
pub fn run<V: Vcs + Send + Sync + 'static>(
  terminal: &mut DefaultTerminal,
  vcs: V,
  request: DiffRequest,
  theme: Option<String>,
  layout: Layout,
  watch: Option<&Path>,
) -> io::Result<()> {
  let vcs = Arc::new(vcs);
  let (tx, rx) = mpsc::channel();
  spawn_input(tx.clone());
  spawn_load(vcs.clone(), request.clone(), 0, tx.clone());
  let _watch = match watch {
    Some(root) => {
      let tx = tx.clone();
      Some(Watch::start(root, move || tx.send(Event::Changed).is_ok()).map_err(io::Error::other)?)
    }
    None => None,
  };
  // `latest` numbers the newest load asked for, `applied` the one on screen.
  let (mut latest, mut applied) = (0, 0);

  let mut app = App {
    layout,
    ..App::default()
  };
  let highlighter = theme
    .and_then(|theme| Highlighter::new(&theme).ok())
    .map(Arc::new);
  let mut worker: Option<Worker> = None;
  while !app.quit {
    terminal.draw(|frame| ui::render(frame, &mut app))?;
    match rx.recv() {
      Ok(Event::Key(key)) => app.on_key(key),
      Ok(Event::Resize) => {}
      Ok(Event::Changed) => {
        latest += 1;
        spawn_load(vcs.clone(), request.clone(), latest, tx.clone());
      }
      Ok(Event::Loaded(id, result)) if id == latest => {
        if app.on_loaded(result) {
          applied = id;
          worker = highlighter
            .clone()
            .filter(|_| !app.files.is_empty())
            .map(|highlighter| {
              let tx = tx.clone();
              Worker::spawn(
                vcs.clone(),
                request.clone(),
                app.files.clone(),
                highlighter,
                app.selected,
                move |i, h| tx.send(Event::Highlighted(id, i, h)).is_ok(),
              )
            });
        }
      }
      Ok(Event::Loaded(..)) => {}
      Ok(Event::Highlighted(id, index, highlight)) if id == applied => {
        app.on_highlighted(index, highlight)
      }
      Ok(Event::Highlighted(..)) => {}
      Ok(Event::InputFailed(e)) => return Err(e),
      Err(_) => break,
    }
    if let Some(worker) = &worker {
      worker.focus(app.selected);
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::diff::{FileStatus, Hunk, Line, LineKind};
  use std::path::{Path, PathBuf};

  fn file(name: &str, lines: usize) -> FileDiff {
    FileDiff {
      old_path: Some(PathBuf::from(name)),
      new_path: Some(PathBuf::from(name)),
      status: FileStatus::Modified,
      binary: false,
      hunks: vec![Hunk {
        old_start: 1,
        old_len: lines as u32,
        new_start: 1,
        new_len: lines as u32,
        heading: Vec::new(),
        lines: (1..=lines as u32)
          .map(|n| Line {
            kind: LineKind::Context,
            old_no: Some(n),
            new_no: Some(n),
            content: format!("line {n}").into_bytes(),
            no_newline: false,
          })
          .collect(),
      }],
    }
  }

  fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
  }

  fn ready(files: Vec<FileDiff>, viewport: usize) -> App {
    let mut app = App::default();
    app.on_loaded(Ok(files));
    app.viewport = viewport;
    app
  }

  #[test]
  fn starts_in_the_loading_state() {
    assert!(matches!(App::default().load, Load::Loading));
  }

  #[test]
  fn loaded_files_select_the_first_one() {
    let app = ready(vec![file("a", 3), file("b", 3)], 5);
    assert!(matches!(app.load, Load::Ready));
    assert_eq!(app.list_state.selected(), Some(0));
  }

  #[test]
  fn load_failure_is_kept_for_display() {
    let mut app = App::default();
    app.on_loaded(Err(vcs::Error::Git("boom".into())));
    assert!(matches!(app.load, Load::Failed(ref m) if m == "boom"));
  }

  #[test]
  fn scrolling_stops_at_both_ends() {
    // 1 hunk header + 10 lines = 11 rows, 4 visible.
    let mut app = ready(vec![file("a", 10)], 4);
    app.on_key(key(KeyCode::Char('k')));
    assert_eq!(app.scroll, 0);
    for _ in 0..20 {
      app.on_key(key(KeyCode::Char('j')));
    }
    assert_eq!(app.scroll, 7);
    app.on_key(key(KeyCode::Char('g')));
    assert_eq!(app.scroll, 0);
    app.on_key(key(KeyCode::Char('G')));
    assert_eq!(app.scroll, 7);
  }

  #[test]
  fn paging_moves_by_viewport_and_half_viewport() {
    let mut app = ready(vec![file("a", 30)], 10);
    app.on_key(key(KeyCode::PageDown));
    assert_eq!(app.scroll, 10);
    app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(app.scroll, 5);
    app.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert_eq!(app.scroll, 10);
  }

  #[test]
  fn switching_files_resets_scroll_and_stays_in_range() {
    let mut app = ready(vec![file("a", 30), file("b", 30)], 5);
    app.on_key(key(KeyCode::PageDown));
    assert!(app.scroll > 0);
    app.on_key(key(KeyCode::Tab));
    assert_eq!((app.selected, app.scroll), (1, 0));
    app.on_key(key(KeyCode::Tab));
    assert_eq!(app.selected, 1);
    app.on_key(key(KeyCode::BackTab));
    app.on_key(key(KeyCode::Char('p')));
    assert_eq!(app.selected, 0);
    assert_eq!(app.list_state.selected(), Some(0));
  }

  #[test]
  fn keys_do_nothing_harmful_without_files() {
    let mut app = ready(Vec::new(), 5);
    for code in [
      KeyCode::Tab,
      KeyCode::Char('j'),
      KeyCode::Char('G'),
      KeyCode::PageDown,
    ] {
      app.on_key(key(code));
    }
    assert_eq!((app.selected, app.scroll), (0, 0));
    assert_eq!(app.list_state.selected(), None);
  }

  #[test]
  fn highlights_attach_to_their_file() {
    let mut app = ready(vec![file("a", 1), file("b", 1)], 5);
    assert!(app.highlights.iter().all(Option::is_none));
    app.on_highlighted(1, FileHighlight::default());
    assert!(app.highlights[0].is_none() && app.highlights[1].is_some());
    app.on_highlighted(9, FileHighlight::default());
  }

  /// Two hunks. Split rows: header, 1 context, 1 pair (3 removed vs 1 added) ...
  fn mixed_file() -> FileDiff {
    let line = |kind, old_no, new_no, text: &str| Line {
      kind,
      old_no,
      new_no,
      content: text.as_bytes().to_vec(),
      no_newline: false,
    };
    let hunk = |start: u32, lines: Vec<Line>| Hunk {
      old_start: start,
      old_len: 0,
      new_start: start,
      new_len: 0,
      heading: Vec::new(),
      lines,
    };
    let (c, r, a) = (LineKind::Context, LineKind::Removed, LineKind::Added);
    FileDiff {
      hunks: vec![
        hunk(
          1,
          vec![
            line(c, Some(1), Some(1), "keep"),
            line(r, Some(2), None, "r1"),
            line(r, Some(3), None, "r2"),
            line(r, Some(4), None, "r3"),
            line(a, None, Some(2), "a1"),
          ],
        ),
        hunk(
          20,
          vec![
            line(c, Some(20), Some(18), "x"),
            line(a, None, Some(19), "y"),
          ],
        ),
      ],
      ..file("m", 0)
    }
  }

  #[test]
  fn split_layout_row_counts_merge_removed_and_added_lines() {
    let f = mixed_file();
    assert_eq!(row_count(&f, Layout::Unified), 2 + 5 + 2);
    // header + keep + 3 shared rows, header + 2 rows
    assert_eq!(row_count(&f, Layout::Split), 5 + 3);
  }

  #[test]
  fn toggling_layout_keeps_the_hunk_and_line_at_the_top() {
    let mut app = ready(vec![mixed_file()], 1);
    // Unified rows: 0 header, 1 keep, 2-4 r1-r3, 5 a1, 6 header, 7 x, 8 y.
    // Split rows: 0 header, 1 keep, 2 r1|a1, 3 r2, 4 r3, 5 header, 6 x, 7 y.
    for (unified, split) in [
      (0, 0),
      (1, 1),
      (2, 2),
      (3, 3),
      (4, 4),
      (5, 2),
      (6, 5),
      (7, 6),
    ] {
      app.layout = Layout::Unified;
      app.scroll = unified;
      app.on_key(key(KeyCode::Char('s')));
      assert_eq!(app.layout, Layout::Split);
      assert_eq!(app.scroll, split, "unified row {unified}");
    }
    app.scroll = 6;
    app.on_key(key(KeyCode::Char('s')));
    assert_eq!(app.layout, Layout::Unified);
    assert_eq!(app.scroll, 7);
  }

  #[test]
  fn toggling_without_files_only_flips_the_layout() {
    let mut app = App::default();
    app.on_key(key(KeyCode::Char('s')));
    assert_eq!(app.layout, Layout::Split);
    assert_eq!(app.scroll, 0);
  }

  #[test]
  fn a_reload_keeps_the_selected_file_and_scroll() {
    let mut app = ready(vec![file("a", 30), file("b", 30), file("c", 30)], 5);
    app.on_key(key(KeyCode::Char('n')));
    app.on_key(key(KeyCode::PageDown));
    let scroll = app.scroll;
    assert!(scroll > 0);
    // "a" disappears and "b" grows; "b" moves to index 0 but stays selected.
    assert!(app.on_loaded(Ok(vec![file("b", 40), file("c", 30)])));
    assert_eq!(app.selected, 0);
    assert_eq!(app.files[app.selected].path(), Path::new("b"));
    assert_eq!(app.scroll, scroll);
    assert_eq!(app.list_state.selected(), Some(0));
  }

  #[test]
  fn a_reload_clamps_the_scroll_when_the_file_shrinks() {
    let mut app = ready(vec![file("a", 30)], 5);
    app.on_key(key(KeyCode::Char('G')));
    assert!(app.on_loaded(Ok(vec![file("a", 10)])));
    assert_eq!(app.scroll, app.max_scroll());
    assert_eq!(app.scroll, 11 - 5);
  }

  #[test]
  fn a_reload_moves_to_the_nearest_file_when_the_selected_one_is_gone() {
    let mut app = ready(vec![file("a", 5), file("b", 5), file("c", 5)], 5);
    app.on_key(key(KeyCode::Char('G')));
    app.on_key(key(KeyCode::Char('n')));
    app.on_key(key(KeyCode::Char('n')));
    assert_eq!(app.selected, 2);
    assert!(app.on_loaded(Ok(vec![file("a", 5), file("b", 5)])));
    assert_eq!(app.selected, 1);
    assert_eq!(app.scroll, 0);
    assert!(app.on_loaded(Ok(Vec::new())));
    assert_eq!(app.selected, 0);
    assert_eq!(app.list_state.selected(), None);
  }

  #[test]
  fn an_identical_reload_changes_nothing() {
    let mut app = ready(vec![file("a", 5)], 5);
    app.on_highlighted(0, FileHighlight::default());
    assert!(!app.on_loaded(Ok(vec![file("a", 5)])));
    assert!(app.highlights[0].is_some());
  }

  #[test]
  fn a_failed_reload_shows_the_error_and_the_next_one_recovers() {
    let mut app = ready(vec![file("a", 5), file("b", 5)], 5);
    app.on_key(key(KeyCode::Char('n')));
    assert!(!app.on_loaded(Err(vcs::Error::Git("boom".into()))));
    assert!(matches!(app.load, Load::Failed(_)));
    assert!(!app.on_loaded(Ok(vec![file("a", 5), file("b", 5)])));
    assert!(matches!(app.load, Load::Ready));
    assert_eq!(app.selected, 1);
  }

  #[test]
  fn quit_keys_stop_the_app() {
    for k in [
      key(KeyCode::Char('q')),
      key(KeyCode::Esc),
      KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ] {
      let mut app = ready(vec![file("a", 1)], 5);
      app.on_key(k);
      assert!(app.quit);
    }
  }

  #[test]
  fn key_release_events_are_ignored() {
    let mut app = ready(vec![file("a", 1)], 5);
    let mut release = key(KeyCode::Char('q'));
    release.kind = KeyEventKind::Release;
    app.on_key(release);
    assert!(!app.quit);
  }

  struct Slow(std::sync::Mutex<mpsc::Receiver<()>>);

  impl Vcs for Slow {
    fn status(&self) -> Result<vcs::Status, vcs::Error> {
      unreachable!()
    }

    fn diff(&self, _: &DiffRequest) -> Result<Vec<FileDiff>, vcs::Error> {
      self.0.lock().unwrap().recv().unwrap();
      Ok(vec![file("slow", 1)])
    }

    fn sides(&self, _: &DiffRequest, _: &FileDiff) -> vcs::Sides {
      vcs::Sides::default()
    }
  }

  #[test]
  fn git_runs_off_the_calling_thread() {
    let (release, gate) = mpsc::channel();
    let (tx, rx) = mpsc::channel();
    // Returns at once even though the backend blocks until released.
    spawn_load(
      Arc::new(Slow(std::sync::Mutex::new(gate))),
      DiffRequest::default(),
      7,
      tx,
    );
    assert!(rx.try_recv().is_err());
    release.send(()).unwrap();
    match rx.recv().unwrap() {
      Event::Loaded(7, Ok(files)) => assert_eq!(files.len(), 1),
      _ => panic!("expected a loaded diff"),
    }
  }
}
