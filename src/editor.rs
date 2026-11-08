use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

/// The editor named by `$VISUAL` or `$EDITOR`, ready to open a file at a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Editor {
  program: String,
  args: Vec<String>,
  kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
  /// Runs in the terminal dunk owns, and takes `+LINE path`.
  Terminal,
  /// A VS Code build. It opens its own window and takes `--goto path:LINE`.
  Code,
}

impl Editor {
  /// `$VISUAL` if set and not blank, else `$EDITOR`.
  pub fn from_env() -> Option<Self> {
    let var = |name: &str| std::env::var_os(name).map(OsString::into_string);
    [var("VISUAL"), var("EDITOR")]
      .into_iter()
      .flatten()
      .flatten()
      .find_map(|value| Self::parse(&value))
  }

  /// Splits `command` on whitespace into the program and its own flags.
  pub fn parse(command: &str) -> Option<Self> {
    let mut words = command.split_whitespace().map(String::from);
    let program = words.next()?;
    let name = Path::new(&program).file_name()?.to_str()?;
    let kind = match name {
      "code" | "code-insiders" | "codium" => Kind::Code,
      _ => Kind::Terminal,
    };
    Some(Self {
      program,
      args: words.collect(),
      kind,
    })
  }

  pub fn program(&self) -> &str {
    &self.program
  }

  /// Whether the editor opens its own window, so dunk can keep the terminal
  /// and need not wait for it.
  pub fn is_gui(&self) -> bool {
    self.kind == Kind::Code
  }

  pub fn command(&self, path: &Path, line: u32) -> Command {
    let mut command = Command::new(&self.program);
    command.args(&self.args);
    match self.kind {
      Kind::Terminal => command.arg(format!("+{line}")).arg(path),
      Kind::Code => {
        let mut target = path.as_os_str().to_os_string();
        target.push(format!(":{line}"));
        command.arg("--goto").arg(target)
      }
    };
    command
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::ffi::OsStr;

  fn args(command: &Command) -> Vec<&OsStr> {
    command.get_args().collect()
  }

  #[test]
  fn a_terminal_editor_gets_plus_line_then_the_path() {
    let editor = Editor::parse("nvim").unwrap();
    assert!(!editor.is_gui());
    let command = editor.command(Path::new("/r/a.rs"), 12);
    assert_eq!(command.get_program(), "nvim");
    assert_eq!(args(&command), ["+12", "/r/a.rs"]);
  }

  #[test]
  fn vs_code_builds_get_goto_with_path_and_line() {
    for name in ["code", "code-insiders", "codium", "/usr/bin/code"] {
      let editor = Editor::parse(name).unwrap();
      assert!(editor.is_gui(), "{name}");
      let command = editor.command(Path::new("/r/a.rs"), 12);
      assert_eq!(args(&command), ["--goto", "/r/a.rs:12"], "{name}");
    }
  }

  #[test]
  fn the_commands_own_flags_come_first() {
    let editor = Editor::parse("code --reuse-window").unwrap();
    let command = editor.command(Path::new("a"), 3);
    assert_eq!(args(&command), ["--reuse-window", "--goto", "a:3"]);
  }

  #[test]
  fn a_blank_command_is_no_editor() {
    assert_eq!(Editor::parse(""), None);
    assert_eq!(Editor::parse("  \t"), None);
  }
}
