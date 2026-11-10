//! A `Vcs` that keeps the commit, the index and the working tree in memory, so
//! a test can stage and unstage without a repository or a subprocess.
//!
//! It supports whole-file, hunk and line staging. Revisions are not supported.

use super::{DiffRequest, Error, Kind, Sides, Target, Vcs, VersionId};
use crate::args::Scope;
use crate::diff::{self, FileDiff, Hunk, LineKind, Section};
use similar::TextDiff;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Context lines around a change when the request sets none, as git does.
const DEFAULT_CONTEXT: u32 = 3;

/// What one path holds in the commit, the index and the working tree. `None`
/// means the file does not exist there.
#[derive(Debug, Clone, Default)]
struct Versions {
  head: Option<String>,
  index: Option<String>,
  work: Option<String>,
}

impl Versions {
  /// The old and new contents a diff of `scope` compares.
  fn sides(&self, scope: Scope) -> (Option<&String>, Option<&String>) {
    match scope {
      Scope::All => (self.head.as_ref(), self.work.as_ref()),
      Scope::Staged => (self.head.as_ref(), self.index.as_ref()),
      Scope::Unstaged => (self.index.as_ref(), self.work.as_ref()),
    }
  }
}

#[derive(Debug, Default)]
pub struct Memory {
  files: Mutex<BTreeMap<PathBuf, Versions>>,
  /// When set, each `diff` waits for one message before it answers.
  gate: Mutex<Option<Receiver<()>>>,
}

impl Memory {
  fn lock(&self) -> MutexGuard<'_, BTreeMap<PathBuf, Versions>> {
    self.files.lock().unwrap_or_else(PoisonError::into_inner)
  }

  /// Adds a file that is committed, staged and in the working tree with
  /// `text`.
  #[must_use]
  pub fn commit(self, path: &str, text: &str) -> Self {
    let text = Some(text.to_string());
    self.lock().insert(
      PathBuf::from(path),
      Versions {
        head: text.clone(),
        index: text.clone(),
        work: text,
      },
    );
    self
  }

  /// Changes the working tree contents of `path`. A path that was never
  /// committed becomes an untracked file.
  #[must_use]
  pub fn edit(self, path: &str, text: &str) -> Self {
    self.lock().entry(PathBuf::from(path)).or_default().work = Some(text.to_string());
    self
  }

  /// Changes the working tree contents of `path` on a repository already in
  /// use, as an editor saving the file does.
  pub fn save(&self, path: &str, text: &str) {
    self.lock().entry(PathBuf::from(path)).or_default().work = Some(text.to_string());
  }

  /// Puts the working tree contents of `path` in the index.
  #[must_use]
  pub fn staged(self, path: &str) -> Self {
    if let Some(versions) = self.lock().get_mut(Path::new(path)) {
      versions.index.clone_from(&versions.work);
    }
    self
  }

  /// Stages or unstages the changed lines of hunk `hunk` of `file` that are in
  /// `lines`, or all of them when `lines` is `None`.
  fn rewrite(
    &self,
    file: &FileDiff,
    hunk: usize,
    lines: Option<&BTreeSet<usize>>,
    stage: bool,
  ) -> Result<(), Error> {
    let hunk = file
      .hunks
      .get(hunk)
      .ok_or_else(|| Error::Git(format!("no hunk {hunk} in {}", file.path().display())))?;
    let with = rewritten(hunk, stage, |i| lines.is_none_or(|l| l.contains(&i)));
    let (start, len) = if stage {
      (hunk.old_start, hunk.old_len)
    } else {
      (hunk.new_start, hunk.new_len)
    };
    self.update([file], |v| {
      let text = splice(v.index.as_deref().unwrap_or(""), start, len, &with);
      let absent = if stage { &v.work } else { &v.head };
      v.index = (!text.is_empty() || absent.is_some()).then_some(text);
    })
  }

  /// Makes each `diff` wait for one message on `gate`, so a test can hold a
  /// load open.
  #[must_use]
  pub fn gated(self, gate: Receiver<()>) -> Self {
    *self.gate.lock().unwrap_or_else(PoisonError::into_inner) = Some(gate);
    self
  }

  /// What the index holds for `path`, or `None` when it holds no such file.
  pub fn index_text(&self, path: &str) -> Option<String> {
    self.lock().get(Path::new(path))?.index.clone()
  }

  fn missing(path: &Path) -> Error {
    Error::Git(format!("no such file: {}", path.display()))
  }

  fn unsupported() -> Error {
    Error::Git("not supported by the in-memory Vcs".into())
  }

  /// Applies `change` to the versions of each of `files`.
  fn update<'a>(
    &self,
    files: impl IntoIterator<Item = &'a FileDiff>,
    change: impl Fn(&mut Versions),
  ) -> Result<(), Error> {
    files.into_iter().try_for_each(|file| {
      self
        .lock()
        .get_mut(file.path())
        .map(&change)
        .ok_or_else(|| Self::missing(file.path()))
    })
  }
}

