use clap::{Arg, ArgAction, ArgMatches, Command};
use std::env;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
  Unified,
  Split,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
  /// Staged, unstaged and untracked changes together.
  #[default]
  All,
  Staged,
  Unstaged,
}

#[derive(Debug)]
pub struct Args {
  pub directory: Option<PathBuf>,
  pub revisions: Vec<String>,
  pub paths: Vec<String>,
  pub scope: Scope,
  pub layout: Layout,
  pub theme: Option<String>,
  pub context: Option<u32>,
  pub watch: bool,
  pub no_syntax: bool,
}

fn command() -> Command {
  Command::new("dunk")
    .version(env!("CARGO_PKG_VERSION"))
    .about("Terminal viewer for git diffs")
    .arg(
      Arg::new("revisions")
        .help("Revision or range to diff, e.g. HEAD~3 or main..feature")
        .value_name("REV")
        .num_args(0..=2),
    )
    .arg(
      Arg::new("paths")
        .help("Limit the diff to these pathspecs")
        .value_name("PATH")
        .last(true)
        .num_args(0..),
    )
    .arg(
      Arg::new("directory")
        .short('C')
        .long("directory")
        .help("Run as if started in this directory")
        .value_name("PATH"),
    )
    .arg(
      Arg::new("staged")
        .long("staged")
        .help("Show only staged changes")
        .action(ArgAction::SetTrue)
        .conflicts_with_all(["unstaged", "revisions"]),
    )
    .arg(
      Arg::new("unstaged")
        .long("unstaged")
        .help("Show only unstaged changes")
        .action(ArgAction::SetTrue)
        .conflicts_with("revisions"),
    )
    .arg(
      Arg::new("layout")
        .long("layout")
        .help("How to lay out the diff")
        .value_name("LAYOUT")
        .value_parser(["unified", "split"])
        .default_value("unified"),
    )
    .arg(
      Arg::new("theme")
        .long("theme")
        .help("Syntax highlighting theme")
        .value_name("NAME")
        .value_parser(crate::highlight::resolve_theme),
    )
    .arg(
      Arg::new("context")
        .short('U')
        .long("context")
        .help("Number of context lines around each change")
        .value_name("N")
        .value_parser(clap::value_parser!(u32)),
    )
    .arg(
      Arg::new("watch")
        .short('w')
        .long("watch")
        .help("Refresh when files change")
        .action(ArgAction::SetTrue),
    )
    .arg(
      Arg::new("no-syntax")
        .long("no-syntax")
        .help("Disable syntax highlighting")
        .action(ArgAction::SetTrue),
    )
}

fn from_matches(m: &ArgMatches) -> Args {
  let scope = if m.get_flag("staged") {
    Scope::Staged
  } else if m.get_flag("unstaged") {
    Scope::Unstaged
  } else {
    Scope::All
  };
  let layout = match m.get_one::<String>("layout").map(String::as_str) {
    Some("split") => Layout::Split,
    _ => Layout::Unified,
  };
  let many = |id: &str| {
    m.get_many::<String>(id)
      .map(|v| v.cloned().collect())
      .unwrap_or_default()
  };

  Args {
    directory: m.get_one::<String>("directory").map(PathBuf::from),
    revisions: many("revisions"),
    paths: many("paths"),
    scope,
    layout,
    theme: m.get_one::<String>("theme").cloned(),
    context: m.get_one::<u32>("context").copied(),
    watch: m.get_flag("watch"),
    no_syntax: m.get_flag("no-syntax"),
  }
}

pub fn parse() -> Args {
  from_matches(&command().get_matches())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn parse_from(args: &[&str]) -> Args {
    from_matches(&command().get_matches_from(args))
  }

  #[test]
  fn defaults() {
    let a = parse_from(&["dunk"]);
    assert_eq!(a.scope, Scope::All);
    assert_eq!(a.layout, Layout::Unified);
    assert!(a.revisions.is_empty() && a.paths.is_empty());
  }

  #[test]
  fn revisions_and_paths() {
    let a = parse_from(&["dunk", "main", "feature", "--", "src/", "*.rs"]);
    assert_eq!(a.revisions, ["main", "feature"]);
    assert_eq!(a.paths, ["src/", "*.rs"]);
  }

  #[test]
  fn staged_conflicts_with_revision() {
    assert!(
      command()
        .try_get_matches_from(["dunk", "--staged", "HEAD"])
        .is_err()
    );
  }

  #[test]
  fn theme_is_validated_and_canonicalized() {
    assert_eq!(
      parse_from(&["dunk", "--theme", "nord"]).theme.as_deref(),
      Some("Nord")
    );
    let err = command()
      .try_get_matches_from(["dunk", "--theme", "nope"])
      .unwrap_err()
      .to_string();
    assert!(err.contains("unknown theme 'nope'"), "{err}");
  }

  #[test]
  fn flags() {
    let a = parse_from(&["dunk", "-w", "--layout", "split", "-U", "5", "--no-syntax"]);
    assert!(a.watch && a.no_syntax);
    assert_eq!(a.layout, Layout::Split);
    assert_eq!(a.context, Some(5));
  }
}
