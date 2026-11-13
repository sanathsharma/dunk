//! Runs background jobs and routes their results back, dropping the ones a
//! newer load has made stale.

use crate::args::Scope;
use crate::diff::FileDiff;
use crate::highlight::{FileHighlight, Highlighter};
use crate::restore::RestoreRequest;
use crate::staging::{Follow, IndexRequest};
use crate::vcs::{self, DiffRequest, Sides, Vcs};
use crate::worker::Worker;
use std::collections::HashSet;
use std::sync::Arc;
use std::thread;

/// The finished read of a changeset, numbered by the load that made it.
#[derive(Debug)]
pub struct Loaded {
  pub id: u64,
  pub result: Result<Vec<FileDiff>, vcs::Error>,
}

/// A file whose context changed, waiting to be colored again.
pub struct Rehighlight {
  pub index: usize,
  /// How many times the file's context changed. Colors made for an earlier
  /// count do not fit its lines.
  pub generation: u32,
  pub file: FileDiff,
  pub sides: Arc<Sides>,
}

/// The result of a job. `load` is the number of the load whose files the job
/// worked on.
#[derive(Debug)]
pub enum Done {
  Loaded(Loaded),
  /// A stage or unstage finished. Carries the entry the selection moves to
  /// once the changeset is read again.
  Indexed(Option<Follow>, Result<(), vcs::Error>),
  /// A restore or nuke finished, wholly or in part. Carries the entry the
  /// selection moves to once the changeset is read again.
  Restored(Option<Follow>, Result<(), vcs::Error>),
  /// The whole working tree changeset, read for a nuke from a narrowed view.
  Widened(Result<Vec<FileDiff>, vcs::Error>),
  Highlighted {
    load: u64,
    index: usize,
    highlight: FileHighlight,
  },
  /// The full contents of a file, read for an expansion.
  Sides {
    load: u64,
    index: usize,
    sides: Arc<Sides>,
  },
  /// The colors of a file shown with other context than it was loaded with.
  Rehighlighted {
    load: u64,
    index: usize,
    generation: u32,
    highlight: FileHighlight,
  },
}

/// Hands a result to whoever waits for it, from the worker thread. Returns
/// false once nobody does.
pub type Deliver = Arc<dyn Fn(Done) -> bool + Send + Sync>;

/// Starts background jobs and takes their results back. Loads are numbered, so
/// a load that finishes after a newer one started is stale, and so is any
/// result for the files of a load other than the one on screen.
pub struct Runner {
  vcs: Arc<dyn Vcs + Send + Sync>,
  request: DiffRequest,
  highlighter: Option<Arc<Highlighter>>,
  deliver: Deliver,
  /// The number of the newest load started.
  latest: u64,
  /// The number of the load on screen.
  applied: u64,
  /// Files of the load on screen whose contents were already asked for.
  sides_asked: HashSet<usize>,
  worker: Option<Worker>,
}

impl Runner {
  pub fn new(
    vcs: Arc<dyn Vcs + Send + Sync>,
    request: DiffRequest,
    highlighter: Option<Arc<Highlighter>>,
    deliver: Deliver,
  ) -> Self {
    Self {
      vcs,
      request,
      highlighter,
      deliver,
      latest: 0,
      applied: 0,
      sides_asked: HashSet::new(),
      worker: None,
    }
  }

  /// Reads the changeset off the calling thread. Returns the number of the
  /// load.
  pub fn load(&mut self) -> u64 {
    self.latest = self.latest.saturating_add(1);
    let id = self.latest;
    let (vcs, request, deliver) = (self.vcs.clone(), self.request.clone(), self.deliver.clone());
    thread::spawn(move || {
      let result = vcs.changeset(&request);
      deliver(Done::Loaded(Loaded { id, result }));
    });
    id
  }

  /// Writes the index off the calling thread.
  pub fn index(&self, request: IndexRequest) {
    let (vcs, deliver) = (self.vcs.clone(), self.deliver.clone());
    thread::spawn(move || {
      let result = request.run(&*vcs);
      deliver(Done::Indexed(request.follow, result));
    });
  }

  /// Whether the view shows only part of the working tree changeset: a
  /// scope of staged or unstaged changes, with no revision.
  pub fn narrowed(&self) -> bool {
    self.request.revisions.is_empty() && self.request.scope != Scope::All
  }

  /// Reads the changeset with every scope, off the calling thread. The paths
  /// stay as the view has them.
  pub fn load_whole(&self) {
    let request = DiffRequest {
      scope: Scope::All,
      ..self.request.clone()
    };
    let (vcs, deliver) = (self.vcs.clone(), self.deliver.clone());
    thread::spawn(move || {
      deliver(Done::Widened(vcs.changeset(&request)));
    });
  }

