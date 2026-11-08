use super::{DiffRequest, Error, Sides, Target, Vcs, VersionId};
use crate::args::Scope;
use crate::diff::{FileDiff, FileStatus, Section, parse};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

pub struct Git {
  dir: PathBuf,
}

impl Git {
  /// Runs git as if started in `dir`; git finds the repository root itself.
  pub fn new(dir: impl Into<PathBuf>) -> Self {
    Self { dir: dir.into() }
  }

  fn command<I, S>(dir: &Path, args: I) -> Result<Output, Error>
  where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
  {
    let out = Command::new("git")
      .args(GIT_FLAGS)
      .args(args)
      .current_dir(dir)
      .output()?;
    Ok(out)
  }

  /// Like `command`, with `input` on the standard input.
  fn command_with_input<I, S>(dir: &Path, args: I, input: &[u8]) -> Result<Output, Error>
  where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
  {
    let mut child = Command::new("git")
      .args(GIT_FLAGS)
      .args(args)
      .current_dir(dir)
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()?;
    // Dropping the handle closes the pipe so git sees the end of the input.
    // Git can exit before it reads everything, and its stderr then says why,
    // so a failed write is not an error here.
    if let Some(mut stdin) = child.stdin.take() {
      let _ = stdin.write_all(input);
    }
    Ok(child.wait_with_output()?)
  }

  fn run<I, S>(&self, args: I) -> Result<Vec<u8>, Error>
  where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
  {
    let out = Self::command(&self.dir, args)?;
    if !out.status.success() {
      return Err(git_error(&out));
    }
    Ok(out.stdout)
  }

  /// The top level directory of the work tree containing the start directory.
  pub fn root(&self) -> Result<PathBuf, Error> {
    let out = self.run(["rev-parse", "--show-toplevel"])?;
    Ok(path(out.strip_suffix(b"\n").unwrap_or(&out)))
  }

  /// The commit to compare the working tree with. An unborn branch has no
  /// HEAD, so it compares with the empty tree.
  fn head(&self) -> Result<String, Error> {
    let probe = Self::command(&self.dir, ["rev-parse", "--verify", "-q", "HEAD"])?;
    if probe.status.success() {
      return Ok("HEAD".into());
    }
    let empty = self.run(["hash-object", "-t", "tree", "/dev/null"])?;
    Ok(String::from_utf8_lossy(&empty).trim().to_string())
  }

  /// Reads a file from a commit, the index or the working tree, with an ID
  /// for that version. The path is relative to the repository root.
  fn read(&self, root: &Path, source: &Source, file: &Path) -> Option<(Vec<u8>, VersionId)> {
    let spec = |prefix: &str| {
      let mut spec = std::ffi::OsString::from(prefix);
      spec.push(file);
      spec
    };
    let spec = match source {
      Source::WorkingTree => {
        let bytes = std::fs::read(root.join(file)).ok()?;
        let id = VersionId::of(&bytes);
        return Some((bytes, id));
      }
      Source::Index => spec(":"),
      Source::Commit(rev) => spec(&format!("{rev}:")),
    };
    let oid = self
      .run([OsStr::new("rev-parse"), OsStr::new("--verify"), &spec])
      .ok()?;
    let oid = String::from_utf8_lossy(&oid).trim().to_string();
    let bytes = self.run(["cat-file", "blob", &oid]).ok()?;
    let id = VersionId::of(&bytes);
    Some((bytes, id))
  }

  /// Runs a git command that writes the index, with the paths taken
  /// literally. It runs at the repository root because the paths of a
  /// `FileDiff` are relative to it.
  fn write_index(&self, args: &[&str], file: &FileDiff) -> Result<(), Error> {
    self.write_index_all(args, std::slice::from_ref(file))
  }

  /// Like `write_index`, for every file of `files` in one git call.
  fn write_index_all(&self, args: &[&str], files: &[FileDiff]) -> Result<(), Error> {
    if files.is_empty() {
      return Ok(());
    }
    let root = self.root()?;
    let mut paths = Vec::new();
    for file in files {
      paths.push(file.path());
      if file.status == FileStatus::Renamed {
        paths.extend(file.old_path.as_deref());
      }
    }
    let mut full: Vec<&OsStr> = vec!["--literal-pathspecs".as_ref()];
    full.extend(args.iter().map(OsStr::new));
    full.push("--".as_ref());
    full.extend(paths.iter().map(|p| p.as_os_str()));
    let out = Self::command(&root, full)?;
    if !out.status.success() {
      return Err(git_error(&out));
    }
    Ok(())
  }

  /// An unstaged added file is untracked or already intent-to-add, and
  /// `add -N` is harmless for the second. Either way its hunks then apply
  /// like those of a tracked file.
  fn mark_intent_to_add(&self, file: &FileDiff) -> Result<(), Error> {
    if file.status == FileStatus::Added {
      self.write_index(&["add", "-N"], file)?;
    }
    Ok(())
  }

  /// Applies `patch` to the index only. It runs at the repository root because patch paths are relative to it.
  /// `--recount` trusts the lines over the header counts, and `--unidiff-zero`
  /// lets a patch without context apply.
  fn apply_to_index(&self, patch: &[u8]) -> Result<(), Error> {
    let root = self.root()?;
    let args = ["apply", "--cached", "--recount", "--unidiff-zero", "-"];
    let out = Self::command_with_input(&root, args, patch)?;
    if !out.status.success() {
      return Err(git_error(&out));
    }
    Ok(())
  }

