# Dunk

A terminal viewer for git diffs.

## Language

**Changeset**:
The set of file changes dunk displays. With no revision it is the working tree's staged, unstaged and untracked changes together. With a revision or range it is that diff, read-only.
_Avoid_: patch, diff (for the whole set)

**Scope**:
Which part of the working tree a changeset covers: all, staged only or unstaged only. Does not apply when a revision is given.

**Revision**:
A git commit-ish or range the user names to select a changeset, such as `HEAD~3` or `main..feature`.

**Layout**:
How a diff is drawn: unified (one column) or split (old and new side by side).