/// Replaces `len` lines of `text` from the 1-based line `start` with `with`.
/// A hunk that covers no lines on its side starts at the line before the gap.
fn splice(text: &str, start: u32, len: u32, with: &[String]) -> String {
  let lines: Vec<&str> = text.split_inclusive('\n').collect();
  let from = if len == 0 {
    start
  } else {
    start.saturating_sub(1)
  };
  let from = usize::try_from(from).unwrap_or(usize::MAX).min(lines.len());
  let to = from.saturating_add(usize::try_from(len).unwrap_or(usize::MAX));
  let to = to.min(lines.len());
  let mut out: String = lines[..from].concat();
  for line in with {
    out.push_str(line);
    out.push('\n');
  }
  out.push_str(&lines[to..].concat());
  out
}

/// What the lines of `hunk` become on the side a stage or unstage rewrites.
/// Staging rewrites the index side, the old one, so the picked added lines come
/// in and the picked removed ones go. Unstaging rewrites the new side of a
/// staged diff, so the picked added lines go and the picked removed ones come
/// back. Lines that are not picked stay as they are in the index.
fn rewritten(hunk: &Hunk, stage: bool, picked: impl Fn(usize) -> bool) -> Vec<String> {
  let (taken, left) = if stage {
    (LineKind::Added, LineKind::Removed)
  } else {
    (LineKind::Removed, LineKind::Added)
  };
  hunk
    .lines
    .iter()
    .enumerate()
    .filter(|(i, l)| {
      if l.kind == taken {
        picked(*i)
      } else if l.kind == left {
        !picked(*i)
      } else {
        true
      }
    })
    .map(|(_, l)| String::from_utf8_lossy(&l.content).into_owned())
    .collect()
}

/// The text of `git diff` for one path, or nothing when the sides match.
fn patch(path: &Path, old: Option<&str>, new: Option<&str>, context: u32) -> String {
  let name = path.display();
  let header = match (old, new) {
    (None, _) => format!("new file mode 100644\n--- /dev/null\n+++ b/{name}\n"),
    (_, None) => format!("deleted file mode 100644\n--- a/{name}\n+++ /dev/null\n"),
    _ => format!("--- a/{name}\n+++ b/{name}\n"),
  };
  let mut out = format!("diff --git a/{name} b/{name}\n{header}");
  let diff = TextDiff::from_lines(old.unwrap_or(""), new.unwrap_or(""));
  let radius = usize::try_from(context).unwrap_or(usize::MAX);
  for hunk in diff.unified_diff().context_radius(radius).iter_hunks() {
    out.push_str(&hunk.to_string());
  }
  out
}

impl Memory {
  fn stage(&self, file: &FileDiff) -> Result<(), Error> {
    self.update([file], |v| v.index.clone_from(&v.work))
  }

  fn unstage(&self, file: &FileDiff) -> Result<(), Error> {
    self.update([file], |v| v.index.clone_from(&v.head))
  }

  fn stage_all(&self, files: &[FileDiff]) -> Result<(), Error> {
    self.update(files, |v| v.index.clone_from(&v.work))
  }

  fn unstage_all(&self, files: &[FileDiff]) -> Result<(), Error> {
    self.update(files, |v| v.index.clone_from(&v.head))
  }

  fn stage_hunk(&self, file: &FileDiff, hunk: usize) -> Result<(), Error> {
    self.rewrite(file, hunk, None, true)
  }

  fn unstage_hunk(&self, file: &FileDiff, hunk: usize) -> Result<(), Error> {
    self.rewrite(file, hunk, None, false)
  }

