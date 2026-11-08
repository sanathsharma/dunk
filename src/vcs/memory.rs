//! A `Vcs` that keeps the commit, the index and the working tree in memory, so
//! a test can stage and unstage without a repository or a subprocess.
//!
//! It supports whole-file staging. Hunk and line staging report an error until
//! the staging module needs them.

use super::{DiffRequest, Error, Sides, Vcs, VersionId};
use crate::args::Scope;
use crate::diff::{self, FileDiff, Section};
use similar::TextDiff;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
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

  /// Puts the working tree contents of `path` in the index.
  #[must_use]
  pub fn staged(self, path: &str) -> Self {
    if let Some(versions) = self.lock().get_mut(Path::new(path)) {
      versions.index.clone_from(&versions.work);
    }
    self
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

impl Vcs for Memory {
  fn diff(&self, request: &DiffRequest) -> Result<Vec<FileDiff>, Error> {
    if !request.revisions.is_empty() {
      return Err(Self::unsupported());
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

  fn stage_hunk(&self, _: &FileDiff, _: usize) -> Result<(), Error> {
    Err(Self::unsupported())
  }

  fn unstage_hunk(&self, _: &FileDiff, _: usize) -> Result<(), Error> {
    Err(Self::unsupported())
  }

  fn stage_lines(&self, _: &FileDiff, _: usize, _: &BTreeSet<usize>) -> Result<(), Error> {
    Err(Self::unsupported())
  }

  fn unstage_lines(&self, _: &FileDiff, _: usize, _: &BTreeSet<usize>) -> Result<(), Error> {
    Err(Self::unsupported())
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

  #[test]
  fn revisions_are_not_supported() {
    let request = DiffRequest {
      revisions: vec!["HEAD".into()],
      ..DiffRequest::default()
    };
    assert!(repo().diff(&request).is_err());
  }
}