  /// Throws away changes off the calling thread.
  pub fn restore(&self, request: RestoreRequest) {
    let (vcs, deliver) = (self.vcs.clone(), self.deliver.clone());
    thread::spawn(move || {
      let result = request.run(&*vcs);
      deliver(Done::Restored(request.follow, result));
    });
  }

  /// Reads both versions of file `index` of `files`, the files on screen,
  /// unless that was already asked for.
  pub fn sides(&mut self, files: &Arc<Vec<FileDiff>>, index: usize) {
    if !self.sides_asked.insert(index) {
      return;
    }
    let (vcs, request, files, deliver) = (
      self.vcs.clone(),
      self.request.clone(),
      files.clone(),
      self.deliver.clone(),
    );
    let load = self.applied;
    thread::spawn(move || {
      let Some(file) = files.get(index) else {
        return;
      };
      let sides = Arc::new(vcs.sides(&request, file));
      deliver(Done::Sides { load, index, sides });
    });
  }

  /// Colors a file again after its context changed. Does nothing without a
  /// syntax theme.
  pub fn rehighlight(&self, job: Rehighlight) {
    let Some(highlighter) = self.highlighter.clone() else {
      return;
    };
    let (load, deliver) = (self.applied, self.deliver.clone());
    thread::spawn(move || {
      let highlight = highlighter.diff(&job.file, &job.sides);
      deliver(Done::Rehighlighted {
        load,
        index: job.index,
        generation: job.generation,
        highlight,
      });
    });
  }

  /// Takes `files`, the result of load `id`, as the ones on screen and starts
  /// coloring them, nearest to `focus` first.
  pub fn apply(&mut self, id: u64, files: &Arc<Vec<FileDiff>>, focus: usize) {
    self.applied = id;
    self.sides_asked.clear();
    self.worker = self
      .highlighter
      .clone()
      .filter(|_| !files.is_empty())
      .map(|highlighter| {
        let deliver = self.deliver.clone();
        Worker::spawn(
          self.vcs.clone(),
          self.request.clone(),
          files.clone(),
          highlighter,
          focus,
          move |index, highlight| {
            deliver(Done::Highlighted {
              load: id,
              index,
              highlight,
            })
          },
        )
      });
  }

  /// Moves the file the coloring does next.
  pub fn focus(&self, index: usize) {
    if let Some(worker) = &self.worker {
      worker.focus(index);
    }
  }