  /// Files with unresolved merge conflicts. `git diff` prints these as a
  /// combined diff the parser skips, so they are listed without hunks.
  fn unmerged(&self, req: &DiffRequest) -> Result<Vec<FileDiff>, Error> {
    let mut ls = vec!["ls-files", "--unmerged", "-z", "--full-name", "--"];
    if req.paths.is_empty() {
      ls.push(":/");
    }
    ls.extend(req.paths.iter().map(String::as_str));
    let listing = self.run(ls)?;
    let mut files: Vec<FileDiff> = Vec::new();
    // Each line is `<mode> <object> <stage>\t<path>` and a path repeats once
    // per stage.
    for entry in listing.split(|&b| b == 0).filter(|e| !e.is_empty()) {
      let Some(tab) = entry.iter().position(|&b| b == b'\t') else {
        continue;
      };
      let Some(name) = entry.get(tab.saturating_add(1)..).map(path) else {
        continue;
      };
      if files.iter().any(|f| f.path() == name) {
        continue;
      }
      files.push(FileDiff {
        old_path: Some(name.clone()),
        new_path: Some(name),
        status: FileStatus::Conflicted,
        binary: false,
        section: None,
        hunks: Vec::new(),
      });
    }
    Ok(files)
  }

  /// Resolves `a...b` to the merge base of `a` and `b` on the old side.
  fn resolve_range_base(&self, req: &DiffRequest, old: Source) -> Source {
    let [range] = req.revisions.as_slice() else {
      return old;
    };
    let Some((a, b)) = range.split_once("...") else {
      return old;
    };
    let a = if a.is_empty() { "HEAD" } else { a };
    let b = if b.is_empty() { "HEAD" } else { b };
    self.run(["merge-base", a, b]).map_or(old, |out| {
      Source::Commit(String::from_utf8_lossy(&out).trim().to_string())
    })
  }

  fn tracked_diff(&self, req: &DiffRequest) -> Result<Vec<FileDiff>, Error> {
    let mut args: Vec<String> = std::iter::once("diff")
      .chain(DIFF_FLAGS)
      .map(String::from)
      .collect();
    if let Some(n) = req.context {
      args.push(format!("-U{n}"));
    }
    if req.revisions.is_empty() {
      match req.scope {
        Scope::All => args.push(self.head()?),
        Scope::Staged => args.push("--cached".into()),
        Scope::Unstaged => {}
      }
    } else {
      args.extend(req.revisions.iter().cloned());
    }
    args.push("--".into());
    args.extend(req.paths.iter().cloned());
    Ok(parse(&self.run(&args)?)?)
  }

  /// Untracked files are not in `git diff`, so each one is diffed against
  /// /dev/null. `--no-index` exits with 1 when the files differ.
  fn untracked_diff(&self, req: &DiffRequest) -> Result<Vec<FileDiff>, Error> {
    let root = self.root()?;

    let mut ls = vec![
      "ls-files",
      "--others",
      "--exclude-standard",
      "-z",
      "--full-name",
      "--",
    ];
    if req.paths.is_empty() {
      ls.push(":/");
    }
    ls.extend(req.paths.iter().map(String::as_str));
    let listing = self.run(ls)?;

    let context = req.context.map(|n| format!("-U{n}"));
    let mut files = Vec::new();
    for file in listing.split(|&b| b == 0).filter(|f| !f.is_empty()) {
      // A nested repository is listed as a directory.
      if file.ends_with(b"/") {
        continue;
      }
      let mut args: Vec<&OsStr> = vec!["diff".as_ref(), "--no-index".as_ref()];
      args.extend(DIFF_FLAGS.iter().map(|f| OsStr::new(*f)));
      if let Some(context) = &context {
        args.push(context.as_ref());
      }
      args.extend(["--".as_ref(), "/dev/null".as_ref(), OsStr::from_bytes(file)]);
      let out = Self::command(&root, args)?;
      if !matches!(out.status.code(), Some(0 | 1)) {
        return Err(git_error(&out));
      }
      files.extend(parse(&out.stdout)?);
    }
    Ok(files)
  }
}

/// Where one side of a diff reads its file from.
#[derive(Debug, PartialEq, Eq)]
enum Source {
  Commit(String),
  Index,
  WorkingTree,
}

/// The sources of the old and new side for a request.
fn sources(req: &DiffRequest, head: &str) -> (Source, Source) {
  let commit = |rev: &str| Source::Commit(if rev.is_empty() { "HEAD" } else { rev }.to_string());
  match req.revisions.as_slice() {
    [] => match req.scope {
      Scope::All => (Source::Commit(head.into()), Source::WorkingTree),
      Scope::Staged => (Source::Commit(head.into()), Source::Index),
      Scope::Unstaged => (Source::Index, Source::WorkingTree),
    },
    [range] => match range.split_once("...").or_else(|| range.split_once("..")) {
      Some((a, b)) => (commit(a), commit(b)),
      None => (commit(range), Source::WorkingTree),
    },
    [a, b, ..] => (commit(a), commit(b)),
  }
}

fn git_error(out: &Output) -> Error {
  Error::Git(String::from_utf8_lossy(&out.stderr).trim().to_string())
}

/// Options for every git call, so user config cannot add a pager or change
/// how paths print.
const GIT_FLAGS: [&str; 4] = [
  "--no-pager",
  "--no-optional-locks",
  "-c",
  "core.quotepath=off",
];

/// Pins the diff output so user config cannot change what the parser sees.
const DIFF_FLAGS: [&str; 7] = [
  "--no-color",
  "--no-ext-diff",
  "--no-textconv",
  "--no-relative",
  "--src-prefix=a/",
  "--dst-prefix=b/",
  "-M",
];

impl Git {
  fn stage(&self, file: &FileDiff) -> Result<(), Error> {
    self.write_index(&["add", "-A"], file)
  }

  fn unstage(&self, file: &FileDiff) -> Result<(), Error> {
    self.write_index(&["reset", "-q"], file)
  }

  fn stage_all(&self, files: &[FileDiff]) -> Result<(), Error> {
    self.write_index_all(&["add", "-A"], files)
  }

  fn unstage_all(&self, files: &[FileDiff]) -> Result<(), Error> {
    self.write_index_all(&["reset", "-q"], files)
  }

