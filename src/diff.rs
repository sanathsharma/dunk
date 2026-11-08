use std::collections::BTreeSet;
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
  /// Has unresolved merge conflicts. Such an entry has no hunks.
  Conflicted,
}

impl FileStatus {
  pub const fn letter(self) -> char {
    match self {
      Self::Added => 'A',
      Self::Deleted => 'D',
      Self::Modified => 'M',
      Self::Renamed => 'R',
      Self::Copied => 'C',
      Self::Conflicted => 'U',
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

/// The part of the working tree a file entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
  Unstaged,
  Staged,
}

fn to_u32(n: usize) -> u32 {
  u32::try_from(n).unwrap_or(u32::MAX)
}

fn to_usize(n: u32) -> usize {
  usize::try_from(n).unwrap_or(usize::MAX)
}

/// A path with its `a/` or `b/` prefix, quoted the way git does when it holds
/// control characters, quotes or backslashes.
fn prefixed(prefix: &[u8], path: &Path) -> Vec<u8> {
  let mut name = prefix.to_vec();
  name.extend(path.as_os_str().as_bytes());
  if !name.iter().any(|&b| b < 0x20 || b == b'"' || b == b'\\') {
    return name;
  }
  let mut quoted = vec![b'"'];
  for b in name {
    match b {
      b'"' | b'\\' => quoted.extend([b'\\', b]),
      b'\t' => quoted.extend(b"\\t"),
      b'\n' => quoted.extend(b"\\n"),
      b if b < 0x20 => quoted.extend(format!("\\{b:03o}").bytes()),
      b => quoted.push(b),
    }
  }
  quoted.push(b'"');
  quoted
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
  pub old_path: Option<PathBuf>,
  pub new_path: Option<PathBuf>,
  pub status: FileStatus,
  pub binary: bool,
  /// Set for entries of a working tree changeset; `None` for a revision.
  pub section: Option<Section>,
  pub hunks: Vec<Hunk>,
}

impl Hunk {
  /// The index just past the run of `kind` lines that starts at `start`.
  pub fn run_end(&self, start: usize, kind: LineKind) -> usize {
    self
      .lines
      .iter()
      .skip(start)
      .position(|l| l.kind != kind)
      .map_or(self.lines.len(), |n| start.saturating_add(n))
  }

  /// Indexes of the added and removed lines.
  pub fn changed(&self) -> impl Iterator<Item = usize> + Clone + '_ {
    self
      .lines
      .iter()
      .enumerate()
      .filter(|(_, l)| l.kind != LineKind::Context)
      .map(|(i, _)| i)
  }
}

impl FileDiff {
  /// The path the file has after the change, or before it for a deletion.
  pub fn path(&self) -> &Path {
    self
      .new_path
      .as_deref()
      .or(self.old_path.as_deref())
      .unwrap_or_else(|| Path::new(""))
  }

  pub fn count(&self, kind: LineKind) -> usize {
    self
      .hunks
      .iter()
      .flat_map(|h| &h.lines)
      .filter(|l| l.kind == kind)
      .count()
  }

  /// A patch that holds only hunk `hunk`, for `git apply`. It has no `diff --git`
  /// line, which would need mode lines for a new or deleted file. A new file is
  /// written as `--- /dev/null` and a deleted one as `+++ /dev/null`.
  pub fn hunk_patch(&self, hunk: usize) -> Vec<u8> {
    let Some(h) = self.hunks.get(hunk) else {
      return Vec::new();
    };
    let old = self.old_path.as_deref().map(|p| prefixed(b"a/", p));
    let new = self.new_path.as_deref().map(|p| prefixed(b"b/", p));
    let name = |side: &Option<Vec<u8>>| side.clone().unwrap_or_else(|| b"/dev/null".to_vec());
    let mut out = Vec::new();
    out.extend(b"--- ");
    out.extend(name(&old));
    out.extend(b"\n+++ ");
    out.extend(name(&new));
    out.extend(
      format!(
        "\n@@ -{},{} +{},{} @@",
        h.old_start, h.old_len, h.new_start, h.new_len
      )
      .bytes(),
    );
    if !h.heading.is_empty() {
      out.push(b' ');
      out.extend(&h.heading);
    }
    out.push(b'\n');
    for line in &h.lines {
      out.push(match line.kind {
        LineKind::Context => b' ',
        LineKind::Added => b'+',
        LineKind::Removed => b'-',
      });
      out.extend(&line.content);
      out.push(b'\n');
      if line.no_newline {
        out.extend(b"\\ No newline at end of file\n");
      }
    }
    out
  }

