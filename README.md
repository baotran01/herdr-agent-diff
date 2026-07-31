# Herdr Agent Diff

Herdr Agent Diff is a macOS arm64 plugin for [Herdr](https://herdr.dev/) that makes local Git changes easy to inspect. It opens a read-only terminal UI beside the current pane or in a separate Herdr tab.

The viewer has two tabs:

- **Changes** shows Git changes and unpushed commits. Selecting a file opens a readable code diff.
- **Files** shows the workspace tree with language badges, line numbers, and syntax highlighting for source files.

The Changes tab has two comparison views:

- **Git diff** compares the working tree with `HEAD`, including staged, unstaged, deleted, renamed, and untracked files.
- **Unpushed commits** compares `HEAD` with the branch's tracked remote (`@{upstream}`), showing committed changes that are not on the remote branch yet.

Press `g` to switch between the two Changes views. This makes it easy to see both edits that still need committing and commits that still need pushing.

## Features

- Combined local Git diff for staged, unstaged, deleted, renamed, and untracked files.
- Unpushed-commit diff for committed changes ahead of the tracked remote branch.
- A grouped, collapsible folder tree shared by the Changes and Files tabs.
- Right-aligned per-file `+` and `-` counts in the Changes tab.
- Unified code diffs with old/new line-number gutters, addition/deletion highlighting, and collapsed unchanged regions.
- File browsing with an immediate plain-text preview, followed by syntax highlighting based on file extensions.
- Text search, keyboard navigation, mouse support, and scrollbars for both the sidebar and diff pane.
- Hideable sidebar shared by the Changes and Files tabs (`b`).
- Read-only operation on the project workspace and Git repository.

## Tech stack

- **Language:** Rust.
- **Terminal UI:** [`ratatui`](https://ratatui.rs/) for layout, widgets, rendering, and test backends.
- **Terminal input and backend:** [`crossterm`](https://github.com/crossterm-rs/crossterm) for keyboard and mouse events, raw mode, alternate-screen handling, and terminal capabilities.
- **File watching:** [`notify`](https://github.com/notify-rs/notify) for detecting workspace changes while the viewer is open.
- **Diff parsing and syntax highlighting:** a bounded unified diff parser and [`syntect`](https://github.com/trishume/syntect) for syntax highlighting.
- **Workspace scanning:** [`ignore`](https://github.com/BurntSushi/ripgrep/tree/master/crates/ignore) for ignore-file-aware traversal, with platform filesystem support through [`rustix`](https://github.com/bytecodealliance/rustix) on Unix.
- **Integration:** A native macOS arm64 Herdr plugin using read-only Git subprocesses.

## Requirements

- macOS on Apple silicon (`aarch64-apple-darwin`).
- Rust and Cargo.
- Herdr 0.7.5 or newer.
- Git.
- A tracked remote branch (`@{upstream}`) to use Unpushed commits mode.

The plugin is currently packaged for macOS arm64 because its manifest declares that target and its release command builds for `aarch64-apple-darwin`.

## Build and install

Build the release binary:

```sh
cargo build --release --target aarch64-apple-darwin
```

Link the project into Herdr from the repository root:

```sh
herdr plugin link "$PWD"
```

The plugin manifest, [`herdr-plugin.toml`](herdr-plugin.toml), registers pane cleanup events and the following actions:

| Name | Purpose |
| --- | --- |
| `pane.closed` | Remove stale viewer mappings for a closed pane. |
| `pane.exited` | Remove stale viewer mappings for an exited pane. |
| `herdr-agent-diff.open` | Open the viewer in a split beside the agent pane. |
| `herdr-agent-diff.open-tab` | Open the viewer in a separate Herdr tab. |

Example Herdr key bindings:

```toml
[[keys.command]]
key = "prefix+d"
type = "plugin_action"
command = "herdr-agent-diff.open"
description = "Git changes"

[[keys.command]]
key = "prefix+shift+d"
type = "plugin_action"
command = "herdr-agent-diff.open-tab"
description = "Git changes in tab"
```

## Using the viewer

### Changes tab

The sidebar is organized by folders. Click a folder row, or place the cursor on it and press Enter, to expand or collapse that folder. Select a file to inspect its diff. Press `b` to hide or show the sidebar.

The Changes tab opens in Git diff mode. Press `g` to switch to Unpushed commits mode:

```text
Git diff  ⇄  Unpushed commits
```

Git diff answers: “What local edits are not in `HEAD`?” Unpushed commits answers: “What committed edits are ahead of the tracked remote branch?”

### Files tab

The Files tab uses the same folder grouping, indentation, collapse behavior, selection gutter, and navigation styling as the Changes tab. Select a file to browse its current contents with line numbers and syntax highlighting. Press `b` to hide or show the sidebar, or `/` to search filenames or relative paths with a case-insensitive substring filter.

### Keyboard controls

| Key | Action |
| --- | --- |
| `1` | Select the Changes tab. |
| `2` | Select the Files tab. |
| `b` | Hide or show the sidebar. |
| `g` | Toggle Git diff and Unpushed commits modes. |
| `Tab` | Move focus between the sidebar and diff/content pane. |
| Arrow keys, `h`/`j`/`k`/`l` | Navigate the focused area. |
| `Enter` | Open a selected file or toggle a selected folder. |
| `/` | Filter the sidebar. |
| `r` | Refresh the current comparison or file view. |
| `⌘C` | Copy selected text from the read-only Files pane. |
| `?` | Show the help overlay. |
| `q` or `Esc` | Close the viewer or dismiss an overlay. |

Mouse clicks select tabs, folders, and files. Drag the sidebar or diff scrollbar to move quickly through long content. Scrolling over a pane keeps the pointer's pane active.

## Diff semantics

### Git diff

Git diff compares the current working tree with `HEAD` using read-only Git commands. It includes:

- staged changes;
- unstaged changes;
- deleted and renamed tracked files; and
- untracked, non-ignored files.

The Changes sidebar groups files by `staged`, `unstaged`, `mixed` (both staged and unstaged), or `untracked` status. Empty status groups are hidden, and folders remain collapsible within each group.

This is the view for edits that still need to be committed.

### Unpushed commits

Unpushed commits compares `HEAD` with `@{upstream}`. It shows committed changes that exist on the local branch but have not reached its tracked remote branch. It does not include current uncommitted or untracked edits; switch to Git diff for those.

If the branch has no upstream, the viewer explains that `git push -u` is required before unpushed commits can be determined.

The plugin does not modify the index, create commits, stage files, push changes, or run write-oriented Git commands.

## What the plugin reads and ignores

The scanner is deliberately conservative:

- It never follows symbolic links while scanning the workspace.
- It honors supported ignore files, including `.gitignore` and `.ignore`.
- It excludes common repository metadata and generated directories such as `.git`, `.hg`, `.svn`, `node_modules`, `target`, `dist`, `.next`, and `.cache`.
- It limits individual text reads to 2 MiB and skips binary or invalid UTF-8 content from inline source browsing.
- It applies file-count, file-size, scan-byte, and Git-output limits to keep the viewer responsive.

The plugin stores only viewer mappings under `HERDR_PLUGIN_STATE_DIR`, which Herdr supplies for the installed plugin. Project files are not rewritten, and the plugin does not transmit workspace contents to a network service.

## Architecture

| Module | Responsibility |
| --- | --- |
| `src/main.rs` | Herdr action/event entry points, viewer startup, and mapping cleanup. |
| `src/model.rs` | Current-file metadata, text eligibility, and Git change kinds. |
| `src/snapshot.rs` | Safe workspace scanning and bounded file reads. |
| `src/diff.rs` | Unified diff parsing and rendered diff rows. |
| `src/git.rs` | Read-only working-tree and unpushed Git comparisons. |
| `src/app.rs` | Terminal UI, tabs, navigation, filtering, scrolling, input, and rendering. |
| `src/state.rs` | Viewer mapping persistence. |

The high-level lifecycle is:

```text
open / open-tab ──► scan workspace ──► load Git changes ──► render viewer
                                      │
                                      ├── g ──► compare committed changes with @{upstream}
                                      └── b ──► hide/show sidebar
```

## Development

Run formatting, linting, tests, and a release build before publishing changes:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release --target aarch64-apple-darwin
```

The test suite covers working-tree Git changes, unpushed commits, workspace scanning, safe reads, viewer mappings, folder navigation/collapse, diff rendering, and terminal UI behavior.

## Troubleshooting

### The Git diff view is empty

Verify that the workspace is a Git repository with an initial commit (`HEAD`). Git diff includes staged, unstaged, and untracked changes, but ignored files are intentionally excluded.

### The Unpushed commits view is unavailable

Verify that the current branch tracks a remote branch. Run `git push -u <remote> <branch>` once to establish the upstream, then refresh the viewer.

### A file is not shown

Check whether it is ignored, a symbolic link, binary/invalid UTF-8, larger than the configured limits, or inside an excluded/generated directory. Large or binary files may still appear in the Changes sidebar as metadata-only diffs.

### The plugin does not appear in Herdr

Re-run `herdr plugin link "$PWD"`, confirm that the release build succeeds, and restart Herdr or the affected pane. Check that the installed Herdr version satisfies the minimum version in `herdr-plugin.toml`.

## Privacy and security

Herdr Agent Diff is designed for local inspection. It does not embed credentials, send source files to an external service, or mutate the workspace. Git subprocesses are invoked with lock-free read behavior and fixed read-only arguments. The viewer displays workspace contents to whoever can access the local Herdr session, so use the same care as with any terminal showing source code.