  fn stage_hunk(&self, file: &FileDiff, hunk: usize) -> Result<(), Error> {
    self.mark_intent_to_add(file)?;
    self.apply_to_index(&file.hunk_patch(hunk))
  }

  fn stage_lines(
    &self,
    file: &FileDiff,
    hunk: usize,
    lines: &BTreeSet<usize>,
  ) -> Result<(), Error> {
    self.mark_intent_to_add(file)?;
    self.apply_to_index(&file.line_patch(hunk, lines))
  }

  fn unstage_lines(
    &self,
    file: &FileDiff,
    hunk: usize,
    lines: &BTreeSet<usize>,
  ) -> Result<(), Error> {
    self.apply_to_index(&file.reverse_line_patch(hunk, lines))
  }

  fn unstage_hunk(&self, file: &FileDiff, hunk: usize) -> Result<(), Error> {
    self.apply_to_index(&file.reverse_hunk_patch(hunk))
  }
}

impl Vcs for Git {
  fn diff(&self, req: &DiffRequest) -> Result<Vec<FileDiff>, Error> {
    if let Some(rev) = req.revisions.iter().find(|r| r.starts_with('-')) {
      return Err(Error::Git(format!("invalid revision: {rev}")));
    }
    // Outside a repository `git diff` falls back to comparing two paths
    // and prints its usage text, so fail early with the real error.
    self.root()?;
    let mut files = self.tracked_diff(req)?;
    if req.revisions.is_empty() && req.scope != Scope::Staged {
      files.extend(self.untracked_diff(req)?);
    }
    if req.revisions.is_empty() && req.scope != Scope::Staged {
      for file in self.unmerged(req)? {
        if !files.iter().any(|f| f.path() == file.path()) {
          files.push(file);
        }
      }
    }
    Ok(files)
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

  fn sides(&self, req: &DiffRequest, file: &FileDiff) -> Sides {
    let (Ok(root), Ok(head)) = (self.root(), self.head()) else {
      return Sides::default();
    };
    let scope = file.section.map_or(req.scope, Section::scope);
    let (old, new) = sources(
      &DiffRequest {
        scope,
        ..req.clone()
      },
      &head,
    );
    let old = self.resolve_range_base(req, old);
    let read =
      |path: Option<&Path>, source: &Source| path.and_then(|p| self.read(&root, source, p)).unzip();
    let (old, old_id) = read(file.old_path.as_deref(), &old);
    let (new, new_id) = read(file.new_path.as_deref(), &new);
    Sides {
      old,
      new,
      old_id,
      new_id,
    }
  }
}

fn path(bytes: &[u8]) -> PathBuf {
  PathBuf::from(OsStr::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::diff::{FileStatus, LineKind, Section};
  use std::fs;
  use tempfile::TempDir;

  fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
      .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
      .args(args)
      .current_dir(dir)
      .status()
      .unwrap();
    assert!(status.success(), "git {args:?} failed");
  }

  fn write(dir: &Path, name: &str, body: &str) {
    fs::write(dir.join(name), body).unwrap();
  }

  fn repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    tmp
  }

  fn commit_all(d: &Path, msg: &str) {
    git(d, &["add", "."]);
    git(d, &["commit", "-q", "-m", msg]);
  }

  fn paths(files: &[FileDiff]) -> Vec<String> {
    files
      .iter()
      .map(|f| f.path().display().to_string())
      .collect()
  }

  #[test]
  fn diffs_staged_unstaged_and_untracked_together() {
    let tmp = repo();
    let d = tmp.path();
    // Distinct content keeps rename detection from pairing the wrong files.
    for f in ["mod.txt", "del.txt", "old.txt", "both.txt"] {
      write(d, f, &format!("{f}\ntwo\nthree\nfour\nfive\nsix\n"));
    }
    commit_all(d, "init");

    write(d, "mod.txt", "mod.txt\nTWO\nthree\nfour\nfive\nsix\n");
    fs::remove_file(d.join("del.txt")).unwrap();
    git(d, &["mv", "old.txt", "new.txt"]);
    write(d, "both.txt", "ONE\ntwo\nthree\nfour\nfive\nsix\n");
    git(d, &["add", "both.txt"]);
    write(d, "both.txt", "ONE\ntwo\nthree\nfour\nfive\nSIX\n");
    write(d, "untracked.txt", "u\n");
    write(d, "empty.txt", "");
    write(d, "my file.txt", "spaced\n");
    write(d, "tab\tname.txt", "tabbed\n");
    fs::write(d.join("bin.dat"), [0u8, 159, 146, 150, 0, 1]).unwrap();

    let files = Git::new(d).diff(&DiffRequest::default()).unwrap();
    assert_eq!(
      paths(&files),
      [
        "both.txt",
        "del.txt",
        "mod.txt",
        "new.txt",
        "bin.dat",
        "empty.txt",
        "my file.txt",
        "tab\tname.txt",
        "untracked.txt"
      ]
    );
    let by = |p: &str| files.iter().find(|f| f.path() == Path::new(p)).unwrap();
    // The staged and unstaged edits to both.txt arrive as one diff.
    let added: Vec<_> = by("both.txt")
      .hunks
      .iter()
      .flat_map(|h| &h.lines)
      .filter(|l| l.kind == LineKind::Added)
      .map(|l| l.content.clone())
      .collect();
    assert_eq!(added, [b"ONE".to_vec(), b"SIX".to_vec()]);
    assert_eq!(by("del.txt").status, FileStatus::Deleted);
    assert_eq!(by("new.txt").status, FileStatus::Renamed);
    assert_eq!(
      by("new.txt").old_path.as_deref(),
      Some(Path::new("old.txt"))
    );
    assert!(by("bin.dat").binary);
    assert_eq!(by("empty.txt").status, FileStatus::Added);
    assert!(by("empty.txt").hunks.is_empty());
    assert_eq!(by("untracked.txt").status, FileStatus::Added);
    assert_eq!(by("untracked.txt").hunks[0].lines[0].content, b"u");
  }