  /// `done` unless it is stale, in which case it is dropped. A read of file
  /// contents that failed can be asked for again.
  pub fn accept(&mut self, done: Done) -> Option<Done> {
    match &done {
      Done::Loaded(load) if load.id != self.latest => return None,
      Done::Highlighted { load, .. }
      | Done::Sides { load, .. }
      | Done::Rehighlighted { load, .. }
        if *load != self.applied =>
      {
        return None;
      }
      Done::Sides { index, sides, .. } if sides.new.is_none() => {
        self.sides_asked.remove(index);
      }
      _ => {}
    }
    Some(done)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::vcs::{Memory, Target};
  use std::sync::mpsc;

  fn repo() -> Arc<Memory> {
    Arc::new(Memory::default().commit("a", "1\n").edit("a", "2\n"))
  }

  fn runner_for(vcs: Arc<Memory>, themed: bool) -> (Runner, mpsc::Receiver<Done>) {
    let (tx, rx) = mpsc::channel();
    let highlighter =
      themed.then(|| Arc::new(Highlighter::new(crate::highlight::DEFAULT_THEME).unwrap()));
    let deliver: Deliver = Arc::new(move |done| tx.send(done).is_ok());
    (
      Runner::new(vcs, DiffRequest::default(), highlighter, deliver),
      rx,
    )
  }

  fn runner() -> (Runner, mpsc::Receiver<Done>) {
    runner_for(repo(), false)
  }

  /// Runs a load to completion and puts its files on screen as load `id`.
  fn show(runner: &mut Runner, rx: &mpsc::Receiver<Done>) -> Arc<Vec<FileDiff>> {
    let id = runner.load();
    let load = loop {
      // Colors of an earlier load may arrive first.
      if let Done::Loaded(load) = rx.recv().unwrap() {
        break load;
      }
    };
    let files = Arc::new(load.result.unwrap());
    runner.apply(id, &files, 0);
    files
  }

  #[test]
  fn a_load_returns_before_the_backend_finishes() {
    let (release, gate) = mpsc::channel();
    let slow = Memory::default()
      .commit("slow", "1\n")
      .edit("slow", "2\n")
      .staged("slow")
      .edit("slow", "3\n")
      .gated(gate);
    let (mut runner, rx) = runner_for(Arc::new(slow), false);
    runner.load();
    assert!(rx.try_recv().is_err());
    // One release per section of the default working tree request.
    release.send(()).unwrap();
    release.send(()).unwrap();
    let Some(Done::Loaded(load)) = runner.accept(rx.recv().unwrap()) else {
      panic!("expected the latest load");
    };
    assert_eq!(load.result.unwrap().len(), 2);
  }

  #[test]
  fn each_load_gets_the_next_number() {
    let (mut runner, _rx) = runner();
    assert_eq!((runner.load(), runner.load()), (1, 2));
  }

  #[test]
  fn a_load_runs_off_the_calling_thread_and_delivers_its_result() {
    let (mut runner, rx) = runner();
    let id = runner.load();
    let Some(Done::Loaded(load)) = runner.accept(rx.recv().unwrap()) else {
      panic!("the only load is the latest");
    };
    assert_eq!(load.id, id);
    assert_eq!(load.result.unwrap().len(), 1);
  }

  #[test]
  fn an_older_load_that_finishes_last_is_discarded() {
    let (mut runner, rx) = runner();
    runner.load();
    let newer = runner.load();
    let (first, second) = (rx.recv().unwrap(), rx.recv().unwrap());
    // Whichever finished first, only the newer load is taken.
    let taken: Vec<_> = <[Done; 2]>::from((first, second))
      .into_iter()
      .filter_map(|done| runner.accept(done))
      .collect();
    assert!(matches!(taken.as_slice(), [Done::Loaded(l)] if l.id == newer));
  }

  #[test]
  fn an_older_load_is_stale_even_before_the_newer_one_finishes() {
    let (mut runner, rx) = runner();
    runner.load();
    let first = rx.recv().unwrap();
    runner.load();
    assert!(runner.accept(first).is_none());
  }

  #[test]
  fn indexing_runs_off_the_calling_thread_and_reports_the_follow_up() {
    let (mut runner, rx) = runner();
    let files = show(&mut runner, &rx);
    runner.index(IndexRequest {
      file: files[0].clone(),
      target: Target::File,
      stage: true,
      follow: None,
    });
    assert!(matches!(
      runner.accept(rx.recv().unwrap()),
      Some(Done::Indexed(None, Ok(())))
    ));
  }

  #[test]
  fn contents_are_read_once_per_file_until_a_read_fails_or_a_load_replaces_them() {
    let (mut runner, rx) = runner();
    let files = show(&mut runner, &rx);
    runner.sides(&files, 0);
    runner.sides(&files, 0);
    assert!(matches!(
      runner.accept(rx.recv().unwrap()),
      Some(Done::Sides { index: 0, .. })
    ));
    assert!(rx.try_recv().is_err(), "the second ask was a duplicate");
    show(&mut runner, &rx);
    runner.sides(&files, 0);
    assert!(rx.recv().is_ok(), "a new load forgets what was asked");
  }

  #[test]
  fn contents_that_could_not_be_read_can_be_asked_for_again() {
    let (mut runner, rx) = runner_for(Arc::new(Memory::default()), false);
    let files = Arc::new(parse_one());
    runner.sides(&files, 0);
    let done = rx.recv().unwrap();
    assert!(matches!(&done, Done::Sides { sides, .. } if sides.new.is_none()));
    assert!(runner.accept(done).is_some());
    runner.sides(&files, 0);
    assert!(rx.recv().is_ok());
  }

  fn parse_one() -> Vec<FileDiff> {
    let diff = "diff --git a/f.rs b/f.rs\n--- a/f.rs\n+++ b/f.rs\n@@ -1 +1 @@\n-a\n+b\n";
    crate::diff::parse(diff.as_bytes()).unwrap()
  }

  #[test]
  fn contents_read_for_a_replaced_load_are_discarded() {
    let (mut runner, rx) = runner();
    let files = show(&mut runner, &rx);
    runner.sides(&files, 0);
    let stale = rx.recv().unwrap();
    show(&mut runner, &rx);
    assert!(runner.accept(stale).is_none());
  }

  #[test]
  fn colors_made_for_a_replaced_load_are_discarded() {
    let (mut runner, rx) = runner_for(repo(), true);
    let files = show(&mut runner, &rx);
    let stale = rx.recv().unwrap();
    assert!(matches!(stale, Done::Highlighted { load: 1, .. }));
    show(&mut runner, &rx);
    assert!(runner.accept(stale).is_none());
    let current = rx.recv().unwrap();
    assert!(matches!(
      runner.accept(current),
      Some(Done::Highlighted { load: 2, .. })
    ));
    assert_eq!(files.len(), 1);
  }

  #[test]
  fn recolored_files_of_a_replaced_load_are_discarded() {
    let (mut runner, rx) = runner_for(repo(), true);
    let files = show(&mut runner, &rx);
    let rehighlight = || Rehighlight {
      index: 0,
      generation: 1,
      file: files[0].clone(),
      sides: Arc::new(Sides::default()),
    };
    runner.rehighlight(rehighlight());
    let recolored = |rx: &mpsc::Receiver<Done>| loop {
      // The initial coloring of the load shares the channel.
      if let done @ Done::Rehighlighted { .. } = rx.recv().unwrap() {
        break done;
      }
    };
    let stale = recolored(&rx);
    show(&mut runner, &rx);
    assert!(runner.accept(stale).is_none());
    runner.rehighlight(rehighlight());
    let fresh = recolored(&rx);
    assert!(runner.accept(fresh).is_some());
  }

  #[test]
  fn without_a_theme_nothing_is_recolored() {
    let (mut runner, rx) = runner();
    let files = show(&mut runner, &rx);
    runner.rehighlight(Rehighlight {
      index: 0,
      generation: 1,
      file: files[0].clone(),
      sides: Arc::new(Sides::default()),
    });
    assert!(
      rx.recv_timeout(std::time::Duration::from_millis(100))
        .is_err()
    );
  }

  /// `a` and `b` unstaged and `c` staged.
  fn three() -> Arc<Memory> {
    Arc::new(
      Memory::default()
        .commit("a", "1\n")
        .commit("b", "1\n")
        .commit("c", "1\n")
        .edit("a", "2\n")
        .edit("b", "2\n")
        .edit("c", "2\n")
        .staged("c"),
    )
  }

  fn runner_with(vcs: Arc<Memory>, request: DiffRequest) -> (Runner, mpsc::Receiver<Done>) {
    let (tx, rx) = mpsc::channel();
    let deliver: Deliver = Arc::new(move |done| tx.send(done).is_ok());
    (Runner::new(vcs, request, None, deliver), rx)
  }

  fn scoped(scope: Scope) -> DiffRequest {
    DiffRequest {
      scope,
      ..DiffRequest::default()
    }
  }

  #[test]
  fn a_restore_runs_off_the_calling_thread_and_delivers_the_follow_target() {
    let vcs = three();
    let (mut runner, rx) = runner_with(vcs.clone(), DiffRequest::default());
    let files = show(&mut runner, &rx);
    let at = files
      .iter()
      .position(|f| f.path() == std::path::Path::new("a"))
      .unwrap();
    let request = crate::restore::restore_file(&files, at).unwrap();
    let follow = request.follow.clone();
    assert!(follow.is_some());
    runner.restore(request);
    let Some(Done::Restored(delivered, result)) = runner.accept(rx.recv().unwrap()) else {
      panic!("a restore result is never stale");
    };
    result.unwrap();
    assert_eq!(delivered, follow);
    let after = vcs.changeset(&DiffRequest::default()).unwrap();
    assert!(after.iter().all(|f| f.path() != std::path::Path::new("a")));
  }

  #[test]
  fn a_restore_that_finishes_after_a_newer_load_started_is_still_taken() {
    let (mut runner, rx) = runner_with(three(), DiffRequest::default());
    let files = show(&mut runner, &rx);
    runner.restore(crate::restore::nuke_all(&files).unwrap());
    runner.load();
    // The load and the restore race, so take messages until the restore.
    let restored = loop {
      let done = rx.recv().unwrap();
      if matches!(done, Done::Restored(..)) {
        break done;
      }
    };
    assert!(matches!(
      runner.accept(restored),
      Some(Done::Restored(None, Ok(())))
    ));
  }

  #[test]
  fn a_narrowed_view_is_a_working_tree_scope_without_revisions() {
    let narrowed = |request: DiffRequest| runner_with(three(), request).0.narrowed();
    assert!(!narrowed(DiffRequest::default()));
    assert!(narrowed(scoped(Scope::Staged)));
    assert!(narrowed(scoped(Scope::Unstaged)));
    let revision = DiffRequest {
      scope: Scope::Staged,
      revisions: vec!["HEAD".into()],
      ..DiffRequest::default()
    };
    assert!(!narrowed(revision));
  }

  #[test]
  fn load_whole_reads_every_scope_and_keeps_the_paths() {
    let (mut runner, rx) = runner_with(three(), scoped(Scope::Staged));
    runner.load_whole();
    let Some(Done::Widened(result)) = runner.accept(rx.recv().unwrap()) else {
      panic!("a widened read is never stale");
    };
    assert_eq!(result.unwrap().len(), 3);

    let limited = DiffRequest {
      scope: Scope::Staged,
      paths: vec!["a".into()],
      ..DiffRequest::default()
    };
    let (runner, rx) = runner_with(three(), limited);
    runner.load_whole();
    let Done::Widened(result) = rx.recv().unwrap() else {
      panic!("expected the widened read");
    };
    let names: Vec<_> = result
      .unwrap()
      .iter()
      .map(|f| f.path().display().to_string())
      .collect();
    assert_eq!(names, ["a"]);
  }
}
