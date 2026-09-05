#![deny(warnings)]
#![warn(unused_extern_crates)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![deny(clippy::unreachable)]
#![deny(clippy::await_holding_lock)]
#![deny(clippy::needless_pass_by_value)]
#![deny(clippy::trivially_copy_pass_by_ref)]

mod git;
mod tui;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
#[command(version, about = "Explore and manage Git worktrees and local branches")]
struct Args {
    /// A repository or a directory contained by one
    #[arg(default_value = ".")]
    directory: PathBuf,

    /// Also scan immediate child directories for Git repositories
    #[arg(long)]
    recursive: bool,

    /// Manage local branches instead of worktrees
    #[arg(long)]
    branches: bool,
}

pub fn run() -> Result<()> {
    let args = Args::parse();
    let repositories = git::discover_repositories(&args.directory, args.recursive)?;
    tui::run(repositories, args.recursive, args.branches)
}