  /// A patch that holds only the changed lines of hunk `hunk` that fall in
  /// `lines`, a set of indexes into the hunk's lines. A removed line outside the
  /// range becomes context and an added one outside it is left out, so the
  /// patch changes nothing but the selection.
  pub fn line_patch(&self, hunk: usize, lines: &BTreeSet<usize>) -> Vec<u8> {
    let Some(h) = self.hunks.get(hunk) else {
      return Vec::new();
    };
    let mut kept: Vec<Line> = Vec::new();
    for (i, line) in h.lines.iter().enumerate() {
      if lines.contains(&i) || line.kind == LineKind::Context {
        kept.push(line.clone());
        continue;
      }
      match line.kind {
        LineKind::Added => {}
        LineKind::Removed
          if line.no_newline
            && h
              .lines
              .iter()
              .enumerate()
              .skip(i.saturating_add(1))
              .any(|(at, l)| l.kind == LineKind::Added && lines.contains(&at)) =>
        {
          // Text can only follow a line that ends in a newline. The old line
          // goes and comes back with its newline, ahead of the added lines.
          kept.push(line.clone());
          kept.push(Line {
            kind: LineKind::Added,
            old_no: None,
            no_newline: false,
            ..line.clone()
          });
        }
        _ => kept.push(Line {
          kind: LineKind::Context,
          ..line.clone()
        }),
      }
    }
    let count = |kinds: [LineKind; 2]| kept.iter().filter(|l| kinds.contains(&l.kind)).count();
    let narrowed = Hunk {
      old_len: to_u32(count([LineKind::Context, LineKind::Removed])),
      new_len: to_u32(count([LineKind::Context, LineKind::Added])),
      lines: kept,
      ..h.clone()
    };
    // A deleted file with lines left over is no longer deleted.
    let new_path = match &self.new_path {
      None if narrowed.new_len > 0 => self.old_path.clone(),
      path => path.clone(),
    };
    Self {
      new_path,
      hunks: vec![narrowed],
      ..self.clone()
    }
    .hunk_patch(0)
  }

  /// A patch that undoes hunk `hunk`. It is written out rather than left to
  /// `git apply --reverse`, which cannot read a `/dev/null` header backwards.
  pub fn reverse_hunk_patch(&self, hunk: usize) -> Vec<u8> {
    self.reversed_hunk(hunk).hunk_patch(0)
  }

  /// A patch that undoes only the changed lines of hunk `hunk` that fall in
  /// `lines`, as `line_patch` selects them.
  pub fn reverse_line_patch(&self, hunk: usize, lines: &BTreeSet<usize>) -> Vec<u8> {
    self.reversed_hunk(hunk).line_patch(0, lines)
  }

  /// A file that holds hunk `hunk` turned around: the sides swap and so do
  /// added and removed lines. The lines keep their order and index.
  fn reversed_hunk(&self, hunk: usize) -> Self {
    let Some(h) = self.hunks.get(hunk) else {
      return Self {
        hunks: Vec::new(),
        ..self.clone()
      };
    };
    let mut h = h.clone();
    (h.old_start, h.new_start) = (h.new_start, h.old_start);
    (h.old_len, h.new_len) = (h.new_len, h.old_len);
    for line in &mut h.lines {
      line.kind = match line.kind {
        LineKind::Added => LineKind::Removed,
        LineKind::Removed => LineKind::Added,
        LineKind::Context => LineKind::Context,
      };
    }
    Self {
      old_path: self.new_path.clone(),
      new_path: self.old_path.clone(),
      hunks: vec![h],
      ..self.clone()
    }
  }

