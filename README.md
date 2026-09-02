# Worktree Explorer

Worktree Explorer is a terminal interface for viewing and managing the Git
worktrees attached to a repository.

It shows each worktree's current commit, the age of that commit, branch, and
location. From the list you can inspect commit history, review local changes,
or remove a worktree. Repositories and their worktrees are displayed as a
tree.

## Build

You need a current Rust toolchain.

    cargo build --release

The executable will be written to **target/release/worktree-explorer**.

## Run

Run it from anywhere inside a Git repository:

    cargo run --release

You can also point it at a repository or any directory inside one:

    cargo run --release -- ../my-repository

If you have installed or copied the executable somewhere on your PATH, use:

    worktree-explorer [DIRECTORY]

**DIRECTORY** defaults to the current directory.

To scan **DIRECTORY** and its immediate child directories for repositories,
use:

    worktree-explorer --recursive [DIRECTORY]

The recursive scan is limited to one level. Repositories discovered through
more than one checkout are shown only once.

## Controls

| Key | Action |
| --- | --- |
| ↑ / ↓ or j / k | Move or scroll |
| Page Up / Page Down | Move or scroll one page |
| h (recursive mode) | Hide or show repositories without linked worktrees |
| l | Show the selected worktree's latest commits |
| s | Show staged, unstaged, conflicted, and untracked changes |
| d | Delete the selected linked worktree |
| r | Refresh the worktree list |
| Esc or q | Return to the worktree list |
| q from the worktree list | Quit |
| Ctrl-C | Quit immediately |

## Deleting worktrees

Deletion always requires confirmation.

If a worktree contains local changes or is locked, Worktree Explorer displays
the affected paths and requires a second force confirmation with uppercase
**D**. Large change lists can be reviewed with the normal scroll keys.

The main worktree cannot be deleted. Worktree Explorer also refuses to delete
the worktree containing its current working directory. Removing a worktree
does not delete its Git branch.

## Terminal cleanup

Worktree Explorer clears the terminal when it starts and when it exits. It also
restores normal input mode and makes the cursor visible again after quitting or
encountering an error.
