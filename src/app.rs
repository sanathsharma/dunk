use crate::args::{Border, Layout};
use crate::config::Settings;
use crate::diff::{FileDiff, FileStatus, Hunk, Section};
use crate::editor::Editor;
use crate::highlight::FileHighlight;
use crate::highlight::Highlighter;
use crate::palette::Palette;
use crate::split;
use crate::ui;
use crate::vcs::{self, DiffRequest, Sides, Vcs};
use crate::watch::Watch;
use crate::words::FileWords;
use crate::worker::Worker;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::ListState;
use std::collections::BTreeSet;
use std::io;
use std::ops::{Range, RangeInclusive};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

/// How long the input thread waits for a key before checking whether it
/// should step aside.
const INPUT_POLL: Duration = Duration::from_millis(50);

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
  /// The full contents of a file of the load with this number, read for an
  /// expansion.
  Sides(u64, usize, Arc<Sides>),
  /// The colors of a file shown with other context than it was loaded with:
  /// the load, the file and which of its context changes they were made for.
  Rehighlighted(u64, usize, u32, FileHighlight),
  /// A stage or unstage finished. Carries the entry the selection moves to
  /// once the changeset is read again.
  Indexed(Option<Follow>, Result<(), vcs::Error>),
  InputFailed(io::Error),
}

/// Where the selection goes after a reload: an entry of the file list, named
/// by path and section so it can be found again, and optionally one of its
/// hunks for the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Follow {
  pub path: PathBuf,
  pub section: Section,
  pub hunk: Option<usize>,
}

/// What a stage or unstage moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
  File,
  Hunk(usize),
  /// The lines of a hunk at these indexes into the hunk's lines.
  Lines(usize, BTreeSet<usize>),
}

/// A stage or unstage of a file, a hunk or some of its lines waiting to run.
pub struct IndexRequest {
  pub file: FileDiff,
  pub target: Target,
  /// True to stage, false to unstage.
  pub stage: bool,
  pub follow: Option<Follow>,
}

impl IndexRequest {
  /// Writes the index as the request asks.
  pub fn run(&self, vcs: &(impl Vcs + ?Sized)) -> Result<(), vcs::Error> {
    let file = &self.file;
    match (&self.target, self.stage) {
      (Target::File, true) => vcs.stage(file),
      (Target::File, false) => vcs.unstage(file),
      (Target::Hunk(hunk), true) => vcs.stage_hunk(file, *hunk),
      (Target::Hunk(hunk), false) => vcs.unstage_hunk(file, *hunk),
      (Target::Lines(hunk, lines), true) => vcs.stage_lines(file, *hunk, lines),
      (Target::Lines(hunk, lines), false) => vcs.unstage_lines(file, *hunk, lines),
    }
  }
}

/// A file, relative to the repository root, and the 1-based line to open it at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditRequest {
  pub path: PathBuf,
  pub line: u32,
}

pub enum Load {
  Loading,
  Ready,
  Failed(String),
}

/// A file whose context changed, waiting to be colored again.
pub struct Rehighlight {
  pub index: usize,
  pub generation: u32,
  pub file: FileDiff,
  pub sides: Arc<Sides>,
}

/// Lines of context a key press adds to or takes from each side of a hunk.
const CONTEXT_STEP: u32 = 3;

/// What a context key does to the hunk under the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
  More,
  Less,
  /// Back to the context the changeset was loaded with.
  Reset,
}

/// What the keys currently do. The footer names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
  Normal,
  /// Keys extend a selection of lines that `a` then stages or unstages.
  Visual,
}

impl Mode {
  pub fn label(self) -> &'static str {
    match self {
      Mode::Normal => "NORMAL",
      Mode::Visual => "VISUAL",
    }
  }
}

pub struct App {
  pub mode: Mode,
  /// Whether the help menu covers the diff. It takes every key until closed.
  pub help: bool,
  pub load: Load,
  /// The files as shown, with any added context.
  pub files: Arc<Vec<FileDiff>>,
  /// The files as git reported them, with the context it was asked for.
  pub loaded: Arc<Vec<FileDiff>>,
  /// Full file contents per file, parallel to `files`. `None` until the
  /// first widening of that file asks for them.
  sides: Vec<Option<Arc<Sides>>>,
  /// Which files `take_sides_request` already handed out.
  sides_requested: Vec<bool>,
  /// A widening waiting for its file contents: the file and the hunks of
  /// `loaded` to widen.
  pending_widen: Option<(usize, Range<usize>)>,
  /// Lines of context added on each side of every hunk of `loaded`, per file.
  /// Always a multiple of `CONTEXT_STEP`.
  context: Vec<Vec<u32>>,
  /// For each hunk of `files`, the index of the first hunk of `loaded` it
  /// holds. The hunks between two entries were merged into one.
  owners: Vec<Vec<usize>>,
  /// How many times each file's context changed. Colors made for an earlier
  /// count do not fit its lines.
  expansions: Vec<u32>,
  rehighlight: Option<Rehighlight>,
  index_request: Option<IndexRequest>,
  /// Where the selection goes when the entry it is on has left the list.
  follow: Option<Follow>,
  /// A short message for the footer, cleared by the next key press.
  pub notice: Option<String>,
  /// The file and line `e` asked to open, until the run loop takes it.
  edit: Option<EditRequest>,
  /// Colors per file, parallel to `files`. `None` until the worker delivers.
  pub highlights: Vec<Option<FileHighlight>>,
  /// Changed word ranges per file, hunk and line, filled when a file is first
  /// drawn. See `words::changed_words`.
  pub words: Vec<Option<FileWords>>,
  pub layout: Layout,
  pub palette: Palette,
  pub border: Border,
  pub selected: usize,
  pub list_state: ListState,
  /// Index of the first visible row of the selected file's diff.
  pub scroll: usize,
  /// Row of the line cursor in the selected file's diff, in the current
  /// layout. The viewport follows it.
  pub cursor: usize,
  /// Row where the selection started, in visual mode.
  visual_anchor: usize,
  /// Height of the diff pane in rows, as of the last render.
  pub viewport: usize,
  pub quit: bool,
}

impl Default for App {
  fn default() -> Self {
    App {
      mode: Mode::Normal,
      help: false,
      load: Load::Loading,
      files: Arc::new(Vec::new()),
      loaded: Arc::new(Vec::new()),
      sides: Vec::new(),
      sides_requested: Vec::new(),
      pending_widen: None,
      context: Vec::new(),
      owners: Vec::new(),
      expansions: Vec::new(),
      rehighlight: None,
      index_request: None,
      follow: None,
      notice: None,
      edit: None,
      highlights: Vec::new(),
      words: Vec::new(),
      layout: Layout::Unified,
      palette: Palette::default(),
      border: Border::default(),
      selected: 0,
      list_state: ListState::default(),
      scroll: 0,
      cursor: 0,
      visual_anchor: 0,
      viewport: 0,
      quit: false,
    }
  }
}

/// One row of the file list. Only entries can be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListRow {
  /// A section heading with the number of entries under it.
  Header(Section, usize),
  /// The file at this index of `App::files`.
  Entry(usize),
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