  fn stage_lines(
    &self,
    file: &FileDiff,
    hunk: usize,
    lines: &BTreeSet<usize>,
  ) -> Result<(), Error> {
    self.rewrite(file, hunk, Some(lines), true)
  }

  fn unstage_lines(
    &self,
    file: &FileDiff,
    hunk: usize,
    lines: &BTreeSet<usize>,
  ) -> Result<(), Error> {
    self.rewrite(file, hunk, Some(lines), false)
  }
}

impl Vcs for Memory {
  fn diff(&self, request: &DiffRequest) -> Result<Vec<FileDiff>, Error> {
    if !request.revisions.is_empty() {
      return Err(Self::unsupported());
    }
    if let Some(gate) = &*self.gate.lock().unwrap_or_else(PoisonError::into_inner) {
      gate.recv().map_err(|_| Self::unsupported())?;
    }
    let context = request.context.unwrap_or(DEFAULT_CONTEXT);
    let mut text = String::new();
    for (path, versions) in self.lock().iter() {
      if !request.paths.is_empty() && !request.paths.iter().any(|p| path.starts_with(p)) {
        continue;
      }
      let (old, new) = versions.sides(request.scope);
      if old != new {
        text.push_str(&patch(
          path,
          old.map(String::as_str),
          new.map(String::as_str),
          context,
        ));
      }
    }
    Ok(diff::parse(text.as_bytes())?)
  }

  fn update_index(&self, file: &FileDiff, target: &Target, stage: bool) -> Result<(), Error> {
    match (target, stage) {
      (Target::File, true) => self.stage(file),
      (Target::File, false) => self.unstage(file),
      (Target::All(files), true) => self.stage_all(files),
      (Target::All(files), false) => self.unstage_all(files),
      (Target::Hunk(hunk), true) => self.stage_hunk(file, *hunk),
      (Target::Hunk(hunk), false) => self.unstage_hunk(file, *hunk),
      (Target::Lines(hunk, lines), true) => self.stage_lines(file, *hunk, lines),
      (Target::Lines(hunk, lines), false) => self.unstage_lines(file, *hunk, lines),
    }
  }

  fn restore(&self, files: &[FileDiff], kind: Kind) -> Result<(), Error> {
    match kind {
      Kind::Restore => self.update(files, |v| v.work.clone_from(&v.index)),
      Kind::Nuke => self.update(files, |v| {
        v.index.clone_from(&v.head);
        v.work.clone_from(&v.head);
      }),
    }
  }

