# Dunk

A terminal tool for reviewing changes before they are committed.

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

**Review**:
Going through a changeset to decide what belongs in the next commit. It ends by staging what the reviewer accepts; the index is the only record of that decision.
_Avoid_: approve, mark as reviewed

**Staged**:
A change that is already in the index and so would go into the next commit. A change that is not is **unstaged**; an untracked file is unstaged until it is added.

**Mode**:
What the keys currently do. In normal mode keys move around and act on a file or hunk. In visual mode keys extend a selection of lines. The footer always names the current mode.

**Selection**:
The run of lines a visual-mode action applies to.

**Section**:
One of the two groups in the file list: unstaged and staged. A file with both kinds of change appears in both. The diff shown for an entry covers only its own section.

**Config file**:
The TOML file holding the user's standing defaults for settings. A flag on the command line overrides it for one run.

**Setting**:
An option that can be set in the config file and on the command line. Scope, revisions and paths are not settings, because they define a changeset rather than how it is displayed.
_Avoid_: preference, option
