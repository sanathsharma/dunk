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

**Context**:
The unchanged lines shown above and below a change in a hunk. The `context` setting, or git's default of 3 when unset, gives the default amount. `+` adds 3 lines on each side of the hunk under the cursor, `-` takes 3 away and `=` returns to the default. Hunks whose context touches merge into one.
_Avoid_: expand, collapse (for changing the amount), unchanged lines (as a term)

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

**Syntax theme**:
The set of colors used to highlight source code in a diff, chosen with the `theme` setting. It does not color the diff itself (added and removed lines, hunk headers).
_Avoid_: theme (on its own), colorscheme

**Highlight group**:
A named style slot for part of the screen, such as `DiffAdd` for added lines or `Border` for pane borders, with an optional foreground, background and text attributes. Names are CamelCase, after Neovim's. A group the user does not set uses its built-in ANSI default.

**Pan**:
The number of columns the code in the diff is shifted left, set with `h` and `l`. One pan applies to every hunk of the file and to both columns of the split layout. The line numbers, `+`/`-` markers and hunk headers do not move. It returns to 0 when another file is selected.
_Avoid_: horizontal scroll (scroll is vertical)

**Hunk header**:
The row that opens a hunk. It is drawn but never holds the cursor or a selection.

**Cursor**:
The row key actions apply to in the diff. It is always on a line, never on a hunk header.
_Avoid_: selected row (selection and section already use "select")