/// Indexes of the lines drawn at `row`: both sides of a split row, the one
/// line of a unified row, none on a header. Ascending.
fn row_lines(file: &FileDiff, layout: Layout, row: usize) -> Vec<usize> {
  let Some((hunk, Some(line))) = locate(file, layout, row) else {
    return Vec::new();
  };
  match layout {
    Layout::Unified => vec![line],
    Layout::Split => {
      let (left, right) =
        split::pairs(&file.hunks[hunk])[row - row_of(file, layout, hunk, None) - 1];
      let mut lines: Vec<usize> = left.into_iter().chain(right).collect();
      lines.dedup();
      lines
    }
  }
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
    let follow = self.follow.take();
    let files = match result {
      Ok(files) => files,
      Err(e) => {
        self.load = Load::Failed(e.to_string());
        return false;
      }
    };
    self.load = Load::Ready;
    if *self.loaded == files {
      return false;
    }
    // The rows the selection covered may be gone or hold other lines now.
    self.mode = Mode::Normal;
    let kept = self.files.get(self.selected).and_then(|old| {
      files
        .iter()
        .position(|f| f.path() == old.path() && f.section == old.section)
    });
    let followed = follow.as_ref().and_then(|target| {
      files
        .iter()
        .position(|f| f.path() == target.path && f.section == Some(target.section))
    });
    let (scroll, cursor) = if followed.is_none() && kept.is_some() {
      (self.scroll, self.cursor)
    } else {
      (0, 0)
    };
    self.highlights = files.iter().map(|_| None).collect();
    self.words = files.iter().map(|_| None).collect();
    self.sides = files.iter().map(|_| None).collect();
    self.sides_requested = vec![false; files.len()];
    self.expansions = vec![0; files.len()];
    self.context = files.iter().map(|f| vec![0; f.hunks.len()]).collect();
    self.owners = files.iter().map(|f| (0..f.hunks.len()).collect()).collect();
    self.rehighlight = None;
    self.pending_widen = None;
    let index = followed
      .or(kept)
      .unwrap_or(self.selected.min(files.len().saturating_sub(1)));
    let hunk = followed.and(follow.and_then(|target| target.hunk));
    self.loaded = Arc::new(files);
    self.files = self.loaded.clone();
    self.select(index);
    self.scroll = scroll;
    self.cursor = cursor;
    if let Some(hunk) = hunk {
      let file = &self.files[index];
      if let Some(last) = file.hunks.len().checked_sub(1) {
        self.cursor = row_of(file, self.layout, hunk.min(last), None);
      }
    }
    self.clamp_scroll();
    true
  }

  pub fn on_highlighted(&mut self, index: usize, highlight: FileHighlight) {
    self.on_rehighlighted(index, 0, highlight);
  }

  /// Takes the colors of file `index` as it was after `generation`
  /// context changes. Colors for any other count would not fit its lines.
  pub fn on_rehighlighted(&mut self, index: usize, generation: u32, highlight: FileHighlight) {
    if self.expansions.get(index) == Some(&generation)
      && let Some(slot) = self.highlights.get_mut(index)
    {
      *slot = Some(highlight);
    }
  }

  /// Takes the file whose context changed and that needs new colors.
  pub fn take_rehighlight(&mut self) -> Option<Rehighlight> {
    self.rehighlight.take()
  }

  /// Takes the file whose full contents a widening is waiting for. Each
  /// file is handed out once, until a reload replaces it.
  pub fn take_sides_request(&mut self) -> Option<usize> {
    let (index, _) = self.pending_widen.as_ref()?;
    let index = *index;
    let requested = self.sides_requested.get_mut(index)?;
    (!std::mem::replace(requested, true)).then_some(index)
  }

  /// Takes the full contents of a file and finishes the widening waiting for
  /// them.
  pub fn on_sides(&mut self, index: usize, sides: Arc<Sides>) {
    let Some(slot) = self.sides.get_mut(index) else {
      return;
    };
    if sides.new.is_none() {
      // Leave the slot empty so the next key press tries the read again.
      self.sides_requested[index] = false;
      self.pending_widen = None;
      self.notice = Some(format!(
        "Cannot read {}",
        self.files[index].path().display()
      ));
      return;
    }
    *slot = Some(sides.clone());
    if let Some((wanted, hunks)) = self.pending_widen.take() {
      if wanted == index {
        self.set_context(index, hunks, Step::More);
      } else {
        self.pending_widen = Some((wanted, hunks));
      }
    }
  }

  /// Takes the stage or unstage the last key press asked for.
  pub fn take_index_request(&mut self) -> Option<IndexRequest> {
    self.index_request.take()
  }

  /// Takes the result of a stage or unstage. Returns true when the changeset
  /// needs reading again.
  pub fn on_indexed(&mut self, follow: Option<Follow>, result: Result<(), vcs::Error>) -> bool {
    match result {
      Ok(()) => {
        self.follow = follow;
        true
      }
      Err(e) => {
        self.notice = Some(e.to_string());
        false
      }
    }
  }

  /// Asks to stage the selected file, or to unstage it when it is in the
  /// staged section.
  fn toggle_stage(&mut self) {
    let Some(file) = self.files.get(self.selected) else {
      return;
    };
    let Some(section) = file.section else {
      self.notice = Some("read-only: revision".into());
      return;
    };
    self.index_request = Some(IndexRequest {
      file: file.clone(),
      target: Target::File,
      stage: section == Section::Unstaged,
      follow: self.neighbour(),
    });
  }

  /// Asks to stage the hunk under the cursor, or to unstage it when the
  /// selected entry is in the staged section, or only the lines `lines` of
  /// that hunk. When the whole hunk goes, the cursor moves to the next hunk of
  /// the entry, or to the next file in the section after the last hunk. When
  /// part of the hunk is left, the cursor stays on it. Returns whether a
  /// request was made.
  fn toggle_stage_hunk(&mut self, lines: Option<BTreeSet<usize>>) -> bool {
    let Some(file) = self.files.get(self.selected) else {
      return false;
    };
    let Some(section) = file.section else {
      self.notice = Some("read-only: revision".into());
      return false;
    };
    if matches!(
      file.status,
      FileStatus::Conflicted | FileStatus::Renamed | FileStatus::Copied
    ) {
      let what = match file.status {
        FileStatus::Conflicted => "a conflicted",
        _ => "a renamed or copied",
      };
      self.notice = Some(format!("Cannot stage hunks of {what} file, use f instead"));
      return false;
    }
    let Some(hunk) = self.cursor_hunk() else {
      self.notice = Some("No hunk under the cursor".into());
      return false;
    };
    let changed = file.hunks[hunk].changed();
    if let Some(lines) = &lines
      && !changed.clone().any(|i| lines.contains(&i))
    {
      self.notice = Some("no changed lines selected".into());
      return false;
    }
    let leaves = lines
      .as_ref()
      .is_none_or(|lines| changed.clone().all(|i| lines.contains(&i)));
    let in_entry = |hunk| Follow {
      path: file.path().to_path_buf(),
      section,
      hunk: Some(hunk),
    };
    // A hunk that goes leaves the entry, so the next one takes its index.
    // One that is left only in part is still there at that index.
    let follow = if !leaves || hunk + 1 < file.hunks.len() {
      Some(in_entry(hunk))
    } else {
      self
        .neighbour()
        .or_else(|| hunk.checked_sub(1).map(in_entry))
    };
    let target = match lines {
      Some(lines) => Target::Lines(hunk, lines),
      None => Target::Hunk(hunk),
    };
    self.index_request = Some(IndexRequest {
      file: file.clone(),
      target,
      stage: section == Section::Unstaged,
      follow,
    });
    true
  }

  /// Starts a selection at the cursor.
  fn enter_visual(&mut self) {
    if self.cursor_hunk().is_some() {
      self.mode = Mode::Visual;
      self.visual_anchor = self.cursor;
    }
  }

  /// The rows the selection covers, in visual mode.
  pub fn selection(&self) -> Option<RangeInclusive<usize>> {
    (self.mode == Mode::Visual)
      .then(|| self.visual_anchor.min(self.cursor)..=self.visual_anchor.max(self.cursor))
  }

  /// The hunk the selection is in and the indexes of the lines it covers.
  /// The header row holds no line. In the split layout a row covers the lines
  /// on both of its sides. The selection never leaves the hunk it started in.
  pub fn selection_lines(&self) -> Option<(usize, BTreeSet<usize>)> {
    let rows = self.selection()?;
    let file = self.files.get(self.selected)?;
    let (hunk, _) = locate(file, self.layout, *rows.start())?;
    let lines = rows
      .flat_map(|row| row_lines(file, self.layout, row))
      .collect();
    Some((hunk, lines))
  }

  /// The mode for the footer, with the size of the selection in visual mode.
  pub fn mode_label(&self) -> String {
    match self.mode {
      Mode::Normal => self.mode.label().to_string(),
      Mode::Visual => {
        let n = self.selected_line_count();
        let noun = if n == 1 { "line" } else { "lines" };
        format!("{} {n} {noun}", self.mode.label())
      }
    }
  }

  /// Rows of the selection that are lines of the diff.
  fn selected_line_count(&self) -> usize {
    self.selection_lines().map_or(0, |(_, lines)| lines.len())
  }

  /// Stages or unstages the selected lines. With none changed it only shows
  /// a notice and stays in visual mode.
  fn stage_selection(&mut self) {
    let Some((_, lines)) = self.selection_lines() else {
      return;
    };
    if self.toggle_stage_hunk(Some(lines)) {
      self.mode = Mode::Normal;
    }
  }

  /// Moves the cursor in visual mode, keeping it in the hunk the selection
  /// started in.
  fn extend_selection(&mut self, delta: isize) {
    let Some(file) = self.files.get(self.selected) else {
      return;
    };
    let Some((hunk, _)) = locate(file, self.layout, self.visual_anchor) else {
      return;
    };
    let first = row_of(file, self.layout, hunk, None) + 1;
    let last = first - 1 + hunk_rows(&file.hunks[hunk], self.layout) - 1;
    self.cursor = self.cursor.saturating_add_signed(delta).clamp(first, last);
    self.clamp_scroll();
  }

  fn on_visual_key(&mut self, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
      KeyCode::Char('q') => self.quit = true,
      KeyCode::Char('c') if ctrl => self.quit = true,
      KeyCode::Esc | KeyCode::Char('v') => self.mode = Mode::Normal,
      KeyCode::Char('j') | KeyCode::Down => self.extend_selection(1),
      KeyCode::Char('k') | KeyCode::Up => self.extend_selection(-1),
      KeyCode::Char('s') => self.toggle_layout(),
      KeyCode::Char('a') => self.stage_selection(),
      KeyCode::Char('?') => self.help = true,
      _ => {}
    }
  }

  /// The entry after the selected one in its section, or the one before it
  /// when the selected entry is the last.
  fn neighbour(&self) -> Option<Follow> {
    let section = self.files.get(self.selected)?.section?;
    let in_section = |i: &usize| self.files[*i].section == Some(section);
    let next = (self.selected + 1..self.files.len()).find(in_section);
    let previous = (0..self.selected).rev().find(in_section);
    let at = next.or(previous)?;
    Some(Follow {
      path: self.files[at].path().to_path_buf(),
      section,
      hunk: None,
    })
  }

  pub fn on_key(&mut self, key: KeyEvent) {
    if key.kind != KeyEventKind::Press {
      return;
    }
    self.notice = None;
    if self.help {
      match key.code {
        KeyCode::Esc | KeyCode::Char('q' | '?') => self.help = false,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,
        _ => {}
      }
      return;
    }
    if self.mode == Mode::Visual {
      return self.on_visual_key(key);
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let page = self.viewport.max(1);
    match key.code {
      KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
      KeyCode::Char('c') if ctrl => self.quit = true,
      KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
      KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
      KeyCode::Char('d') if ctrl => self.scroll_by(page as isize / 2),
      KeyCode::Char('u') if ctrl => self.scroll_by(-(page as isize / 2)),
      KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_by(page as isize),
      KeyCode::PageUp => self.scroll_by(-(page as isize)),
      KeyCode::Char('g') | KeyCode::Home => {
        self.scroll = 0;
        self.cursor = 0;
        self.clamp_scroll();
      }
      KeyCode::Char('G') | KeyCode::End => {
        self.scroll = self.max_scroll();
        self.cursor = self.rows().saturating_sub(1);
      }
      KeyCode::Char('s') => self.toggle_layout(),
      KeyCode::Char('+') => self.change_context(Step::More),
      KeyCode::Char('-') => self.change_context(Step::Less),
      KeyCode::Char('=') => self.change_context(Step::Reset),
      KeyCode::Char('f') => self.toggle_stage(),
      KeyCode::Char('a') => {
        self.toggle_stage_hunk(None);
      }
      KeyCode::Char('v') => self.enter_visual(),
      KeyCode::Char('e') => self.request_edit(),
      KeyCode::Tab | KeyCode::Char('n' | '}') => self.select_by(1),
      KeyCode::BackTab | KeyCode::Char('p' | '{') => self.select_by(-1),
      KeyCode::Char(']') => self.jump_hunk(1),
      KeyCode::Char('[') => self.jump_hunk(-1),
      KeyCode::Char('?') => self.help = true,
      _ => {}
    }
  }

  /// Rows in the selected file's diff.
  fn rows(&self) -> usize {
    self
      .files
      .get(self.selected)
      .map_or(0, |f| row_count(f, self.layout))
  }

  pub fn max_scroll(&self) -> usize {
    self.rows().saturating_sub(self.viewport)
  }

  /// Keeps the scroll position and the cursor inside the diff, and the
  /// cursor inside the viewport.
  pub fn clamp_scroll(&mut self) {
    self.scroll = self.scroll.min(self.max_scroll());
    self.cursor = self.cursor.min(self.rows().saturating_sub(1));
    self.snap_cursor(false);
    if self.cursor < self.scroll {
      self.scroll = self.cursor;
    } else if self.viewport > 0 && self.cursor >= self.scroll + self.viewport {
      self.scroll = self.cursor + 1 - self.viewport;
    }
  }

  /// Moves the cursor off a hunk header: forward onto the hunk's first line,
  /// or when `backward` onto the last line of the hunk before it, if any.
  fn snap_cursor(&mut self, backward: bool) {
    let Some(file) = self.files.get(self.selected) else {
      return;
    };
    let Some((hunk, None)) = locate(file, self.layout, self.cursor) else {
      return;
    };
    self.cursor = if backward && hunk > 0 {
      self.cursor - 1
    } else {
      self.cursor + 1
    };
  }

  fn move_cursor(&mut self, delta: isize) {
    self.cursor = self.cursor.saturating_add_signed(delta);
    self.cursor = self.cursor.min(self.rows().saturating_sub(1));
    self.snap_cursor(delta < 0);
    // Moving up onto a hunk's first line keeps its header in view.
    if delta < 0 && self.scroll >= self.cursor {
      let on_first_line = self.files.get(self.selected).is_some_and(|file| {
        locate(file, self.layout, self.cursor)
          .is_some_and(|(hunk, _)| row_of(file, self.layout, hunk, None) + 1 == self.cursor)
      });
      if on_first_line {
        self.scroll = self.cursor - 1;
      }
    }
    self.clamp_scroll();
  }

  /// Moves the cursor to the first line of the next hunk, or with a negative
  /// `delta` to the first line of the hunk under the cursor, or of the one
  /// before it when the cursor is already there. Stays put past either end.
  /// The hunk's header goes to the top of the pane, or as near as the diff's
  /// end allows.
  fn jump_hunk(&mut self, delta: isize) {
    let Some(file) = self.files.get(self.selected) else {
      return;
    };
    let Some((hunk, _)) = locate(file, self.layout, self.cursor) else {
      return;
    };
    let header = row_of(file, self.layout, hunk, None);
    let target = if delta > 0 {
      hunk + 1
    } else if self.cursor > header + 1 {
      hunk
    } else if let Some(previous) = hunk.checked_sub(1) {
      previous
    } else {
      return;
    };
    if target >= file.hunks.len() {
      return;
    }
    self.scroll = row_of(file, self.layout, target, None);
    self.cursor = self.scroll + 1;
    self.clamp_scroll();
  }

  /// Asks the run loop to open the cursor's file at the cursor's line. A
  /// removed line has no line in the file, so it opens at the next line that
  /// has one in the hunk, or the previous one when none follows.
  fn request_edit(&mut self) {
    let Some(file) = self.files.get(self.selected) else {
      return;
    };
    if file.status == FileStatus::Deleted {
      self.notice = Some("Cannot edit a deleted file".into());
      return;
    }
    let Some((hunk, Some(index))) = locate(file, self.layout, self.cursor) else {
      return;
    };
    let lines = &file.hunks[hunk].lines;
    let new_no = |l: &crate::diff::Line| l.new_no;
    let line = lines[index..]
      .iter()
      .find_map(new_no)
      .or_else(|| lines[..index].iter().rev().find_map(new_no));
    let Some(line) = line else {
      self.notice = Some("No line of the file to open".into());
      return;
    };
    self.edit = Some(EditRequest {
      path: file.path().to_path_buf(),
      line,
    });
  }

  pub fn take_edit(&mut self) -> Option<EditRequest> {
    self.edit.take()
  }

  /// The hunk under the cursor, as an index into the selected file's hunks.
  pub fn cursor_hunk(&self) -> Option<usize> {
    let file = self.files.get(self.selected)?;
    locate(file, self.layout, self.cursor).map(|(hunk, _)| hunk)
  }

  /// Switches between unified and split, keeping the hunk and line at the top
  /// of the pane in view.
  fn toggle_layout(&mut self) {
    let from = self.layout;
    let to = match from {
      Layout::Unified => Layout::Split,
      Layout::Split => Layout::Unified,
    };
    if let Some(file) = self.files.get(self.selected) {
      if let Some((hunk, line)) = locate(file, from, self.scroll) {
        self.scroll = row_of(file, to, hunk, line);
      }
      if self.mode == Mode::Visual {
        // The selection spans the rows that show its lines, so it keeps the
        // lines it had.
        if let Some((hunk, lines)) = self.selection_lines() {
          let rows = |line| row_of(file, to, hunk, Some(line));
          let mut shown = lines.iter().map(|&line| rows(line));
          let Some(first) = shown.next() else {
            return;
          };
          let (low, high) = shown.fold((first, first), |(lo, hi), row| (lo.min(row), hi.max(row)));
          (self.visual_anchor, self.cursor) = if self.cursor >= self.visual_anchor {
            (low, high)
          } else {
            (high, low)
          };
        }
      } else if let Some((hunk, line)) = locate(file, from, self.cursor) {
        self.cursor = row_of(file, to, hunk, line);
      }
    }
    self.layout = to;
    self.clamp_scroll();
  }

  /// Changes the context of the hunk under the cursor, and of the hunks
  /// merged into it. Adding context needs the file's contents, which are asked
  /// for first.
  fn change_context(&mut self, step: Step) {
    let Some(file) = self.files.get(self.selected) else {
      return;
    };
    let Some((hunk, _)) = locate(file, self.layout, self.cursor) else {
      return;
    };
    let owners = &self.owners[self.selected];
    let end = owners
      .get(hunk + 1)
      .copied()
      .unwrap_or(self.loaded[self.selected].hunks.len());
    let hunks = owners[hunk]..end;
    if step == Step::More && self.sides[self.selected].is_none() {
      self.pending_widen = Some((self.selected, hunks));
    } else {
      self.set_context(self.selected, hunks, step);
    }
  }

  /// Gives hunks `hunks` of file `index` more or less context, or none.
  /// Says so when that changes nothing.
  fn set_context(&mut self, index: usize, hunks: Range<usize>, step: Step) {
    let before = &self.context[index];
    let mut context = before.clone();
    for extra in &mut context[hunks.clone()] {
      *extra = match step {
        Step::More => *extra + CONTEXT_STEP,
        Step::Less => extra.saturating_sub(CONTEXT_STEP),
        Step::Reset => 0,
      };
    }
    if context == *before {
      self.notice = Some("Already at the default context".into());
      return;
    }
    let loaded = &self.loaded[index];
    let (file, owners) = if context.iter().all(|&extra| extra == 0) {
      (loaded.clone(), (0..loaded.hunks.len()).collect())
    } else {
      let Some(text) = self.sides[index].as_ref().and_then(|s| s.new.as_deref()) else {
        return;
      };
      loaded.widened(&context, text)
    };
    let lines = |f: &FileDiff| f.hunks.iter().map(|h| h.lines.len()).sum::<usize>();
    if step == Step::More && lines(&file) == lines(&self.files[index]) {
      self.notice = Some("No more unchanged lines to show here".into());
      return;
    }
    let anchor = self.cursor_line_numbers();
    let shown = owners
      .iter()
      .rposition(|&first| first <= hunks.start)
      .unwrap_or(0);
    Arc::make_mut(&mut self.files)[index] = file;
    self.context[index] = context;
    self.owners[index] = owners;
    self.expansions[index] += 1;
    self.words[index] = None;
    self.highlights[index] = None;
    if let Some(sides) = self.sides[index].clone() {
      self.rehighlight = Some(Rehighlight {
        index,
        generation: self.expansions[index],
        file: self.files[index].clone(),
        sides,
      });
    }
    self.scroll = self.scroll.min(self.max_scroll());
    match anchor.and_then(|numbers| self.row_with_numbers(numbers)) {
      Some(row) => self.cursor = row,
      None => {
        // The line under the cursor is hidden now. Go to the hunk that held it.
        let file = &self.files[index];
        let first = file.hunks[shown].changed().next();
        self.cursor = row_of(file, self.layout, shown, first);
      }
    }
    self.clamp_scroll();
  }

  /// Old and new line numbers of the line under the cursor, if it is on a
  /// line rather than a header.
  fn cursor_line_numbers(&self) -> Option<(Option<u32>, Option<u32>)> {
    let file = self.files.get(self.selected)?;
    let (hunk, line) = locate(file, self.layout, self.cursor)?;
    let l = &file.hunks[hunk].lines[line?];
    Some((l.old_no, l.new_no))
  }

  /// The row showing the line with these numbers.
  fn row_with_numbers(&self, numbers: (Option<u32>, Option<u32>)) -> Option<usize> {
    let file = self.files.get(self.selected)?;
    file.hunks.iter().enumerate().find_map(|(hi, h)| {
      let li = h
        .lines
        .iter()
        .position(|l| (l.old_no, l.new_no) == numbers)?;
      Some(row_of(file, self.layout, hi, Some(li)))
    })
  }

  /// Moves the cursor into the viewport without scrolling.
  fn clamp_cursor_to_view(&mut self) {
    if self.viewport > 0 {
      self.cursor = self
        .cursor
        .clamp(self.scroll, self.scroll + self.viewport - 1);
    }
    self.cursor = self.cursor.min(self.rows().saturating_sub(1));
  }

  fn scroll_by(&mut self, delta: isize) {
    self.scroll = self
      .scroll
      .saturating_add_signed(delta)
      .min(self.max_scroll());
    self.clamp_cursor_to_view();
    self.clamp_scroll();
  }

  /// The file list as drawn: a header before each section's entries, none
  /// when the entries have no section.
  pub fn list_rows(&self) -> Vec<ListRow> {
    let mut rows = Vec::new();
    let mut current = None;
    for (index, file) in self.files.iter().enumerate() {
      if let Some(section) = file.section
        && current != Some(section)
      {
        current = Some(section);
        let count = self
          .files
          .iter()
          .filter(|f| f.section == Some(section))
          .count();
        rows.push(ListRow::Header(section, count));
      }
      rows.push(ListRow::Entry(index));
    }
    rows
  }

  fn select(&mut self, index: usize) {
    self.selected = index;
    self.scroll = 0;
    self.cursor = 0;
    let row = self
      .list_rows()
      .iter()
      .position(|r| *r == ListRow::Entry(index));
    self.list_state.select(row);
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
    let _ = tx.send(Event::Loaded(id, vcs.changeset(&request)));
  });
}

