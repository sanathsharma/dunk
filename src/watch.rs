use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Component, Path};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

/// How long the repository must stay quiet before a refresh fires.
pub const QUIET: Duration = Duration::from_millis(150);

/// Keeps a filesystem watcher alive. Dropping it stops the watching.
pub struct Watch {
  _watcher: RecommendedWatcher,
}

impl Watch {
  /// Watches the work tree rooted at `root` and calls `changed` once per
  /// burst of relevant changes. `changed` returns false to stop.
  pub fn start(
    root: &Path,
    changed: impl FnMut() -> bool + Send + 'static,
  ) -> notify::Result<Watch> {
    let (tx, rx) = mpsc::channel();
    let base = root.to_path_buf();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
      if let Ok(event) = event
        && event.paths.iter().any(|p| relevant(&base, p))
      {
        let _ = tx.send(());
      }
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    thread::spawn(move || debounce(&rx, QUIET, changed));
    Ok(Watch { _watcher: watcher })
  }
}

/// Whether a change at `path` can alter the changeset. Edits anywhere in the
/// work tree count. Inside `.git` only the index (staging) and `HEAD`
/// (commits and checkouts) do, since git also churns objects and lock files
/// while it works.
pub fn relevant(root: &Path, path: &Path) -> bool {
  let Ok(rel) = path.strip_prefix(root) else {
    return false;
  };
  let parts: Vec<_> = rel
    .components()
    .filter_map(|c| match c {
      Component::Normal(s) => Some(s),
      _ => None,
    })
    .collect();
  match parts.iter().position(|p| *p == ".git") {
    None => true,
    Some(at) => matches!(&parts[at + 1..], [name] if *name == "index" || *name == "HEAD"),
  }
}

/// Turns bursts of signals on `rx` into single calls of `fire`: after a
/// signal it waits until `quiet` passes with no further signal. Stops when
/// the sender is dropped or `fire` returns false.
pub fn debounce(rx: &Receiver<()>, quiet: Duration, mut fire: impl FnMut() -> bool) {
  while rx.recv().is_ok() {
    loop {
      match rx.recv_timeout(quiet) {
        Ok(()) => continue,
        Err(RecvTimeoutError::Timeout) => break,
        Err(RecvTimeoutError::Disconnected) => {
          fire();
          return;
        }
      }
    }
    if !fire() {
      return;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;
  use std::sync::mpsc::channel;
  use std::time::Instant;

  #[test]
  fn work_tree_edits_count_but_git_internals_mostly_do_not() {
    let root = Path::new("/repo");
    for yes in [
      "src/a.rs",
      "new.txt",
      ".gitignore",
      ".git/index",
      ".git/HEAD",
    ] {
      assert!(relevant(root, &root.join(yes)), "{yes}");
    }
    for no in [
      ".git/index.lock",
      ".git/objects/ab/cdef",
      ".git/refs/heads/main",
      ".git/FETCH_HEAD",
    ] {
      assert!(!relevant(root, &root.join(no)), "{no}");
    }
    assert!(!relevant(root, Path::new("/elsewhere/a.rs")));
  }

  #[test]
  fn a_burst_of_signals_fires_once() {
    let (tx, rx) = channel();
    let (fired_tx, fired_rx) = channel();
    let handle = thread::spawn(move || {
      debounce(&rx, Duration::from_millis(80), || {
        fired_tx.send(Instant::now()).is_ok()
      })
    });
    let start = Instant::now();
    for _ in 0..20 {
      tx.send(()).unwrap();
      thread::sleep(Duration::from_millis(5));
    }
    let fired = fired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(fired.duration_since(start) >= Duration::from_millis(80));
    drop(tx);
    handle.join().unwrap();
    assert!(fired_rx.try_recv().is_err());
  }

  #[test]
  fn separate_bursts_fire_separately() {
    let (tx, rx) = channel();
    let (fired_tx, fired_rx) = channel();
    thread::spawn(move || debounce(&rx, Duration::from_millis(30), || fired_tx.send(()).is_ok()));
    tx.send(()).unwrap();
    fired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    tx.send(()).unwrap();
    fired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
  }

  #[test]
  fn a_real_edit_in_the_work_tree_signals_a_change() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (tx, rx) = channel();
    let _watch = Watch::start(&root, move || tx.send(()).is_ok()).unwrap();
    thread::sleep(Duration::from_millis(100));
    fs::write(root.join("a.txt"), "x").unwrap();
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
  }

  #[test]
  fn staging_and_new_files_signal_a_change_in_a_real_repo() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let git = |args: &[&str]| {
      let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(&root)
        .status()
        .unwrap()
        .success();
      assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    let (tx, rx) = channel();
    let _watch = Watch::start(&root, move || tx.send(()).is_ok()).unwrap();
    thread::sleep(Duration::from_millis(100));

    fs::write(root.join("new.txt"), "x").unwrap();
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    while rx.recv_timeout(Duration::from_millis(500)).is_ok() {}

    git(&["add", "new.txt"]);
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
  }
}
