# Herdr Agent Diff

Herdr Agent Diff is a macOS arm64 plugin for [Herdr](https://github.com/baotran/herdr) that makes an agent's filesystem changes easy to inspect. When Herdr detects an agent pane, the plugin captures a best-effort read-only baseline. When opened, it compares the current workspace with that baseline and presents the result in a terminal UI.

The viewer has two tabs:

- **Changes** shows added, modified, deleted, and exact-content renamed files. Selecting a file opens a readable code diff.
- **Files** shows the workspace tree with line numbers and syntax highlighting for source files.

The viewer can open beside the agent pane or in a separate Herdr tab. It does not open automatically when an agent starts.

## Features

- Agent-session diffs based on the filesystem state captured when the agent is detected.
- Git diffs against `HEAD`, including tracked, untracked, deleted, and renamed files where Git can identify them.
- A grouped, collapsible folder tree shared by the Changes and Files tabs.
- Right-aligned per-file `+` and `-` counts in the Changes tab.
- Unified code diffs with old/new line-number gutters, addition/deletion highlighting, and collapsed unchanged regions.
- Syntax-highlighted file browsing with language detection from file extensions.
- Text search, keyboard navigation, mouse support, and scrollbars for both the sidebar and diff pane.
- Explicit mode hint in the footer: `g: Git diff / Agent`.
- Read-only operation on the project workspace.

## Requirements

- macOS on Apple silicon (`aarch64-apple-darwin`).
- Rust and Cargo.
- Herdr 0.7.5 or newer.
- Git for Git diff mode.

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

The plugin manifest, [`herdr-plugin.toml`](herdr-plugin.toml), registers the following Herdr events and actions:

| Name | Purpose |
| --- | --- |
| `pane.agent_detected` | Capture the agent-session baseline. |
| `pane.closed` | Remove state for a closed pane. |
| `pane.exited` | Remove state for an exited pane. |
| `herdr-agent-diff.open` | Open the viewer in a split beside the agent pane. |
| `herdr-agent-diff.open-tab` | Open the viewer in a separate Herdr tab. |

Example Herdr key bindings:

```toml
[[keys.command]]
key = "prefix+d"
type = "plugin_action"
command = "herdr-agent-diff.open"
description = "agent filesystem changes"

[[keys.command]]
key = "prefix+shift+d"
type = "plugin_action"
command = "herdr-agent-diff.open-tab"
description = "agent filesystem changes in tab"
```

After linking, restart an already-running agent pane if it was not detected with the new plugin installed. New agent panes will be handled automatically.

## Using the viewer

### Changes tab

The sidebar is organized by folders. Click a folder row, or place the cursor on it and press Enter, to expand or collapse that folder. The text column remains aligned when folders are collapsed.

Select a file to inspect its diff. The sidebar displays file status and right-aligned additions/removals. Status colors are informational and follow the active Herdr theme.

The footer calls out the most important mode switch:

```text
g: Git diff / Agent
```

Press `g` at any time to switch between the agent-session comparison and the Git comparison.

### Files tab

The Files tab uses the same folder grouping, indentation, collapse behavior, selection gutter, and navigation styling as the Changes tab. Select a file to browse its current contents with line numbers and syntax highlighting.

### Keyboard controls

| Key | Action |
| --- | --- |
| `1` | Select the Changes tab. |
| `2` | Select the Files tab. |
| `g` | Toggle Git diff and Agent diff modes. |
| `Tab` | Move focus between the sidebar and diff/content pane. |
| Arrow keys, `h`/`j`/`k`/`l` | Navigate the focused area. |
| `Enter` | Open a selected file or toggle a selected folder. |
| `/` | Filter the sidebar. |
| `r` | Refresh the current comparison or file view. |
| `⌘C` | Copy the selected text from the read-only Files pane. Mouse selections are also copied on release. |
| `?` | Show the help overlay. |
| `q` or `Esc` | Close the viewer or dismiss an overlay. |

Mouse clicks select tabs, folders, and files. Drag the sidebar or diff scrollbar to move quickly through long content. Scrolling over a pane keeps the pointer's pane active.

## Diff semantics

### Agent diff

Agent diff mode compares the current workspace with the baseline captured when Herdr reports `pane.agent_detected`. It is intended to answer: “What changed during this agent session?”

The baseline is stored per Herdr pane in the plugin state directory. It is refreshed when a new agent-detection event is received and removed when the pane closes or exits.

### Git diff

Git diff mode compares the workspace with `HEAD` using read-only Git commands. It is intended to answer: “What differs from the current commit?”

Git mode includes tracked modifications, deleted files, untracked files, and Git-reported renames when available. It requires a repository with a valid `HEAD`; a repository without an initial commit cannot provide a `HEAD` comparison. The plugin does not modify the index, create commits, stage files, or run write-oriented Git commands.

## What the plugin reads and ignores

The scanner is deliberately conservative:

- It never follows symbolic links while scanning the workspace.
- It honors supported ignore files, including `.gitignore` and `.ignore`.
- It excludes common repository metadata and generated directories such as `.git`, `.hg`, `.svn`, `node_modules`, `target`, `dist`, `.next`, and `.cache`.
- It limits individual text snapshots to 2 MiB and skips binary or invalid UTF-8 content from inline diff rendering.
- It caps the stored snapshot data for a pane at 256 MiB.
- It applies file-count, file-size, scan-byte, and Git-output limits to keep the viewer responsive.

The plugin stores snapshots and viewer mappings under `HERDR_PLUGIN_STATE_DIR`, which Herdr supplies for the installed plugin. Project files are not rewritten, and the plugin does not transmit workspace contents to a network service.

## Architecture

The implementation is split into small Rust modules:

| Module | Responsibility |
| --- | --- |
| `src/main.rs` | Herdr event/action entry points, state lookup, and viewer startup. |
| `src/model.rs` | Manifest, file records, change kinds, and shared limits. |
| `src/snapshot.rs` | Safe workspace scanning, baseline capture, and change classification. |
| `src/diff.rs` | Text diff generation, hunk parsing, and rendered diff rows. |
| `src/git.rs` | Read-only Git status, file listing, and `HEAD` comparisons. |
| `src/app.rs` | Terminal UI, tabs, navigation, filtering, scrolling, input, and rendering. |
| `src/theme.rs` | Herdr-aware theme colors and syntax-highlighting styles. |
| `src/manifest.rs` | Plugin manifest and state serialization helpers. |

The high-level lifecycle is:

```text
agent_detected
      │
      ▼
scan workspace ──► save per-pane baseline
      │
      ▼
open / open-tab ──► load baseline ──► classify changes ──► render viewer
      │
      ├── g ──► run read-only Git comparison
      └── close/exit ──► remove per-pane state
```

## Project layout

```text
.
├── herdr-plugin.toml       # Herdr plugin metadata, events, and actions
├── Cargo.toml               # Rust package and dependency configuration
├── src/
│   ├── app.rs               # Terminal UI
│   ├── diff.rs              # Agent diff rendering
│   ├── git.rs               # Git diff mode
│   ├── main.rs              # Plugin entry point
│   ├── model.rs             # Shared data model
│   ├── snapshot.rs          # Workspace snapshots
│   └── theme.rs             # Theme integration
└── tests/
    └── snapshots/           # UI snapshot fixtures
```

## Development

Run formatting, linting, tests, and a release build before publishing changes:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release --target aarch64-apple-darwin
```

The test suite covers snapshot classification, Git behavior, manifest handling, folder navigation/collapse, diff rendering, and terminal UI snapshots. To inspect the plugin state for a specific Herdr pane, use the binary's status command:

```sh
target/aarch64-apple-darwin/release/herdr-agent-diff status --pane <pane-id>
```

The event and viewer commands are normally invoked by Herdr through the plugin manifest. They can be run directly only when the corresponding Herdr environment variables are present.

## Troubleshooting

### The viewer is empty

Confirm that the agent pane was detected after the plugin was linked. Restart the agent pane if necessary, then open the viewer again. Agent mode only shows changes made after the baseline event.

### Git mode has no results

Verify that the workspace is a Git repository with an initial commit (`HEAD`). Remember that Git mode and Agent mode have different comparison points.

### A file is not shown

Check whether it is ignored, a symbolic link, binary/invalid UTF-8, larger than the configured snapshot limits, or inside an excluded/generated directory. Large or binary files may still appear as metadata-only changes rather than inline code diffs.

### The plugin does not appear in Herdr

Re-run `herdr plugin link "$PWD"`, confirm that the release build succeeds, and restart Herdr or the affected agent pane. Check that the installed Herdr version satisfies the minimum version in `herdr-plugin.toml`.

## Privacy and security

Herdr Agent Diff is designed for local inspection. It does not embed credentials, send source files to an external service, or mutate the workspace. Git subprocesses are invoked with lock-free read behavior and fixed read-only arguments. Even so, the viewer displays workspace contents to whoever can access the local Herdr session, so use the same care you would use with any terminal showing source code.
