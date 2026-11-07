mod args;
mod vcs;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use vcs::{FileChange, Git, Vcs};

fn print_changes(title: &str, changes: &[FileChange]) {
    if changes.is_empty() {
        return;
    }
    println!("{title}");
    for c in changes {
        match &c.kind {
            vcs::ChangeKind::Renamed { from } | vcs::ChangeKind::Copied { from } => println!(
                "  {} {} -> {}",
                c.kind.letter(),
                from.display(),
                c.path.display()
            ),
            _ => println!("  {} {}", c.kind.letter(), c.path.display()),
        }
    }
}

fn print_untracked(paths: &[PathBuf]) {
    if paths.is_empty() {
        return;
    }
    println!("Untracked");
    for p in paths {
        println!("  ? {}", p.display());
    }
}

fn run() -> Result<(), vcs::Error> {
    let args = args::parse();
    let dir = args.directory.as_deref().unwrap_or(Path::new("."));
    let mut status = Git::new(dir).status()?;
    status.retain_scope(args.scope);

    if status.is_empty() {
        println!("No changes");
        return Ok(());
    }
    print_changes("Staged", &status.staged);
    print_changes("Unstaged", &status.unstaged);
    print_untracked(&status.untracked);
    Ok(())
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