  /// The file with `extra[i]` more lines of context above and below hunk `i`,
  /// read from `text`, the file after the change. Context that reaches a
  /// neighboring hunk's context, or touches the hunk itself, merges the two.
  /// Lines beyond either end of the file are not shown. Also returns, for each
  /// hunk of the result, the index of the first hunk of `self` it holds.
  pub fn widened(&self, extra: &[u32], text: &[u8]) -> (Self, Vec<usize>) {
    let hunks = &self.hunks;
    let extra_of = |i: usize| to_usize(extra.get(i).copied().unwrap_or(0));
    let mut file: Vec<&[u8]> = text.split(|&b| b == b'\n').collect();
    let ends_with_newline = file.last().is_some_and(|l| l.is_empty());
    if ends_with_newline {
      file.pop();
    }
    let total = to_u32(file.len());
    let line_at = |new_no: u32, delta: i64| Line {
      kind: LineKind::Context,
      old_no: u32::try_from(i64::from(new_no).saturating_add(delta)).ok(),
      new_no: Some(new_no),
      content: file
        .get(to_usize(new_no).saturating_sub(1))
        .copied()
        .unwrap_or_default()
        .to_vec(),
      no_newline: !ends_with_newline && new_no == total,
    };
    // Gap `g` lies above hunk `g`, so gap 0 is the top of the file and gap
    // `hunks.len()` the bottom. `below` is what hunk `g` shows above itself,
    // `above` what hunk `g - 1` shows below itself. A merged gap shows all of
    // its lines as `above`.
    let gaps: Vec<Gap> = (0..=hunks.len())
      .map(|gap| {
        let up = gap.checked_sub(1).and_then(|i| hunks.get(i));
        let down = hunks.get(gap);
        let lo = up.map_or(1, |h| h.last_new().saturating_add(1));
        let hi = down
          .map_or(total, |h| h.first_new().saturating_sub(1))
          .min(total);
        if lo > hi {
          return Gap::default();
        }
        // Old line numbers trail the new ones by the same amount across a gap.
        let delta = match (up, down) {
          (Some(h), _) => i64::from(h.last_old()).saturating_sub(i64::from(h.last_new())),
          (None, Some(h)) => i64::from(h.first_old()).saturating_sub(i64::from(h.first_new())),
          (None, None) => return Gap::default(),
        };
        let size = to_usize(hi.saturating_sub(lo).saturating_add(1));
        let from_above = up.map_or(0, |_| extra_of(gap.saturating_sub(1)));
        let from_below = down.map_or(0, |_| extra_of(gap));
        if up.is_some() && down.is_some() && from_above.saturating_add(from_below) >= size {
          return Gap {
            above: (lo..=hi).map(|n| line_at(n, delta)).collect(),
            below: Vec::new(),
            merged: true,
          };
        }
        let shown_above = to_u32(from_above.min(size));
        let shown_below = to_u32(from_below.min(size));
        Gap {
          above: (lo..lo.saturating_add(shown_above))
            .map(|n| line_at(n, delta))
            .collect(),
          below: (hi.saturating_add(1).saturating_sub(shown_below)..=hi)
            .map(|n| line_at(n, delta))
            .collect(),
          merged: false,
        }
      })
      .collect();
    let mut shown: Vec<Hunk> = Vec::new();
    let mut owners = Vec::new();
    let mut i = 0;
    while let Some(first) = hunks.get(i) {
      owners.push(i);
      let mut hunk = first.clone();
      let own = gaps.get(i);
      let mut changed = own.is_some_and(|g| !g.below.is_empty());
      if let Some(g) = own {
        hunk.lines.splice(0..0, g.below.iter().cloned());
      }
      loop {
        let next = gaps.get(i.saturating_add(1));
        if let Some(g) = next {
          changed |= !g.above.is_empty();
          hunk.lines.extend(g.above.iter().cloned());
        }
        if !next.is_some_and(|g| g.merged) {
          break;
        }
        i = i.saturating_add(1);
        if let Some(merged) = hunks.get(i) {
          hunk.lines.extend(merged.lines.iter().cloned());
        }
      }
      if changed {
        hunk.recount();
      }
      shown.push(hunk);
      i = i.saturating_add(1);
    }
    (
      Self {
        hunks: shown,
        ..self.clone()
      },
      owners,
    )
  }
}

/// What the context around one hunk looks like in `FileDiff::widened`.
#[derive(Default)]
struct Gap {
  above: Vec<Line>,
  below: Vec<Line>,
  merged: bool,
}

impl Hunk {
  /// Takes the start and length of each side from the hunk's lines.
  fn recount(&mut self) {
    let old = self.lines.iter().filter_map(|l| l.old_no);
    let new = self.lines.iter().filter_map(|l| l.new_no);
    self.old_start = old.clone().next().unwrap_or(self.old_start);
    self.old_len = to_u32(old.count());
    self.new_start = new.clone().next().unwrap_or(self.new_start);
    self.new_len = to_u32(new.count());
  }

