//! Decides what a restore or a nuke touches and where the selection goes once
//! the changeset is read again. The app sends a request and applies the
//! result.

use crate::diff::{FileDiff, FileStatus, Section};
use crate::staging::{self, Follow, Refusal};
use crate::vcs::{self, Kind, Vcs};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
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
  /// A hunk restore takes this many changed lines.
  Hunk(usize),
  /// A selection restore takes this many changed lines.
  Lines(usize),
}

impl Loss {
  /// The words the confirmation shows next to the file.
  pub fn label(self) -> Cow<'static, str> {
    let plural = |n: usize| if n == 1 { "line" } else { "lines" };
    match self {
      Self::Unstaged => "unstaged".into(),
      Self::Staged => "staged".into(),
      Self::StagedAndUnstaged => "staged + unstaged".into(),
      Self::Untracked => "untracked, deleted".into(),
      Self::ConflictRestored => "conflicted, markers recreated".into(),
      Self::ConflictReset => "conflicted, reset to HEAD".into(),
      Self::Hunk(n) => format!("hunk, {n} {}", plural(n)).into(),
      Self::Lines(n) => format!("{n} selected {}", plural(n)).into(),
    }
  }
}

/// A file a request touches and what it loses.
#[derive(Debug, Clone)]
pub struct Entry {
  pub file: FileDiff,
  pub loss: Loss,
}

/// The part of one file a restore takes: a hunk, or only some of its lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
  pub hunk: usize,
  /// Indexes into the hunk's lines. `None` takes the whole hunk.
  pub lines: Option<BTreeSet<usize>>,
}

/// A restore waiting to run. The entries are the ones the confirmation
/// listed, captured when it opened.
#[derive(Debug, Clone)]
pub struct RestoreRequest {
  pub kind: Kind,
  pub entries: Vec<Entry>,
  /// Set when the restore takes a hunk or some lines of the only entry,
  /// instead of whole files.
  pub part: Option<Part>,
  pub follow: Option<Follow>,
}

impl RestoreRequest {
  /// The files to hand to the version control system.
  pub fn files(&self) -> Vec<FileDiff> {
    self.entries.iter().map(|e| e.file.clone()).collect()
  }

  /// Throws the changes away as the request says.
  pub fn run(&self, vcs: &(impl Vcs + ?Sized)) -> Result<(), vcs::Error> {
    match (&self.part, self.entries.first()) {
      (Some(part), Some(entry)) => vcs.restore_lines(&entry.file, part.hunk, part.lines.as_ref()),
      _ => vcs.restore(&self.files(), self.kind),
    }
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
    part: None,
    follow: staging::neighbour(files, selected),
  })
}