  fn sides(&self, request: &DiffRequest, file: &FileDiff) -> Sides {
    let scope = file.section.map_or(request.scope, Section::scope);
    let Some(versions) = self.lock().get(file.path()).cloned() else {
      return Sides::default();
    };
    let (old, new) = versions.sides(scope);
    let bytes = |text: Option<&String>| text.map(|t| t.clone().into_bytes());
    let (old, new) = (bytes(old), bytes(new));
    Sides {
      old_id: old.as_deref().map(VersionId::of),
      new_id: new.as_deref().map(VersionId::of),
      old,
      new,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn repo() -> Memory {
    Memory::default()
      .commit("a", "1\n")
      .commit("b", "1\n")
      .edit("a", "2\n")
      .edit("new", "x\n")
  }

  fn changeset(vcs: &Memory) -> Vec<(String, Option<Section>)> {
    vcs
      .changeset(&DiffRequest::default())
      .unwrap()
      .iter()
      .map(|f| (f.path().display().to_string(), f.section))
      .collect()
  }

  #[test]
  fn unchanged_files_are_not_listed() {
    let names: Vec<_> = changeset(&repo()).into_iter().map(|(n, _)| n).collect();
    assert_eq!(names, ["a", "new"]);
  }

  #[test]
  fn restoring_an_unstaged_file_drops_it_from_the_changeset() {
    let vcs = repo().commit("c", "1\n").edit("c", "2\n").staged("c");
    let files = vcs.changeset(&DiffRequest::default()).unwrap();
    let a = files.iter().find(|f| f.path() == Path::new("a")).unwrap();
    vcs.restore(std::slice::from_ref(a), Kind::Restore).unwrap();
    assert_eq!(
      changeset(&vcs),
      [
        ("new".into(), Some(Section::Unstaged)),
        ("c".into(), Some(Section::Staged)),
      ]
    );
  }

  #[test]
  fn restoring_keeps_the_staged_part_of_a_file() {
    let vcs = Memory::default()
      .commit("a", "1\n")
      .edit("a", "2\n")
      .staged("a");
    vcs.save("a", "3\n");
    let files = vcs.changeset(&DiffRequest::default()).unwrap();
    let unstaged = files
      .iter()
      .find(|f| f.section == Some(Section::Unstaged))
      .unwrap();
    vcs
      .restore(std::slice::from_ref(unstaged), Kind::Restore)
      .unwrap();
    assert_eq!(vcs.index_text("a"), Some("2\n".into()));
    assert_eq!(changeset(&vcs), [("a".into(), Some(Section::Staged))]);
  }

  #[test]
  fn a_nuke_returns_the_index_and_the_working_tree_to_head() {
    let vcs = Memory::default()
      .commit("both", "1\n")
      .commit("staged", "1\n")
      .commit("plain", "1\n")
      .edit("both", "2\n")
      .staged("both")
      .edit("staged", "2\n")
      .staged("staged")
      .edit("plain", "2\n")
      .edit("new", "x\n")
      .edit("added", "y\n")
      .staged("added");
    vcs.save("both", "3\n");
    let files = vcs.changeset(&DiffRequest::default()).unwrap();
    vcs.restore(&files, Kind::Nuke).unwrap();
    assert!(changeset(&vcs).is_empty());
    assert_eq!(vcs.index_text("both"), Some("1\n".into()));
    assert_eq!(vcs.index_text("staged"), Some("1\n".into()));
    assert_eq!(vcs.index_text("added"), None);
  }

  #[test]
  fn restoring_an_untracked_file_deletes_it() {
    let vcs = repo();
    let files = vcs.changeset(&DiffRequest::default()).unwrap();
    let new = files.iter().find(|f| f.path() == Path::new("new")).unwrap();
    vcs
      .restore(std::slice::from_ref(new), Kind::Restore)
      .unwrap();
    assert!(changeset(&vcs).iter().all(|(name, _)| name != "new"));
  }

  #[test]
  fn restoring_an_unknown_file_is_an_error() {
    let vcs = repo();
    let mut files = vcs.changeset(&DiffRequest::default()).unwrap();
    files[0].new_path = Some("ghost".into());
    files[0].old_path = Some("ghost".into());
    assert!(vcs.restore(&files[..1], Kind::Restore).is_err());
  }

  #[test]
  fn staging_a_file_moves_it_to_the_staged_section() {
    let vcs = repo();
    let file = vcs.diff(&DiffRequest::default()).unwrap().remove(0);
    vcs.stage(&file).unwrap();
    assert_eq!(
      changeset(&vcs),
      [
        ("new".into(), Some(Section::Unstaged)),
        ("a".into(), Some(Section::Staged)),
      ]
    );
  }

  #[test]
  fn unstaging_returns_the_file_to_the_unstaged_section() {
    let vcs = repo();
    let file = vcs.diff(&DiffRequest::default()).unwrap().remove(0);
    vcs.stage(&file).unwrap();
    vcs.unstage(&file).unwrap();
    assert_eq!(changeset(&vcs), changeset(&repo()));
  }

  #[test]
  fn an_untracked_file_leaves_the_changeset_when_unstaged_again() {
    let vcs = repo();
    let all = vcs.diff(&DiffRequest::default()).unwrap();
    vcs.stage_all(&all).unwrap();
    assert!(
      changeset(&vcs)
        .iter()
        .all(|(_, s)| *s == Some(Section::Staged))
    );
    vcs.unstage_all(&all).unwrap();
    assert!(
      changeset(&vcs)
        .iter()
        .all(|(_, s)| *s == Some(Section::Unstaged))
    );
  }

  #[test]
  fn sides_follow_the_section_of_the_entry() {
    let vcs = repo();
    let mut file = vcs.diff(&DiffRequest::default()).unwrap().remove(0);
    file.section = Some(Section::Unstaged);
    let sides = vcs.sides(&DiffRequest::default(), &file);
    assert_eq!(sides.old.as_deref(), Some(b"1\n".as_slice()));
    assert_eq!(sides.new.as_deref(), Some(b"2\n".as_slice()));
    assert_eq!(sides.new_id, Some(VersionId::of(b"2\n")));
  }

  /// `f` with lines 1 to 20 and changes at lines 2 and 19, so two hunks.
  fn two_hunks() -> Memory {
    let text = |a: &str, b: &str| {
      (1..=20)
        .map(|n| match n {
          2 => format!("{a}\n"),
          19 => format!("{b}\n"),
          _ => format!("{n}\n"),
        })
        .collect::<String>()
    };
    Memory::default()
      .commit("f", &text("2", "19"))
      .edit("f", &text("TWO", "NINETEEN"))
  }

  fn entry(vcs: &Memory, section: Section) -> FileDiff {
    vcs
      .changeset(&DiffRequest::default())
      .unwrap()
      .into_iter()
      .find(|f| f.section == Some(section))
      .unwrap()
  }

  #[test]
  fn staging_a_hunk_leaves_the_other_unstaged() {
    let vcs = two_hunks();
    vcs.stage_hunk(&entry(&vcs, Section::Unstaged), 1).unwrap();
    assert_eq!(entry(&vcs, Section::Staged).hunks.len(), 1);
    let rest = entry(&vcs, Section::Unstaged);
    assert_eq!(rest.hunks.len(), 1);
    assert_eq!(rest.hunks[0].lines[2].content, b"TWO");
  }

  #[test]
  fn unstaging_a_hunk_puts_it_back_in_the_unstaged_section() {
    let vcs = two_hunks().staged("f");
    vcs.unstage_hunk(&entry(&vcs, Section::Staged), 0).unwrap();
    assert_eq!(entry(&vcs, Section::Staged).hunks.len(), 1);
    assert_eq!(entry(&vcs, Section::Unstaged).hunks.len(), 1);
  }

  #[test]
  fn staging_every_hunk_leaves_nothing_unstaged() {
    let vcs = two_hunks();
    vcs.stage_hunk(&entry(&vcs, Section::Unstaged), 0).unwrap();
    vcs.stage_hunk(&entry(&vcs, Section::Unstaged), 0).unwrap();
    assert!(
      vcs
        .changeset(&DiffRequest::default())
        .unwrap()
        .iter()
        .all(|f| f.section == Some(Section::Staged))
    );
  }

  #[test]
  fn staging_one_line_of_a_replacement_keeps_the_other_half_unstaged() {
    let vcs = two_hunks();
    let file = entry(&vcs, Section::Unstaged);
    // Hunk 0 holds the removed line, then the added one.
    let removed = file.hunks[0]
      .lines
      .iter()
      .position(|l| l.kind == LineKind::Removed)
      .unwrap();
    vcs
      .stage_lines(&file, 0, &BTreeSet::from([removed]))
      .unwrap();
    let staged = entry(&vcs, Section::Staged);
    assert_eq!(staged.hunks.len(), 1);
    assert!(
      staged.hunks[0]
        .lines
        .iter()
        .all(|l| l.kind != LineKind::Added)
    );
    vcs
      .unstage_lines(&staged, 0, &BTreeSet::from([removed]))
      .unwrap();
    assert!(
      vcs
        .changeset(&DiffRequest::default())
        .unwrap()
        .iter()
        .all(|f| f.section == Some(Section::Unstaged))
    );
  }

  #[test]
  fn staging_an_untracked_file_by_hunk_and_back_removes_it_from_the_index() {
    let vcs = Memory::default().edit("n", "x\ny\n");
    vcs.stage_hunk(&entry(&vcs, Section::Unstaged), 0).unwrap();
    let staged = entry(&vcs, Section::Staged);
    vcs.unstage_hunk(&staged, 0).unwrap();
    assert!(
      vcs
        .changeset(&DiffRequest::default())
        .unwrap()
        .iter()
        .all(|f| f.section == Some(Section::Unstaged))
    );
  }

  #[test]
  fn revisions_are_not_supported() {
    let request = DiffRequest {
      revisions: vec!["HEAD".into()],
      ..DiffRequest::default()
    };
    assert!(repo().diff(&request).is_err());
  }
}
