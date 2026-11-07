use super::{ChangeKind, Error, FileChange, Status, Vcs};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Git {
    dir: PathBuf,
}

impl Git {
    /// Runs git as if started in `dir`; git finds the repository root itself.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Git { dir: dir.into() }
    }

    fn run(&self, args: &[&str]) -> Result<Vec<u8>, Error> {
        let out = Command::new("git")
            .args(["--no-optional-locks", "-c", "core.quotepath=off"])
            .args(args)
            .current_dir(&self.dir)
            .output()?;
        if !out.status.success() {
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(Error::Git(msg));
        }
        Ok(out.stdout)
    }
}

impl Vcs for Git {
    fn status(&self) -> Result<Status, Error> {
        let out = self.run(&["status", "--porcelain=v2", "-z", "--untracked-files=all"])?;
        parse_status(&out)
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
        status.staged.push(FileChange { path: path(file), kind });
    }
    if let Some(kind) = kind(xy[1], from)? {
        status.unstaged.push(FileChange { path: path(file), kind });
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
    use std::fs;
    use tempfile::TempDir;

    fn change(p: &str, kind: ChangeKind) -> FileChange {
        FileChange { path: p.into(), kind }
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
            [change("new.rs", ChangeKind::Added), change("both.rs", ChangeKind::Modified)]
        );
        assert_eq!(
            s.unstaged,
            [change("src/a.rs", ChangeKind::Modified), change("both.rs", ChangeKind::Modified)]
        );
        assert_eq!(s.untracked, [PathBuf::from("notes.txt")]);
    }

    #[test]
    fn parses_rename_with_original_path() {
        let out = b"2 R. N... 100644 100644 100644 aaaa bbbb R100 new name.rs\0old name.rs\0";
        let s = parse_status(out).unwrap();
        assert_eq!(
            s.staged,
            [change("new name.rs", ChangeKind::Renamed { from: "old name.rs".into() })]
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
                change("new.txt", ChangeKind::Renamed { from: "old.txt".into() }),
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

    #[test]
    fn reports_a_clear_error_outside_a_repository() {
        let tmp = TempDir::new().unwrap();
        let err = Git::new(tmp.path()).status().unwrap_err();
        assert!(matches!(err, Error::Git(ref m) if m.contains("not a git repository")));
    }
}