  #[test]
  fn scopes_split_staged_from_unstaged() {
    let tmp = repo();
    let d = tmp.path();
    write(d, "a.txt", "1\n");
    write(d, "b.txt", "1\n");
    commit_all(d, "init");
    write(d, "a.txt", "2\n");
    git(d, &["add", "a.txt"]);
    write(d, "b.txt", "2\n");
    write(d, "new.txt", "n\n");

    let diff = |scope| {
      Git::new(d)
        .diff(&DiffRequest {
          scope,
          ..Default::default()
        })
        .unwrap()
    };
    assert_eq!(paths(&diff(Scope::Staged)), ["a.txt"]);
    assert_eq!(paths(&diff(Scope::Unstaged)), ["b.txt", "new.txt"]);
  }

  fn sections(files: &[FileDiff]) -> Vec<(String, Option<Section>)> {
    files
      .iter()
      .map(|f| (f.path().display().to_string(), f.section))
      .collect()
  }

  /// `both.txt` has a staged and an unstaged edit, `staged.txt` only a staged
  /// one, `loose.txt` is untracked and `unstaged.txt` has only a worktree edit.
  fn mixed_repo() -> TempDir {
    let tmp = repo();
    let d = tmp.path();
    for f in ["both.txt", "staged.txt", "unstaged.txt"] {
      write(d, f, "1\n2\n3\n");
    }
    commit_all(d, "init");
    write(d, "both.txt", "A\n2\n3\n");
    write(d, "staged.txt", "A\n2\n3\n");
    git(d, &["add", "both.txt", "staged.txt"]);
    write(d, "both.txt", "A\n2\nC\n");
    write(d, "unstaged.txt", "A\n2\n3\n");
    write(d, "loose.txt", "l\n");
    tmp
  }

  #[test]
  fn the_changeset_puts_unstaged_first_and_lists_a_file_with_both_in_both() {
    let tmp = mixed_repo();
    let files = Git::new(tmp.path())
      .changeset(&DiffRequest::default())
      .unwrap();
    let (u, s) = (Some(Section::Unstaged), Some(Section::Staged));
    assert_eq!(
      sections(&files),
      [
        ("both.txt".into(), u),
        ("unstaged.txt".into(), u),
        ("loose.txt".into(), u),
        ("both.txt".into(), s),
        ("staged.txt".into(), s),
      ]
    );
    let added = |f: &FileDiff| -> Vec<Vec<u8>> {
      f.hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter(|l| l.kind == LineKind::Added)
        .map(|l| l.content.clone())
        .collect()
    };
    assert_eq!(added(&files[0]), [b"C".to_vec()]);
    assert_eq!(added(&files[3]), [b"A".to_vec()]);
    assert_eq!(files[2].status, FileStatus::Added);
  }

  #[test]
  fn the_scope_flags_keep_only_their_own_section() {
    let tmp = mixed_repo();
    let git = Git::new(tmp.path());
    let changeset = |scope| {
      git
        .changeset(&DiffRequest {
          scope,
          ..Default::default()
        })
        .unwrap()
    };
    let staged = changeset(Scope::Staged);
    assert_eq!(paths(&staged), ["both.txt", "staged.txt"]);
    assert!(staged.iter().all(|f| f.section == Some(Section::Staged)));
    let unstaged = changeset(Scope::Unstaged);
    assert_eq!(paths(&unstaged), ["both.txt", "unstaged.txt", "loose.txt"]);
    assert!(
      unstaged
        .iter()
        .all(|f| f.section == Some(Section::Unstaged))
    );
  }

  #[test]
  fn a_revision_changeset_is_flat() {
    let tmp = mixed_repo();
    let files = Git::new(tmp.path())
      .changeset(&DiffRequest {
        revisions: vec!["HEAD".into()],
        ..Default::default()
      })
      .unwrap();
    assert_eq!(paths(&files), ["both.txt", "staged.txt", "unstaged.txt"]);
    assert!(files.iter().all(|f| f.section.is_none()));
  }

  #[test]
  fn an_entry_reads_the_file_versions_of_its_own_section() {
    let tmp = mixed_repo();
    let git = Git::new(tmp.path());
    let files = git.changeset(&DiffRequest::default()).unwrap();
    let text = |b: Option<Vec<u8>>| String::from_utf8(b.unwrap()).unwrap();

    let unstaged = git.sides(&DiffRequest::default(), &files[0]);
    assert_eq!(text(unstaged.old), "A\n2\n3\n");
    assert_eq!(text(unstaged.new), "A\n2\nC\n");
    assert_eq!(unstaged.old_id, Some(VersionId::of(b"A\n2\n3\n")));
    assert_eq!(unstaged.new_id, Some(VersionId::of(b"A\n2\nC\n")));

    let staged = git.sides(&DiffRequest::default(), &files[3]);
    assert_eq!(text(staged.old), "1\n2\n3\n");
    assert_eq!(text(staged.new), "A\n2\n3\n");
    assert_eq!(staged.new_id, Some(VersionId::of(b"A\n2\n3\n")));
  }

  #[test]
  fn diffs_an_unborn_branch_against_the_empty_tree() {
    let tmp = repo();
    let d = tmp.path();
    write(d, "staged.txt", "s\n");
    git(d, &["add", "staged.txt"]);
    write(d, "loose.txt", "l\n");
    let files = Git::new(d).diff(&DiffRequest::default()).unwrap();
    assert_eq!(paths(&files), ["staged.txt", "loose.txt"]);
    assert!(files.iter().all(|f| f.status == FileStatus::Added));
  }

