<p align="center">
  <a href="https://herdr.dev/">
    <img src="https://herdr.dev/assets/logo.svg" alt="Herdr" width="190">
  </a>
</p>

<h1 align="center">Herdr Agent Diff</h1>

<p align="center">
  A fast, read-only Git review pane for <a href="https://herdr.dev/">Herdr</a>.<br>
  See local changes, unpushed commits, and source files without leaving your terminal.
</p>

<p align="center">
  <a href="https://github.com/baotran01/herdr-agent-diff/releases/latest"><img src="https://img.shields.io/github/v/release/baotran01/herdr-agent-diff?display_name=tag&style=for-the-badge&logo=github&logoColor=white&label=release" alt="Latest release"></a>
  <a href="https://github.com/baotran01/herdr-agent-diff/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/baotran01/herdr-agent-diff/ci.yml?branch=main&style=for-the-badge&logo=githubactions&logoColor=white&label=CI" alt="CI status"></a>
  <a href="https://github.com/baotran01/herdr-agent-diff/blob/main/LICENSE"><img src="https://img.shields.io/github/license/baotran01/herdr-agent-diff?style=for-the-badge&color=blue" alt="MIT license"></a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/macOS-arm64-111827?style=flat-square&logo=apple&logoColor=white" alt="macOS arm64">
  <img src="https://img.shields.io/badge/Linux-x86__64-111827?style=flat-square&logo=linux&logoColor=FCC624" alt="Linux x86_64">
  <img src="https://img.shields.io/badge/Rust-stable-111827?style=flat-square&logo=rust&logoColor=DEA584" alt="Rust stable">
  <img src="https://img.shields.io/badge/read--only-by%20design-16a34a?style=flat-square&logo=git&logoColor=white" alt="Read-only by design">
</p>

<p align="center">
  <a href="#-features">Features</a> ·
  <a href="#-install">Install</a> ·
  <a href="#-controls">Controls</a> ·
  <a href="#-architecture">Architecture</a>
</p>

> [!TIP]
> Press `g` in **Changes** to flip between **Git diff** and **Unpushed commits**. One pane, two answers: *what still needs committing?* and *what still needs pushing?*

```text
┌─ Changes ─────────────────────────┬─ Diff ───────────────────────────────────┐
│ ▾ staged/                         │  12  - fn build_snapshot()               │
│   M src/snapshot.rs         +18 -4│  13  + fn build_snapshot()               │
│ ▾ unstaged/                       │  14    │                                   │
│   M src/app.rs              +42 -9│  @@ unchanged lines collapsed @@        │
│   ? scripts/run.sh          +31   │  15  + viewer refreshes on filesystem      │
└───────────────────────────────────┴───────────────────────────────────────────┘
```

## ✨ Features

| | What you get |
| --- | --- |
| 🔍 | Combined local Git diff for staged, unstaged, deleted, renamed, and untracked files |
| 🚀 | Unpushed-commit diff for work that is ahead of `@{upstream}` |
| 🌳 | Grouped, collapsible folder trees shared by **Changes** and **Files** |
| 🎨 | Syntax highlighting, language badges, line numbers, and readable unified diffs |
| 🖱️ | Mouse support, text selection, scrollbars, and keyboard navigation |
| 🛡️ | Read-only operation: no staging, commits, pushes, or workspace rewrites |

## 🧰 Tech stack

