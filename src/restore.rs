//! Decides what a restore or a nuke touches and where the selection goes once
//! the changeset is read again. The app sends a request and applies the
//! result.

use crate::diff::{FileDiff, FileStatus, Section};
use crate::staging::{self, Follow, Refusal};
use crate::vcs::Kind;
use std::collections::BTreeMap;
use std::path::Path;

/// What a restore or a nuke takes from one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loss {
  Unstaged,
  Staged,
  StagedAndUnstaged,
  /// The file has no version in the index or in `HEAD`, so it is deleted.
  Untracked,
  /// A restore puts the conflict markers back.
  ConflictRestored,
  /// A nuke resolves the conflict to the `HEAD` version.
  ConflictReset,
}

impl Loss {
  /// The words the confirmation shows next to the file.
  pub const fn label(self) -> &'static str {
    match self {
      Self::Unstaged => "unstaged",
      Self::Staged => "staged",
      Self::StagedAndUnstaged => "staged + unstaged",
      Self::Untracked => "untracked, deleted",
      Self::ConflictRestored => "conflicted, markers recreated",
      Self::ConflictReset => "conflicted, reset to HEAD",
    }
  }
}

/// A file a request touches and what it loses.
#[derive(Debug, Clone)]
pub struct Entry {
  pub file: FileDiff,
  pub loss: Loss,
}

/// A restore waiting to run. The entries are the ones the confirmation
/// listed, captured when it opened.
#[derive(Debug, Clone)]
pub struct RestoreRequest {
  pub kind: Kind,
  pub entries: Vec<Entry>,
  pub follow: Option<Follow>,
}

impl RestoreRequest {
  /// The files to hand to the version control system.
  pub fn files(&self) -> Vec<FileDiff> {
    self.entries.iter().map(|e| e.file.clone()).collect()
  }
}

/// The confirmation a request waits behind: the user types a word, and only
/// the exact word lets the request run.
#[derive(Debug, Clone)]
pub struct Prompt {
  request: RestoreRequest,
  input: String,
  /// Whether the view showed less than the request covers.
  narrowed: bool,
}

impl Prompt {
  pub const fn new(request: RestoreRequest) -> Self {
    Self {
      request,
      input: String::new(),
      narrowed: false,
    }
  }

  /// Marks the prompt as covering more than the view showed.
  #[must_use]
  pub const fn narrowed_view(mut self) -> Self {
    self.narrowed = true;
    self
  }

  pub const fn narrowed(&self) -> bool {
    self.narrowed
  }

  /// The word to type.
  pub const fn word(&self) -> &'static str {
    match self.request.kind {
      Kind::Restore => "restore",
      Kind::Nuke => "nuke",
    }
  }

  pub fn input(&self) -> &str {
    &self.input
  }

  pub const fn request(&self) -> &RestoreRequest {
    &self.request
  }

  pub fn push(&mut self, c: char) {
    self.input.push(c);
  }

  pub fn pop(&mut self) {
    self.input.pop();
  }

  /// Whether the input is exactly the word. Case counts.
  pub fn confirmed(&self) -> bool {
    self.input == self.word()
  }

  /// The request, once confirmed.
  pub fn into_request(self) -> RestoreRequest {
    self.request
  }
}

/// Whether `file` is an untracked file: new in the working tree, with no
/// staged change for the same path.
fn untracked(file: &FileDiff, files: &[FileDiff]) -> bool {
  file.section == Some(Section::Unstaged)
    && file.status == FileStatus::Added
    && !files
      .iter()
      .any(|f| f.section == Some(Section::Staged) && f.path() == file.path())
}

/// Restores the selected file.
pub fn restore_file(files: &[FileDiff], selected: usize) -> Result<RestoreRequest, Refusal> {
  let file = files.get(selected).ok_or(Refusal::NothingToRestore)?;
  match file.section.ok_or(Refusal::ReadOnly)? {
    Section::Staged => return Err(Refusal::Staged),
    Section::Unstaged => {}
  }
  let loss = if untracked(file, files) {
    Loss::Untracked
  } else if file.status == FileStatus::Conflicted {
    Loss::ConflictRestored
  } else {
    Loss::Unstaged
  };
  Ok(RestoreRequest {
    kind: Kind::Restore,
    entries: vec![Entry {
      file: file.clone(),
      loss,
    }],
    follow: staging::neighbour(files, selected),
  })
}

