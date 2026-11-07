use std::ffi::OsStr;
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
}

impl FileStatus {
    pub fn letter(self) -> char {
        match self {
            FileStatus::Added => 'A',
            FileStatus::Deleted => 'D',
            FileStatus::Modified => 'M',
            FileStatus::Renamed => 'R',
            FileStatus::Copied => 'C',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    /// The line without its marker or newline. Raw bytes, not necessarily UTF-8.
    pub content: Vec<u8>,
    /// The file ends without a newline after this line.
    pub no_newline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    /// The text after the closing `@@`, usually the enclosing function.
    pub heading: Vec<u8>,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub old_path: Option<PathBuf>,
    pub new_path: Option<PathBuf>,
    pub status: FileStatus,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    /// The path the file has after the change, or before it for a deletion.
    pub fn path(&self) -> &Path {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .expect("parser guarantees one path")
    }

    pub fn count(&self, kind: LineKind) -> usize {
        self.hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.kind == kind)
            .count()
    }
}

#[derive(Debug)]
pub struct ParseError(String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

fn err(what: &str, line: &[u8]) -> ParseError {
    ParseError(format!("{what}: {}", String::from_utf8_lossy(line)))
}

fn path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(bytes))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn num(bytes: &[u8]) -> Option<u32> {
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

/// Returns the index just past the closing quote of a C-style quoted string.
fn quoted_end(s: &[u8]) -> Option<usize> {
    let mut i = 1;
    while i < s.len() {
        match s[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Decodes git's C-style quoting. Unquoted input is returned as is.
fn unquote(s: &[u8]) -> Vec<u8> {
    let Some(inner) = s.strip_prefix(b"\"").and_then(|s| s.strip_suffix(b"\"")) else {
        return s.to_vec();
    };
    let mut out = Vec::with_capacity(inner.len());
    let mut it = inner.iter().copied().peekable();
    while let Some(b) = it.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match it.next() {
            Some(b'a') => out.push(7),
            Some(b'b') => out.push(8),
            Some(b'f') => out.push(12),
            Some(b'n') => out.push(b'\n'),
            Some(b'r') => out.push(b'\r'),
            Some(b't') => out.push(b'\t'),
            Some(b'v') => out.push(11),
            Some(d @ b'0'..=b'7') => {
                let mut v = u32::from(d - b'0');
                for _ in 0..2 {
                    match it.peek() {
                        Some(&n @ b'0'..=b'7') => {
                            v = v * 8 + u32::from(n - b'0');
                            it.next();
                        }
                        _ => break,
                    }
                }
                out.push(v as u8);
            }
            Some(c) => out.push(c),
            None => out.push(b'\\'),
        }
    }
    out
}

/// Reads the path on a `---`, `+++`, `rename` or `copy` line. `/dev/null` is no path.
fn header_path(raw: &[u8], prefix: &[u8]) -> Option<PathBuf> {
    let raw = raw.strip_suffix(b"\t").unwrap_or(raw);
    let bytes = unquote(raw);
    if bytes == b"/dev/null" {
        return None;
    }
    Some(path(bytes.strip_prefix(prefix).unwrap_or(&bytes)))
}

/// Reads both paths from `diff --git a/P b/P`. Only a same-path header can be
/// split reliably when names contain spaces; renames get their paths from
/// the `rename from` and `rename to` lines instead.
fn git_line_paths(rest: &[u8]) -> Option<(PathBuf, PathBuf)> {
    if rest.first() == Some(&b'"') {
        let end = quoted_end(rest)?;
        let old = unquote(&rest[..end]);
        let new = unquote(rest.get(end + 1..)?);
        return Some((
            path(old.strip_prefix(b"a/")?),
            path(new.strip_prefix(b"b/")?),
        ));
    }
    let n = rest.len().checked_sub(5)?;
    if n % 2 != 0 {
        return None;
    }
    let n = n / 2;
    let (old, new) = (&rest[..n + 2], &rest[n + 3..]);
    if rest[n + 2] == b' '
        && old.starts_with(b"a/")
        && new.starts_with(b"b/")
        && old[2..] == new[2..]
    {
        Some((path(&old[2..]), path(&new[2..])))
    } else {
        None
    }
}

fn range(s: &[u8]) -> Option<(u32, u32)> {
    match s.iter().position(|&b| b == b',') {
        Some(p) => Some((num(&s[..p])?, num(&s[p + 1..])?)),
        None => Some((num(s)?, 1)),
    }
}

/// Parses `@@ -a[,b] +c[,d] @@ heading` into a hunk with no lines yet.
fn hunk_header(line: &[u8]) -> Option<Hunk> {
    let rest = line.strip_prefix(b"@@ -")?;
    let end = find(rest, b" @@")?;
    let (ranges, tail) = (&rest[..end], &rest[end + 3..]);
    let sep = find(ranges, b" +")?;
    let (old_start, old_len) = range(&ranges[..sep])?;
    let (new_start, new_len) = range(&ranges[sep + 2..])?;
    Some(Hunk {
        old_start,
        old_len,
        new_start,
        new_len,
        heading: tail.strip_prefix(b" ").unwrap_or(tail).to_vec(),
        lines: Vec::new(),
    })
}

/// Reads hunk lines by the counts in the header, so a content line that looks
/// like `diff --git` or `--- ` cannot end the hunk early.
fn hunk_lines<'a>(hunk: &mut Hunk, lines: &[&'a [u8]], mut i: usize) -> Result<usize, ParseError> {
    let (mut old_left, mut new_left) = (hunk.old_len, hunk.new_len);
    let (mut old_no, mut new_no) = (hunk.old_start, hunk.new_start);
    while old_left > 0 || new_left > 0 {
        let raw = *lines.get(i).ok_or_else(|| err("truncated hunk", b""))?;
        i += 1;
        let (kind, content) = match raw.split_first() {
            Some((b' ', c)) => (LineKind::Context, c),
            None => (LineKind::Context, raw),
            Some((b'+', c)) => (LineKind::Added, c),
            Some((b'-', c)) => (LineKind::Removed, c),
            _ => return Err(err("unexpected line in hunk", raw)),
        };
        let counts = |l: u32| {
            l.checked_sub(1)
                .ok_or_else(|| err("hunk longer than its header", raw))
        };
        let (old, new) = match kind {
            LineKind::Context => {
                old_left = counts(old_left)?;
                new_left = counts(new_left)?;
                (Some(old_no), Some(new_no))
            }
            LineKind::Removed => {
                old_left = counts(old_left)?;
                (Some(old_no), None)
            }
            LineKind::Added => {
                new_left = counts(new_left)?;
                (None, Some(new_no))
            }
        };
        old_no += u32::from(old.is_some());
        new_no += u32::from(new.is_some());
        let no_newline = lines.get(i).is_some_and(|l| l.starts_with(b"\\"));
        i += usize::from(no_newline);
        hunk.lines.push(Line {
            kind,
            old_no: old,
            new_no: new,
            content: content.to_vec(),
            no_newline,
        });
    }
    Ok(i)
}

fn file_diff<'a>(
    rest: &[u8],
    lines: &[&'a [u8]],
    mut i: usize,
) -> Result<(FileDiff, usize), ParseError> {
    let (old_path, new_path) = match git_line_paths(rest) {
        Some((o, n)) => (Some(o), Some(n)),
        None => (None, None),
    };
    let mut file = FileDiff {
        old_path,
        new_path,
        status: FileStatus::Modified,
        binary: false,
        hunks: Vec::new(),
    };

    while let Some(&l) = lines.get(i) {
        if l.starts_with(b"@@ ") || l.starts_with(b"diff --git ") {
            break;
        }
        i += 1;
        if l.starts_with(b"new file mode") {
            file.status = FileStatus::Added;
            file.old_path = None;
        } else if l.starts_with(b"deleted file mode") {
            file.status = FileStatus::Deleted;
            file.new_path = None;
        } else if let Some(p) = l.strip_prefix(b"rename from ") {
            file.status = FileStatus::Renamed;
            file.old_path = header_path(p, b"");
        } else if let Some(p) = l.strip_prefix(b"rename to ") {
            file.new_path = header_path(p, b"");
        } else if let Some(p) = l.strip_prefix(b"copy from ") {
            file.status = FileStatus::Copied;
            file.old_path = header_path(p, b"");
        } else if let Some(p) = l.strip_prefix(b"copy to ") {
            file.new_path = header_path(p, b"");
        } else if let Some(p) = l.strip_prefix(b"--- ") {
            file.old_path = header_path(p, b"a/");
        } else if let Some(p) = l.strip_prefix(b"+++ ") {
            file.new_path = header_path(p, b"b/");
        } else if l.starts_with(b"Binary files ") || l.starts_with(b"GIT binary patch") {
            file.binary = true;
        }
    }

    while let Some(&l) = lines.get(i).filter(|l| l.starts_with(b"@@ ")) {
        let mut hunk = hunk_header(l).ok_or_else(|| err("bad hunk header", l))?;
        i = hunk_lines(&mut hunk, lines, i + 1)?;
        file.hunks.push(hunk);
    }

    if file.old_path.is_none() && file.new_path.is_none() {
        return Err(err("diff without a path", rest));
    }
    Ok((file, i))
}

/// Parses the output of `git diff` with a/ and b/ prefixes and no color.
pub fn parse(input: &[u8]) -> Result<Vec<FileDiff>, ParseError> {
    let mut lines: Vec<&[u8]> = input.split(|&b| b == b'\n').collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let mut files = Vec::new();
    let mut i = 0;
    while let Some(&l) = lines.get(i) {
        i += 1;
        if let Some(rest) = l.strip_prefix(b"diff --git ") {
            let (file, next) = file_diff(rest, &lines, i)?;
            files.push(file);
            i = next;
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(input: &str) -> FileDiff {
        let mut files = parse(input.as_bytes()).unwrap();
        assert_eq!(files.len(), 1);
        files.remove(0)
    }

    const MODIFIED: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,4 +1,5 @@ fn main() {
 keep
-old
+new
+extra
 tail
 last
@@ -20 +21 @@
-x
+y
";

    #[test]
    fn parses_hunks_with_line_numbers() {
        let f = one(MODIFIED);
        assert_eq!(f.status, FileStatus::Modified);
        assert_eq!(f.path(), Path::new("src/a.rs"));
        assert_eq!(f.hunks.len(), 2);
        let h = &f.hunks[0];
        assert_eq!(
            (h.old_start, h.old_len, h.new_start, h.new_len),
            (1, 4, 1, 5)
        );
        assert_eq!(h.heading, b"fn main() {");
        let nums: Vec<_> = h
            .lines
            .iter()
            .map(|l| (l.kind, l.old_no, l.new_no))
            .collect();
        assert_eq!(
            nums,
            [
                (LineKind::Context, Some(1), Some(1)),
                (LineKind::Removed, Some(2), None),
                (LineKind::Added, None, Some(2)),
                (LineKind::Added, None, Some(3)),
                (LineKind::Context, Some(3), Some(4)),
                (LineKind::Context, Some(4), Some(5)),
            ]
        );
        let h = &f.hunks[1];
        assert_eq!(
            (h.old_start, h.old_len, h.new_start, h.new_len),
            (20, 1, 21, 1)
        );
        assert_eq!(f.count(LineKind::Added), 3);
        assert_eq!(f.count(LineKind::Removed), 2);
    }

    #[test]
    fn parses_several_files() {
        let two = format!(
            "{MODIFIED}diff --git a/b b/b\nindex 1..2 100644\n--- a/b\n+++ b/b\n@@ -1 +1 @@\n-1\n+2\n"
        );
        let files = parse(two.as_bytes()).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].path(), Path::new("b"));
    }

    #[test]
    fn parses_added_and_deleted_files() {
        let added = one(
            "diff --git a/n b/n\nnew file mode 100644\nindex 0..1\n--- /dev/null\n+++ b/n\n@@ -0,0 +1,2 @@\n+a\n+b\n",
        );
        assert_eq!(added.status, FileStatus::Added);
        assert_eq!(added.old_path, None);
        assert_eq!(added.hunks[0].lines[1].new_no, Some(2));

        let deleted = one(
            "diff --git a/d b/d\ndeleted file mode 100644\nindex 1..0\n--- a/d\n+++ /dev/null\n@@ -1 +0,0 @@\n-gone\n",
        );
        assert_eq!(deleted.status, FileStatus::Deleted);
        assert_eq!(deleted.new_path, None);
        assert_eq!(deleted.path(), Path::new("d"));
    }

    #[test]
    fn parses_empty_new_file_without_hunks() {
        let f = one("diff --git a/empty b/empty\nnew file mode 100644\nindex 0000000..e69de29\n");
        assert_eq!(f.status, FileStatus::Added);
        assert_eq!(f.path(), Path::new("empty"));
        assert!(f.hunks.is_empty() && !f.binary);
    }

    #[test]
    fn parses_pure_rename_and_rename_with_changes() {
        let pure = one(
            "diff --git a/old name b/new name\nsimilarity index 100%\nrename from old name\nrename to new name\n",
        );
        assert_eq!(pure.status, FileStatus::Renamed);
        assert_eq!(pure.old_path.as_deref(), Some(Path::new("old name")));
        assert_eq!(pure.path(), Path::new("new name"));

        let edited = one(
            "diff --git a/a b/b\nsimilarity index 80%\nrename from a\nrename to b\nindex 1..2 100644\n--- a/a\n+++ b/b\n@@ -1 +1 @@\n-x\n+y\n",
        );
        assert_eq!(edited.status, FileStatus::Renamed);
        assert_eq!(edited.hunks.len(), 1);
    }

    #[test]
    fn parses_binary_files() {
        let f = one(
            "diff --git a/i.png b/i.png\nindex 1..2 100644\nBinary files a/i.png and b/i.png differ\n",
        );
        assert!(f.binary);
        assert!(f.hunks.is_empty());
        assert_eq!(f.path(), Path::new("i.png"));
    }

    #[test]
    fn parses_mode_only_change_with_spaces_in_name() {
        let f = one("diff --git a/my script.sh b/my script.sh\nold mode 100644\nnew mode 100755\n");
        assert_eq!(f.status, FileStatus::Modified);
        assert_eq!(f.path(), Path::new("my script.sh"));
    }

    #[test]
    fn marks_missing_newline_at_end_of_file() {
        let f = one(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+b\n\\ No newline at end of file\n",
        );
        let l = &f.hunks[0].lines;
        assert!(l[0].no_newline && l[1].no_newline);
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn content_that_looks_like_headers_stays_in_the_hunk() {
        let f = one(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,2 +1,3 @@\n--- a/not a header\n+diff --git a/x b/x\n+++ b/y\n context\n",
        );
        assert_eq!(f.hunks[0].lines.len(), 4);
        assert_eq!(f.hunks[0].lines[0].content, b"-- a/not a header");
    }

    #[test]
    fn decodes_quoted_paths() {
        let f = one(
            "diff --git \"a/we ird\\tname\" \"b/we ird\\tname\"\nnew file mode 100644\nindex 0..1\n--- /dev/null\n+++ \"b/we ird\\tname\"\t\n@@ -0,0 +1 @@\n+x\n",
        );
        assert_eq!(f.path(), Path::new("we ird\tname"));

        let f =
            one("diff --git \"a/\\303\\251\" \"b/\\303\\251\"\nold mode 100644\nnew mode 100755\n");
        assert_eq!(f.path(), Path::new("é"));
    }

    #[test]
    fn keeps_non_utf8_bytes() {
        let mut input = b"diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+".to_vec();
        input.extend([0xff, 0xfe, b'\n']);
        let f = parse(&input).unwrap().remove(0);
        assert_eq!(f.hunks[0].lines[1].content, b"\xff\xfe");
    }

    #[test]
    fn empty_input_has_no_files() {
        assert!(parse(b"").unwrap().is_empty());
    }

    #[test]
    fn rejects_malformed_hunks() {
        assert!(parse(b"diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ nonsense @@\n").is_err());
        assert!(parse(b"diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n").is_err());
        assert!(parse(b"diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1 @@\n?bad\n").is_err());
    }
}
