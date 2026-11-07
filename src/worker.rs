use crate::diff::FileDiff;
use crate::highlight::{FileHighlight, Highlighter};
use crate::vcs::{DiffRequest, Vcs};
use std::sync::{Arc, Mutex};
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
  ) -> Worker
  where
    V: Vcs + Send + Sync + 'static,
    F: Fn(usize, FileHighlight) -> bool + Send + 'static,
  {
    let shared = Arc::new(Shared {
      state: Mutex::new(State {
        pending: (0..files.len()).collect(),
        focus,
        closed: false,
      }),
    });
    let worker = Worker(shared.clone());
    thread::spawn(move || {
      while let Some(index) = shared.next() {
        let file = &files[index];
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
    self.0.state.lock().unwrap().focus = index;
  }
}

impl Drop for Worker {
  fn drop(&mut self) {
    self.0.state.lock().unwrap().closed = true;
  }
}

impl Shared {
  /// Takes the pending file closest to the focus, or `None` when the queue
  /// is empty or closed.
  fn next(&self) -> Option<usize> {
    let mut state = self.state.lock().unwrap();
    if state.closed || state.pending.is_empty() {
      return None;
    }
    let focus = state.focus;
    let at = (0..state.pending.len())
      .min_by_key(|&i| state.pending[i].abs_diff(focus))
      .expect("pending is not empty");
    Some(state.pending.remove(at))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::diff::parse;
  use crate::vcs::{Error, Sides, VersionId};
  use std::sync::mpsc;

  struct Fake;

  impl Vcs for Fake {
    fn diff(&self, _: &DiffRequest) -> Result<Vec<FileDiff>, Error> {
      unreachable!()
    }

    fn sides(&self, _: &DiffRequest, _: &FileDiff) -> Sides {
      Sides {
        old: Some(b"a\n".to_vec()),
        new: Some(b"b\n".to_vec()),
        old_id: Some(VersionId::Object("old".into())),
        new_id: Some(VersionId::Object("new".into())),
      }
    }

    fn stage(&self, _: &FileDiff) -> Result<(), Error> {
      unreachable!()
    }

    fn unstage(&self, _: &FileDiff) -> Result<(), Error> {
      unreachable!()
    }

    fn stage_hunk(&self, _: &FileDiff, _: usize) -> Result<(), Error> {
      unreachable!()
    }

    fn unstage_hunk(&self, _: &FileDiff, _: usize) -> Result<(), Error> {
      unreachable!()
    }
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
      Arc::new(Fake),
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
        Arc::new(Fake),
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