| Layer | Built with |
| --- | --- |
| Language | Rust |
| Terminal UI | [`ratatui`](https://ratatui.rs/) |
| Input & terminal | [`crossterm`](https://github.com/crossterm-rs/crossterm) |
| File watching | [`notify`](https://github.com/notify-rs/notify) |
| Diffs & highlighting | Bounded unified diff parser + [`syntect`](https://github.com/trishume/syntect) |
| Workspace scanning | [`ignore`](https://github.com/BurntSushi/ripgrep/tree/master/crates/ignore) + Unix-safe [`rustix`](https://github.com/bytecodealliance/rustix) reads |

## 💻 Requirements

- macOS on Apple silicon (`aarch64-apple-darwin`) or Linux x86_64 (`x86_64-unknown-linux-gnu`). Linux arm64 (`aarch64-unknown-linux-gnu`) is also supported when built locally.
- Herdr 0.7.0 or newer.
- Git.
- A tracked remote branch (`@{upstream}`) to use Unpushed commits mode.

Linux clipboard copying uses `wl-copy`, `xclip`, or `xsel` when available, and falls back to the terminal's OSC 52 clipboard protocol.

When the viewer runs inside Docker, Podman, or a similar Linux container, it uses
an ignore-aware metadata poller every 500 milliseconds for smaller workspaces,
backing off to two seconds for workspaces with 10,000 or more files. This is
intentional: inotify events from bind-mounted host directories do not always
cross a VM boundary. The poller skips ignored directories and never reads file
contents. Native event watching remains the default on regular Linux hosts.

Marketplace installation does not require Rust or Cargo when the matching GitHub Release is available. The installer downloads the host-specific binary and verifies its SHA-256 checksum. Rust and Cargo are only needed for a local source build or as a fallback before a release is published.

## 🚀 Install

Install from the Herdr Marketplace:

```sh
herdr plugin install baotran01/herdr-agent-diff
```

Herdr runs `scripts/install.sh` during installation. It detects the host target, downloads the matching release, verifies the checksum, and places the binary in the plugin build directory. If that release is unavailable, the script uses Cargo when it is already installed; it does not install Rust or modify the user's toolchain.

### Updating an installed plugin

For a GitHub-managed installation, reinstall the plugin to fetch the latest version and rerun the build step:

```sh
herdr plugin install baotran01/herdr-agent-diff --yes
```

To install a specific release, use its tag:

```sh
herdr plugin install baotran01/herdr-agent-diff --ref v0.1.3 --yes
```

Reinstalling replaces the managed checkout and reruns `scripts/install.sh`. The installer detects macOS or Linux, atomically replaces the binary after a successful download/build, and preserves an existing binary if the update fails. Close any already-open viewer pane and reopen it so Herdr starts the new binary.

For a locally linked checkout, pull the changes and relink it:

```sh
git pull
herdr plugin link "$PWD"
```

For a local source build, build the release binary:

```sh
cargo build --release --target x86_64-unknown-linux-gnu

# macOS arm64
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

## 🎛️ Using the viewer

### 🔄 Changes tab

The sidebar is organized by folders. Click a folder row, or place the cursor on it and press Enter, to expand or collapse that folder. Select a file to inspect its diff. Press `b` to hide or show the sidebar.

The Changes tab opens in Git diff mode. Press `g` to switch to Unpushed commits mode:

```text
Git diff  ⇄  Unpushed commits
```

Git diff answers: “What local edits are not in `HEAD`?” Unpushed commits answers: “What committed edits are ahead of the tracked remote branch?”

### 📄 Files tab

The Files tab uses the same folder grouping, indentation, collapse behavior, selection gutter, and navigation styling as the Changes tab. Select a file to browse its current contents with line numbers and syntax highlighting. Press `b` to hide or show the sidebar, or `/` to search filenames or relative paths with a case-insensitive substring filter.

### 🎛️ Controls

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
| `Ctrl+C` on Linux (`⌘C` on macOS) | Copy selected text from the read-only Files pane. |
| `?` | Show the help overlay. |
| `q` or `Esc` | Close the viewer or dismiss an overlay. |

Mouse clicks select tabs, folders, and files. Drag the sidebar or diff scrollbar to move quickly through long content. Scrolling over a pane keeps the pointer's pane active.

## 🧭 Diff semantics

### 🟢 Git diff

Git diff compares the current working tree with `HEAD` using read-only Git commands. It includes:

- staged changes;
- unstaged changes;
- deleted and renamed tracked files; and
- untracked, non-ignored files.

The Changes sidebar groups files by `staged`, `unstaged`, `mixed` (both staged and unstaged), or `untracked` status. Empty status groups are hidden, and folders remain collapsible within each group.

This is the view for edits that still need to be committed.

### 🔵 Unpushed commits

Unpushed commits compares `HEAD` with `@{upstream}`. It shows committed changes that exist on the local branch but have not reached its tracked remote branch. It does not include current uncommitted or untracked edits; switch to Git diff for those.

If the branch has no upstream, the viewer explains that `git push -u` is required before unpushed commits can be determined.

For bind-mounted repositories whose owner differs from the container user, Git's
workspace trust check is scoped to the canonical workspace for each read-only
command. The plugin does not change global Git configuration. If the mounted
workspace is read-only, the viewer still works as long as Herdr's plugin state
directory is writable.

The plugin does not modify the index, create commits, stage files, push changes, or run write-oriented Git commands.

## 🔒 What the plugin reads and ignores

The scanner is deliberately conservative:

- It never follows symbolic links while scanning the workspace.
- It honors supported ignore files, including `.gitignore` and `.ignore`.
- If `.bazelproject` or `.eclipse/.bazelproject` is found in the workspace or
  one of its ancestors up to the Git root, it limits the Files and Git views to
  the paths listed in its `directories:` section and honors `-` directory
  exclusions. Repositories without that file keep the normal full-workspace
  view.
- It excludes common repository metadata and generated directories such as `.git`, `.hg`, `.svn`, `node_modules`, `target`, `dist`, `.next`, and `.cache`.
- It limits individual text reads to 2 MiB and skips binary or invalid UTF-8 content from inline source browsing.
- It applies file-count, file-size, scan-byte, and Git-output limits to keep the viewer responsive.

The plugin stores only viewer mappings under `HERDR_PLUGIN_STATE_DIR`, which Herdr supplies for the installed plugin. Project files are not rewritten, and the plugin does not transmit workspace contents to a network service.

## 🏗️ Architecture

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

## 🛠️ Development

Run formatting, linting, tests, and a release build before publishing changes:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release --target x86_64-unknown-linux-gnu
cargo build --release --target aarch64-apple-darwin
```

The test suite covers working-tree Git changes, unpushed commits, workspace scanning, safe reads, viewer mappings, folder navigation/collapse, diff rendering, and terminal UI behavior.

GitHub Actions runs formatting, Clippy, tests, and macOS arm64/Linux x86_64 release builds for pushes to `main` and pull requests targeting `main`. Pushing a version tag publishes GitHub Release archives and SHA-256 checksums for both targets:

```sh
# Use a tag matching the version in Cargo.toml and herdr-plugin.toml.
git tag v0.1.3
git push origin v0.1.3
```

## 🩺 Troubleshooting

### The Git diff view is empty

Verify that the workspace is a Git repository with an initial commit (`HEAD`). Git diff includes staged, unstaged, and untracked changes, but ignored files are intentionally excluded.

### The Unpushed commits view is unavailable

Verify that the current branch tracks a remote branch. Run `git push -u <remote> <branch>` once to establish the upstream, then refresh the viewer.

### A file is not shown

Check whether it is ignored, a symbolic link, binary/invalid UTF-8, larger than the configured limits, or inside an excluded/generated directory. Large or binary files may still appear in the Changes sidebar as metadata-only diffs.

### The plugin does not appear in Herdr

Re-run `herdr plugin link "$PWD"`, confirm that the release build succeeds, and restart Herdr or the affected pane. Check that the installed Herdr version satisfies the minimum version in `herdr-plugin.toml`.

## 🔐 Privacy and security

Herdr Agent Diff is designed for local inspection. It does not embed credentials, send source files to an external service, or mutate the workspace. Git subprocesses are invoked with lock-free read behavior and fixed read-only arguments. The viewer displays workspace contents to whoever can access the local Herdr session, so use the same care as with any terminal showing source code.