  /// The first line the hunk covers on the old side. A hunk with no lines on a
  /// side names the line before it, so its first line is the next one.
  fn first_old(&self) -> u32 {
    self.old_start.saturating_add(u32::from(self.old_len == 0))
  }

  fn first_new(&self) -> u32 {
    self.new_start.saturating_add(u32::from(self.new_len == 0))
  }

  fn last_old(&self) -> u32 {
    self
      .old_start
      .saturating_add(self.old_len)
      .saturating_sub(1)
      .max(self.old_start)
  }

  fn last_new(&self) -> u32 {
    self
      .new_start
      .saturating_add(self.new_len)
      .saturating_sub(1)
      .max(self.new_start)
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
  while let Some(&b) = s.get(i) {
    match b {
      b'\\' => i = i.saturating_add(2),
      b'"' => return Some(i.saturating_add(1)),
      _ => i = i.saturating_add(1),
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
        let mut v = u32::from(d.saturating_sub(b'0'));
        for _ in 0..2 {
          match it.peek() {
            Some(&n @ b'0'..=b'7') => {
              v = v
                .saturating_mul(8)
                .saturating_add(u32::from(n.saturating_sub(b'0')));
              it.next();
            }
            _ => break,
          }
        }
        out.push(u8::try_from(v & 0xFF).unwrap_or_default());
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
    let old = unquote(rest.get(..end)?);
    let new = unquote(rest.get(end.saturating_add(1)..)?);
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
  let old = rest.get(..n.saturating_add(2))?.strip_prefix(b"a/")?;
  let new = rest.get(n.saturating_add(3)..)?.strip_prefix(b"b/")?;
  (rest.get(n.saturating_add(2)) == Some(&b' ') && old == new).then(|| (path(old), path(new)))
}

fn range(s: &[u8]) -> Option<(u32, u32)> {
  match s.iter().position(|&b| b == b',') {
    Some(p) => Some((num(s.get(..p)?)?, num(s.get(p.saturating_add(1)..)?)?)),
    None => Some((num(s)?, 1)),
  }
}

/// Parses `@@ -a[,b] +c[,d] @@ heading` into a hunk with no lines yet.
fn hunk_header(line: &[u8]) -> Option<Hunk> {
  let rest = line.strip_prefix(b"@@ -")?;
  let end = find(rest, b" @@")?;
  let (ranges, tail) = (rest.get(..end)?, rest.get(end.saturating_add(3)..)?);
  let sep = find(ranges, b" +")?;
  let (old_start, old_len) = range(ranges.get(..sep)?)?;
  let (new_start, new_len) = range(ranges.get(sep.saturating_add(2)..)?)?;
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
fn hunk_lines(hunk: &mut Hunk, lines: &[&[u8]], mut i: usize) -> Result<usize, ParseError> {
  let (mut old_left, mut new_left) = (hunk.old_len, hunk.new_len);
  let (mut old_no, mut new_no) = (hunk.old_start, hunk.new_start);
  while old_left > 0 || new_left > 0 {
    let raw = *lines.get(i).ok_or_else(|| err("truncated hunk", b""))?;
    i = i.saturating_add(1);
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
    old_no = old_no.saturating_add(u32::from(old.is_some()));
    new_no = new_no.saturating_add(u32::from(new.is_some()));
    let no_newline = lines.get(i).is_some_and(|l| l.starts_with(b"\\"));
    i = i.saturating_add(usize::from(no_newline));
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

fn file_diff(rest: &[u8], lines: &[&[u8]], mut i: usize) -> Result<(FileDiff, usize), ParseError> {
  let (old_path, new_path) = match git_line_paths(rest) {
    Some((o, n)) => (Some(o), Some(n)),
    None => (None, None),
  };
  let mut file = FileDiff {
    old_path,
    new_path,
    status: FileStatus::Modified,
    binary: false,
    section: None,
    hunks: Vec::new(),
  };

  while let Some(&l) = lines.get(i) {
    if l.starts_with(b"@@ ") || l.starts_with(b"diff --git ") {
      break;
    }
    i = i.saturating_add(1);
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
    i = hunk_lines(&mut hunk, lines, i.saturating_add(1))?;
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
    i = i.saturating_add(1);
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

  /// The new side of `MODIFIED`: 22 lines named `l1` to `l22`.
  fn new_text() -> Vec<u8> {
    (1..=22)
      .map(|n| format!("l{n}\n"))
      .collect::<String>()
      .into_bytes()
  }

  fn widen(f: &FileDiff, extra: &[u32], text: &[u8]) -> FileDiff {
    f.widened(extra, text).0
  }

  #[test]
  fn widening_across_a_gap_merges_the_hunks_and_keeps_line_numbers() {
    let f = one(MODIFIED);
    let (f, owners) = f.widened(&[15, 0], &new_text());
    assert_eq!(owners, [0]);
    assert_eq!(f.hunks.len(), 1);
    let h = &f.hunks[0];
    assert_eq!(
      (h.old_start, h.old_len, h.new_start, h.new_len),
      (1, 20, 1, 21)
    );
    assert_eq!(h.heading, b"fn main() {");
    let revealed: Vec<_> = h
      .lines
      .iter()
      .filter(|l| l.kind == LineKind::Context && l.new_no > Some(5) && l.new_no < Some(21))
      .collect();
    assert_eq!(revealed.len(), 15);
    assert_eq!(revealed[0].content, b"l6");
    assert_eq!((revealed[0].old_no, revealed[0].new_no), (Some(5), Some(6)));
    assert_eq!(revealed[14].content, b"l20");
    assert_eq!(
      (revealed[14].old_no, revealed[14].new_no),
      (Some(19), Some(20))
    );
    // The hunk after the gap keeps its own numbers.
    let last = h.lines.last().unwrap();
    assert_eq!(
      (last.kind, last.old_no, last.new_no),
      (LineKind::Added, None, Some(21))
    );
  }

  #[test]
  fn widening_a_gap_part_way_leaves_the_hunks_apart() {
    let f = one(MODIFIED);
    let (f, owners) = f.widened(&[5, 5], &new_text());
    assert_eq!(owners, [0, 1]);
    assert_eq!(f.hunks.len(), 2);
    let first: Vec<_> = f.hunks[0].lines.iter().filter_map(|l| l.new_no).collect();
    assert_eq!(first, (1..=10).collect::<Vec<_>>());
    let second: Vec<_> = f.hunks[1].lines.iter().filter_map(|l| l.new_no).collect();
    assert_eq!(second, (16..=22).collect::<Vec<_>>());
    assert_eq!((f.hunks[1].old_start, f.hunks[1].old_len), (15, 7));
  }

  #[test]
  fn contexts_that_touch_merge_and_a_hunk_far_away_stays_apart() {
    let f = one(MODIFIED);
    // The gap holds lines 6 to 20, fifteen lines: 8 and 7 just cover it.
    assert_eq!(f.widened(&[8, 7], &new_text()).0.hunks.len(), 1);
    assert_eq!(f.widened(&[8, 6], &new_text()).0.hunks.len(), 2);
  }

  #[test]
  fn widening_nothing_gives_the_hunks_as_they_were() {
    let f = one(MODIFIED);
    let (same, owners) = f.widened(&[0, 0], &new_text());
    assert_eq!(same, f);
    assert_eq!(owners, [0, 1]);
  }

  #[test]
  fn widening_the_top_and_bottom_extends_the_first_and_last_hunk() {
    let last = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -20 +20 @@\n-x\n+y\n";
    let f = widen(&one(last), &[5], &new_text());
    let h = &f.hunks[0];
    assert_eq!(
      (h.old_start, h.old_len, h.new_start, h.new_len),
      (15, 8, 15, 8)
    );
    let end = h.lines.last().unwrap();
    assert_eq!(end.content, b"l22");
    assert_eq!((end.old_no, end.new_no), (Some(22), Some(22)));
    let mut text = new_text();
    text.extend(b"l23");
    let f = widen(&one(last), &[5], &text);
    assert_eq!(f.hunks[0].lines.last().unwrap().content, b"l23");
    assert!(f.hunks[0].lines.last().unwrap().no_newline);

    let top = one("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -3 +3 @@\n-x\n+y\n");
    let top = widen(&top, &[5], b"a\nb\ny\n");
    let h = &top.hunks[0];
    assert_eq!(
      (h.old_start, h.old_len, h.new_start, h.new_len),
      (1, 3, 1, 3)
    );
    assert_eq!((h.lines[0].old_no, h.lines[0].new_no), (Some(1), Some(1)));
    assert_eq!(h.lines[1].content, b"b");
  }

  #[test]
  fn widening_past_the_ends_of_the_file_shows_what_exists() {
    let f = one(MODIFIED);
    let once = widen(&f, &[15, 15], &new_text());
    assert_eq!(widen(&f, &[50, 50], &new_text()), once);
    assert_eq!(once.hunks[0].lines.len(), 24);
  }

  fn numbers(f: &FileDiff) -> Vec<(Option<u32>, Option<u32>)> {
    f.hunks[0]
      .lines
      .iter()
      .map(|l| (l.old_no, l.new_no))
      .collect()
  }

  #[test]
  fn widening_around_a_pure_addition_numbers_both_sides() {
    let f = one("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -2,0 +3,2 @@\n+x\n+y\n");
    let f = widen(&f, &[5], b"l1\nl2\nx\ny\nl5\nl6\n");
    let h = &f.hunks[0];
    assert_eq!(
      (h.old_start, h.old_len, h.new_start, h.new_len),
      (1, 4, 1, 6)
    );
    assert_eq!(
      numbers(&f),
      [
        (Some(1), Some(1)),
        (Some(2), Some(2)),
        (None, Some(3)),
        (None, Some(4)),
        (Some(3), Some(5)),
        (Some(4), Some(6)),
      ]
    );
  }

  #[test]
  fn widening_around_a_pure_deletion_numbers_both_sides() {
    let f = one("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -3,2 +2,0 @@\n-c\n-d\n");
    let f = widen(&f, &[5], b"a\nb\ne\nf\n");
    let h = &f.hunks[0];
    assert_eq!(
      (h.old_start, h.old_len, h.new_start, h.new_len),
      (1, 6, 1, 4)
    );
    assert_eq!(
      numbers(&f),
      [
        (Some(1), Some(1)),
        (Some(2), Some(2)),
        (Some(3), None),
        (Some(4), None),
        (Some(5), Some(3)),
        (Some(6), Some(4)),
      ]
    );
  }

  #[test]
  fn widening_a_whole_file_change_or_a_missing_file_reveals_nothing() {
    let added = one(
      "diff --git a/f b/f\nnew file mode 100644\n--- /dev/null\n+++ b/f\n@@ -0,0 +1,2 @@\n+a\n+b\n",
    );
    assert_eq!(widen(&added, &[5], b"a\nb\n"), added);

    let deleted = one(
      "diff --git a/f b/f\ndeleted file mode 100644\n--- a/f\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-a\n-b\n",
    );
    assert_eq!(widen(&deleted, &[5], b""), deleted);
  }

  #[test]
  fn revealed_lines_keep_carriage_returns_like_diff_lines_do() {
    let f = one("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -2 +2 @@\n-x\r\n+y\r\n");
    let f = widen(&f, &[5], b"l1\r\ny\r\n");
    let lines = &f.hunks[0].lines;
    assert_eq!(lines[0].content, b"l1\r");
    assert_eq!(lines[2].content, b"y\r");
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

    let f = one("diff --git \"a/\\303\\251\" \"b/\\303\\251\"\nold mode 100644\nnew mode 100755\n");
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

  #[test]
  fn a_hunk_patch_parses_back_to_that_hunk() {
    let f = one(MODIFIED);
    // The parser wants the header line that `git apply` can do without.
    let patch = String::from_utf8(f.hunk_patch(1)).unwrap();
    let again = one(&format!("diff --git a/src/a.rs b/src/a.rs\n{patch}"));
    assert_eq!(again.hunks, [f.hunks[1].clone()]);
    assert_eq!(again.path(), f.path());
  }

  #[test]
  fn a_hunk_patch_marks_new_and_deleted_files_and_missing_newlines() {
    let added = one(
      "diff --git a/f b/f\nnew file mode 100644\n--- /dev/null\n+++ b/f\n@@ -0,0 +1 @@\n+a\n\\ No newline at end of file\n",
    );
    let patch = String::from_utf8(added.hunk_patch(0)).unwrap();
    assert!(patch.contains("--- /dev/null\n+++ b/f\n"));
    assert!(patch.ends_with("+a\n\\ No newline at end of file\n"));
    let deleted = one(
      "diff --git a/f b/f\ndeleted file mode 100644\n--- a/f\n+++ /dev/null\n@@ -1 +0,0 @@\n-a\n",
    );
    let patch = String::from_utf8(deleted.hunk_patch(0)).unwrap();
    assert!(patch.contains("--- a/f\n+++ /dev/null\n"));
  }

  #[test]
  fn a_hunk_patch_quotes_unusual_paths() {
    let mut f = one(MODIFIED);
    f.old_path = Some(PathBuf::from("a\tb\"c.txt"));
    f.new_path = f.old_path.clone();
    let patch = String::from_utf8(f.hunk_patch(0)).unwrap();
    assert!(patch.contains("--- \"a/a\\tb\\\"c.txt\"\n"));
  }

  #[test]
  fn a_reverse_patch_swaps_the_sides() {
    let f = one(
      "diff --git a/f b/f\nnew file mode 100644\n--- /dev/null\n+++ b/f\n@@ -0,0 +1,2 @@\n+a\n+b\n",
    );
    let patch = String::from_utf8(f.reverse_hunk_patch(0)).unwrap();
    assert!(patch.contains("--- a/f\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-a\n-b\n"));
  }
}