  #[test]
  fn diffs_revisions_and_ranges_without_untracked_files() {
    let tmp = repo();
    let d = tmp.path();
    write(d, "a.txt", "1\n");
    commit_all(d, "one");
    write(d, "a.txt", "2\n");
    write(d, "b.txt", "b\n");
    commit_all(d, "two");
    write(d, "untracked.txt", "u\n");

    let req = |revs: &[&str]| DiffRequest {
      revisions: revs.iter().map(std::string::ToString::to_string).collect(),
      ..Default::default()
    };
    let git = Git::new(d);
    assert_eq!(
      paths(&git.diff(&req(&["HEAD~1"])).unwrap()),
      ["a.txt", "b.txt"]
    );
    assert_eq!(
      paths(&git.diff(&req(&["HEAD~1", "HEAD"])).unwrap()),
      ["a.txt", "b.txt"]
    );
    assert_eq!(
      paths(&git.diff(&req(&["HEAD~1..HEAD"])).unwrap()),
      ["a.txt", "b.txt"]
    );
    assert!(git.diff(&req(&["HEAD", "HEAD"])).unwrap().is_empty());
  }

  #[test]
  fn diff_outside_a_repository_reports_the_real_error() {
    let tmp = TempDir::new().unwrap();
    let err = Git::new(tmp.path())
      .diff(&DiffRequest::default())
      .unwrap_err();
    assert!(matches!(err, Error::Git(ref m) if m.contains("not a git repository")));
  }

  #[test]
  fn rejects_revisions_that_look_like_options() {
    let tmp = repo();
    let req = DiffRequest {
      revisions: vec!["--output=pwned".into()],
      ..Default::default()
    };
    assert!(Git::new(tmp.path()).diff(&req).is_err());
    assert!(!tmp.path().join("pwned").exists());
  }

  #[test]
  fn pathspecs_limit_tracked_and_untracked_files() {
    let tmp = repo();
    let d = tmp.path();
    fs::create_dir(d.join("src")).unwrap();
    write(d, "src/a.rs", "1\n");
    write(d, "b.txt", "1\n");
    commit_all(d, "init");
    write(d, "src/a.rs", "2\n");
    write(d, "b.txt", "2\n");
    write(d, "src/new.rs", "n\n");
    write(d, "new.txt", "n\n");

    let req = DiffRequest {
      paths: vec!["src/".into()],
      ..Default::default()
    };
    assert_eq!(
      paths(&Git::new(d).diff(&req).unwrap()),
      ["src/a.rs", "src/new.rs"]
    );
    let req = DiffRequest {
      paths: vec!["*.txt".into()],
      ..Default::default()
    };
    assert_eq!(
      paths(&Git::new(d).diff(&req).unwrap()),
      ["b.txt", "new.txt"]
    );
  }

  #[test]
  fn context_option_changes_hunk_size() {
    let tmp = repo();
    let d = tmp.path();
    let body = |mid: &str| format!("1\n2\n3\n4\n{mid}\n6\n7\n8\n9\n");
    write(d, "f.txt", &body("5"));
    commit_all(d, "init");
    write(d, "f.txt", &body("FIVE"));

    let lines = |n| {
      let req = DiffRequest {
        context: Some(n),
        ..Default::default()
      };
      Git::new(d).diff(&req).unwrap()[0].hunks[0].lines.len()
    };
    assert_eq!(lines(0), 2);
    assert_eq!(lines(1), 4);
    assert_eq!(lines(3), 8);
  }

  #[test]
  fn user_config_cannot_change_the_parsed_output() {
    let tmp = repo();
    let d = tmp.path();
    write(d, "f.txt", "one\n");
    commit_all(d, "init");
    write(d, "f.txt", "two\n");
    write(d, "new.txt", "n\n");
    for kv in [
      "color.ui=always",
      "diff.noprefix=true",
      "diff.mnemonicPrefix=true",
      "diff.relative=true",
      "diff.external=false",
      "diff.renames=false",
    ] {
      let (k, v) = kv.split_once('=').unwrap();
      git(d, &["config", k, v]);
    }
    let files = Git::new(d).diff(&DiffRequest::default()).unwrap();
    assert_eq!(paths(&files), ["f.txt", "new.txt"]);
    assert_eq!(files[0].hunks[0].lines[0].content, b"one");
  }

  #[test]
  fn marks_missing_trailing_newline_and_keeps_non_utf8_content() {
    let tmp = repo();
    let d = tmp.path();
    fs::write(d.join("f.txt"), b"a\n").unwrap();
    commit_all(d, "init");
    fs::write(d.join("f.txt"), b"a\xff\xfe").unwrap();
    let files = Git::new(d).diff(&DiffRequest::default()).unwrap();
    let lines = &files[0].hunks[0].lines;
    assert_eq!(lines[1].content, b"a\xff\xfe");
    assert!(lines[1].no_newline);
  }

  #[test]
  fn diffs_the_whole_repository_from_a_subdirectory() {
    let tmp = repo();
    let d = tmp.path();
    fs::create_dir(d.join("sub")).unwrap();
    write(d, "top.txt", "1\n");
    commit_all(d, "init");
    write(d, "top.txt", "2\n");
    write(d, "sub/new.txt", "n\n");
    write(d, "other.txt", "o\n");
    let files = Git::new(d.join("sub"))
      .diff(&DiffRequest::default())
      .unwrap();
    assert_eq!(paths(&files), ["top.txt", "other.txt", "sub/new.txt"]);
  }

  #[test]
  fn reports_a_clear_error_outside_a_repository() {
    let tmp = TempDir::new().unwrap();
    let err = Git::new(tmp.path())
      .diff(&DiffRequest::default())
      .unwrap_err();
    assert!(matches!(err, Error::Git(ref m) if m.contains("not a git repository")));
  }

