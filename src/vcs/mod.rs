mod git;
#[cfg(test)]
mod memory;

use crate::args::Scope;
use crate::diff::{FileDiff, ParseError, Section};
use std::collections::BTreeSet;
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::io;

pub use git::Git;
#[cfg(test)]
pub use memory::Memory;

#[derive(Debug)]
pub enum Error {
  Io(io::Error),
  Git(String),
  Parse(String),
}

impl fmt::Display for Error {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Io(e) => write!(f, "failed to run git: {e}"),
      Self::Git(msg) => write!(f, "{msg}"),
      Self::Parse(msg) => write!(f, "unexpected git output: {msg}"),
    }
  }
}

impl std::error::Error for Error {}

impl From<ParseError> for Error {
  fn from(e: ParseError) -> Self {
    Self::Parse(e.to_string())
  }
}

impl From<io::Error> for Error {
  fn from(e: io::Error) -> Self {
    Self::Io(e)
  }
}

impl Section {
  /// The scope that selects only this section.
  pub const fn scope(self) -> Scope {
    match self {
      Self::Unstaged => Scope::Unstaged,
      Self::Staged => Scope::Staged,
    }
  }
}

/// What changeset to diff. With revisions the working tree scope is ignored.
#[derive(Debug, Default, Clone)]
pub struct DiffRequest {
  pub scope: Scope,
  pub revisions: Vec<String>,
  pub paths: Vec<String>,
  pub context: Option<u32>,
}

/// Names one version of a file by a hash of its contents, so its highlighting
/// can be reused wherever the same bytes show up, such as a working-tree file
/// that is then staged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VersionId(pub u64);

impl VersionId {
  pub fn of(bytes: &[u8]) -> Self {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    Self(hasher.finish())
  }
}

/// The full contents of a file on each side of a diff. A side is `None` when
/// the file does not exist there or cannot be read. An ID is `None` when the
/// side has no contents or the backend cannot name the version.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Sides {
  pub old: Option<Vec<u8>>,
  pub new: Option<Vec<u8>>,
  pub old_id: Option<VersionId>,
  pub new_id: Option<VersionId>,
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

pub trait Vcs {
  fn diff(&self, request: &DiffRequest) -> Result<Vec<FileDiff>, Error>;

  /// The entries to list for `request`. A working tree request gives the
  /// unstaged entries, then the staged ones, each tagged with its section and
  /// limited by the scope. Revisions give one untagged list.
  fn changeset(&self, request: &DiffRequest) -> Result<Vec<FileDiff>, Error> {
    if !request.revisions.is_empty() {
      return self.diff(request);
    }
    let mut entries = Vec::new();
    for section in [Section::Unstaged, Section::Staged] {
      if request.scope != Scope::All && request.scope != section.scope() {
        continue;
      }
      let part = DiffRequest {
        scope: section.scope(),
        ..request.clone()
      };
      entries.extend(self.diff(&part)?.into_iter().map(|mut f| {
        f.section = Some(section);
        f
      }));
    }
    Ok(entries)
  }

  /// Writes the index as `target` of `file` says: staging it when `stage`
  /// is true, otherwise unstaging it and keeping the working tree. An
  /// untracked file can be staged by hunk or by line.
  fn update_index(&self, file: &FileDiff, target: &Target, stage: bool) -> Result<(), Error>;

  /// Reads both versions of `file` as `request` compared them.
  fn sides(&self, request: &DiffRequest, file: &FileDiff) -> Sides;
}
