use crate::diff::{Hunk, LineKind};

/// Indexes into `Hunk::lines` for the left (old) and right (new) column of
/// one split row. `None` leaves that column blank.
pub type Pair = (Option<usize>, Option<usize>);

/// Lays a hunk out as split rows. Context lines appear on both sides. A run
/// of removed lines followed by a run of added lines shares rows, so the
/// first removed line sits beside the first added line. Surplus lines of
/// the longer run get a blank opposite column.
pub fn pairs(hunk: &Hunk) -> Vec<Pair> {
  let lines = &hunk.lines;
  let mut out = Vec::with_capacity(lines.len());
  let mut i = 0;
  while let Some(line) = lines.get(i) {
    if line.kind == LineKind::Context {
      out.push((Some(i), Some(i)));
      i = i.saturating_add(1);
    } else {
      let removed_end = hunk.run_end(i, LineKind::Removed);
      let added_end = hunk.run_end(removed_end, LineKind::Added);
      let (removed, added) = (i..removed_end, removed_end..added_end);
      for k in 0..removed.len().max(added.len()) {
        out.push((removed.clone().nth(k), added.clone().nth(k)));
      }
      i = added_end;
    }
  }
  out
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
  fn context_lines_fill_both_columns() {
    assert_eq!(
      pairs(&hunk(" a\n b\n")),
      [(Some(0), Some(0)), (Some(1), Some(1))]
    );
  }

  #[test]
  fn removed_and_added_runs_share_rows() {
    let p = pairs(&hunk(" a\n-b\n-c\n+d\n+e\n f\n"));
    assert_eq!(
      p,
      [
        (Some(0), Some(0)),
        (Some(1), Some(3)),
        (Some(2), Some(4)),
        (Some(5), Some(5))
      ]
    );
  }

  #[test]
  fn surplus_lines_get_a_blank_opposite_column() {
    assert_eq!(
      pairs(&hunk("-a\n-b\n+c\n")),
      [(Some(0), Some(2)), (Some(1), None)]
    );
    assert_eq!(
      pairs(&hunk("-a\n+b\n+c\n")),
      [(Some(0), Some(1)), (None, Some(2))]
    );
  }

  #[test]
  fn pure_additions_and_removals_have_one_blank_column() {
    assert_eq!(pairs(&hunk("+a\n")), [(None, Some(0))]);
    assert_eq!(pairs(&hunk("-a\n")), [(Some(0), None)]);
  }
}
