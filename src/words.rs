use crate::diff::{Hunk, LineKind};
use similar::{ChangeTag, TextDiff};
use std::ops::Range;

/// Byte ranges of changed words for every line of a hunk, indexed like
/// `Hunk::lines`. A run of removed lines directly followed by a run of added
/// lines is paired in order. Lines without a partner get an empty list.
/// Offsets index the line's content decoded with `String::from_utf8_lossy`.
pub fn changed_words(hunk: &Hunk) -> Vec<Vec<Range<usize>>> {
  let lines = &hunk.lines;
  let mut out = vec![Vec::new(); lines.len()];
  let mut i = 0;
  while i < lines.len() {
    if lines[i].kind != LineKind::Removed {
      i += 1;
      continue;
    }
    let removed_end = run_end(hunk, i, LineKind::Removed);
    let added_end = run_end(hunk, removed_end, LineKind::Added);
    let pairs = (removed_end - i).min(added_end - removed_end);
    for k in 0..pairs {
      let (old, new) = (i + k, removed_end + k);
      let (old_ranges, new_ranges) = diff_words(
        &String::from_utf8_lossy(&lines[old].content),
        &String::from_utf8_lossy(&lines[new].content),
      );
      out[old] = old_ranges;
      out[new] = new_ranges;
    }
    i = added_end.max(removed_end);
  }
  out
}

fn run_end(hunk: &Hunk, start: usize, kind: LineKind) -> usize {
  hunk.lines[start..]
    .iter()
    .position(|l| l.kind != kind)
    .map_or(hunk.lines.len(), |n| start + n)
}

fn diff_words(old: &str, new: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
  let diff = TextDiff::configure().diff_unicode_words(old, new);
  let (mut old_ranges, mut new_ranges) = (Vec::new(), Vec::new());
  let (mut old_at, mut new_at) = (0, 0);
  for change in diff.iter_all_changes() {
    let len = change.value().len();
    match change.tag() {
      ChangeTag::Equal => {
        old_at += len;
        new_at += len;
      }
      ChangeTag::Delete => {
        push(&mut old_ranges, old_at..old_at + len);
        old_at += len;
      }
      ChangeTag::Insert => {
        push(&mut new_ranges, new_at..new_at + len);
        new_at += len;
      }
    }
  }
  (old_ranges, new_ranges)
}

/// Appends `range`, merging it into the previous one when they touch.
fn push(ranges: &mut Vec<Range<usize>>, range: Range<usize>) {
  match ranges.last_mut() {
    Some(last) if last.end == range.start => last.end = range.end,
    _ => ranges.push(range),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::diff::parse;

  fn hunk(body: &str) -> Hunk {
    let count = |marks: [char; 2]| body.lines().filter(|l| l.starts_with(marks)).count();
    let (old, new) = (count([' ', '-']), count([' ', '+']));
    let diff = format!("diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,{old} +1,{new} @@\n{body}");
    parse(diff.as_bytes()).unwrap().remove(0).hunks.remove(0)
  }

  #[test]
  fn marks_only_the_changed_words_of_a_pair() {
    let h = hunk("-let x = foo(a);\n+let x = foo(b);\n");
    let r = changed_words(&h);
    assert_eq!(r[0], vec![12..13]);
    assert_eq!(r[1], vec![12..13]);
  }

  #[test]
  fn pairs_runs_in_order_and_leaves_extras_unmarked() {
    let h = hunk("-one a\n-two a\n-three\n+one b\n+two b\n");
    let r = changed_words(&h);
    assert_eq!(r[0], vec![4..5]);
    assert_eq!(r[1], vec![4..5]);
    assert!(r[2].is_empty());
    assert_eq!(r[3], vec![4..5]);
    assert_eq!(r[4], vec![4..5]);
  }

  #[test]
  fn unpaired_and_context_lines_get_nothing() {
    let h = hunk(" keep\n-gone\n keep\n+new\n");
    assert!(changed_words(&h).iter().all(Vec::is_empty));
  }

  #[test]
  fn handles_a_pure_addition_run() {
    let h = hunk("+added\n+more\n");
    assert!(changed_words(&h).iter().all(Vec::is_empty));
  }
}
