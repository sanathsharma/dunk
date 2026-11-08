use crate::diff::FileDiff;
use crate::highlight::{FileHighlight, Highlighter};
use crate::vcs::{DiffRequest, Vcs};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;

struct State {
  pending: Vec<usize>,
  focus: usize,
  closed: bool,
}

struct Shared {
  state: Mutex<State>,
}

/// Handle to the highlighting thread. Dropping it stops the thread once it
/// finishes the file in hand.
pub struct Worker(Arc<Shared>);

impl Worker {
  /// Highlights `files` on a new thread with the shared `highlighter`, nearest to the focused file first,
  /// and calls `done` with each result. `done` returns false to stop.
  pub fn spawn<V, F>(
    vcs: Arc<V>,
    request: DiffRequest,
    files: Arc<Vec<FileDiff>>,
    highlighter: Arc<Highlighter>,
    focus: usize,
    done: F,
  ) -> Self
  where
    V: Vcs + Send + Sync + ?Sized + 'static,
    F: Fn(usize, FileHighlight) -> bool + Send + 'static,
  {
    let shared = Arc::new(Shared {
      state: Mutex::new(State {
        pending: (0..files.len()).collect(),
        focus,
        closed: false,
      }),
    });
    let worker = Self(shared.clone());
    thread::spawn(move || {
      while let Some(index) = shared.next() {
        let Some(file) = files.get(index) else {
          continue;
        };
        let result = if file.binary || file.hunks.is_empty() {
          FileHighlight::default()
        } else {
          let sides = vcs.sides(&request, file);
          highlighter.diff(file, &sides)
        };
        if !done(index, result) {
          break;
        }
      }
    });
    worker
  }

  /// Moves the file the worker should do next to the front of the queue.
  pub fn focus(&self, index: usize) {
    self.0.lock().focus = index;
  }
}

impl Drop for Worker {
  fn drop(&mut self) {
    self.0.lock().closed = true;
  }
}

impl Shared {
  /// Takes the pending file closest to the focus, or `None` when the queue
  /// is empty or closed.
  fn next(&self) -> Option<usize> {
    let mut state = self.lock();
    if state.closed {
      return None;
    }
    let focus = state.focus;
    let at = state
      .pending
      .iter()
      .enumerate()
      .min_by_key(|(_, file)| file.abs_diff(focus))
      .map(|(at, _)| at)?;
    Some(state.pending.remove(at))
  }

  /// The queue stays usable after a panic elsewhere, since each update to it
  /// is a single assignment or removal.
  fn lock(&self) -> MutexGuard<'_, State> {
    self.state.lock().unwrap_or_else(PoisonError::into_inner)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::diff::parse;
  use crate::vcs::Memory;
  use std::sync::mpsc;

  /// One file whose contents differ between its old and new side.
  fn fake() -> Memory {
    Memory::default().commit("f.rs", "a\n").edit("f.rs", "b\n")
  }

  fn files(n: usize) -> Arc<Vec<FileDiff>> {
    let one = "diff --git a/f.rs b/f.rs\n--- a/f.rs\n+++ b/f.rs\n@@ -1 +1 @@\n-a\n+b\n";
    Arc::new(
      (0..n)
        .flat_map(|_| parse(one.as_bytes()).unwrap())
        .collect(),
    )
  }

  #[test]
  fn picks_the_pending_file_nearest_the_focus() {
    let shared = Shared {
      state: Mutex::new(State {
        pending: (0..6).collect(),
        focus: 4,
        closed: false,
      }),
    };
    let order: Vec<_> = std::iter::from_fn(|| shared.next()).take(3).collect();
    assert_eq!(order, [4, 3, 5]);
    shared.state.lock().unwrap().focus = 0;
    assert_eq!(shared.next(), Some(0));
  }

  #[test]
  fn highlights_every_file_off_the_calling_thread() {
    let (tx, rx) = mpsc::channel();
    let _worker = Worker::spawn(
      Arc::new(fake()),
      DiffRequest::default(),
      files(3),
      Arc::new(Highlighter::new(crate::highlight::DEFAULT_THEME).unwrap()),
      2,
      move |i, h| tx.send((i, h)).is_ok(),
    );
    let mut seen: Vec<_> = (0..3).map(|_| rx.recv().unwrap()).collect();
    seen.sort_by_key(|(i, _)| *i);
    assert_eq!(seen.iter().map(|(i, _)| *i).collect::<Vec<_>>(), [0, 1, 2]);
    assert!(seen[0].1.hunks[0].iter().all(Option::is_some));
  }

  #[test]
  fn a_refresh_with_unchanged_files_highlights_nothing() {
    let highlighter = Arc::new(Highlighter::new(crate::highlight::DEFAULT_THEME).unwrap());
    let refresh = || {
      let (tx, rx) = mpsc::channel();
      let _worker = Worker::spawn(
        Arc::new(fake()),
        DiffRequest::default(),
        files(3),
        highlighter.clone(),
        0,
        move |i, h| tx.send((i, h)).is_ok(),
      );
      (0..3).for_each(|_| drop(rx.recv().unwrap()));
      3
    };
    assert_eq!(refresh(), 3);
    let after_first = highlighter.runs();
    assert_eq!(refresh(), 3);
    assert_eq!(highlighter.runs(), after_first);
  }
}