/// Writes the index off the UI thread and reports the result over the
/// channel.
fn spawn_index(
  vcs: Arc<impl Vcs + Send + Sync + 'static>,
  request: IndexRequest,
  tx: Sender<Event>,
) {
  thread::spawn(move || {
    let result = request.run(&*vcs);
    let _ = tx.send(Event::Indexed(request.follow, result));
  });
}

/// Reads both versions of a file off the UI thread for an expansion.
fn spawn_sides(
  vcs: Arc<impl Vcs + Send + Sync + 'static>,
  request: DiffRequest,
  files: Arc<Vec<FileDiff>>,
  index: usize,
  id: u64,
  tx: Sender<Event>,
) {
  thread::spawn(move || {
    let sides = vcs.sides(&request, &files[index]);
    let _ = tx.send(Event::Sides(id, index, Arc::new(sides)));
  });
}

/// Lets the run loop take the keyboard from the input thread while an editor
/// uses the terminal.
#[derive(Default)]
struct InputGate {
  paused: AtomicBool,
  /// Held by the input thread while it polls and reads.
  reading: Mutex<()>,
}

impl InputGate {
  /// Blocks until the input thread is out of its read, then keeps it out
  /// until the guard drops.
  fn pause(&self) -> PauseGuard<'_> {
    self.paused.store(true, Ordering::SeqCst);
    let held = self.reading.lock().unwrap_or_else(|e| e.into_inner());
    PauseGuard {
      gate: self,
      _held: held,
    }
  }
}

