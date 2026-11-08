//! Decides what a stage or unstage request touches and where the selection
//! goes once the changeset is read again. The app sends a request and applies
//! the result.

use crate::diff::{FileDiff, FileStatus, Hunk, Section};
use crate::vcs::{self, Vcs};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Where the selection goes after a reload: an entry of the file list, named
/// by path and section so it can be found again, and optionally one of its
/// hunks for the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Follow {
  pub path: PathBuf,
  pub section: Section,
  pub hunk: Option<usize>,
}

/// What a stage or unstage moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
  File,
  Hunk(usize),
  /// The lines of a hunk at these indexes into the hunk's lines.
  Lines(usize, BTreeSet<usize>),
  /// Every file of the list that is in the section the request moves out of.
  All(Vec<FileDiff>),
}

/// A stage or unstage of a file, a hunk or some of its lines waiting to run.
pub struct IndexRequest {
  pub file: FileDiff,
  pub target: Target,
  /// True to stage, false to unstage.
  pub stage: bool,
  pub follow: Option<Follow>,
}

impl IndexRequest {
  /// Writes the index as the request asks.
  pub fn run(&self, vcs: &(impl Vcs + ?Sized)) -> Result<(), vcs::Error> {
    let file = &self.file;
    match (&self.target, self.stage) {
      (Target::File, true) => vcs.stage(file),
      (Target::File, false) => vcs.unstage(file),
      (Target::All(files), true) => vcs.stage_all(files),
      (Target::All(files), false) => vcs.unstage_all(files),
      (Target::Hunk(hunk), true) => vcs.stage_hunk(file, *hunk),
      (Target::Hunk(hunk), false) => vcs.unstage_hunk(file, *hunk),
      (Target::Lines(hunk, lines), true) => vcs.stage_lines(file, *hunk, lines),
      (Target::Lines(hunk, lines), false) => vcs.unstage_lines(file, *hunk, lines),
    }
  }
}

/// Why no request was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
  /// There is no selected entry, so there is nothing to say.
  NothingSelected,
  /// The changeset is a revision, which is read-only.
  ReadOnly,
  /// A conflicted file has no hunks to stage on their own.
  Conflicted,
  /// A renamed or copied file has no hunks to stage on their own.
  RenamedOrCopied,
  /// The cursor is not on a hunk.
  NoHunk,
  /// A selection holds no added or removed line.
  NoChangedLines,
}

impl Refusal {
  /// The notice to show, if any.
  pub const fn notice(self) -> Option<&'static str> {
    match self {
      Self::NothingSelected => None,
      Self::ReadOnly => Some("read-only: revision"),
      Self::Conflicted => Some("Cannot stage hunks of a conflicted file, use f instead"),
      Self::RenamedOrCopied => {
        Some("Cannot stage hunks of a renamed or copied file, use f instead")
      }
      Self::NoHunk => Some("No hunk under the cursor"),
      Self::NoChangedLines => Some("no changed lines selected"),
    }
  }
}

/// The selected entry and its section, or why there is no request to make.
fn entry(files: &[FileDiff], selected: usize) -> Result<(&FileDiff, Section), Refusal> {
  let file = files.get(selected).ok_or(Refusal::NothingSelected)?;
  let section = file.section.ok_or(Refusal::ReadOnly)?;
  Ok((file, section))
}

/// The entry after the selected one in its section, or the one before it
/// when the selected entry is the last.
pub fn neighbour(files: &[FileDiff], selected: usize) -> Option<Follow> {
  let section = files.get(selected)?.section?;
  let in_section = |i: &usize| files.get(*i).is_some_and(|f| f.section == Some(section));
  let next = (selected.saturating_add(1)..files.len()).find(in_section);
  let previous = (0..selected).rev().find(in_section);
  let at = next.or(previous)?;
  Some(Follow {
    path: files.get(at)?.path().to_path_buf(),
    section,
    hunk: None,
  })
}

/// Stages the selected file, or unstages it when it is in the staged section.
/// The selection moves to its neighbour in the section it leaves.
pub fn toggle_file(files: &[FileDiff], selected: usize) -> Result<IndexRequest, Refusal> {
  let (file, section) = entry(files, selected)?;
  Ok(IndexRequest {
    file: file.clone(),
    target: Target::File,
    stage: section == Section::Unstaged,
    follow: neighbour(files, selected),
  })
}

