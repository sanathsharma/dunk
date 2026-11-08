mod git;

use crate::args::Scope;
use crate::diff::{FileDiff, ParseError, Section};
use std::collections::BTreeSet;
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::io;

pub use git::Git;

#[derive(Debug)]
pub enum Error {
  Io(io::Error),
  Git(String),
  Parse(String),
}

impl fmt::Display for Error {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Error::Io(e) => write!(f, "failed to run git: {e}"),
      Error::Git(msg) => write!(f, "{msg}"),
      Error::Parse(msg) => write!(f, "unexpected git output: {msg}"),
    }
  }
}

impl std::error::Error for Error {}

impl From<ParseError> for Error {
  fn from(e: ParseError) -> Self {
    Error::Parse(e.to_string())
  }
}

impl From<io::Error> for Error {
  fn from(e: io::Error) -> Self {
    Error::Io(e)
  }
}

impl Section {
  /// The scope that selects only this section.
  pub fn scope(self) -> Scope {
    match self {
      Section::Unstaged => Scope::Unstaged,
      Section::Staged => Scope::Staged,
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
    VersionId(hasher.finish())
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

  /// Adds all of `file`'s working tree changes to the index.
  fn stage(&self, file: &FileDiff) -> Result<(), Error>;

  /// Resets `file` in the index to the commit, keeping the working tree.
  fn unstage(&self, file: &FileDiff) -> Result<(), Error>;

  /// Adds all working tree changes of `files` to the index in one git call.
  fn stage_all(&self, files: &[FileDiff]) -> Result<(), Error>;

  /// Resets `files` in the index to the commit in one git call, keeping the
  /// working tree.
  fn unstage_all(&self, files: &[FileDiff]) -> Result<(), Error>;

  /// Adds only hunk `hunk` of `file` to the index. An untracked file is first
  /// marked intent-to-add so the hunk applies like a tracked change.
  fn stage_hunk(&self, file: &FileDiff, hunk: usize) -> Result<(), Error>;

  /// Takes hunk `hunk` of a staged `file` out of the index, keeping the
  /// working tree.
  fn unstage_hunk(&self, file: &FileDiff, hunk: usize) -> Result<(), Error>;

  /// Adds only the changed lines of hunk `hunk` of `file` that fall in
  /// `lines`, a set of indexes into the hunk's lines.
  fn stage_lines(&self, file: &FileDiff, hunk: usize, lines: &BTreeSet<usize>)
  -> Result<(), Error>;

  /// Takes only the changed lines of hunk `hunk` of a staged `file` that fall
  /// in `lines` out of the index, keeping the working tree.
  fn unstage_lines(
    &self,
    file: &FileDiff,
    hunk: usize,
    lines: &BTreeSet<usize>,
  ) -> Result<(), Error>;

  /// Reads both versions of `file` as `request` compared them.
  fn sides(&self, request: &DiffRequest, file: &FileDiff) -> Sides;
}
