//! Runs background jobs and drops the results of ones that a newer job has
//! replaced.

use crate::diff::FileDiff;
use crate::vcs::{self, DiffRequest, Vcs};
use std::sync::Arc;
use std::thread;

/// The finished read of a changeset, numbered by the load that made it.
#[derive(Debug)]
pub struct Loaded {
  pub id: u64,
  pub result: Result<Vec<FileDiff>, vcs::Error>,
}

/// Numbers changeset loads. A load that finishes after a newer one started is
/// stale, so a slow older read cannot replace a newer result.
#[derive(Debug, Default)]
pub struct Loader {
  latest: u64,
}

impl Loader {
  /// Reads the changeset off the calling thread and hands the result to
  /// `deliver` from the worker thread. Returns the number of the load.
  pub fn start<V: Vcs + Send + Sync + 'static>(
    &mut self,
    vcs: Arc<V>,
    request: DiffRequest,
    deliver: impl FnOnce(Loaded) + Send + 'static,
  ) -> u64 {
    self.latest = self.latest.saturating_add(1);
    let id = self.latest;
    thread::spawn(move || {
      deliver(Loaded {
        id,
        result: vcs.changeset(&request),
      });
    });
    id
  }

  /// The result of `load` when no newer load has started, otherwise `None`.
  pub fn accept(&self, load: Loaded) -> Option<(u64, Result<Vec<FileDiff>, vcs::Error>)> {
    (load.id == self.latest).then_some((load.id, load.result))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::vcs::Memory;
  use std::sync::mpsc;

  fn start(loader: &mut Loader, vcs: &Arc<Memory>) -> (u64, mpsc::Receiver<Loaded>) {
    let (tx, rx) = mpsc::channel();
    let id = loader.start(vcs.clone(), DiffRequest::default(), move |load| {
      let _ = tx.send(load);
    });
    (id, rx)
  }

  fn repo() -> Arc<Memory> {
    Arc::new(Memory::default().commit("a", "1\n").edit("a", "2\n"))
  }

  #[test]
  fn each_load_gets_the_next_number() {
    let (mut loader, vcs) = (Loader::default(), repo());
    let (first, _) = start(&mut loader, &vcs);
    let (second, _) = start(&mut loader, &vcs);
    assert_eq!((first, second), (1, 2));
  }

  #[test]
  fn a_load_runs_off_the_calling_thread_and_delivers_its_result() {
    let (mut loader, vcs) = (Loader::default(), repo());
    let (id, rx) = start(&mut loader, &vcs);
    let load = rx.recv().unwrap();
    let (accepted, result) = loader.accept(load).expect("the only load is the latest");
    assert_eq!(accepted, id);
    assert_eq!(result.unwrap().len(), 1);
  }

  #[test]
  fn an_older_load_that_finishes_last_is_discarded() {
    let (mut loader, vcs) = (Loader::default(), repo());
    let (_, older) = start(&mut loader, &vcs);
    let (newer_id, newer) = start(&mut loader, &vcs);
    let (older, newer) = (older.recv().unwrap(), newer.recv().unwrap());
    // The newer load finishes first, then the older one arrives.
    assert_eq!(loader.accept(newer).map(|(id, _)| id), Some(newer_id));
    assert!(loader.accept(older).is_none());
  }

  #[test]
  fn an_older_load_is_stale_even_before_the_newer_one_finishes() {
    let (mut loader, vcs) = (Loader::default(), repo());
    let (_, older) = start(&mut loader, &vcs);
    let _ = start(&mut loader, &vcs);
    assert!(loader.accept(older.recv().unwrap()).is_none());
  }
}
