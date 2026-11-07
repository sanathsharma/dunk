use super::{ChangeKind, DiffRequest, Error, FileChange, Status, Vcs};
use crate::args::Scope;
use crate::diff::{FileDiff, parse};
use std::ffi::OsStr;
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
        let root = self.run(["rev-parse", "--show-toplevel"])?;
        let root = path(root.strip_suffix(b"\n").unwrap_or(&root));

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
    fn status(&self) -> Result<Status, Error> {
        let out = self.run(["status", "--porcelain=v2", "-z", "--untracked-files=all"])?;
        parse_status(&out)
    }

    fn diff(&self, req: &DiffRequest) -> Result<Vec<FileDiff>, Error> {
        if let Some(rev) = req.revisions.iter().find(|r| r.starts_with('-')) {
            return Err(Error::Git(format!("invalid revision: {rev}")));
        }
        let mut files = self.tracked_diff(req)?;
        if req.revisions.is_empty() && req.scope != Scope::Staged {
            files.extend(self.untracked_diff(req)?);
        }
        Ok(files)
    }
}

fn path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(bytes))
}

fn fields(record: &[u8], n: usize) -> Result<Vec<&[u8]>, Error> {
    let f: Vec<&[u8]> = record.splitn(n, |&b| b == b' ').collect();
    if f.len() == n {
        Ok(f)
    } else {
        Err(Error::Parse(String::from_utf8_lossy(record).into_owned()))
    }
}

fn kind(letter: u8, from: Option<&Path>) -> Result<Option<ChangeKind>, Error> {
    let from = || from.map(Path::to_path_buf).unwrap_or_default();
    Ok(Some(match letter {
        b'.' => return Ok(None),
        b'A' => ChangeKind::Added,
        b'M' => ChangeKind::Modified,
        b'D' => ChangeKind::Deleted,
        b'R' => ChangeKind::Renamed { from: from() },
        b'C' => ChangeKind::Copied { from: from() },
        b'T' => ChangeKind::TypeChanged,
        other => {
            return Err(Error::Parse(format!(
                "unknown status letter {:?}",
                other as char
            )));
        }
    }))
}

fn push_xy(status: &mut Status, xy: &[u8], file: &[u8], from: Option<&Path>) -> Result<(), Error> {
    if xy.len() != 2 {
        return Err(Error::Parse(String::from_utf8_lossy(xy).into_owned()));
    }
    if let Some(kind) = kind(xy[0], from)? {
        status.staged.push(FileChange {
            path: path(file),
            kind,
        });
    }
    if let Some(kind) = kind(xy[1], from)? {
        status.unstaged.push(FileChange {
            path: path(file),
            kind,
        });
    }
    Ok(())
}

