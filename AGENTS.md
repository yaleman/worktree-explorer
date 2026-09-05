# Repository Guidelines

## Project Structure & Module Organization

This is a small Rust binary crate:

- `src/main.rs` is the executable entry point.
- `src/lib.rs` parses CLI arguments and starts the application.
- `src/git.rs` contains pure-Rust Git operations built on `gix`, including
  repository discovery, worktrees, branches, status, logs, and deletion.
- `src/tui.rs` contains the `ratatui`/`crossterm` interface and interaction
  state.
- Unit tests live beside their implementation under `#[cfg(test)]` modules.
- `README.md` documents user-facing behavior; there are no separate assets or
  generated source directories.

## Build, Test, and Development Commands

Run these from the repository root:

```text
cargo build
cargo run -- [DIRECTORY]
cargo run -- --recursive [DIRECTORY]
cargo run -- --branches [DIRECTORY]
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

Use `cargo run` for interactive checks. The application defaults to the
current directory; `--recursive` scans one level of child directories, and
`--branches` switches from worktrees to local branches.

## Coding Style & Naming Conventions

Use stable Rust, four-space indentation, and idiomatic `snake_case` functions
and variables with `UpperCamelCase` types and enums. Run `cargo fmt` before
committing. Prefer typed enums and structs for Git outcomes and handle production
errors with `anyhow::Result` and context. Keep Git access in `src/git.rs`; do
not add shell-command or `git2` dependencies.

## Testing Guidelines

Add focused unit tests beside the code they cover. Use descriptive names such
as `recursive_discovery_deduplicates_linked_checkouts`. Tests should use
temporary repositories and avoid modifying a developer's checkout. Before a
change is submitted, run `cargo test`, formatting checks, and strict Clippy.

## Commit & Pull Request Guidelines

Use short, imperative commit subjects that describe one cohesive change, for
example `Add local branch management mode`. Pull requests should explain the
user-visible behavior, mention validation commands, and include terminal
screenshots or a short recording for meaningful TUI changes. Keep unrelated
refactors out of feature commits.

## Safety Notes

Deletion is destructive and must retain confirmation, force handling, and
checked-out/current-directory protections. Preserve terminal cleanup behavior
on both normal exit and errors.