  #[test]
  fn reads_the_old_side_from_the_revision_and_the_new_side_from_disk() {
    let d = repo();
    write(d.path(), "a.txt", "one\ntwo\n");
    commit_all(d.path(), "one");
    write(d.path(), "a.txt", "one\ntwo\nthree\n");
    commit_all(d.path(), "two");
    write(d.path(), "a.txt", "uncommitted\n");
    let git = Git::new(d.path());

    let request = DiffRequest::default();
    let file = git.diff(&request).unwrap().remove(0);
    let sides = git.sides(&request, &file);
    assert_eq!(sides.old.as_deref(), Some(&b"one\ntwo\nthree\n"[..]));
    assert_eq!(sides.new.as_deref(), Some(&b"uncommitted\n"[..]));

    let request = DiffRequest {
      revisions: vec!["HEAD~1".into()],
      ..DiffRequest::default()
    };
    let file = git.diff(&request).unwrap().remove(0);
    let sides = git.sides(&request, &file);
    assert_eq!(sides.old.as_deref(), Some(&b"one\ntwo\n"[..]));
    assert_eq!(sides.new.as_deref(), Some(&b"uncommitted\n"[..]));
  }

  #[test]
  fn names_every_version_by_a_hash_of_its_contents() {
    let d = repo();
    write(d.path(), "a.rs", "fn a() {}\n");
    commit_all(d.path(), "one");
    write(d.path(), "a.rs", "fn b() {}\n");
    let git = Git::new(d.path());
    let request = DiffRequest::default();
    let file = git.diff(&request).unwrap().remove(0);
    let sides = git.sides(&request, &file);
    assert_eq!(sides.old_id, Some(VersionId::of(b"fn a() {}\n")));
    assert_eq!(sides.new_id, Some(VersionId::of(b"fn b() {}\n")));

    let same = git.sides(&request, &file);
    assert_eq!(sides.new_id, same.new_id);
    write(d.path(), "a.rs", "fn c() {}\n");
    assert_ne!(sides.new_id, git.sides(&request, &file).new_id);

    git.run(["add", "a.rs"]).unwrap();
    let staged = DiffRequest {
      scope: Scope::Staged,
      ..DiffRequest::default()
    };
    let file = git.diff(&staged).unwrap().remove(0);
    // Staging moves the bytes from the working tree to the index without
    // changing their ID, so their highlighting is reused.
    assert_eq!(
      git.sides(&staged, &file).new_id,
      Some(VersionId::of(b"fn c() {}\n"))
    );
  }

  fn status(d: &Path) -> String {
    let out = Command::new("git")
      .args(["status", "--porcelain"])
      .current_dir(d)
      .output()
      .unwrap();
    String::from_utf8(out.stdout).unwrap()
  }

  fn entry(git: &Git, name: &str, section: Section) -> FileDiff {
    git
      .changeset(&DiffRequest::default())
      .unwrap()
      .into_iter()
      .find(|f| f.path() == Path::new(name) && f.section == Some(section))
      .unwrap_or_else(|| panic!("no {section:?} entry for {name}"))
  }

  #[test]
  fn stages_and_unstages_a_whole_file() {
    let tmp = mixed_repo();
    let d = tmp.path();
    let git = Git::new(d);
    git
      .stage(&entry(&git, "unstaged.txt", Section::Unstaged))
      .unwrap();
    assert!(status(d).contains("M  unstaged.txt"));
    // The unstaged half of both.txt joins its staged half.
    git
      .stage(&entry(&git, "both.txt", Section::Unstaged))
      .unwrap();
    assert!(status(d).contains("M  both.txt"));
    git
      .unstage(&entry(&git, "staged.txt", Section::Staged))
      .unwrap();
    assert!(status(d).contains(" M staged.txt"));
  }

  #[test]
  fn stages_an_untracked_file_and_a_deletion() {
    let tmp = mixed_repo();
    let d = tmp.path();
    fs::remove_file(d.join("unstaged.txt")).unwrap();
    let git = Git::new(d);
    git
      .stage(&entry(&git, "loose.txt", Section::Unstaged))
      .unwrap();
    git
      .stage(&entry(&git, "unstaged.txt", Section::Unstaged))
      .unwrap();
    let status = status(d);
    assert!(status.contains("A  loose.txt"));
    assert!(status.contains("D  unstaged.txt"));
  }

  #[test]
  fn unstages_a_file_on_an_unborn_branch() {
    let tmp = repo();
    let d = tmp.path();
    write(d, "a.txt", "a\n");
    git(d, &["add", "a.txt"]);
    let git = Git::new(d);
    git.unstage(&entry(&git, "a.txt", Section::Staged)).unwrap();
    assert!(status(d).contains("?? a.txt"));
  }

  #[test]
  fn unstaging_a_rename_resets_both_paths() {
    let tmp = repo();
    let d = tmp.path();
    write(d, "old.txt", "1\n2\n3\n4\n5\n6\n");
    commit_all(d, "init");
    git(d, &["mv", "old.txt", "new.txt"]);
    let git = Git::new(d);
    git
      .unstage(&entry(&git, "new.txt", Section::Staged))
      .unwrap();
    let status = status(d);
    assert!(status.contains(" D old.txt"));
    assert!(status.contains("?? new.txt"));
  }

  #[test]
  fn paths_are_taken_literally_and_from_the_repository_root() {
    let tmp = repo();
    let d = tmp.path();
    fs::create_dir(d.join("sub")).unwrap();
    write(d, "sub/a.txt", "a\n");
    write(d, "*.txt", "star\n");
    let git = Git::new(d.join("sub"));
    git.stage(&entry(&git, "*.txt", Section::Unstaged)).unwrap();
    let status = status(d);
    assert!(status.contains("A  *.txt"));
    assert!(status.contains("?? sub/"));
  }

