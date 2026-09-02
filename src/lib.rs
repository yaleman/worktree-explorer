mod git;
mod tui;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(version, about = "Explore and manage Git worktrees")]
struct Args {
    /// A repository or a directory contained by one
    #[arg(default_value = ".")]
    directory: PathBuf,

    /// Also scan immediate child directories for Git repositories
    #[arg(long)]
    recursive: bool,
}

pub fn run() -> Result<()> {
    let args = Args::parse();
    let repositories = git::discover_repositories(&args.directory, args.recursive)?;
    tui::run(repositories, args.recursive)
}