/// Returns every path of `files`, whatever its section, to `HEAD`. A path
/// listed in both sections is taken once. The list is sorted by path.
pub fn nuke_all(files: &[FileDiff]) -> Result<RestoreRequest, Refusal> {
  if files.is_empty() {
    return Err(Refusal::NothingToRestore);
  }
  if files.iter().any(|f| f.section.is_none()) {
    return Err(Refusal::ReadOnly);
  }
  let mut by_path: BTreeMap<&Path, Vec<&FileDiff>> = BTreeMap::new();
  for file in files {
    by_path.entry(file.path()).or_default().push(file);
  }
  let entries = by_path
    .into_values()
    .filter_map(|group| {
      let file = *group.first()?;
      let has = |section| group.iter().any(|f| f.section == Some(section));
      let loss = if group.iter().any(|f| f.status == FileStatus::Conflicted) {
        Loss::ConflictReset
      } else if untracked(file, files) {
        Loss::Untracked
      } else {
        match (has(Section::Staged), has(Section::Unstaged)) {
          (true, true) => Loss::StagedAndUnstaged,
          (true, false) => Loss::Staged,
          _ => Loss::Unstaged,
        }
      };
      Some(Entry {
        file: file.clone(),
        loss,
      })
    })
    .collect();
  Ok(RestoreRequest {
    kind: Kind::Nuke,
    entries,
    follow: None,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::diff::Section;
  use crate::staging::Follow;
  use crate::vcs::{DiffRequest, Memory, Vcs};

  /// `a` and `b` unstaged and `c` staged.
  fn repo() -> Memory {
    Memory::default()
      .commit("a", "1\n")
      .commit("b", "1\n")
      .commit("c", "1\n")
      .edit("a", "2\n")
      .edit("b", "2\n")
      .edit("c", "2\n")
      .staged("c")
  }

  fn load(vcs: &Memory) -> Vec<crate::diff::FileDiff> {
    vcs.changeset(&DiffRequest::default()).unwrap()
  }

  fn position_of(files: &[crate::diff::FileDiff], name: &str, section: Section) -> usize {
    files
      .iter()
      .position(|f| f.path().to_str() == Some(name) && f.section == Some(section))
      .unwrap()
  }

  /// `repo` plus `d`, which has a staged change and a further unstaged one.
  fn repo_with_both() -> Memory {
    let vcs = repo().commit("d", "1\n").edit("d", "2\n").staged("d");
    vcs.save("d", "3\n");
    vcs
  }

  #[test]
  fn a_nuke_covers_every_path_of_both_sections_once() {
    let files = load(&repo_with_both());
    assert_eq!(files.len(), 5, "d is listed in both sections");
    let request = nuke_all(&files).unwrap();
    assert_eq!(request.kind, Kind::Nuke);
    let paths: Vec<_> = request
      .entries
      .iter()
      .map(|e| e.file.path().to_owned())
      .collect();
    assert_eq!(paths, ["a", "b", "c", "d"].map(std::path::PathBuf::from));
    assert_eq!(request.follow, None);
  }

  fn labels(request: &RestoreRequest) -> Vec<(String, &'static str)> {
    request
      .entries
      .iter()
      .map(|e| (e.file.path().display().to_string(), e.loss.label()))
      .collect()
  }

  #[test]
  fn a_restore_says_what_the_file_loses() {
    let vcs = repo().edit("new", "x\n");
    let files = load(&vcs);
    let at = |name| position_of(&files, name, Section::Unstaged);
    let label = |name| labels(&restore_file(&files, at(name)).unwrap());
    assert_eq!(label("a"), [("a".into(), "unstaged")]);
    assert_eq!(label("new"), [("new".into(), "untracked, deleted")]);
    let mut conflicted = files.clone();
    conflicted[at("b")].status = crate::diff::FileStatus::Conflicted;
    assert_eq!(
      labels(&restore_file(&conflicted, at("b")).unwrap()),
      [("b".into(), "conflicted, markers recreated")]
    );
  }

  #[test]
  fn a_nuke_says_what_each_file_loses() {
    let vcs = repo_with_both().edit("new", "x\n");
    let mut files = load(&vcs);
    let b = position_of(&files, "b", Section::Unstaged);
    files[b].status = crate::diff::FileStatus::Conflicted;
    assert_eq!(
      labels(&nuke_all(&files).unwrap()),
      [
        ("a".into(), "unstaged"),
        ("b".into(), "conflicted, reset to HEAD"),
        ("c".into(), "staged"),
        ("d".into(), "staged + unstaged"),
        ("new".into(), "untracked, deleted"),
      ]
    );
  }

  #[test]
  fn a_staged_entry_is_refused() {
    let files = load(&repo());
    let refusal = restore_file(&files, position_of(&files, "c", Section::Staged)).unwrap_err();
    assert_eq!(refusal, Refusal::Staged);
    assert_eq!(
      refusal.notice(),
      Some("Staged: unstage with f before restoring")
    );
  }

  #[test]
  fn an_entry_of_a_revision_is_refused_as_read_only() {
    let mut files = load(&repo());
    for file in &mut files {
      file.section = None;
    }
    let refusal = restore_file(&files, 0).unwrap_err();
    assert_eq!(refusal, Refusal::ReadOnly);
  }

  #[test]
  fn a_conflicted_entry_can_be_restored() {
    let mut files = load(&repo());
    let at = position_of(&files, "a", Section::Unstaged);
    files[at].status = crate::diff::FileStatus::Conflicted;
    let request = restore_file(&files, at).unwrap();
    assert_eq!(request.entries.len(), 1);
  }

  #[test]
  fn an_empty_list_has_nothing_to_restore() {
    let refusal = restore_file(&[], 0).unwrap_err();
    assert_eq!(refusal, Refusal::NothingToRestore);
    assert_eq!(refusal.notice(), Some("Nothing to restore"));
  }

  #[test]
  fn a_nuke_of_an_empty_list_has_nothing_to_restore() {
    assert_eq!(nuke_all(&[]).unwrap_err(), Refusal::NothingToRestore);
  }

  #[test]
  fn a_nuke_of_a_revision_is_refused_as_read_only() {
    let mut files = load(&repo());
    for file in &mut files {
      file.section = None;
    }
    assert_eq!(nuke_all(&files).unwrap_err(), Refusal::ReadOnly);
  }

  #[test]
  fn restoring_an_unstaged_file_asks_for_that_file_alone() {
    let files = load(&repo());
    let request = restore_file(&files, position_of(&files, "a", Section::Unstaged)).unwrap();
    assert_eq!(request.kind, Kind::Restore);
    let paths: Vec<_> = request
      .entries
      .iter()
      .map(|e| e.file.path().to_owned())
      .collect();
    assert_eq!(paths, [std::path::PathBuf::from("a")]);
    assert_eq!(
      request.follow,
      Some(Follow {
        path: "b".into(),
        section: Section::Unstaged,
        hunk: None
      })
    );
  }
}
