mod args;
mod diff;
mod vcs;

use diff::{FileDiff, LineKind};
use std::path::Path;
use std::process::ExitCode;
use vcs::{DiffRequest, Git, Vcs};

fn print_diff(files: &[FileDiff]) {
    for f in files {
        let (added, removed) = (f.count(LineKind::Added), f.count(LineKind::Removed));
        match (&f.old_path, f.status) {
            (Some(from), diff::FileStatus::Renamed | diff::FileStatus::Copied) => println!(
                "{} {} -> {} (+{added} -{removed})",
                f.status.letter(),
                from.display(),
                f.path().display()
            ),
            _ => println!(
                "{} {} (+{added} -{removed})",
                f.status.letter(),
                f.path().display()
            ),
        }
        if f.binary {
            println!("  binary file");
        }
        for h in &f.hunks {
            println!(
                "@@ -{},{} +{},{} @@ {}",
                h.old_start,
                h.old_len,
                h.new_start,
                h.new_len,
                String::from_utf8_lossy(&h.heading)
            );
            for l in &h.lines {
                let marker = match l.kind {
                    LineKind::Context => ' ',
                    LineKind::Added => '+',
                    LineKind::Removed => '-',
                };
                let no = |n: Option<u32>| n.map_or(String::new(), |n| n.to_string());
                println!(
                    "{:>5} {:>5} {marker}{}",
                    no(l.old_no),
                    no(l.new_no),
                    String::from_utf8_lossy(&l.content)
                );
                if l.no_newline {
                    println!("{:>11} \\ No newline at end of file", "");
                }
            }
        }
    }
}

fn run() -> Result<(), vcs::Error> {
    let args = args::parse();
    let dir = args.directory.as_deref().unwrap_or(Path::new("."));
    let request = DiffRequest {
        scope: args.scope,
        revisions: args.revisions,
        paths: args.paths,
        context: args.context,
    };
    let files = Git::new(dir).diff(&request)?;

    if files.is_empty() {
        println!("No changes");
    } else {
        print_diff(&files);
    }
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