  fn conflicted_repo() -> TempDir {
    let tmp = repo();
    let d = tmp.path();
    write(d, "c.txt", "base\n");
    commit_all(d, "init");
    git(d, &["checkout", "-q", "-b", "other"]);
    write(d, "c.txt", "theirs\n");
    commit_all(d, "theirs");
    git(d, &["checkout", "-q", "main"]);
    write(d, "c.txt", "ours\n");
    commit_all(d, "ours");
    let merged = Command::new("git")
      .args(["merge", "other"])
      .current_dir(d)
      .output()
      .unwrap();
    assert!(!merged.status.success());
    tmp
  }

  #[test]
  fn lists_a_conflicted_file_once_and_stages_it_whole() {
    let tmp = conflicted_repo();
    let d = tmp.path();
    let git = Git::new(d);
    let files = git.changeset(&DiffRequest::default()).unwrap();
    assert_eq!(
      sections(&files),
      [("c.txt".into(), Some(Section::Unstaged))]
    );
    git.stage(&entry(&git, "c.txt", Section::Unstaged)).unwrap();
    assert!(status(d).contains("M  c.txt"));
  }

  /// Twenty lines with edits at the top and the bottom, so the diff has two
  /// hunks.
  fn two_hunk_repo() -> TempDir {
    let tmp = repo();
    let d = tmp.path();
    write(d, "f.txt", &numbered(&[]));
    commit_all(d, "init");
    write(d, "f.txt", &numbered(&[(2, "TWO"), (19, "NINETEEN")]));
    tmp
  }

  fn numbered(edits: &[(usize, &str)]) -> String {
    (1..=20)
      .flat_map(|n| {
        let line = edits
          .iter()
          .find(|(at, _)| *at == n)
          .map_or_else(|| n.to_string(), |(_, text)| (*text).to_string());
        [line, "\n".to_string()]
      })
      .collect()
  }

  fn index_of(d: &Path, name: &str) -> String {
    let out = Command::new("git")
      .args(["show", &format!(":{name}")])
      .current_dir(d)
      .output()
      .unwrap();
    String::from_utf8(out.stdout).unwrap()
  }

  #[test]
  fn stages_one_hunk_and_leaves_the_other_unstaged() {
    let tmp = two_hunk_repo();
    let d = tmp.path();
    let git = Git::new(d);
    let file = entry(&git, "f.txt", Section::Unstaged);
    assert_eq!(file.hunks.len(), 2);
    git.stage_hunk(&file, 1).unwrap();
    assert_eq!(index_of(d, "f.txt"), numbered(&[(19, "NINETEEN")]));
    let rest = entry(&git, "f.txt", Section::Unstaged);
    assert_eq!(rest.hunks.len(), 1);
    assert_eq!(rest.hunks[0].lines[1].content, b"2");
    assert_eq!(
      fs::read_to_string(d.join("f.txt")).unwrap().lines().nth(1),
      Some("TWO")
    );
  }

  #[test]
  fn unstages_one_hunk_of_a_staged_file() {
    let tmp = two_hunk_repo();
    let d = tmp.path();
    git(d, &["add", "f.txt"]);
    let git = Git::new(d);
    let file = entry(&git, "f.txt", Section::Staged);
    git.unstage_hunk(&file, 0).unwrap();
    assert_eq!(index_of(d, "f.txt"), numbered(&[(19, "NINETEEN")]));
    assert_eq!(entry(&git, "f.txt", Section::Staged).hunks.len(), 1);
    assert_eq!(entry(&git, "f.txt", Section::Unstaged).hunks.len(), 1);
  }

  /// Lines 3 and 5 changed, with 4 between them, so one hunk holds
  /// `-3 +x3 4 -5 +x5` after two lines of context.
  fn interleaved_repo() -> TempDir {
    let tmp = repo();
    let d = tmp.path();
    write(d, "f.txt", &numbered(&[]));
    commit_all(d, "init");
    write(d, "f.txt", &numbered(&[(3, "x3"), (5, "x5")]));
    tmp
  }

  #[test]
  fn stages_only_the_selected_lines_of_a_hunk() {
    // Hunk lines: 0 `1`, 1 `2`, 2 `-3`, 3 `+x3`, 4 ` 4`, 5 `-5`, 6 `+x5`, ...
    let cases: [(std::ops::Range<usize>, String); 4] = [
      (2..4, numbered(&[(3, "x3")])),
      (5..7, numbered(&[(5, "x5")])),
      // Only the removal: the line goes away without its replacement.
      (2..3, numbered(&[(3, "")]).replacen("\n\n", "\n", 1)),
      // Only the addition: it lands next to the old line.
      (3..4, numbered(&[(3, "3\nx3")])),
    ];
    for (lines, expected) in cases {
      let tmp = interleaved_repo();
      let d = tmp.path();
      let git = Git::new(d);
      let file = entry(&git, "f.txt", Section::Unstaged);
      assert_eq!(file.hunks.len(), 1);
      git.stage_lines(&file, 0, &lines.clone().collect()).unwrap();
      assert_eq!(index_of(d, "f.txt"), expected, "{lines:?}");
    }
  }

  #[test]
  fn unstages_only_the_selected_lines_of_a_staged_hunk() {
    let cases: [(std::ops::Range<usize>, String); 3] = [
      // The second pair leaves the index, the first stays.
      (5..7, numbered(&[(3, "x3")])),
      (2..4, numbered(&[(5, "x5")])),
      // Only the removal of 3 is taken back: the old line returns but the
      // staged replacement stays.
      (2..3, numbered(&[(3, "3\nx3"), (5, "x5")])),
    ];
    for (lines, expected) in cases {
      let tmp = interleaved_repo();
      let d = tmp.path();
      git(d, &["add", "f.txt"]);
      let git = Git::new(d);
      let file = entry(&git, "f.txt", Section::Staged);
      git
        .unstage_lines(&file, 0, &lines.clone().collect())
        .unwrap();
      assert_eq!(index_of(d, "f.txt"), expected, "{lines:?}");
    }
  }

