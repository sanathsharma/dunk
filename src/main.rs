mod app;
mod args;
mod diff;
mod highlight;
mod ui;
mod vcs;
mod words;
mod worker;

use std::error::Error;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::ExitCode;
use vcs::{DiffRequest, Git};

fn run() -> Result<(), Box<dyn Error>> {
  let args = args::parse();
  if !io::stdout().is_terminal() {
    return Err("dunk needs a terminal".into());
  }
  let dir = args.directory.as_deref().unwrap_or(Path::new("."));
  let request = DiffRequest {
    scope: args.scope,
    revisions: args.revisions,
    paths: args.paths,
    context: args.context,
  };

  let theme = (!args.no_syntax).then(|| {
    args
      .theme
      .unwrap_or_else(|| highlight::DEFAULT_THEME.to_string())
  });

  let mut terminal = ratatui::try_init()?;
  let result = app::run(&mut terminal, Git::new(dir), request, theme);
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