/// Parses `git status --porcelain=v2 -z` output. Paths stay as raw bytes.
fn parse_status(out: &[u8]) -> Result<Status, Error> {
    let mut status = Status::default();
    let mut records = out.split(|&b| b == 0).filter(|r| !r.is_empty());
    while let Some(record) = records.next() {
        match record[0] {
            b'1' => {
                let f = fields(record, 9)?;
                push_xy(&mut status, f[1], f[8], None)?;
            }
            b'2' => {
                let f = fields(record, 10)?;
                let from = records
                    .next()
                    .map(path)
                    .ok_or_else(|| Error::Parse("rename without original path".into()))?;
                push_xy(&mut status, f[1], f[9], Some(&from))?;
            }
            b'u' => {
                let f = fields(record, 11)?;
                status.unstaged.push(FileChange {
                    path: path(f[10]),
                    kind: ChangeKind::Unmerged,
                });
            }
            b'?' => status.untracked.push(path(&record[2..])),
            b'!' | b'#' => {}
            _ => return Err(Error::Parse(String::from_utf8_lossy(record).into_owned())),
        }
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{FileStatus, LineKind};
    use std::fs;
    use tempfile::TempDir;

    fn change(p: &str, kind: ChangeKind) -> FileChange {
        FileChange {
            path: p.into(),
            kind,
        }
    }

    #[test]
    fn parses_ordinary_and_untracked_entries() {
        let out = b"1 .M N... 100644 100644 100644 aaaa bbbb src/a.rs\0\
                    1 A. N... 000000 100644 100644 0000 cccc new.rs\0\
                    1 MM N... 100644 100644 100644 aaaa bbbb both.rs\0\
                    ? notes.txt\0";
        let s = parse_status(out).unwrap();
        assert_eq!(
            s.staged,
            [
                change("new.rs", ChangeKind::Added),
                change("both.rs", ChangeKind::Modified)
            ]
        );
        assert_eq!(
            s.unstaged,
            [
                change("src/a.rs", ChangeKind::Modified),
                change("both.rs", ChangeKind::Modified)
            ]
        );
        assert_eq!(s.untracked, [PathBuf::from("notes.txt")]);
    }

    #[test]
    fn parses_rename_with_original_path() {
        let out = b"2 R. N... 100644 100644 100644 aaaa bbbb R100 new name.rs\0old name.rs\0";
        let s = parse_status(out).unwrap();
        assert_eq!(
            s.staged,
            [change(
                "new name.rs",
                ChangeKind::Renamed {
                    from: "old name.rs".into()
                }
            )]
        );
        assert!(s.unstaged.is_empty());
    }

    #[test]
    fn parses_unmerged_entry_as_unstaged() {
        let out = b"u UU N... 100644 100644 100644 100644 aaaa bbbb cccc conflict.rs\0";
        let s = parse_status(out).unwrap();
        assert_eq!(s.unstaged, [change("conflict.rs", ChangeKind::Unmerged)]);
    }

    #[test]
    fn keeps_non_utf8_paths() {
        let mut out = b"? bad-".to_vec();
        out.extend([0xff, 0xfe]);
        out.push(0);
        let s = parse_status(&out).unwrap();
        assert_eq!(s.untracked[0].as_os_str().as_bytes(), b"bad-\xff\xfe");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_status(b"x what\0").is_err());
        assert!(parse_status(b"1 .M too short\0").is_err());
    }

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

    #[test]
    fn reads_status_of_a_real_repository() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        git(d, &["init", "-q", "-b", "main"]);
        for f in ["mod.txt", "del.txt", "old.txt", "both.txt"] {
            write(d, f, "one\ntwo\nthree\nfour\n");
        }
        git(d, &["add", "."]);
        git(d, &["commit", "-q", "-m", "init"]);

        write(d, "mod.txt", "changed\n");
        fs::remove_file(d.join("del.txt")).unwrap();
        git(d, &["mv", "old.txt", "new.txt"]);
        write(d, "both.txt", "staged\n");
        git(d, &["add", "both.txt"]);
        write(d, "both.txt", "staged\nand more\n");
        write(d, "added.txt", "x\n");
        git(d, &["add", "added.txt"]);
        fs::create_dir(d.join("sub")).unwrap();
        write(d, "sub/untracked.txt", "u\n");

        let mut s = Git::new(d).status().unwrap();
        assert_eq!(
            s.staged,
            [
                change("added.txt", ChangeKind::Added),
                change("both.txt", ChangeKind::Modified),
                change(
                    "new.txt",
                    ChangeKind::Renamed {
                        from: "old.txt".into()
                    }
                ),
            ]
        );
        assert_eq!(
            s.unstaged,
            [
                change("both.txt", ChangeKind::Modified),
                change("del.txt", ChangeKind::Deleted),
                change("mod.txt", ChangeKind::Modified),
            ]
        );
        assert_eq!(s.untracked, [PathBuf::from("sub/untracked.txt")]);

        s.retain_scope(crate::args::Scope::Staged);
        assert!(s.unstaged.is_empty() && s.untracked.is_empty());
    }

    #[test]
    fn works_from_a_subdirectory_and_reports_root_relative_paths() {
        let tmp = TempDir::new().unwrap();
        let d = tmp.path();
        git(d, &["init", "-q", "-b", "main"]);
        fs::create_dir(d.join("sub")).unwrap();
        write(d, "top.txt", "x\n");
        let s = Git::new(d.join("sub")).status().unwrap();
        assert_eq!(s.untracked, [PathBuf::from("top.txt")]);
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
        let err = Git::new(tmp.path()).status().unwrap_err();
        assert!(matches!(err, Error::Git(ref m) if m.contains("not a git repository")));
    }
}
