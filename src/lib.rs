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
}

pub fn run() -> Result<()> {
    let args = Args::parse();
    let repository = git::RepositoryManager::discover(&args.directory)?;
    tui::run(repository)
}