/// Stages every unstaged entry, or unstages every staged entry when none is
/// left to stage. The selection follows its file into the section the file
/// moves to.
pub fn toggle_all(files: &[FileDiff], selected: usize) -> Result<IndexRequest, Refusal> {
  let (file, _) = entry(files, selected)?;
  let stage = files.iter().any(|f| f.section == Some(Section::Unstaged));
  let (from, to) = if stage {
    (Section::Unstaged, Section::Staged)
  } else {
    (Section::Staged, Section::Unstaged)
  };
  let moving = files
    .iter()
    .filter(|f| f.section == Some(from))
    .cloned()
    .collect();
  Ok(IndexRequest {
    file: file.clone(),
    target: Target::All(moving),
    stage,
    follow: Some(Follow {
      path: file.path().to_path_buf(),
      section: to,
      hunk: Some(0),
    }),
  })
}

/// Stages hunk `hunk` of the selected entry, or unstages it when the entry is
/// in the staged section. With `lines`, only those lines of the hunk go.
///
/// When the whole hunk goes, it leaves the entry, so the next hunk takes its
/// index. After the last hunk the selection moves to the next file in the
/// section, or the previous hunk if there is none. When part of the hunk is
/// left, the cursor stays on it.
pub fn toggle_hunk(
  files: &[FileDiff],
  selected: usize,
  hunk: Option<usize>,
  lines: Option<BTreeSet<usize>>,
) -> Result<IndexRequest, Refusal> {
  let (file, section) = entry(files, selected)?;
  match file.status {
    FileStatus::Conflicted => return Err(Refusal::Conflicted),
    FileStatus::Renamed | FileStatus::Copied => return Err(Refusal::RenamedOrCopied),
    _ => {}
  }
  let hunk = hunk.ok_or(Refusal::NoHunk)?;
  let changed = file
    .hunks
    .get(hunk)
    .map(Hunk::changed)
    .ok_or(Refusal::NoHunk)?;
  if let Some(lines) = &lines
    && !changed.clone().any(|i| lines.contains(&i))
  {
    return Err(Refusal::NoChangedLines);
  }
  let leaves = lines
    .as_ref()
    .is_none_or(|lines| changed.clone().all(|i| lines.contains(&i)));
  let in_entry = |hunk| Follow {
    path: file.path().to_path_buf(),
    section,
    hunk: Some(hunk),
  };
  let follow = if !leaves || hunk.saturating_add(1) < file.hunks.len() {
    Some(in_entry(hunk))
  } else {
    neighbour(files, selected).or_else(|| hunk.checked_sub(1).map(in_entry))
  };
  Ok(IndexRequest {
    file: file.clone(),
    target: lines.map_or(Target::Hunk(hunk), |lines| Target::Lines(hunk, lines)),
    stage: section == Section::Unstaged,
    follow,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::vcs::{DiffRequest, Memory};

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

  fn load(vcs: &Memory) -> Vec<FileDiff> {
    vcs.changeset(&DiffRequest::default()).unwrap()
  }

  fn position_of(files: &[FileDiff], name: &str, section: Section) -> usize {
    files
      .iter()
      .position(|f| f.path().to_str() == Some(name) && f.section == Some(section))
      .unwrap()
  }

  #[allow(clippy::unnecessary_wraps)]
  fn follow(name: &str, section: Section, hunk: Option<usize>) -> Option<Follow> {
    Some(Follow {
      path: name.into(),
      section,
      hunk,
    })
  }

  #[test]
  fn toggling_an_unstaged_file_stages_it_and_follows_its_neighbour() {
    let vcs = repo();
    let files = load(&vcs);
    let request = toggle_file(&files, position_of(&files, "a", Section::Unstaged)).unwrap();
    assert!(request.stage);
    assert_eq!(request.follow, follow("b", Section::Unstaged, None));
    request.run(&vcs).unwrap();
    let after = load(&vcs);
    assert_eq!(
      after[position_of(&after, "a", Section::Staged)].section,
      Some(Section::Staged)
    );
  }

  #[test]
  fn the_last_file_of_a_section_follows_the_one_before_it() {
    let files = load(&repo());
    let request = toggle_file(&files, position_of(&files, "b", Section::Unstaged)).unwrap();
    assert_eq!(request.follow, follow("a", Section::Unstaged, None));
  }

  #[test]
  fn the_only_file_of_a_section_has_nothing_to_follow() {
    let files = load(&repo());
    let request = toggle_file(&files, position_of(&files, "c", Section::Staged)).unwrap();
    assert!(!request.stage);
    assert_eq!(request.follow, None);
  }

  #[test]
  fn a_neighbour_is_never_taken_from_the_other_section() {
    let files = load(&repo());
    let request = toggle_file(&files, position_of(&files, "c", Section::Staged)).unwrap();
    assert_eq!(request.follow, None);
    assert_eq!(
      neighbour(&files, position_of(&files, "a", Section::Unstaged)),
      follow("b", Section::Unstaged, None)
    );
  }

  #[test]
  fn toggling_all_stages_every_unstaged_file_while_one_is_left() {
    let vcs = repo();
    let files = load(&vcs);
    let chosen = position_of(&files, "b", Section::Unstaged);
    let request = toggle_all(&files, chosen).unwrap();
    assert!(request.stage);
    let Target::All(moving) = &request.target else {
      panic!("expected Target::All");
    };
    assert_eq!(moving.len(), 2);
    assert!(moving.iter().all(|f| f.section == Some(Section::Unstaged)));
    assert_eq!(request.follow, follow("b", Section::Staged, Some(0)));
    request.run(&vcs).unwrap();
    assert!(
      load(&vcs)
        .iter()
        .all(|f| f.section == Some(Section::Staged))
    );
  }

  #[test]
  fn toggling_all_unstages_every_file_when_none_is_unstaged() {
    let vcs = repo().staged("a").staged("b");
    let files = load(&vcs);
    let request = toggle_all(&files, 1).unwrap();
    assert!(!request.stage);
    let Target::All(moving) = &request.target else {
      panic!("expected Target::All");
    };
    assert_eq!(moving.len(), 3);
    assert_eq!(
      request.follow.as_ref().map(|f| f.section),
      Some(Section::Unstaged)
    );
    request.run(&vcs).unwrap();
    assert!(
      load(&vcs)
        .iter()
        .all(|f| f.section == Some(Section::Unstaged))
    );
  }

  #[test]
  fn a_revision_entry_is_read_only() {
    let mut files = load(&repo());
    files[0].section = None;
    for result in [toggle_file(&files, 0), toggle_all(&files, 0)] {
      assert_eq!(result.err(), Some(Refusal::ReadOnly));
    }
    assert_eq!(Refusal::ReadOnly.notice(), Some("read-only: revision"));
  }

  #[test]
  fn with_nothing_selected_there_is_no_request_and_no_notice() {
    for result in [toggle_file(&[], 0), toggle_all(&[], 0)] {
      let refusal = result.err().unwrap();
      assert_eq!(refusal, Refusal::NothingSelected);
      assert_eq!(refusal.notice(), None);
    }
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

  fn hunk_follow(request: &IndexRequest) -> Option<(String, Section, Option<usize>)> {
    request
      .follow
      .as_ref()
      .map(|f| (f.path.display().to_string(), f.section, f.hunk))
  }

  #[test]
  fn a_hunk_that_goes_hands_its_index_to_the_next_one() {
    let files = load(&hunk_repo());
    let request = toggle_hunk(&files, 0, Some(0), None).unwrap();
    assert_eq!(request.target, Target::Hunk(0));
    assert!(request.stage);
    assert_eq!(
      hunk_follow(&request),
      Some(("f".into(), Section::Unstaged, Some(0)))
    );
  }

  #[test]
  fn the_last_hunk_hands_the_cursor_to_the_next_file_in_the_section() {
    let files = load(&hunk_repo());
    let request = toggle_hunk(&files, 0, Some(1), None).unwrap();
    assert_eq!(
      hunk_follow(&request),
      Some(("g".into(), Section::Unstaged, None))
    );
  }

  #[test]
  fn the_last_hunk_of_an_only_file_falls_back_to_the_hunk_before() {
    let vcs = hunk_repo();
    let files: Vec<_> = load(&vcs).into_iter().take(1).collect();
    let request = toggle_hunk(&files, 0, Some(1), None).unwrap();
    assert_eq!(
      hunk_follow(&request),
      Some(("f".into(), Section::Unstaged, Some(0)))
    );
  }

  #[test]
  fn part_of_a_hunk_leaves_the_cursor_on_it() {
    let files = load(&hunk_repo());
    let changed: Vec<usize> = files[0].hunks[1].changed().collect();
    let some = BTreeSet::from([changed[0]]);
    let request = toggle_hunk(&files, 0, Some(1), Some(some.clone())).unwrap();
    assert_eq!(request.target, Target::Lines(1, some));
    assert_eq!(
      hunk_follow(&request),
      Some(("f".into(), Section::Unstaged, Some(1)))
    );
    let all = changed.into_iter().collect();
    let request = toggle_hunk(&files, 0, Some(1), Some(all)).unwrap();
    assert_eq!(
      hunk_follow(&request),
      Some(("g".into(), Section::Unstaged, None))
    );
  }

  #[test]
  fn a_staged_entry_unstages_its_hunk() {
    let vcs = hunk_repo().staged("f");
    let files = load(&vcs);
    let staged = position_of(&files, "f", Section::Staged);
    let request = toggle_hunk(&files, staged, Some(0), None).unwrap();
    assert!(!request.stage);
    request.run(&vcs).unwrap();
    let after = load(&vcs);
    assert_eq!(
      after[position_of(&after, "f", Section::Staged)].hunks.len(),
      1
    );
    assert_eq!(
      after[position_of(&after, "f", Section::Unstaged)]
        .hunks
        .len(),
      1
    );
  }

  #[test]
  fn staging_a_hunk_writes_only_that_hunk() {
    let vcs = hunk_repo();
    let files = load(&vcs);
    toggle_hunk(&files, 0, Some(1), None)
      .unwrap()
      .run(&vcs)
      .unwrap();
    let after = load(&vcs);
    let staged = after
      .iter()
      .find(|f| f.section == Some(Section::Staged))
      .unwrap();
    assert_eq!(staged.hunks.len(), 1);
    let rest = after
      .iter()
      .find(|f| f.path().to_str() == Some("f") && f.section == Some(Section::Unstaged))
      .unwrap();
    assert_eq!(rest.hunks.len(), 1);
  }

  #[test]
  fn hunks_are_refused_for_conflicted_renamed_and_copied_files() {
    for (status, refusal) in [
      (FileStatus::Conflicted, Refusal::Conflicted),
      (FileStatus::Renamed, Refusal::RenamedOrCopied),
      (FileStatus::Copied, Refusal::RenamedOrCopied),
    ] {
      let mut files = load(&hunk_repo());
      files[0].status = status;
      assert_eq!(toggle_hunk(&files, 0, Some(0), None).err(), Some(refusal));
    }
  }

  #[test]
  fn hunks_are_refused_without_a_hunk_or_changed_lines_or_in_a_revision() {
    let mut files = load(&hunk_repo());
    assert_eq!(
      toggle_hunk(&files, 0, None, None).err(),
      Some(Refusal::NoHunk)
    );
    assert_eq!(
      toggle_hunk(&files, 0, Some(9), None).err(),
      Some(Refusal::NoHunk)
    );
    let context_only = BTreeSet::from([0]);
    assert_eq!(
      toggle_hunk(&files, 0, Some(0), Some(context_only)).err(),
      Some(Refusal::NoChangedLines)
    );
    assert_eq!(
      toggle_hunk(&[], 0, Some(0), None).err(),
      Some(Refusal::NothingSelected)
    );
    files[0].section = None;
    assert_eq!(
      toggle_hunk(&files, 0, Some(0), None).err(),
      Some(Refusal::ReadOnly)
    );
  }

  #[test]
  fn staging_lines_of_an_untracked_file_and_taking_them_back() {
    let vcs = Memory::default().edit("n", "a\nb\nc\n");
    let files = load(&vcs);
    let changed: Vec<usize> = files[0].hunks[0].changed().collect();
    let first_two = BTreeSet::from([changed[0], changed[1]]);

    let request = toggle_hunk(&files, 0, Some(0), Some(first_two.clone())).unwrap();
    assert!(request.stage);
    request.run(&vcs).unwrap();
    assert_eq!(vcs.index_text("n").as_deref(), Some("a\nb\n"));

    let files = load(&vcs);
    let staged = position_of(&files, "n", Section::Staged);
    let request = toggle_hunk(&files, staged, Some(0), Some(first_two)).unwrap();
    assert!(!request.stage);
    request.run(&vcs).unwrap();
    assert_eq!(vcs.index_text("n"), None);
  }
}
