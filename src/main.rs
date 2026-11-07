mod app;
mod args;
mod config;
mod diff;
mod highlight;
mod palette;
mod split;
mod ui;
mod vcs;
mod watch;
mod words;
mod worker;

use config::{Config, Settings};
use std::error::Error;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::ExitCode;
use vcs::{DiffRequest, Git};

fn run() -> Result<(), Box<dyn Error>> {
  let args = args::parse();
  let config = Config::load(args.config.as_deref())?;
  let settings = Settings::resolve(&args, config);
  if !io::stdout().is_terminal() {
    return Err("dunk needs a terminal".into());
  }
  let dir = args.directory.as_deref().unwrap_or(Path::new("."));
  let request = DiffRequest {
    scope: args.scope,
    revisions: args.revisions,
    paths: args.paths,
    context: settings.context,
  };

  let git = Git::new(dir);
  let root = if settings.watch {
    Some(git.root()?)
  } else {
    None
  };

  let mut terminal = ratatui::try_init()?;
  let result = app::run(&mut terminal, git, request, settings, root.as_deref());
  ratatui::try_restore()?;
  Ok(result?)
}

fn main() -> ExitCode {
  match run() {
    Ok(()) => ExitCode::SUCCESS,
    Err(e) => {
      eprintln!("dunk: {e}");
      ExitCode::FAILURE
    }
  }
}
