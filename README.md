# Worktree Explorer

Worktree Explorer is a terminal interface for viewing and managing the Git
worktrees and local branches attached to a repository.

It shows each worktree's current commit, the age of that commit, branch, and
location. From the list you can inspect commit history, review local changes,
or remove a worktree. Repositories and their worktrees are displayed as a
tree.

## Build

You need a current Rust toolchain.

```shell
cargo build --release
```

The executable will be written to **target/release/worktree-explorer**.

## Run

Run it from anywhere inside a Git repository:

```shell
cargo run --release
```

You can also point it at a repository or any directory inside one:

```shell
cargo run --release -- ../my-repository
```

If you have installed or copied the executable somewhere on your PATH, use:

```shell
worktree-explorer [DIRECTORY]
```

**DIRECTORY** defaults to the current directory.

To scan **DIRECTORY** and its immediate child directories for repositories,
use:

```shell
worktree-explorer --recursive [DIRECTORY]
```

The recursive scan is limited to one level. Repositories discovered through
more than one checkout are shown only once.

To manage local branches instead of worktrees, use:

```shell
worktree-explorer --branches [DIRECTORY]
```

Branch mode can also be combined with `--recursive`.

## Controls

| Key | Action |
| --- | --- |
| ↑ / ↓ or j / k | Move or scroll |
| Page Up / Page Down | Move or scroll one page |
| h (recursive mode) | Hide or show repositories without linked worktrees |
| l | Show the selected worktree or branch's latest commits |
| s | Show status and upstream information |
| d | Delete the selected linked worktree or local branch |
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

## Branch mode

Branch mode lists local branches with their latest commit and any worktree
where they are checked out. `l` shows branch history. `s` shows the configured
upstream and ahead/behind counts; checked-out branches also show nonzero
working-tree change counts. `d` deletes a local branch after confirmation.
Checked-out branches cannot be deleted, and unmerged branches require the
uppercase **D** force confirmation. Remote-tracking branches are never
modified.
