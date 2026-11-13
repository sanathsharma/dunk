//! Two-key chords: a prefix key, then one follow-up key, with a popup that
//! lists the follow-ups after a short delay.
//!
//! This file depends only on `ratatui` and `std`, so it can be copied into
//! another terminal app as it is. The app owns the table of chords, and the
//! action type `A` is its own enum.
//!
//! The interface is three calls:
//!
//! - [`Chords::feed`] takes every key press and says what to do with it.
//! - [`Chords::wake_in`] says how long the event loop may sleep before the
//!   popup is due, so the loop can redraw without waiting for a key.
//! - [`Chords::render`] draws the popup once the delay has passed.
//!
//! Time comes in as a parameter, so tests never sleep. A chord has no
//! timeout: it ends with a follow-up key, `Esc`, or any other key.
//!
//! ```ignore
//! const TABLE: &[Group<Action>] = &[Group {
//!   prefix: 'r',
//!   title: "restore",
//!   keys: &[
//!     Binding { key: 'f', label: "file", action: Action::RestoreFile },
//!     Binding { key: 'h', label: "hunk", action: Action::RestoreHunk },
//!   ],
//! }];
//! let mut chords = Chords::new(TABLE, Duration::from_millis(300));
//! match chords.feed(key, Instant::now()) {
//!   Feed::Pass => { /* handle the key as usual */ }
//!   Feed::Wait | Feed::Cancel => {}
//!   Feed::Done(action) => { /* run it */ }
//! }
//! ```

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use std::time::{Duration, Instant};

/// One follow-up key of a chord and what it does.
#[derive(Debug, Clone, Copy)]
pub struct Binding<A> {
  pub key: char,
  /// What the popup says the key does.
  pub label: &'static str,
  pub action: A,
}

/// A prefix key and the follow-ups that finish its chord.
#[derive(Debug, Clone, Copy)]
pub struct Group<A: 'static> {
  pub prefix: char,
  /// The popup's title.
  pub title: &'static str,
  pub keys: &'static [Binding<A>],
}

/// What the caller does with a key press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed<A> {
  /// Not part of a chord. Handle the key as usual.
  Pass,
  /// The prefix was pressed. Swallow the key and wait for the follow-up.
  Wait,
  /// The chord finished. Run the action.
  Done(A),
  /// The chord ended without an action. Swallow the key.
  Cancel,
}

/// How the popup looks.
#[derive(Debug, Clone, Copy)]
pub struct Styles {
  /// The box's border and title.
  pub border: Style,
  /// The key letters.
  pub key: Style,
  /// The text of the box, and what fills it.
  pub text: Style,
  pub border_type: BorderType,
}

#[derive(Debug, Clone, Copy)]
struct Pending {
  group: usize,
  since: Instant,
}

/// The chords of an app and the one that is waiting for its follow-up, if any.
#[derive(Debug, Clone)]
pub struct Chords<A: 'static> {
  groups: &'static [Group<A>],
  delay: Duration,
  pending: Option<Pending>,
}

/// The character of a key press that has no modifier but shift.
const fn plain(key: KeyEvent) -> Option<char> {
  if key
    .modifiers
    .intersects(KeyModifiers::CONTROL.union(KeyModifiers::ALT))
  {
    return None;
  }
  match key.code {
    KeyCode::Char(c) => Some(c),
    _ => None,
  }
}

impl<A: Copy> Chords<A> {
  /// `delay` is how long a pending chord waits before its popup shows.
  pub const fn new(groups: &'static [Group<A>], delay: Duration) -> Self {
    Self {
      groups,
      delay,
      pending: None,
    }
  }

  /// Takes a key press made at `now`.
  pub fn feed(&mut self, key: KeyEvent, now: Instant) -> Feed<A> {
    let Some(pending) = self.pending.take() else {
      let started = plain(key).and_then(|c| self.groups.iter().position(|g| g.prefix == c));
      return started.map_or(Feed::Pass, |group| {
        self.pending = Some(Pending { group, since: now });
        Feed::Wait
      });
    };
    plain(key)
      .and_then(|c| {
        self
          .groups
          .get(pending.group)?
          .keys
          .iter()
          .find(|k| k.key == c)
      })
      .map_or(Feed::Cancel, |k| Feed::Done(k.action))
  }

