mod git;

use crate::args::Scope;
use std::fmt;
use std::io;
use std::path::PathBuf;

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

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed { from: PathBuf },
    Copied { from: PathBuf },
    TypeChanged,
    Unmerged,
}

impl ChangeKind {
    pub fn letter(&self) -> char {
        match self {
            ChangeKind::Added => 'A',
            ChangeKind::Modified => 'M',
            ChangeKind::Deleted => 'D',
            ChangeKind::Renamed { .. } => 'R',
            ChangeKind::Copied { .. } => 'C',
            ChangeKind::TypeChanged => 'T',
            ChangeKind::Unmerged => 'U',
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: PathBuf,
    pub kind: ChangeKind,
}

/// The changed files of a repository, grouped by where the change lives.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub staged: Vec<FileChange>,
    pub unstaged: Vec<FileChange>,
    pub untracked: Vec<PathBuf>,
}

impl Status {
    /// Drops the groups outside `scope`. Untracked files count as unstaged.
    pub fn retain_scope(&mut self, scope: Scope) {
        match scope {
            Scope::All => {}
            Scope::Staged => {
                self.unstaged.clear();
                self.untracked.clear();
            }
            Scope::Unstaged => self.staged.clear(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.staged.is_empty() && self.unstaged.is_empty() && self.untracked.is_empty()
    }
}

pub trait Vcs {
    fn status(&self) -> Result<Status, Error>;
}
