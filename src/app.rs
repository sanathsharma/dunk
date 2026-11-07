use crate::diff::FileDiff;
use crate::ui;
use crate::vcs::{self, DiffRequest, Vcs};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::ListState;
use std::io;
use std::sync::mpsc::{self, Sender};
use std::thread;

pub enum Event {
    Key(KeyEvent),
    Resize,
    Loaded(Result<Vec<FileDiff>, vcs::Error>),
    InputFailed(io::Error),
}

pub enum Load {
    Loading,
    Ready,
    Failed(String),
}

pub struct App {
    pub load: Load,
    pub files: Vec<FileDiff>,
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
            files: Vec::new(),
            selected: 0,
            list_state: ListState::default(),
            scroll: 0,
            viewport: 0,
            quit: false,
        }
    }
}

/// Rows a file's diff occupies: one header per hunk plus its lines.
pub fn row_count(file: &FileDiff) -> usize {
    file.hunks.iter().map(|h| 1 + h.lines.len()).sum()
}

impl App {
    pub fn on_loaded(&mut self, result: Result<Vec<FileDiff>, vcs::Error>) {
        match result {
            Ok(files) => {
                self.files = files;
                self.load = Load::Ready;
                self.select(0);
            }
            Err(e) => self.load = Load::Failed(e.to_string()),
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
            KeyCode::Tab | KeyCode::Char('n') => self.select_by(1),
            KeyCode::BackTab | KeyCode::Char('p') => self.select_by(-1),
            _ => {}
        }
    }

    pub fn max_scroll(&self) -> usize {
        let rows = self.files.get(self.selected).map_or(0, row_count);
        rows.saturating_sub(self.viewport)
    }

    pub fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.min(self.max_scroll());
    }

    fn scroll_by(&mut self, delta: isize) {
        self.scroll = self.scroll.saturating_add_signed(delta);
        self.clamp_scroll();
    }

    fn select(&mut self, index: usize) {
        self.selected = index;
        self.scroll = 0;
        self.list_state
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
fn spawn_load(vcs: impl Vcs + Send + 'static, request: DiffRequest, tx: Sender<Event>) {
    thread::spawn(move || {
        let _ = tx.send(Event::Loaded(vcs.diff(&request)));
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

pub fn run(
    terminal: &mut DefaultTerminal,
    vcs: impl Vcs + Send + 'static,
    request: DiffRequest,
) -> io::Result<()> {
    let (tx, rx) = mpsc::channel();
    spawn_input(tx.clone());
    spawn_load(vcs, request, tx);

    let mut app = App::default();
    while !app.quit {
        terminal.draw(|frame| ui::render(frame, &mut app))?;
        match rx.recv() {
            Ok(Event::Key(key)) => app.on_key(key),
            Ok(Event::Resize) => {}
            Ok(Event::Loaded(result)) => app.on_loaded(result),
            Ok(Event::InputFailed(e)) => return Err(e),
            Err(_) => break,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{FileStatus, Hunk, Line, LineKind};
    use std::path::PathBuf;

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

    struct Slow(mpsc::Receiver<()>);

    impl Vcs for Slow {
        fn status(&self) -> Result<vcs::Status, vcs::Error> {
            unreachable!()
        }

        fn diff(&self, _: &DiffRequest) -> Result<Vec<FileDiff>, vcs::Error> {
            self.0.recv().unwrap();
            Ok(vec![file("slow", 1)])
        }
    }

    #[test]
    fn git_runs_off_the_calling_thread() {
        let (release, gate) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        // Returns at once even though the backend blocks until released.
        spawn_load(Slow(gate), DiffRequest::default(), tx);
        assert!(rx.try_recv().is_err());
        release.send(()).unwrap();
        match rx.recv().unwrap() {
            Event::Loaded(Ok(files)) => assert_eq!(files.len(), 1),
            _ => panic!("expected a loaded diff"),
        }
    }
}