  /// How long until the popup is due, when a chord is pending and the popup
  /// is not showing yet. The event loop wakes up then and redraws.
  pub fn wake_in(&self, now: Instant) -> Option<Duration> {
    let pending = self.pending?;
    let left = self
      .delay
      .saturating_sub(now.saturating_duration_since(pending.since));
    (!left.is_zero()).then_some(left)
  }

  /// The group whose popup shows at `now`.
  fn showing(&self, now: Instant) -> Option<&Group<A>> {
    let pending = self.pending?;
    if now.saturating_duration_since(pending.since) < self.delay {
      return None;
    }
    self.groups.get(pending.group)
  }

  /// Draws the popup in the bottom-right corner of `area`, once the delay has
  /// passed. Draws nothing before that or when no chord is pending.
  pub fn render(&self, frame: &mut Frame, area: Rect, styles: &Styles, now: Instant) {
    let Some(group) = self.showing(now) else {
      return;
    };
    let title = format!(" {} ", group.title);
    let rows: Vec<Line> = group
      .keys
      .iter()
      .map(|k| {
        Line::from(vec![
          Span::raw(" "),
          Span::styled(k.key.to_string(), styles.key),
          Span::raw(format!("  {} ", k.label)),
        ])
      })
      .collect();
    let widest = rows
      .iter()
      .map(Line::width)
      .max()
      .unwrap_or(0)
      .max(title.chars().count());
    // Two columns and two rows go to the borders. The box is twice as wide
    // as its text needs, so the keys do not look cramped.
    let width = u16::try_from(widest.saturating_add(2).saturating_mul(2))
      .unwrap_or(u16::MAX)
      .min(area.width);
    let height = u16::try_from(rows.len().saturating_add(2))
      .unwrap_or(u16::MAX)
      .min(area.height);
    let popup = Rect {
      x: area.right().saturating_sub(width),
      y: area.bottom().saturating_sub(height),
      width,
      height,
    };
    let block = Block::bordered()
      .border_type(styles.border_type)
      .border_style(styles.border)
      .title_style(styles.border)
      .title(title);
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(rows).block(block).style(styles.text), popup);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use ratatui::Terminal;
  use ratatui::backend::TestBackend;

  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  enum Act {
    File,
    Hunk,
  }

  const TABLE: &[Group<Act>] = &[Group {
    prefix: 'r',
    title: "restore",
    keys: &[
      Binding {
        key: 'f',
        label: "file",
        action: Act::File,
      },
      Binding {
        key: 'h',
        label: "hunk",
        action: Act::Hunk,
      },
    ],
  }];

  const DELAY: Duration = Duration::from_millis(300);

  fn chords() -> Chords<Act> {
    Chords::new(TABLE, DELAY)
  }

  fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
  }

  fn after(start: Instant, ms: u64) -> Instant {
    start.checked_add(Duration::from_millis(ms)).unwrap()
  }

  fn drawn(chords: &Chords<Act>, now: Instant) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(30, 8)).unwrap();
    let styles = Styles {
      border: Style::new(),
      key: Style::new(),
      text: Style::new(),
      border_type: BorderType::Plain,
    };
    terminal
      .draw(|f| chords.render(f, f.area(), &styles, now))
      .unwrap();
    terminal
      .backend()
      .to_string()
      .lines()
      .map(str::to_string)
      .collect()
  }

  #[test]
  fn a_key_that_starts_no_chord_passes() {
    let mut c = chords();
    assert_eq!(c.feed(key('a'), Instant::now()), Feed::Pass);
    assert_eq!(c.feed(key('f'), Instant::now()), Feed::Pass);
  }

  #[test]
  fn the_prefix_waits_and_a_follow_up_finishes_the_chord() {
    let mut c = chords();
    let t = Instant::now();
    assert_eq!(c.feed(key('r'), t), Feed::Wait);
    assert_eq!(c.feed(key('h'), t), Feed::Done(Act::Hunk));
    assert_eq!(c.feed(key('r'), t), Feed::Wait);
    assert_eq!(c.feed(key('f'), t), Feed::Done(Act::File));
  }

  #[test]
  fn a_finished_chord_leaves_nothing_pending() {
    let mut c = chords();
    let t = Instant::now();
    c.feed(key('r'), t);
    c.feed(key('h'), t);
    assert_eq!(c.feed(key('h'), t), Feed::Pass);
    assert_eq!(c.wake_in(t), None);
  }

  #[test]
  fn esc_and_other_keys_cancel_and_are_swallowed() {
    let t = Instant::now();
    for cancelling in [
      KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
      key('x'),
      key('r'),
      KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
      KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL),
    ] {
      let mut c = chords();
      c.feed(key('r'), t);
      assert_eq!(c.feed(cancelling, t), Feed::Cancel, "{cancelling:?}");
      assert_eq!(
        c.feed(key('h'), t),
        Feed::Pass,
        "{cancelling:?} left it open"
      );
    }
  }

  #[test]
  fn a_prefix_with_control_or_alt_starts_nothing() {
    let mut c = chords();
    for modifier in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
      let event = KeyEvent::new(KeyCode::Char('r'), modifier);
      assert_eq!(c.feed(event, Instant::now()), Feed::Pass);
    }
  }

  #[test]
  fn the_loop_wakes_when_the_popup_is_due_and_not_before_or_after() {
    let mut c = chords();
    let t = Instant::now();
    assert_eq!(c.wake_in(t), None, "nothing pending");
    c.feed(key('r'), t);
    assert_eq!(c.wake_in(t), Some(DELAY));
    assert_eq!(c.wake_in(after(t, 100)), Some(Duration::from_millis(200)));
    assert_eq!(c.wake_in(after(t, 300)), None, "the popup is showing");
    assert_eq!(c.wake_in(after(t, 900)), None);
  }

  #[test]
  fn the_popup_shows_only_after_the_delay() {
    let mut c = chords();
    let t = Instant::now();
    assert!(
      drawn(&c, t)
        .iter()
        .all(|r| r.trim_matches('"').trim().is_empty())
    );
    c.feed(key('r'), t);
    let early = drawn(&c, after(t, 299));
    assert!(early.iter().all(|r| !r.contains("restore")), "{early:#?}");
    let rows = drawn(&c, after(t, 300));
    assert!(rows.iter().any(|r| r.contains("restore")), "{rows:#?}");
    assert!(rows.iter().any(|r| r.contains("f  file")), "{rows:#?}");
    assert!(rows.iter().any(|r| r.contains("h  hunk")), "{rows:#?}");
  }

  #[test]
  fn the_popup_sits_in_the_bottom_right_corner() {
    let mut c = chords();
    let t = Instant::now();
    c.feed(key('r'), t);
    let rows = drawn(&c, after(t, 300));
    let last = rows.last().unwrap();
    assert!(
      last.trim_end_matches('"').ends_with('┘') || last.contains('┘'),
      "{rows:#?}"
    );
    let top = rows.iter().position(|r| r.contains("restore")).unwrap();
    assert_eq!(top, rows.len().saturating_sub(4), "{rows:#?}");
  }

  #[test]
  fn the_popup_is_twice_as_wide_as_its_text_needs() {
    let mut c = chords();
    let t = Instant::now();
    c.feed(key('r'), t);
    let rows = drawn(&c, after(t, 300));
    let top = rows.iter().find(|r| r.contains('┌')).unwrap();
    // The widest text is the title, ` restore `, 9 columns. With 2 for the
    // borders that is 11, and the box is twice that.
    let boxed = top.trim_matches('"').trim_start();
    assert_eq!(boxed.chars().count(), 22, "{top}");
  }

  #[test]
  fn the_popup_closes_with_the_chord() {
    let mut c = chords();
    let t = Instant::now();
    c.feed(key('r'), t);
    c.feed(key('f'), t);
    let rows = drawn(&c, after(t, 900));
    assert!(rows.iter().all(|r| !r.contains("restore")), "{rows:#?}");
  }

  #[test]
  fn the_popup_fits_a_tiny_area() {
    let mut c = chords();
    let t = Instant::now();
    c.feed(key('r'), t);
    let mut terminal = Terminal::new(TestBackend::new(5, 2)).unwrap();
    let styles = Styles {
      border: Style::new(),
      key: Style::new(),
      text: Style::new(),
      border_type: BorderType::Plain,
    };
    terminal
      .draw(|f| c.render(f, f.area(), &styles, after(t, 300)))
      .unwrap();
  }
}