  #[test]
  fn stages_selected_lines_of_an_untracked_file() {
    let tmp = repo();
    let d = tmp.path();
    write(d, "keep.txt", "k\n");
    commit_all(d, "init");
    write(d, "new.txt", "a\nb\nc\nd\n");
    let git = Git::new(d);
    let file = entry(&git, "new.txt", Section::Unstaged);
    git.stage_lines(&file, 0, &(1..3).collect()).unwrap();
    assert_eq!(index_of(d, "new.txt"), "b\nc\n");
    let rest = entry(&git, "new.txt", Section::Unstaged);
    let added: Vec<&[u8]> = rest.hunks[0]
      .lines
      .iter()
      .filter(|l| l.kind == crate::diff::LineKind::Added)
      .map(|l| l.content.as_slice())
      .collect();
    assert_eq!(added, [b"a".as_slice(), b"d".as_slice()]);
  }

  #[test]
  fn stages_part_of_a_deleted_file_and_unstages_part_of_a_new_one() {
    let tmp = repo();
    let d = tmp.path();
    write(d, "gone.txt", "1\n2\n3\n");
    commit_all(d, "init");
    fs::remove_file(d.join("gone.txt")).unwrap();
    write(d, "new.txt", "a\nb\nc\n");
    git(d, &["add", "new.txt"]);
    let git = Git::new(d);
    git
      .stage_lines(
        &entry(&git, "gone.txt", Section::Unstaged),
        0,
        &(0..1).collect(),
      )
      .unwrap();
    // Only the first line is gone from the index.
    assert_eq!(index_of(d, "gone.txt"), "2\n3\n");

    git
      .unstage_lines(
        &entry(&git, "new.txt", Section::Staged),
        0,
        &(1..2).collect(),
      )
      .unwrap();
    assert_eq!(index_of(d, "new.txt"), "a\nc\n");
  }

  #[test]
  fn stages_an_addition_after_a_line_that_had_no_trailing_newline() {
    let tmp = repo();
    let d = tmp.path();
    fs::write(d.join("f.txt"), "a\nb").unwrap();
    commit_all(d, "init");
    fs::write(d.join("f.txt"), "a\nc\n").unwrap();
    let git = Git::new(d);
    let file = entry(&git, "f.txt", Section::Unstaged);
    // Lines: ` a`, `-b` without a newline, `+c`. Only the addition is picked,
    // so `b` stays but now ends its line.
    git.stage_lines(&file, 0, &(2..3).collect()).unwrap();
    assert_eq!(index_of(d, "f.txt"), "a\nb\nc\n");
  }

  #[test]
  fn stages_a_hunk_of_an_untracked_file() {
    let tmp = repo();
    let d = tmp.path();
    write(d, "keep.txt", "k\n");
    commit_all(d, "init");
    write(d, "new.txt", "a\nb\nc\n");
    let git = Git::new(d);
    git
      .stage_hunk(&entry(&git, "new.txt", Section::Unstaged), 0)
      .unwrap();
    assert_eq!(index_of(d, "new.txt"), "a\nb\nc\n");
    assert!(status(d).contains("A  new.txt"));
    // Unstaging the hunk takes the file out of the index again.
    git
      .unstage_hunk(&entry(&git, "new.txt", Section::Staged), 0)
      .unwrap();
    assert!(status(d).contains("?? new.txt") || status(d).contains(" A new.txt"));
  }

  #[test]
  fn stages_a_hunk_of_a_deleted_file_and_one_without_a_trailing_newline() {
    let tmp = repo();
    let d = tmp.path();
    write(d, "gone.txt", "1\n2\n");
    fs::write(d.join("tail.txt"), "x\ny").unwrap();
    commit_all(d, "init");
    fs::remove_file(d.join("gone.txt")).unwrap();
    fs::write(d.join("tail.txt"), "x\nz").unwrap();
    let git = Git::new(d);
    git
      .stage_hunk(&entry(&git, "gone.txt", Section::Unstaged), 0)
      .unwrap();
    git
      .stage_hunk(&entry(&git, "tail.txt", Section::Unstaged), 0)
      .unwrap();
    let status = status(d);
    assert!(status.contains("D  gone.txt"));
    assert!(status.contains("M  tail.txt"));
    assert_eq!(index_of(d, "tail.txt"), "x\nz");
  }

  #[test]
  fn stages_a_hunk_of_a_file_with_an_unusual_name() {
    let tmp = repo();
    let d = tmp.path();
    write(d, "my file.txt", "1\n");
    write(d, "tab\tname.txt", "1\n");
    commit_all(d, "init");
    write(d, "my file.txt", "2\n");
    write(d, "tab\tname.txt", "2\n");
    let git = Git::new(d);
    for name in ["my file.txt", "tab\tname.txt"] {
      git
        .stage_hunk(&entry(&git, name, Section::Unstaged), 0)
        .unwrap();
      assert_eq!(index_of(d, name), "2\n");
    }
  }

  #[test]
  fn a_conflicted_file_has_its_own_status() {
    let tmp = conflicted_repo();
    let git = Git::new(tmp.path());
    let file = entry(&git, "c.txt", Section::Unstaged);
    assert_eq!(file.status, FileStatus::Conflicted);
    assert!(file.hunks.is_empty());
  }

  #[test]
  fn a_patch_that_does_not_apply_reports_gits_message() {
    let tmp = two_hunk_repo();
    let d = tmp.path();
    let git = Git::new(d);
    let file = entry(&git, "f.txt", Section::Unstaged);
    // The file changes on disk after the diff was read.
    git.stage(&file).unwrap();
    let err = git.stage_hunk(&file, 0).unwrap_err();
    assert!(matches!(err, Error::Git(ref m) if m.contains("patch")));
  }
}