struct PauseGuard<'a> {
  gate: &'a InputGate,
  _held: MutexGuard<'a, ()>,
}

impl Drop for PauseGuard<'_> {
  fn drop(&mut self) {
    self.gate.paused.store(false, Ordering::SeqCst);
  }
}

fn spawn_input(tx: Sender<Event>, gate: Arc<InputGate>) {
  thread::spawn(move || {
    loop {
      if gate.paused.load(Ordering::SeqCst) {
        thread::sleep(INPUT_POLL);
        continue;
      }
      let _reading = gate.reading.lock().unwrap_or_else(|e| e.into_inner());
      match event::poll(INPUT_POLL) {
        Ok(true) => {}
        Ok(false) => continue,
        Err(e) => {
          let _ = tx.send(Event::InputFailed(e));
          break;
        }
      }
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

/// Opens `request` in `editor`. A terminal editor gets the screen until it
/// exits and the call returns true so the caller reloads. An editor with its
/// own window is started and left running.
fn open_editor(
  terminal: &mut DefaultTerminal,
  gate: &InputGate,
  editor: &Editor,
  root: &Path,
  request: &EditRequest,
) -> io::Result<Result<bool, String>> {
  let mut command = editor.command(&root.join(&request.path), request.line);
  let failed = |e: io::Error| format!("Cannot run {}: {e}", editor.program());
  if editor.is_gui() {
    let child = command
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .spawn();
    return Ok(match child {
      Ok(mut child) => {
        thread::spawn(move || {
          let _ = child.wait();
        });
        Ok(false)
      }
      Err(e) => Err(failed(e)),
    });
  }
  let _paused = gate.pause();
  ratatui::try_restore()?;
  let status = command.status();
  *terminal = ratatui::try_init()?;
  Ok(status.map(|_| true).map_err(failed))
}

/// A `theme` in `settings` turns syntax highlighting on; `None` leaves diffs
/// uncolored.
pub fn run<V: Vcs + Send + Sync + 'static>(
  terminal: &mut DefaultTerminal,
  vcs: V,
  request: DiffRequest,
  settings: Settings,
  root: &Path,
) -> io::Result<()> {
  let vcs = Arc::new(vcs);
  let (tx, rx) = mpsc::channel();
  let gate = Arc::new(InputGate::default());
  spawn_input(tx.clone(), gate.clone());
  let editor = Editor::from_env();
  spawn_load(vcs.clone(), request.clone(), 0, tx.clone());
  let _watch = if settings.watch {
    let tx = tx.clone();
    Some(Watch::start(root, move || tx.send(Event::Changed).is_ok()).map_err(io::Error::other)?)
  } else {
    None
  };
  // `latest` numbers the newest load asked for, `applied` the one on screen.
  let (mut latest, mut applied) = (0, 0);

  let mut app = App {
    layout: settings.layout,
    palette: settings.palette,
    border: settings.border,
    ..App::default()
  };
  let highlighter = settings
    .theme
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
      Ok(Event::Indexed(follow, result)) => {
        if app.on_indexed(follow, result) {
          latest += 1;
          spawn_load(vcs.clone(), request.clone(), latest, tx.clone());
        }
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
                app.loaded.clone(),
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
      Ok(Event::Sides(id, index, sides)) if id == applied => app.on_sides(index, sides),
      Ok(Event::Sides(..)) => {}
      Ok(Event::Rehighlighted(id, index, generation, highlight)) if id == applied => {
        app.on_rehighlighted(index, generation, highlight)
      }
      Ok(Event::Rehighlighted(..)) => {}
      Ok(Event::InputFailed(e)) => return Err(e),
      Err(_) => break,
    }
    if let Some(edit) = app.take_edit() {
      match &editor {
        None => app.notice = Some("Set $VISUAL or $EDITOR to open files".into()),
        Some(editor) => match open_editor(terminal, &gate, editor, root, &edit)? {
          Ok(true) => {
            latest += 1;
            spawn_load(vcs.clone(), request.clone(), latest, tx.clone());
          }
          Ok(false) => {}
          Err(message) => app.notice = Some(message),
        },
      }
    }
    if let Some(r) = app.take_rehighlight()
      && let Some(highlighter) = highlighter.clone()
    {
      let (tx, id) = (tx.clone(), applied);
      thread::spawn(move || {
        let highlight = highlighter.diff(&r.file, &r.sides);
        let _ = tx.send(Event::Rehighlighted(id, r.index, r.generation, highlight));
      });
    }
    if let Some(index_request) = app.take_index_request() {
      spawn_index(vcs.clone(), index_request, tx.clone());
    }
    if let Some(index) = app.take_sides_request() {
      spawn_sides(
        vcs.clone(),
        request.clone(),
        app.loaded.clone(),
        index,
        applied,
        tx.clone(),
      );
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
  use crate::diff::{FileStatus, Hunk, Line, LineKind, parse};
  use crate::vcs::Sides;
  use std::path::{Path, PathBuf};

  fn file(name: &str, lines: usize) -> FileDiff {
    FileDiff {
      old_path: Some(PathBuf::from(name)),
      new_path: Some(PathBuf::from(name)),
      status: FileStatus::Modified,
      binary: false,
      section: None,
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

  const EDIT_DIFF: &str = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,4 +1,2 @@\n a\n-b\n-c\n d\n@@ -9,2 +7,3 @@\n h\n+I\n i\n";

  fn edit_after(keys: &[char]) -> (Option<EditRequest>, Option<String>) {
    let mut app = ready(parse(EDIT_DIFF.as_bytes()).unwrap(), 20);
    for &c in keys {
      app.on_key(key(KeyCode::Char(c)));
    }
    app.on_key(key(KeyCode::Char('e')));
    (app.take_edit(), app.notice.clone())
  }

  #[test]
  fn e_asks_to_open_the_file_at_the_cursor_line() {
    let (edit, _) = edit_after(&[]);
    assert_eq!(
      edit,
      Some(EditRequest {
        path: PathBuf::from("f"),
        line: 1
      })
    );
    assert_eq!(edit_after(&['j'; 5]).0.unwrap().line, 8);
  }

  #[test]
  fn e_on_a_removed_line_opens_the_next_line_of_the_file() {
    assert_eq!(edit_after(&['j']).0.unwrap().line, 2);
    assert_eq!(edit_after(&['j', 'j']).0.unwrap().line, 2);
  }

  #[test]
  fn e_on_a_removed_line_with_none_after_it_opens_the_one_before() {
    let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,1 @@\n a\n-b\n-c\n";
    let mut app = ready(parse(diff.as_bytes()).unwrap(), 20);
    app.on_key(key(KeyCode::Char('G')));
    app.on_key(key(KeyCode::Char('e')));
    assert_eq!(app.take_edit().unwrap().line, 1);
  }

  #[test]
  fn e_on_a_deleted_file_only_shows_a_notice() {
    let diff =
      "diff --git a/f b/f\ndeleted file mode 100644\n--- a/f\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-a\n";
    let mut app = ready(parse(diff.as_bytes()).unwrap(), 20);
    app.on_key(key(KeyCode::Char('e')));
    assert_eq!(app.take_edit(), None);
    assert_eq!(app.notice.as_deref(), Some("Cannot edit a deleted file"));
  }

  #[test]
  fn e_in_visual_mode_does_nothing() {
    let (edit, _) = edit_after(&['v']);
    assert_eq!(edit, None);
  }

  #[test]
  fn brackets_jump_between_hunks_onto_their_first_line() {
    let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -8,2 +8,2 @@\n h\n-i\n+I\n";
    let mut app = ready(parse(diff.as_bytes()).unwrap(), 20);
    app.on_key(key(KeyCode::Char(']')));
    assert_eq!(app.cursor_hunk(), Some(1));
    assert_eq!(app.cursor, 6);
    app.on_key(key(KeyCode::Char(']')));
    assert_eq!(app.cursor, 6);
    app.on_key(key(KeyCode::Char('j')));
    app.on_key(key(KeyCode::Char('[')));
    assert_eq!(app.cursor, 6);
    app.on_key(key(KeyCode::Char('[')));
    assert_eq!(app.cursor, 1);
    app.on_key(key(KeyCode::Char('[')));
    assert_eq!(app.cursor, 1);
  }

  #[test]
  fn a_hunk_jump_puts_the_header_at_the_top_of_the_pane() {
    let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -8,2 +8,2 @@\n h\n-i\n+I\n@@ -20,2 +20,2 @@\n x\n-y\n+Y\n";
    let mut app = ready(parse(diff.as_bytes()).unwrap(), 4);
    app.on_key(key(KeyCode::Char(']')));
    assert_eq!((app.cursor, app.scroll), (6, 5));
    app.on_key(key(KeyCode::Char(']')));
    assert_eq!(app.cursor, 10);
    assert_eq!(app.scroll, app.max_scroll());
    app.on_key(key(KeyCode::Char('[')));
    assert_eq!((app.cursor, app.scroll), (6, 5));
  }

  #[test]
  fn brackets_do_nothing_in_visual_mode_or_without_hunks() {
    let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -8,2 +8,2 @@\n h\n-i\n+I\n";
    let mut app = ready(parse(diff.as_bytes()).unwrap(), 20);
    app.on_key(key(KeyCode::Char('j')));
    app.on_key(key(KeyCode::Char('v')));
    app.on_key(key(KeyCode::Char(']')));
    assert_eq!(app.cursor, 2);
    let mut empty = file("e", 1);
    empty.hunks.clear();
    let mut app = ready(vec![empty], 20);
    app.on_key(key(KeyCode::Char(']')));
    app.on_key(key(KeyCode::Char('[')));
    assert_eq!(app.cursor, 0);
  }

  #[test]
  fn braces_move_between_files_like_n_and_p() {
    let mut app = ready(vec![file("a", 3), file("b", 3)], 5);
    app.on_key(key(KeyCode::Char('}')));
    assert_eq!(app.selected, 1);
    app.on_key(key(KeyCode::Char('{')));
    assert_eq!(app.selected, 0);
  }

  #[test]
  fn question_mark_opens_help_and_escape_closes_it_without_quitting() {
    let mut app = ready(vec![file("a", 3)], 5);
    app.on_key(key(KeyCode::Char('?')));
    assert!(app.help);
    app.on_key(key(KeyCode::Char('j')));
    assert!(app.help && app.cursor == 1);
    app.on_key(key(KeyCode::Char('q')));
    assert!(!app.help && !app.quit);
    app.on_key(key(KeyCode::Char('?')));
    app.on_key(key(KeyCode::Esc));
    assert!(!app.help && !app.quit);
    app.on_key(key(KeyCode::Char('v')));
    app.on_key(key(KeyCode::Char('?')));
    assert!(app.help);
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
  fn j_and_k_move_the_cursor_and_the_viewport_follows_it() {
    // 11 rows, 4 visible. The cursor starts on row 1, below the header.
    let mut app = ready(vec![file("a", 10)], 4);
    for _ in 0..2 {
      app.on_key(key(KeyCode::Char('j')));
    }
    assert_eq!((app.cursor, app.scroll), (3, 0));
    app.on_key(key(KeyCode::Char('j')));
    assert_eq!((app.cursor, app.scroll), (4, 1));
    app.on_key(key(KeyCode::Char('k')));
    assert_eq!((app.cursor, app.scroll), (3, 1));
    for _ in 0..20 {
      app.on_key(key(KeyCode::Char('k')));
    }
    // The hunk header comes back into view with the first line.
    assert_eq!((app.cursor, app.scroll), (1, 0));
    for _ in 0..20 {
      app.on_key(key(KeyCode::Char('j')));
    }
    assert_eq!((app.cursor, app.scroll), (10, 7));
  }

  #[test]
  fn the_cursor_moves_by_row_in_the_split_layout() {
    let mut app = ready(vec![mixed_file()], 20);
    app.layout = Layout::Split;
    // Split rows: 0 header, 1 keep, 2 r1|a1, 3 r2, 4 r3, 5 header, 6 x, 7 y.
    for _ in 0..20 {
      app.on_key(key(KeyCode::Char('j')));
    }
    assert_eq!(app.cursor, 7);
  }

  #[test]
  fn the_cursor_stays_on_its_line_when_the_layout_changes() {
    let mut app = ready(vec![mixed_file()], 20);
    app.cursor = 7; // unified: line "x" of the second hunk
    app.on_key(key(KeyCode::Char('s')));
    assert_eq!(app.cursor, 6);
    app.on_key(key(KeyCode::Char('s')));
    assert_eq!(app.cursor, 7);
  }

  #[test]
  fn the_hunk_comes_from_the_cursor_row() {
    let mut app = ready(vec![mixed_file()], 20);
    for (layout, rows) in [
      (Layout::Unified, [0, 5, 6, 8]),
      (Layout::Split, [0, 4, 5, 7]),
    ] {
      app.layout = layout;
      let hunks: Vec<_> = rows
        .iter()
        .map(|&row| {
          app.cursor = row;
          app.cursor_hunk()
        })
        .collect();
      assert_eq!(hunks, [Some(0), Some(0), Some(1), Some(1)], "{layout:?}");
    }
    assert_eq!(App::default().cursor_hunk(), None);
  }

  #[test]
  fn the_cursor_starts_on_the_first_line_of_the_first_hunk() {
    let app = ready(vec![file("a", 10)], 4);
    assert_eq!((app.cursor, app.scroll), (1, 0));
  }

  #[test]
  fn j_and_k_skip_the_header_between_hunks() {
    let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -8,2 +8,2 @@\n h\n-i\n+I\n";
    let mut app = ready(parse(diff.as_bytes()).unwrap(), 20);
    for _ in 0..3 {
      app.on_key(key(KeyCode::Char('j')));
    }
    assert_eq!(app.cursor, 4);
    app.on_key(key(KeyCode::Char('j')));
    assert_eq!(app.cursor, 6);
    app.on_key(key(KeyCode::Char('k')));
    assert_eq!(app.cursor, 4);
    for _ in 0..10 {
      app.on_key(key(KeyCode::Char('k')));
    }
    assert_eq!(app.cursor, 1);
  }

  #[test]
  fn g_and_paging_never_leave_the_cursor_on_a_header() {
    let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -8,2 +8,2 @@\n h\n-i\n+I\n";
    let mut app = ready(parse(diff.as_bytes()).unwrap(), 20);
    app.on_key(key(KeyCode::Char('G')));
    app.on_key(key(KeyCode::Char('g')));
    assert_eq!((app.cursor, app.scroll), (1, 0));
    // Row 5 is the second header. Scrolling its row to the top of the pane
    // pulls the cursor into view, onto the hunk's first line.
    app.viewport = 4;
    app.on_key(key(KeyCode::PageDown));
    app.on_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert_eq!((app.scroll, app.cursor), (5, 6));
  }

  #[test]
  fn a_selection_never_takes_in_a_hunk_header() {
    let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -8,2 +8,2 @@\n h\n-i\n+I\n";
    let mut app = ready(parse(diff.as_bytes()).unwrap(), 20);
    app.on_key(key(KeyCode::Char('v')));
    for _ in 0..10 {
      app.on_key(key(KeyCode::Char('k')));
    }
    assert_eq!(app.selection(), Some(1..=1));
    for _ in 0..10 {
      app.on_key(key(KeyCode::Char('j')));
    }
    assert_eq!(app.selection(), Some(1..=4));
  }

  #[test]
  fn changing_file_puts_the_cursor_back_at_the_top() {
    let mut app = ready(vec![file("a", 10), file("b", 10)], 4);
    app.on_key(key(KeyCode::Char('j')));
    app.on_key(key(KeyCode::Tab));
    assert_eq!(app.cursor, 0);
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
    let mut app = ready(vec![mixed_file()], 2);
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
      // The cursor sits on the line below a header.
      app.cursor = if unified % 6 == 0 {
        unified + 1
      } else {
        unified
      };
      app.on_key(key(KeyCode::Char('s')));
      assert_eq!(app.layout, Layout::Split);
      assert_eq!(app.scroll, split, "unified row {unified}");
    }
    app.scroll = 6;
    app.cursor = 6;
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

  fn entry(name: &str, section: Section) -> FileDiff {
    FileDiff {
      section: Some(section),
      ..file(name, 3)
    }
  }

  fn sectioned() -> Vec<FileDiff> {
    vec![
      entry("a", Section::Unstaged),
      entry("b", Section::Unstaged),
      entry("a", Section::Staged),
    ]
  }

  #[test]
  fn list_rows_put_a_header_with_its_count_before_each_section() {
    let app = ready(sectioned(), 5);
    assert_eq!(
      app.list_rows(),
      [
        ListRow::Header(Section::Unstaged, 2),
        ListRow::Entry(0),
        ListRow::Entry(1),
        ListRow::Header(Section::Staged, 1),
        ListRow::Entry(2),
      ]
    );
    let staged_only = ready(vec![entry("a", Section::Staged)], 5);
    assert_eq!(
      staged_only.list_rows(),
      [ListRow::Header(Section::Staged, 1), ListRow::Entry(0)]
    );
    let flat = ready(vec![file("a", 1), file("b", 1)], 5);
    assert_eq!(flat.list_rows(), [ListRow::Entry(0), ListRow::Entry(1)]);
  }

  #[test]
  fn selection_moves_through_both_sections_and_skips_headers() {
    let mut app = ready(sectioned(), 5);
    // Row 0 is the unstaged header, so the first entry sits on row 1.
    assert_eq!((app.selected, app.list_state.selected()), (0, Some(1)));
    app.on_key(key(KeyCode::Tab));
    assert_eq!((app.selected, app.list_state.selected()), (1, Some(2)));
    app.on_key(key(KeyCode::Char('n')));
    assert_eq!((app.selected, app.list_state.selected()), (2, Some(4)));
    app.on_key(key(KeyCode::Char('n')));
    assert_eq!(app.selected, 2);
    app.on_key(key(KeyCode::Char('p')));
    assert_eq!((app.selected, app.list_state.selected()), (1, Some(2)));
    app.on_key(key(KeyCode::BackTab));
    app.on_key(key(KeyCode::BackTab));
    assert_eq!((app.selected, app.list_state.selected()), (0, Some(1)));
  }

  #[test]
  fn a_reload_keeps_the_same_path_in_the_same_section_selected() {
    let mut app = ready(sectioned(), 5);
    app.on_key(key(KeyCode::Char('G')));
    app.on_key(key(KeyCode::Char('n')));
    app.on_key(key(KeyCode::Char('n')));
    assert_eq!(app.selected, 2);
    // The unstaged "b" disappears; the staged "a" moves up but stays selected.
    let reloaded = vec![entry("a", Section::Unstaged), entry("a", Section::Staged)];
    assert!(app.on_loaded(Ok(reloaded)));
    assert_eq!(app.selected, 1);
    assert_eq!(app.files[1].section, Some(Section::Staged));
    assert_eq!(app.list_state.selected(), Some(3));
  }

  #[test]
  fn a_file_in_both_sections_is_two_entries_with_their_own_diffs() {
    let mut staged = entry("a", Section::Staged);
    staged.hunks.clear();
    let mut app = ready(vec![entry("a", Section::Unstaged), staged], 5);
    assert_eq!(row_count(&app.files[0], Layout::Unified), 4);
    app.on_key(key(KeyCode::Tab));
    assert_eq!(app.files[app.selected].path(), Path::new("a"));
    assert_eq!(row_count(&app.files[app.selected], Layout::Unified), 0);
  }

  #[test]
  fn a_reload_that_only_moves_a_file_between_sections_counts_as_a_change() {
    let mut app = ready(vec![entry("a", Section::Unstaged)], 5);
    assert!(app.on_loaded(Ok(vec![entry("a", Section::Staged)])));
    assert_eq!(app.files[0].section, Some(Section::Staged));
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

  /// Two hunks with four unchanged lines (`d` to `g`) between them.
  fn gapped_file() -> FileDiff {
    let diff = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -8,2 +8,2 @@\n h\n-i\n+I\n";
    parse(diff.as_bytes()).unwrap().remove(0)
  }

  fn gapped_sides() -> Arc<Sides> {
    Arc::new(Sides {
      new: Some(b"a\nB\nc\nd\ne\nf\ng\nh\nI\n".to_vec()),
      ..Sides::default()
    })
  }

  fn plus(app: &mut App) {
    app.on_key(key(KeyCode::Char('+')));
  }

  #[test]
  fn the_plus_key_asks_for_the_file_then_widens_the_hunk_under_the_cursor() {
    let mut app = ready(vec![gapped_file()], 5);
    app.scroll = 2;
    app.cursor = 2;
    plus(&mut app);
    assert_eq!(app.take_sides_request(), Some(0));
    assert_eq!(app.take_sides_request(), None);
    assert_eq!(app.files[0].hunks.len(), 2);

    app.on_sides(0, gapped_sides());
    assert_eq!(app.files[0].hunks.len(), 2);
    assert_eq!(app.files[0].hunks[0].lines.len(), 4 + 3);
    plus(&mut app);
    let hunks = &app.files[0].hunks;
    assert_eq!(hunks.len(), 1);
    assert_eq!(row_count(&app.files[0], Layout::Unified), 1 + 9 + 2);
    let numbers: Vec<_> = hunks[0].lines[4..8]
      .iter()
      .map(|l| (l.old_no, l.new_no))
      .collect();
    assert_eq!(
      numbers,
      [
        (Some(4), Some(4)),
        (Some(5), Some(5)),
        (Some(6), Some(6)),
        (Some(7), Some(7))
      ]
    );
    assert_eq!(app.scroll, 2);
  }

  #[test]
  fn widening_keeps_the_viewport_and_the_cursor_inside_it() {
    let mut app = ready(vec![gapped_file()], 5);
    app.cursor = 4;
    plus(&mut app);
    app.on_sides(0, gapped_sides());
    assert!((app.scroll..app.scroll + 5).contains(&app.cursor));
    let line = app.files[0].hunks[0].lines[app.cursor - app.scroll - 1].clone();
    assert_eq!(line.new_no, Some(3));
  }

  #[test]
  fn the_minus_key_takes_a_step_of_context_back_and_splits_merged_hunks() {
    let mut app = ready(vec![gapped_file()], 20);
    plus(&mut app);
    app.on_sides(0, gapped_sides());
    plus(&mut app);
    assert_eq!(app.files[0].hunks.len(), 1);
    app.cursor = 2;
    app.on_key(key(KeyCode::Char('-')));
    assert_eq!(app.files[0].hunks.len(), 2);
    app.on_key(key(KeyCode::Char('-')));
    assert_eq!(app.files[0], gapped_file());
    assert_eq!(app.notice, None);
    assert_eq!(app.take_sides_request(), None);
  }

  #[test]
  fn the_equals_key_returns_to_the_loaded_context() {
    let mut app = ready(vec![gapped_file()], 20);
    plus(&mut app);
    app.on_sides(0, gapped_sides());
    plus(&mut app);
    app.cursor = 2;
    app.on_key(key(KeyCode::Char('=')));
    assert_eq!(app.files[0], gapped_file());
  }

  #[test]
  fn narrowing_at_the_loaded_context_says_so() {
    let mut app = ready(vec![gapped_file()], 20);
    app.on_key(key(KeyCode::Char('-')));
    assert_eq!(
      app.notice.as_deref(),
      Some("Already at the default context")
    );
    app.on_key(key(KeyCode::Char('=')));
    assert_eq!(
      app.notice.as_deref(),
      Some("Already at the default context")
    );
    assert_eq!(app.take_sides_request(), None);
  }

  #[test]
  fn the_old_expand_key_does_nothing() {
    let mut app = ready(vec![gapped_file()], 20);
    app.on_key(key(KeyCode::Char('e')));
    assert_eq!(app.take_sides_request(), None);
  }

  #[test]
  fn an_identical_reload_keeps_the_context_and_the_file_contents() {
    let mut app = ready(vec![gapped_file()], 20);
    plus(&mut app);
    app.on_sides(0, gapped_sides());
    plus(&mut app);
    assert!(!app.on_loaded(Ok(vec![gapped_file()])));
    assert_eq!(app.files[0].hunks.len(), 1);
    plus(&mut app);
    assert_eq!(app.take_sides_request(), None);
  }

  #[test]
  fn a_changed_reload_drops_the_context_and_reads_the_file_again() {
    let mut app = ready(vec![gapped_file()], 20);
    plus(&mut app);
    app.on_sides(0, gapped_sides());
    let mut other = gapped_file();
    other.hunks.pop();
    assert!(app.on_loaded(Ok(vec![other])));
    assert_eq!(app.files[0].hunks[0].lines.len(), 4);
    plus(&mut app);
    assert_eq!(app.take_sides_request(), Some(0));
  }

  #[test]
  fn a_file_with_changed_context_is_recolored_and_stale_colors_are_dropped() {
    let mut app = ready(vec![gapped_file()], 20);
    app.on_highlighted(0, FileHighlight::default());
    plus(&mut app);
    assert!(app.take_rehighlight().is_none());
    app.on_sides(0, gapped_sides());
    assert!(app.highlights[0].is_none());
    let job = app.take_rehighlight().unwrap();
    assert_eq!((job.index, job.generation, job.file.hunks.len()), (0, 1, 2));

    app.on_highlighted(0, FileHighlight::default());
    assert!(app.highlights[0].is_none());
    app.on_rehighlighted(0, 1, FileHighlight::default());
    assert!(app.highlights[0].is_some());
  }

  #[test]
  fn an_unreadable_file_leaves_the_diff_as_it_was_says_so_and_can_be_retried() {
    let mut app = ready(vec![gapped_file()], 20);
    plus(&mut app);
    assert_eq!(app.take_sides_request(), Some(0));
    app.on_sides(0, Arc::new(Sides::default()));
    assert_eq!(app.files[0], gapped_file());
    assert_eq!(app.notice.as_deref(), Some("Cannot read f"));

    plus(&mut app);
    assert_eq!(app.notice, None);
    assert_eq!(app.take_sides_request(), Some(0));
  }

  #[test]
  fn widening_with_nothing_left_to_show_says_so_and_keeps_the_step_count() {
    let mut app = ready(vec![gapped_file()], 20);
    plus(&mut app);
    app.on_sides(0, gapped_sides());
    plus(&mut app);
    plus(&mut app);
    assert_eq!(
      app.notice.as_deref(),
      Some("No more unchanged lines to show here")
    );
    app.cursor = 2;
    app.on_key(key(KeyCode::Char('-')));
    app.on_key(key(KeyCode::Char('-')));
    assert_eq!(app.files[0], gapped_file());
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
    fn diff(&self, _: &DiffRequest) -> Result<Vec<FileDiff>, vcs::Error> {
      self.0.lock().unwrap().recv().unwrap();
      Ok(vec![file("slow", 1)])
    }

    fn sides(&self, _: &DiffRequest, _: &FileDiff) -> vcs::Sides {
      vcs::Sides::default()
    }

    fn stage(&self, _: &FileDiff) -> Result<(), vcs::Error> {
      unreachable!()
    }

    fn unstage(&self, _: &FileDiff) -> Result<(), vcs::Error> {
      unreachable!()
    }

    fn stage_hunk(&self, _: &FileDiff, _: usize) -> Result<(), vcs::Error> {
      unreachable!()
    }

    fn unstage_hunk(&self, _: &FileDiff, _: usize) -> Result<(), vcs::Error> {
      unreachable!()
    }

    fn stage_lines(&self, _: &FileDiff, _: usize, _: &BTreeSet<usize>) -> Result<(), vcs::Error> {
      unreachable!()
    }

    fn unstage_lines(&self, _: &FileDiff, _: usize, _: &BTreeSet<usize>) -> Result<(), vcs::Error> {
      unreachable!()
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
    // One release per section of the default working tree request.
    release.send(()).unwrap();
    release.send(()).unwrap();
    match rx.recv().unwrap() {
      Event::Loaded(7, Ok(files)) => assert_eq!(files.len(), 2),
      _ => panic!("expected a loaded diff"),
    }
  }

  fn file_in(name: &str, section: Option<Section>) -> FileDiff {
    FileDiff {
      section,
      ..file(name, 2)
    }
  }

  /// A real repository with `a` and `b` unstaged and `c` staged.
  fn stage_repo() -> (tempfile::TempDir, crate::vcs::Git) {
    use std::process::Command;
    let tmp = tempfile::TempDir::new().unwrap();
    let d = tmp.path().to_path_buf();
    let d = d.as_path();
    let git = |args: &[&str]| {
      let status = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(d)
        .status()
        .unwrap();
      assert!(status.success());
    };
    git(&["init", "-q", "-b", "main"]);
    for f in ["a", "b", "c"] {
      std::fs::write(d.join(f), "1\n").unwrap();
    }
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    for f in ["a", "b", "c"] {
      std::fs::write(d.join(f), "2\n").unwrap();
    }
    git(&["add", "c"]);
    let repo = crate::vcs::Git::new(d);
    (tmp, repo)
  }

  fn load(app: &mut App, git: &crate::vcs::Git) -> bool {
    app.on_loaded(git.changeset(&DiffRequest::default()))
  }

  /// Presses `f` and runs what it asked for, as the event loop does.
  fn press_minus(app: &mut App, git: &crate::vcs::Git) {
    app.on_key(key(KeyCode::Char('f')));
    let request = app.take_index_request().expect("a stage request");
    let result = request.run(git);
    assert!(app.on_indexed(request.follow, result));
    load(app, git);
  }

  fn selected_name(app: &App) -> (String, Option<Section>) {
    let f = &app.files[app.selected];
    (f.path().display().to_string(), f.section)
  }

  #[test]
  fn minus_moves_a_file_between_sections_and_selects_the_next_one() {
    let (_tmp, git) = stage_repo();
    let mut app = App::default();
    load(&mut app, &git);
    let (u, s) = (Some(Section::Unstaged), Some(Section::Staged));
    assert_eq!(selected_name(&app), ("a".into(), u));

    press_minus(&mut app, &git);
    assert_eq!(selected_name(&app), ("b".into(), u));
    let count = |section| app.files.iter().filter(|f| f.section == section).count();
    assert_eq!((count(u), count(s)), (1, 2));

    // The last entry of a section hands the selection to the one before it.
    press_minus(&mut app, &git);
    assert_eq!(app.files.iter().filter(|f| f.section == u).count(), 0);
    assert_eq!(app.files.len(), 3);
    assert_eq!(selected_name(&app).1, s);

    // Unstaging keeps the selection in the staged section.
    app.select(
      app
        .files
        .iter()
        .position(|f| f.path() == Path::new("b"))
        .unwrap(),
    );
    press_minus(&mut app, &git);
    assert_eq!(selected_name(&app).1, s);
    assert_eq!(app.files.iter().filter(|f| f.section == u).count(), 1);
  }

  #[test]
  fn a_watch_refresh_after_a_stage_keeps_the_selection() {
    let (_tmp, git) = stage_repo();
    let mut app = App::default();
    load(&mut app, &git);
    press_minus(&mut app, &git);
    let selected = selected_name(&app);
    assert!(!load(&mut app, &git));
    assert_eq!(selected_name(&app), selected);
  }

  #[test]
  fn minus_on_a_revision_changeset_only_shows_a_notice() {
    let mut app = ready(vec![file_in("a", None)], 5);
    app.on_key(key(KeyCode::Char('f')));
    assert!(app.take_index_request().is_none());
    assert_eq!(app.notice.as_deref(), Some("read-only: revision"));
  }

  #[test]
  fn a_failed_stage_shows_the_error_and_does_not_reload() {
    let mut app = ready(vec![file_in("a", Some(Section::Unstaged))], 5);
    assert!(!app.on_indexed(None, Err(vcs::Error::Git("locked".into()))));
    assert_eq!(app.notice.as_deref(), Some("locked"));
  }

  #[test]
  fn indexing_runs_off_the_calling_thread() {
    let (_tmp, git) = stage_repo();
    let mut app = App::default();
    load(&mut app, &git);
    app.on_key(key(KeyCode::Char('f')));
    let (tx, rx) = mpsc::channel();
    spawn_index(Arc::new(git), app.take_index_request().unwrap(), tx);
    assert!(matches!(
      rx.recv().unwrap(),
      Event::Indexed(Some(_), Ok(()))
    ));
  }

  /// `one` and `two` each have two unstaged hunks.
  fn hunk_repo() -> (tempfile::TempDir, crate::vcs::Git) {
    use std::process::Command;
    let tmp = tempfile::TempDir::new().unwrap();
    let d = tmp.path().to_path_buf();
    let git = |args: &[&str]| {
      let status = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(&d)
        .status()
        .unwrap();
      assert!(status.success());
    };
    let body = |top: &str, bottom: &str| {
      (1..=20)
        .map(|n| match n {
          2 => format!("{top}\n"),
          19 => format!("{bottom}\n"),
          n => format!("{n}\n"),
        })
        .collect::<String>()
    };
    git(&["init", "-q", "-b", "main"]);
    for f in ["one", "two"] {
      std::fs::write(d.join(f), body("2", "19")).unwrap();
    }
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    for f in ["one", "two"] {
      std::fs::write(d.join(f), body("TOP", "BOTTOM")).unwrap();
    }
    let repo = crate::vcs::Git::new(&d);
    (tmp, repo)
  }

  /// Presses `a` and runs what it asked for, as the event loop does.
  fn press_a(app: &mut App, git: &crate::vcs::Git) {
    app.on_key(key(KeyCode::Char('a')));
    let request = app.take_index_request().expect("a hunk request");
    assert!(matches!(request.target, Target::Hunk(_)));
    let result = request.run(git);
    assert!(app.on_indexed(request.follow, result));
    load(app, git);
  }

  #[test]
  fn a_stages_the_hunk_under_the_cursor_and_the_cursor_moves_to_the_next_hunk() {
    let (_tmp, git) = hunk_repo();
    let mut app = App::default();
    load(&mut app, &git);
    let u = Some(Section::Unstaged);
    assert_eq!(app.files[0].hunks.len(), 2);

    press_a(&mut app, &git);
    assert_eq!(selected_name(&app), ("one".into(), u));
    assert_eq!(app.files[app.selected].hunks.len(), 1);
    assert_eq!(app.cursor_hunk(), Some(0));
    assert_eq!(app.cursor, 1);
    let staged = app
      .files
      .iter()
      .find(|f| f.section == Some(Section::Staged))
      .unwrap();
    assert_eq!(staged.hunks.len(), 1);
    assert_eq!(staged.hunks[0].lines[2].content, b"TOP");
  }

  #[test]
  fn a_on_the_last_hunk_moves_to_the_next_file_in_the_section() {
    let (_tmp, git) = hunk_repo();
    let mut app = App::default();
    load(&mut app, &git);
    let u = Some(Section::Unstaged);
    press_a(&mut app, &git);
    // The one hunk left in `one` is its last.
    press_a(&mut app, &git);
    assert_eq!(selected_name(&app), ("two".into(), u));
    assert_eq!(app.cursor, 1);
  }

  #[test]
  fn a_in_the_staged_section_unstages_that_hunk_only() {
    let (_tmp, git) = hunk_repo();
    let mut app = App::default();
    load(&mut app, &git);
    press_a(&mut app, &git);
    let staged = app
      .files
      .iter()
      .position(|f| f.section == Some(Section::Staged) && f.path() == Path::new("one"))
      .unwrap();
    app.select(staged);
    press_a(&mut app, &git);
    assert!(app.files.iter().all(|f| f.section != Some(Section::Staged)));
    let one = app
      .files
      .iter()
      .find(|f| f.path() == Path::new("one"))
      .unwrap();
    assert_eq!(one.hunks.len(), 2);
  }

  #[test]
  fn a_refuses_on_a_revision_a_conflict_and_a_file_without_hunks() {
    let mut app = ready(vec![file_in("a", None)], 5);
    app.on_key(key(KeyCode::Char('a')));
    assert_eq!(app.notice.as_deref(), Some("read-only: revision"));

    let conflicted = FileDiff {
      status: FileStatus::Conflicted,
      hunks: Vec::new(),
      ..file_in("c", Some(Section::Unstaged))
    };
    let mut app = ready(vec![conflicted], 5);
    app.on_key(key(KeyCode::Char('a')));
    assert_eq!(
      app.notice.as_deref(),
      Some("Cannot stage hunks of a conflicted file, use f instead")
    );

    let binary = FileDiff {
      binary: true,
      hunks: Vec::new(),
      ..file_in("b", Some(Section::Unstaged))
    };
    let mut app = ready(vec![binary], 5);
    app.on_key(key(KeyCode::Char('a')));
    assert_eq!(app.notice.as_deref(), Some("No hunk under the cursor"));
    assert!(app.take_index_request().is_none());
  }

  /// One file `f` whose single hunk reads, by hunk line and by unified row
  /// (the header is row 0): `1` `2` `-3` `+x3` ` 4` `-5` `+x5` and context.
  fn line_repo() -> (tempfile::TempDir, crate::vcs::Git) {
    use std::process::Command;
    let tmp = tempfile::TempDir::new().unwrap();
    let d = tmp.path().to_path_buf();
    let git = |args: &[&str]| {
      let status = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(&d)
        .status()
        .unwrap();
      assert!(status.success());
    };
    let body = |three: &str, five: &str| {
      (1..=20)
        .map(|n| match n {
          3 => format!("{three}\n"),
          5 => format!("{five}\n"),
          n => format!("{n}\n"),
        })
        .collect::<String>()
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(d.join("f"), body("3", "5")).unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    std::fs::write(d.join("f"), body("x3", "x5")).unwrap();
    let repo = crate::vcs::Git::new(&d);
    (tmp, repo)
  }

  /// The file as the index holds it.
  fn index_of(dir: &Path, name: &str) -> String {
    let out = std::process::Command::new("git")
      .args(["show", &format!(":{name}")])
      .current_dir(dir)
      .output()
      .unwrap();
    String::from_utf8(out.stdout).unwrap()
  }

  /// Runs the stage or unstage the last key press asked for, as the event
  /// loop does, and reloads.
  fn run_request(app: &mut App, git: &crate::vcs::Git) {
    let request = app.take_index_request().expect("an index request");
    let result = request.run(git);
    assert!(app.on_indexed(request.follow, result));
    load(app, git);
  }

  fn press(app: &mut App, keys: &str) {
    for c in keys.chars() {
      app.on_key(key(KeyCode::Char(c)));
    }
  }

  /// Puts the cursor on `row` of the unified diff.
  fn cursor_to(app: &mut App, row: usize) {
    app.cursor = row;
    app.clamp_scroll();
  }

  #[test]
  fn v_then_a_stages_only_the_selected_lines() {
    let (tmp, git) = line_repo();
    let d = tmp.path();
    let mut app = App::default();
    load(&mut app, &git);
    cursor_to(&mut app, 3); // `-3`
    press(&mut app, "vj");
    assert_eq!(app.mode, Mode::Visual);
    press(&mut app, "a");
    assert_eq!(app.mode, Mode::Normal);
    run_request(&mut app, &git);
    let expected: String = (1..=20)
      .map(|n| {
        if n == 3 {
          "x3\n".into()
        } else {
          format!("{n}\n")
        }
      })
      .collect();
    assert_eq!(index_of(d, "f"), expected);
  }

  #[test]
  fn selections_of_other_sizes_stage_what_they_cover() {
    let numbered = |edits: &[(usize, &str)]| -> String {
      (1..=20)
        .map(|n| match edits.iter().find(|(at, _)| *at == n) {
          Some((_, text)) => format!("{text}\n"),
          None => format!("{n}\n"),
        })
        .collect()
    };
    // (first row, the keys after `v`, the index afterwards)
    let cases = [
      (3, "", numbered(&[(3, "")]).replacen("\n\n", "\n", 1)),
      (4, "", numbered(&[(3, "3\nx3")])),
      (6, "j", numbered(&[(5, "x5")])),
      // Extending upwards works as well as downwards.
      (4, "kk", numbered(&[(3, "x3")])),
    ];
    for (row, keys, expected) in cases {
      let (tmp, git) = line_repo();
      let mut app = App::default();
      load(&mut app, &git);
      cursor_to(&mut app, row);
      press(&mut app, "v");
      press(&mut app, keys);
      press(&mut app, "a");
      run_request(&mut app, &git);
      assert_eq!(
        index_of(tmp.path(), "f"),
        expected,
        "row {row} keys {keys:?}"
      );
    }
  }

  #[test]
  fn a_in_the_staged_section_unstages_only_the_selected_lines() {
    let (tmp, git) = line_repo();
    let d = tmp.path();
    std::process::Command::new("git")
      .args(["add", "f"])
      .current_dir(d)
      .status()
      .unwrap();
    let mut app = App::default();
    load(&mut app, &git);
    assert_eq!(selected_name(&app).1, Some(Section::Staged));
    cursor_to(&mut app, 6); // `-5`
    press(&mut app, "vj");
    press(&mut app, "a");
    run_request(&mut app, &git);
    let expected: String = (1..=20)
      .map(|n| match n {
        3 => "x3\n".into(),
        n => format!("{n}\n"),
      })
      .collect();
    assert_eq!(index_of(d, "f"), expected);
  }

  #[test]
  fn esc_and_v_leave_visual_mode_without_quitting() {
    let (_tmp, git) = line_repo();
    let mut app = App::default();
    load(&mut app, &git);
    press(&mut app, "v");
    app.on_key(key(KeyCode::Esc));
    assert_eq!((app.mode, app.quit), (Mode::Normal, false));
    press(&mut app, "vv");
    assert_eq!((app.mode, app.quit), (Mode::Normal, false));
    // Esc in normal mode still quits, and so does q in visual mode.
    app.on_key(key(KeyCode::Esc));
    assert!(app.quit);
    let mut app = App::default();
    load(&mut app, &git);
    press(&mut app, "vq");
    assert!(app.quit);
  }

  #[test]
  fn a_selection_without_changed_lines_shows_a_notice_and_changes_nothing() {
    let (_tmp, git) = line_repo();
    let mut app = App::default();
    load(&mut app, &git);
    // The two context lines above the first change.
    press(&mut app, "vj");
    press(&mut app, "a");
    assert_eq!(app.notice.as_deref(), Some("no changed lines selected"));
    assert!(app.take_index_request().is_none());
    assert_eq!(app.mode, Mode::Visual);
  }

  #[test]
  fn the_selection_stays_inside_its_hunk() {
    let (_tmp, git) = hunk_repo();
    let mut app = App::default();
    load(&mut app, &git);
    let first = app.files[0].hunks[0].lines.len();
    press(&mut app, "v");
    for _ in 0..first + 5 {
      press(&mut app, "j");
    }
    // The last row of the first hunk, not the header of the second.
    assert_eq!(app.cursor, first);
    assert_eq!(app.selection_lines(), Some((0, (0..first).collect())));
    for _ in 0..first + 5 {
      press(&mut app, "k");
    }
    // The first line of the hunk, not its header.
    assert_eq!(app.cursor, 1);
  }

  /// One file `f` whose single hunk has unpaired rows. By hunk line: `1` `2`
  /// `-3` `-4` `+x3` ` 5` `+new` ` 6` and context. By split row (the header
  /// is row 0): 3 is `-3|+x3`, 4 is `-4|` and 6 is `|+new`.
  fn uneven_repo() -> (tempfile::TempDir, crate::vcs::Git) {
    use std::process::Command;
    let tmp = tempfile::TempDir::new().unwrap();
    let d = tmp.path().to_path_buf();
    let git = |args: &[&str]| {
      let status = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(&d)
        .status()
        .unwrap();
      assert!(status.success());
    };
    let body = |lines: &[&str]| lines.iter().map(|l| format!("{l}\n")).collect::<String>();
    let numbers: Vec<String> = (1..=20).map(|n| n.to_string()).collect();
    let old: Vec<&str> = numbers.iter().map(String::as_str).collect();
    let mut new = old.clone();
    new.insert(5, "new");
    new.splice(2..4, ["x3"]);
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(d.join("f"), body(&old)).unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    std::fs::write(d.join("f"), body(&new)).unwrap();
    let repo = crate::vcs::Git::new(&d);
    (tmp, repo)
  }

  /// The index of `uneven_repo` with `edits` applied to its numbered lines.
  fn uneven_index(skip: &[&str], x3_before: bool, new_after_5: bool) -> String {
    let mut out: Vec<String> = Vec::new();
    for n in 1..=20 {
      let n = n.to_string();
      if n == "3" && x3_before {
        out.push("x3".into());
      }
      if !skip.contains(&n.as_str()) {
        out.push(n.clone());
      }
      if n == "5" && new_after_5 {
        out.push("new".into());
      }
    }
    out.iter().map(|l| format!("{l}\n")).collect()
  }

  fn split_cursor_to(app: &mut App, row: usize) {
    app.layout = Layout::Split;
    cursor_to(app, row);
  }

  #[test]
  fn a_split_row_selects_the_lines_on_both_sides() {
    let (_tmp, git) = uneven_repo();
    let mut app = App::default();
    load(&mut app, &git);
    split_cursor_to(&mut app, 3);
    press(&mut app, "v");
    assert_eq!(app.selection_lines(), Some((0, [2, 4].into())));
    assert_eq!(app.mode_label(), "VISUAL 2 lines");
    press(&mut app, "j");
    assert_eq!(app.selection_lines(), Some((0, [2, 3, 4].into())));
  }

  #[test]
  fn selected_split_rows_stage_both_sides_of_a_pair() {
    let (tmp, git) = uneven_repo();
    let mut app = App::default();
    load(&mut app, &git);
    split_cursor_to(&mut app, 3);
    press(&mut app, "va");
    run_request(&mut app, &git);
    // `-3` and `+x3` are staged, `-4` is not, so `x3` lands after the old 4.
    let expected = uneven_index(&["3"], false, false).replacen("4\n", "4\nx3\n", 1);
    assert_eq!(index_of(tmp.path(), "f"), expected);
  }

  #[test]
  fn a_split_row_with_only_a_removed_line_stages_the_removal() {
    let (tmp, git) = uneven_repo();
    let mut app = App::default();
    load(&mut app, &git);
    split_cursor_to(&mut app, 4);
    press(&mut app, "va");
    run_request(&mut app, &git);
    assert_eq!(
      index_of(tmp.path(), "f"),
      uneven_index(&["4"], false, false)
    );
  }

  #[test]
  fn a_split_row_with_only_an_added_line_stages_the_addition() {
    let (tmp, git) = uneven_repo();
    let mut app = App::default();
    load(&mut app, &git);
    split_cursor_to(&mut app, 6);
    press(&mut app, "va");
    run_request(&mut app, &git);
    assert_eq!(index_of(tmp.path(), "f"), uneven_index(&[], false, true));
  }

  #[test]
  fn split_rows_across_a_pair_and_an_unpaired_row_stage_all_their_lines() {
    let (tmp, git) = uneven_repo();
    let mut app = App::default();
    load(&mut app, &git);
    split_cursor_to(&mut app, 3);
    press(&mut app, "vja");
    run_request(&mut app, &git);
    assert_eq!(
      index_of(tmp.path(), "f"),
      uneven_index(&["3", "4"], true, false)
    );
  }

  #[test]
  fn unstaging_split_rows_takes_back_both_sides() {
    let (tmp, git) = uneven_repo();
    let d = tmp.path();
    std::process::Command::new("git")
      .args(["add", "f"])
      .current_dir(d)
      .status()
      .unwrap();
    let mut app = App::default();
    load(&mut app, &git);
    assert_eq!(selected_name(&app).1, Some(Section::Staged));
    split_cursor_to(&mut app, 3);
    press(&mut app, "va");
    run_request(&mut app, &git);
    // `-3` and `+x3` leave the index, the rest of the change stays.
    assert_eq!(index_of(d, "f"), uneven_index(&["4"], false, true));
  }

  #[test]
  fn toggling_the_layout_keeps_the_selected_lines() {
    let (_tmp, git) = uneven_repo();
    let mut app = App::default();
    load(&mut app, &git);
    cursor_to(&mut app, 3); // unified `-3`
    press(&mut app, "vjj"); // `-3` `-4` `+x3`
    assert_eq!(app.selection_lines(), Some((0, [2, 3, 4].into())));
    press(&mut app, "s");
    assert_eq!(app.layout, Layout::Split);
    assert_eq!((app.mode, app.selection()), (Mode::Visual, Some(3..=4)));
    assert_eq!(app.selection_lines(), Some((0, [2, 3, 4].into())));
    press(&mut app, "s");
    assert_eq!(app.layout, Layout::Unified);
    assert_eq!(app.selection(), Some(3..=5));
    assert_eq!(app.selection_lines(), Some((0, [2, 3, 4].into())));
  }

  #[test]
  fn toggling_the_layout_keeps_the_direction_of_the_selection() {
    let (_tmp, git) = uneven_repo();
    let mut app = App::default();
    load(&mut app, &git);
    split_cursor_to(&mut app, 4);
    press(&mut app, "vk");
    assert_eq!(app.selection(), Some(3..=4));
    press(&mut app, "s");
    // Cursor above the anchor stays above it: `+x3` down to `-4`.
    assert_eq!((app.cursor, app.visual_anchor), (3, 5));
  }

  #[test]
  fn a_in_visual_mode_on_a_revision_shows_the_read_only_notice() {
    let mut changed = file_in("a", None);
    changed.hunks[0].lines[0].kind = LineKind::Added;
    let mut app = ready(vec![changed], 5);
    cursor_to(&mut app, 1);
    press(&mut app, "va");
    assert_eq!(app.notice.as_deref(), Some("read-only: revision"));
    assert!(app.take_index_request().is_none());
  }

  #[test]
  fn the_cursor_stays_on_a_hunk_that_is_only_partly_staged() {
    let (_tmp, git) = line_repo();
    let mut app = App::default();
    load(&mut app, &git);
    cursor_to(&mut app, 3); // `-3`
    press(&mut app, "vja");
    run_request(&mut app, &git);
    // The pair on line 5 is left, in the same entry and hunk.
    assert_eq!(selected_name(&app), ("f".into(), Some(Section::Unstaged)));
    assert_eq!(app.cursor_hunk(), Some(0));
    assert_eq!(app.cursor, 1);
    assert_eq!(app.files[app.selected].hunks[0].changed().count(), 2);
  }

  #[test]
  fn the_cursor_moves_on_when_the_selection_takes_the_whole_hunk() {
    let (_tmp, git) = hunk_repo();
    let mut app = App::default();
    load(&mut app, &git);
    let first = app.files[0].hunks[0].clone();
    let last = first.lines.len();
    cursor_to(&mut app, 1);
    press(&mut app, "v");
    for _ in 0..last {
      press(&mut app, "j");
    }
    press(&mut app, "a");
    run_request(&mut app, &git);
    // The first hunk is gone, so the second one is under the cursor.
    assert_eq!(app.files[app.selected].hunks.len(), 1);
    assert_eq!(app.cursor_hunk(), Some(0));
    assert_eq!(app.files[app.selected].hunks[0].lines[4].content, b"BOTTOM");
  }

  #[test]
  fn the_refusals_come_before_the_check_for_changed_lines() {
    let mut app = ready(vec![file_in("a", None)], 5);
    cursor_to(&mut app, 1);
    // Context only, on a revision.
    press(&mut app, "va");
    assert_eq!(app.notice.as_deref(), Some("read-only: revision"));
  }

  #[test]
  fn unstaging_only_the_added_lines_of_a_selection() {
    let (tmp, git) = line_repo();
    let d = tmp.path();
    std::process::Command::new("git")
      .args(["add", "f"])
      .current_dir(d)
      .status()
      .unwrap();
    let mut app = App::default();
    load(&mut app, &git);
    cursor_to(&mut app, 4); // `+x3`
    press(&mut app, "va");
    run_request(&mut app, &git);
    // `x3` leaves the index and the old 3 does not come back.
    let expected: String = (1..=20)
      .filter(|&n| n != 3)
      .map(|n| {
        if n == 5 {
          "x5\n".into()
        } else {
          format!("{n}\n")
        }
      })
      .collect();
    assert_eq!(index_of(d, "f"), expected);
  }
}
