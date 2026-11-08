//! Decides what a stage or unstage request touches and where the selection
//! goes once the changeset is read again. The app sends a request and applies
//! the result.

use crate::diff::{FileDiff, Section};
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
}

impl Refusal {
  /// The notice to show, if any.
  pub const fn notice(self) -> Option<&'static str> {
    match self {
      Self::NothingSelected => None,
      Self::ReadOnly => Some("read-only: revision"),
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
}