/// Restores hunk `hunk` of the selected file, or only the lines `lines` of it,
/// which are indexes into the hunk's lines.
///
/// A conflicted file and a renamed or copied one are refused, since they have
/// no hunks of their own to put back. The cursor follows as it does after
/// staging a hunk: it stays on a hunk that is only partly restored, and moves
/// on when the whole hunk goes.
pub fn restore_hunk(
  files: &[FileDiff],
  selected: usize,
  hunk: Option<usize>,
  lines: Option<BTreeSet<usize>>,
) -> Result<RestoreRequest, Refusal> {
  let file = files.get(selected).ok_or(Refusal::NothingToRestore)?;
  match file.section.ok_or(Refusal::ReadOnly)? {
    Section::Staged => return Err(Refusal::Staged),
    Section::Unstaged => {}
  }
  match file.status {
    FileStatus::Conflicted => return Err(Refusal::RestoreConflicted),
    FileStatus::Renamed | FileStatus::Copied => return Err(Refusal::RestoreRenamed),
    _ => {}
  }
  let hunk = hunk.ok_or(Refusal::NoHunk)?;
  let changed: Vec<usize> = file
    .hunks
    .get(hunk)
    .ok_or(Refusal::NoHunk)?
    .changed()
    .collect();
  let taken = lines.as_ref().map_or(changed.len(), |lines| {
    changed.iter().filter(|i| lines.contains(i)).count()
  });
  if taken == 0 {
    return Err(Refusal::NoChangedLines);
  }
  let loss = if lines.is_some() {
    Loss::Lines(taken)
  } else {
    Loss::Hunk(taken)
  };
  let leaves = taken == changed.len();
  Ok(RestoreRequest {
    kind: Kind::Restore,
    entries: vec![Entry {
      file: file.clone(),
      loss,
    }],
    part: Some(Part { hunk, lines }),
    follow: staging::follow_after(files, selected, Section::Unstaged, hunk, leaves),
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
    part: None,
    follow: None,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::diff::Section;
  use crate::staging::Follow;
  use crate::vcs::{DiffRequest, Memory, Vcs};
  use std::collections::BTreeSet;

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

  fn labels(request: &RestoreRequest) -> Vec<(String, String)> {
    request
      .entries
      .iter()
      .map(|e| {
        (
          e.file.path().display().to_string(),
          e.loss.label().into_owned(),
        )
      })
      .collect()
  }

  #[test]
  fn a_restore_says_what_the_file_loses() {
    let vcs = repo().edit("new", "x\n");
    let files = load(&vcs);
    let at = |name| position_of(&files, name, Section::Unstaged);
    let label = |name| labels(&restore_file(&files, at(name)).unwrap());
    assert_eq!(label("a"), [("a".into(), "unstaged".into())]);
    assert_eq!(label("new"), [("new".into(), "untracked, deleted".into())]);
    let mut conflicted = files.clone();
    conflicted[at("b")].status = crate::diff::FileStatus::Conflicted;
    assert_eq!(
      labels(&restore_file(&conflicted, at("b")).unwrap()),
      [("b".into(), "conflicted, markers recreated".into())]
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
        ("a".into(), "unstaged".into()),
        ("b".into(), "conflicted, reset to HEAD".into()),
        ("c".into(), "staged".into()),
        ("d".into(), "staged + unstaged".into()),
        ("new".into(), "untracked, deleted".into()),
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
  /// `f` has hunks at lines 2 and 19, `g` one at line 2.
  fn hunk_repo() -> Memory {
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
      .commit("g", &text("2", "19"))
      .edit("f", &text("TWO", "NINETEEN"))
      .edit("g", &text("TWO", "19"))
  }

  fn follow_of(request: &RestoreRequest) -> Option<(String, Option<usize>)> {
    request
      .follow
      .as_ref()
      .map(|f| (f.path.display().to_string(), f.hunk))
  }

  #[test]
  fn a_hunk_restore_names_the_file_and_how_many_lines_it_takes() {
    let files = load(&hunk_repo());
    let request = restore_hunk(&files, 0, Some(1), None).unwrap();
    assert_eq!(request.kind, Kind::Restore);
    assert_eq!(labels(&request), [("f".into(), "hunk, 2 lines".into())]);
    assert_eq!(
      request.part,
      Some(Part {
        hunk: 1,
        lines: None
      })
    );
  }

  #[test]
  fn a_selection_restore_counts_only_the_changed_lines_it_covers() {
    let files = load(&hunk_repo());
    let changed: Vec<usize> = files[0].hunks[0].changed().collect();
    let context = (0..files[0].hunks[0].lines.len())
      .find(|i| !changed.contains(i))
      .unwrap();
    let picked = BTreeSet::from([changed[0], context]);
    let request = restore_hunk(&files, 0, Some(0), Some(picked)).unwrap();
    assert_eq!(labels(&request), [("f".into(), "1 selected line".into())]);
  }

  #[test]
  fn a_hunk_that_goes_hands_the_cursor_on_and_part_of_one_keeps_it() {
    let files = load(&hunk_repo());
    let whole = restore_hunk(&files, 0, Some(0), None).unwrap();
    assert_eq!(follow_of(&whole), Some(("f".into(), Some(0))));
    let last = restore_hunk(&files, 0, Some(1), None).unwrap();
    assert_eq!(follow_of(&last), Some(("g".into(), None)));
    let changed: Vec<usize> = files[0].hunks[1].changed().collect();
    let some = BTreeSet::from([changed[0]]);
    let part = restore_hunk(&files, 0, Some(1), Some(some)).unwrap();
    assert_eq!(follow_of(&part), Some(("f".into(), Some(1))));
  }

  #[test]
  fn a_hunk_restore_is_refused_for_staged_read_only_conflicted_and_renamed_entries() {
    let files = load(&repo());
    let staged = position_of(&files, "c", Section::Staged);
    assert_eq!(
      restore_hunk(&files, staged, Some(0), None).unwrap_err(),
      Refusal::Staged
    );
    let mut revision = files.clone();
    revision[0].section = None;
    assert_eq!(
      restore_hunk(&revision, 0, Some(0), None).unwrap_err(),
      Refusal::ReadOnly
    );
    for (status, refusal) in [
      (FileStatus::Conflicted, Refusal::RestoreConflicted),
      (FileStatus::Renamed, Refusal::RestoreRenamed),
      (FileStatus::Copied, Refusal::RestoreRenamed),
    ] {
      let mut files = files.clone();
      files[0].status = status;
      assert_eq!(restore_hunk(&files, 0, Some(0), None).unwrap_err(), refusal);
    }
    assert_eq!(
      Refusal::RestoreConflicted.notice(),
      Some("Cannot restore hunks of a conflicted file, use rf instead")
    );
    assert_eq!(
      restore_hunk(&[], 0, Some(0), None).unwrap_err(),
      Refusal::NothingToRestore
    );
  }

  #[test]
  fn a_hunk_restore_needs_a_hunk_and_a_changed_line() {
    let files = load(&hunk_repo());
    assert_eq!(
      restore_hunk(&files, 0, None, None).unwrap_err(),
      Refusal::NoHunk
    );
    assert_eq!(
      restore_hunk(&files, 0, Some(9), None).unwrap_err(),
      Refusal::NoHunk
    );
    let changed: Vec<usize> = files[0].hunks[0].changed().collect();
    let context = (0..files[0].hunks[0].lines.len())
      .find(|i| !changed.contains(i))
      .unwrap();
    for lines in [BTreeSet::new(), BTreeSet::from([context])] {
      let refusal = restore_hunk(&files, 0, Some(0), Some(lines)).unwrap_err();
      assert_eq!(refusal, Refusal::NoChangedLines);
    }
  }

  #[test]
  fn running_a_hunk_restore_changes_the_working_tree_and_not_the_index() {
    let vcs = hunk_repo();
    let files = load(&vcs);
    restore_hunk(&files, 0, Some(1), None)
      .unwrap()
      .run(&vcs)
      .unwrap();
    let work = vcs.work_text("f").unwrap();
    assert!(work.contains("TWO") && work.contains("\n19\n"), "{work}");
    assert_eq!(vcs.index_text("f"), vcs.index_text("g"));
    let after = load(&vcs);
    assert_eq!(
      after[position_of(&after, "f", Section::Unstaged)]
        .hunks
        .len(),
      1
    );
  }

  #[test]
  fn running_a_selection_restore_takes_only_the_selected_lines() {
    let vcs = Memory::default()
      .commit("a", "1\n2\n3\n")
      .edit("a", "1\nX\nY\n3\n");
    let files = load(&vcs);
    let lines: BTreeSet<usize> = files[0].hunks[0]
      .lines
      .iter()
      .enumerate()
      .filter(|(_, l)| l.content == b"Y")
      .map(|(i, _)| i)
      .collect();
    restore_hunk(&files, 0, Some(0), Some(lines))
      .unwrap()
      .run(&vcs)
      .unwrap();
    assert_eq!(vcs.work_text("a").as_deref(), Some("1\nX\n3\n"));
  }

  #[test]
  fn restoring_every_line_of_an_untracked_file_leaves_it_empty() {
    let vcs = Memory::default().commit("a", "1\n").edit("n", "x\ny\n");
    let files = load(&vcs);
    let at = position_of(&files, "n", Section::Unstaged);
    restore_hunk(&files, at, Some(0), None)
      .unwrap()
      .run(&vcs)
      .unwrap();
    assert_eq!(vcs.work_text("n").as_deref(), Some(""));
  }
}
