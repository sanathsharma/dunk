use super::{DiffRequest, Error, Sides, Vcs, VersionId};
use crate::args::Scope;
use crate::diff::{FileDiff, FileStatus, Section, parse};
use std::collections::hash_map::DefaultHasher;
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub struct Git {
  dir: PathBuf,
}

impl Git {
  /// Runs git as if started in `dir`; git finds the repository root itself.
  pub fn new(dir: impl Into<PathBuf>) -> Self {
    Git { dir: dir.into() }
  }

  fn command<I, S>(&self, dir: &Path, args: I) -> Result<Output, Error>
  where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
  {
    let out = Command::new("git")
      .args([
        "--no-pager",
        "--no-optional-locks",
        "-c",
        "core.quotepath=off",
      ])
      .args(args)
      .current_dir(dir)
      .output()?;
    Ok(out)
  }

  fn run<I, S>(&self, args: I) -> Result<Vec<u8>, Error>
  where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
  {
    let out = self.command(&self.dir, args)?;
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
    let probe = self.command(&self.dir, ["rev-parse", "--verify", "-q", "HEAD"])?;
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
        let mut hasher = DefaultHasher::new();
        bytes.hash(&mut hasher);
        return Some((bytes, VersionId::Content(hasher.finish())));
      }
      Source::Index => spec(":"),
      Source::Commit(rev) => spec(&format!("{rev}:")),
    };
    let oid = self
      .run([OsStr::new("rev-parse"), OsStr::new("--verify"), &spec])
      .ok()?;
    let oid = String::from_utf8_lossy(&oid).trim().to_string();
    let bytes = self.run(["cat-file", "blob", &oid]).ok()?;
    Some((bytes, VersionId::Object(oid)))
  }

  /// Runs a git command that writes the index, with the paths taken
  /// literally. It runs at the repository root because the paths of a
  /// `FileDiff` are relative to it.
  fn write_index(&self, args: &[&str], file: &FileDiff) -> Result<(), Error> {
    let root = self.root()?;
    let mut paths = vec![file.path()];
    if file.status == FileStatus::Renamed {
      paths.extend(file.old_path.as_deref());
    }
    let mut full: Vec<&OsStr> = vec!["--literal-pathspecs".as_ref()];
    full.extend(args.iter().map(OsStr::new));
    full.push("--".as_ref());
    full.extend(paths.iter().map(|p| p.as_os_str()));
    let out = self.command(&root, full)?;
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
      let name = path(&entry[tab + 1..]);
      if files.iter().any(|f| f.path() == name) {
        continue;
      }
      files.push(FileDiff {
        old_path: Some(name.clone()),
        new_path: Some(name),
        status: FileStatus::Modified,
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
    match self.run(["merge-base", a, b]) {
      Ok(out) => Source::Commit(String::from_utf8_lossy(&out).trim().to_string()),
      Err(_) => old,
    }
  }

  fn tracked_diff(&self, req: &DiffRequest) -> Result<Vec<FileDiff>, Error> {
    let mut args: Vec<String> = ["diff"]
      .into_iter()
      .chain(DIFF_FLAGS)
      .map(String::from)
      .collect();
    if let Some(n) = req.context {
      args.push(format!("-U{n}"));
    }
    if !req.revisions.is_empty() {
      args.extend(req.revisions.iter().cloned());
    } else {
      match req.scope {
        Scope::All => args.push(self.head()?),
        Scope::Staged => args.push("--cached".into()),
        Scope::Unstaged => {}
      }
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
      let out = self.command(&root, args)?;
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

  fn stage(&self, file: &FileDiff) -> Result<(), Error> {
    self.write_index(&["add", "-A"], file)
  }

  fn unstage(&self, file: &FileDiff) -> Result<(), Error> {
    self.write_index(&["reset", "-q"], file)
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
    assert!(matches!(unstaged.old_id, Some(VersionId::Object(_))));
    assert!(matches!(unstaged.new_id, Some(VersionId::Content(_))));

    let staged = git.sides(&DiffRequest::default(), &files[3]);
    assert_eq!(text(staged.old), "1\n2\n3\n");
    assert_eq!(text(staged.new), "A\n2\n3\n");
    assert!(matches!(staged.new_id, Some(VersionId::Object(_))));
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
      revisions: revs.iter().map(|s| s.to_string()).collect(),
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
  fn names_committed_and_staged_versions_by_object_id_and_working_files_by_content() {
    let d = repo();
    write(d.path(), "a.rs", "fn a() {}\n");
    commit_all(d.path(), "one");
    write(d.path(), "a.rs", "fn b() {}\n");
    let git = Git::new(d.path());
    let request = DiffRequest::default();
    let file = git.diff(&request).unwrap().remove(0);
    let sides = git.sides(&request, &file);
    assert!(matches!(&sides.old_id, Some(VersionId::Object(id)) if id.len() >= 40));
    assert!(matches!(sides.new_id, Some(VersionId::Content(_))));

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
    assert!(matches!(
      git.sides(&staged, &file).new_id,
      Some(VersionId::Object(_))
    ));
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
}
