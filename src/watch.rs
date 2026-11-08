use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, RwLock};
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
  /// burst of relevant changes. `changed` returns false to stop. Symlinks are
  /// not followed, since git tracks a link and not what it points to, and
  /// changes inside ignored directories are dropped.
  pub fn start(
    root: &Path,
    changed: impl FnMut() -> bool + Send + 'static,
  ) -> notify::Result<Watch> {
    let (tx, rx) = mpsc::channel();
    let base = root.to_path_buf();
    let ignored = Arc::new(RwLock::new(ignored_roots(root)));
    let mut watcher = RecommendedWatcher::new(
      move |event: notify::Result<notify::Event>| {
        // Reads are not changes, and git itself reads the index and HEAD.
        let Ok(event) = event else { return };
        if event.kind.is_access() {
          return;
        }
        if event.paths.iter().any(|p| edits_ignore_rules(p)) {
          *ignored.write().unwrap() = ignored_roots(&base);
        }
        let ignored = ignored.read().unwrap();
        if event.paths.iter().any(|p| relevant(&base, &ignored, p)) {
          let _ = tx.send(());
        }
      },
      Config::default().with_follow_symlinks(false),
    )?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    thread::spawn(move || debounce(&rx, QUIET, changed));
    Ok(Watch { _watcher: watcher })
  }
}

/// The directories git ignores, relative to the work tree root `root`. A
/// directory that holds tracked files is not listed. Empty when git fails,
/// which leaves nothing filtered.
fn ignored_roots(root: &Path) -> HashSet<PathBuf> {
  let output = Command::new("git")
    .args([
      "ls-files",
      "--full-name",
      "--others",
      "--ignored",
      "--exclude-standard",
      "--directory",
      "-z",
    ])
    .current_dir(root)
    .stderr(Stdio::null())
    .output();
  let Ok(output) = output else {
    return HashSet::new();
  };
  if !output.status.success() {
    return HashSet::new();
  }
  output
    .stdout
    .split(|byte| *byte == 0)
    .filter_map(|entry| entry.strip_suffix(b"/"))
    .map(|entry| PathBuf::from(String::from_utf8_lossy(entry).into_owned()))
    .collect()
}

/// Whether a change at `path` can change which directories git ignores.
fn edits_ignore_rules(path: &Path) -> bool {
  path.file_name().is_some_and(|name| name == ".gitignore") || path.ends_with(".git/info/exclude")
}

/// Whether a change at `path` can alter the changeset. Edits in the work tree
/// count, except inside the `ignored` directories. Inside `.git` only the
/// index (staging) and `HEAD` (commits and checkouts) do, since git also
/// churns objects and lock files while it works.
pub fn relevant(root: &Path, ignored: &HashSet<PathBuf>, path: &Path) -> bool {
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
    None => !rel.ancestors().any(|dir| ignored.contains(dir)),
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
    let ignored = HashSet::new();
    for yes in [
      "src/a.rs",
      "new.txt",
      ".gitignore",
      ".git/index",
      ".git/HEAD",
    ] {
      assert!(relevant(root, &ignored, &root.join(yes)), "{yes}");
    }
    for no in [
      ".git/index.lock",
      ".git/objects/ab/cdef",
      ".git/refs/heads/main",
      ".git/FETCH_HEAD",
    ] {
      assert!(!relevant(root, &ignored, &root.join(no)), "{no}");
    }
    assert!(!relevant(root, &ignored, Path::new("/elsewhere/a.rs")));
  }

  #[test]
  fn changes_inside_ignored_directories_do_not_count() {
    let root = Path::new("/repo");
    let ignored = HashSet::from([PathBuf::from("node_modules"), PathBuf::from("a/target")]);
    for no in ["node_modules", "node_modules/x/y.js", "a/target/debug/out"] {
      assert!(!relevant(root, &ignored, &root.join(no)), "{no}");
    }
    for yes in ["src/node_modules.rs", "a/src/main.rs", "a/target.txt"] {
      assert!(relevant(root, &ignored, &root.join(yes)), "{yes}");
    }
  }

  #[test]
  fn ignored_roots_lists_only_ignored_directories() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let git = |args: &[&str]| {
      let ok = Command::new("git")
        .args(args)
        .current_dir(&root)
        .status()
        .unwrap()
        .success();
      assert!(ok, "git {args:?}");
    };
    git(&["init", "-q"]);
    fs::write(root.join(".gitignore"), "node_modules\n*.log\n").unwrap();
    fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
    fs::write(root.join("node_modules/pkg/i.js"), "x").unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "x").unwrap();
    fs::write(root.join("src/b.log"), "x").unwrap();
    assert_eq!(
      ignored_roots(&root),
      HashSet::from([PathBuf::from("node_modules")])
    );
    assert!(ignored_roots(&root.join("src/nope")).is_empty());
  }

  #[test]
  fn a_symlinked_directory_is_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root").canonicalize().unwrap_or_else(|_| {
      fs::create_dir(dir.path().join("root")).unwrap();
      dir.path().join("root").canonicalize().unwrap()
    });
    let outside = dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
    let (tx, rx) = channel();
    let _watch = Watch::start(&root, move || tx.send(()).is_ok()).unwrap();
    thread::sleep(Duration::from_millis(100));
    fs::write(outside.join("a.txt"), "x").unwrap();
    assert!(rx.recv_timeout(Duration::from_millis(600)).is_err());
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
