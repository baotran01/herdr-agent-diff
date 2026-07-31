use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::diff::{DiffLine, parse_unified_diff};
use crate::model::ChangeKind;

const MAX_GIT_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitComparison {
    WorkingTree,
    Unpushed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitFileState {
    Staged,
    Unstaged,
    StagedAndUnstaged,
    Untracked,
    Committed,
}

impl GitFileState {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Staged => "staged",
            Self::Unstaged => "unstaged",
            Self::StagedAndUnstaged => "mixed",
            Self::Untracked => "untracked",
            Self::Committed => "committed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitChange {
    pub kind: ChangeKind,
    pub path: PathBuf,
    pub old_path: Option<PathBuf>,
    pub untracked: bool,
    pub comparison: GitComparison,
    pub state: GitFileState,
}

pub fn scan(root: &Path, comparison: GitComparison) -> Result<Vec<GitChange>, String> {
    ensure_head(root)?;
    let reference = match comparison {
        GitComparison::WorkingTree => "HEAD",
        GitComparison::Unpushed => {
            let upstream = run_git(
                root,
                ["rev-parse", "--verify", "--abbrev-ref", "@{upstream}"],
            )?;
            if !upstream.status.success() {
                return Err(
                    "This branch has no tracked remote branch. Push it with `git push -u` to view unpushed commits."
                        .into(),
                );
            }
            "@{upstream}..HEAD"
        }
    };

    let tracked = run_git(
        root,
        [
            "diff",
            "--name-status",
            "-z",
            "--find-renames",
            "--no-ext-diff",
            "--no-textconv",
            reference,
            "--",
        ],
    )?;
    if !tracked.status.success() {
        return Err(command_error(&tracked, "Git status failed"));
    }
    ensure_output_limit(&tracked.stdout, "Git status")?;

    let mut changes = parse_name_status(&tracked.stdout, comparison)?;
    if comparison == GitComparison::WorkingTree {
        let statuses = scan_worktree_states(root)?;
        let untracked = run_git(
            root,
            ["ls-files", "--others", "--exclude-standard", "-z", "--"],
        )?;
        if !untracked.status.success() {
            return Err(command_error(&untracked, "Git untracked-file scan failed"));
        }
        ensure_output_limit(&untracked.stdout, "Git untracked-file scan")?;

        for change in &mut changes {
            change.state = change_state(&statuses, change);
        }

        let tracked_paths: BTreeSet<PathBuf> = changes
            .iter()
            .flat_map(|change| {
                change
                    .old_path
                    .iter()
                    .chain(std::iter::once(&change.path))
                    .cloned()
            })
            .collect();
        for path in untracked
            .stdout
            .split(|byte| *byte == 0)
            .filter(|path| !path.is_empty())
            .map(path_from_bytes)
        {
            let path = path?;
            if !tracked_paths.contains(&path) {
                changes.push(GitChange {
                    kind: ChangeKind::Added,
                    path,
                    old_path: None,
                    untracked: true,
                    comparison,
                    state: GitFileState::Untracked,
                });
            }
        }
    }
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(changes)
}

#[must_use]
pub fn unpushed_commit_count(root: &Path) -> Option<usize> {
    let upstream = run_git(
        root,
        ["rev-parse", "--verify", "--abbrev-ref", "@{upstream}"],
    )
    .ok()?;
    if !upstream.status.success() {
        return None;
    }
    let output = run_git(root, ["rev-list", "--count", "@{upstream}..HEAD"]).ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

pub fn diff(root: &Path, change: &GitChange) -> Result<Vec<DiffLine>, String> {
    let output = if change.untracked {
        let mut command = git_command(root);
        command.args([
            "diff",
            "--no-index",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--unified=3",
            "--",
        ]);
        command.arg("/dev/null").arg(root.join(&change.path));
        command
            .output()
            .map_err(|error| format!("Unable to run Git diff: {error}"))?
    } else {
        let reference = match change.comparison {
            GitComparison::WorkingTree => "HEAD",
            GitComparison::Unpushed => "@{upstream}..HEAD",
        };
        let mut command = git_command(root);
        command.args([
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--find-renames",
            "--unified=3",
            reference,
            "--",
        ]);
        command
            .arg(&change.path)
            .output()
            .map_err(|error| format!("Unable to run Git diff: {error}"))?
    };
    ensure_output_limit(&output.stdout, "Git diff")?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(command_error(&output, "Git diff failed"));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| "Git diff contains non-UTF-8 output.".to_owned())?;
    Ok(parse_unified_diff(&text))
}

fn ensure_head(root: &Path) -> Result<(), String> {
    let head = run_git(root, ["rev-parse", "--verify", "HEAD"])?;
    if head.status.success() {
        Ok(())
    } else if String::from_utf8_lossy(&head.stderr).contains("not a git repository") {
        Err("This workspace is not inside a Git repository.".into())
    } else {
        Err("This Git repository has no HEAD commit yet.".into())
    }
}

fn parse_name_status(bytes: &[u8], comparison: GitComparison) -> Result<Vec<GitChange>, String> {
    let mut tokens = bytes.split(|byte| *byte == 0);
    let mut changes = Vec::new();
    while let Some(status) = tokens.next().filter(|status| !status.is_empty()) {
        let status = String::from_utf8_lossy(status);
        let code = status.as_bytes().first().copied().unwrap_or(b'M') as char;
        let first = tokens
            .next()
            .ok_or_else(|| "Git returned an incomplete name-status record.".to_owned())
            .and_then(path_from_bytes)?;
        let (kind, path, old_path) = match code {
            'R' | 'C' => {
                let second = tokens
                    .next()
                    .ok_or_else(|| "Git returned an incomplete rename record.".to_owned())
                    .and_then(path_from_bytes)?;
                (
                    if code == 'R' {
                        ChangeKind::Renamed
                    } else {
                        ChangeKind::Added
                    },
                    second,
                    Some(first),
                )
            }
            'A' => (ChangeKind::Added, first, None),
            'D' => (ChangeKind::Deleted, first, None),
            _ => (ChangeKind::Modified, first, None),
        };
        changes.push(GitChange {
            kind,
            path,
            old_path,
            untracked: false,
            comparison,
            state: match comparison {
                GitComparison::WorkingTree => GitFileState::Unstaged,
                GitComparison::Unpushed => GitFileState::Committed,
            },
        });
    }
    Ok(changes)
}

fn scan_worktree_states(root: &Path) -> Result<BTreeMap<PathBuf, GitFileState>, String> {
    let output = run_git(
        root,
        [
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--",
        ],
    )?;
    if !output.status.success() {
        return Err(command_error(&output, "Git status failed"));
    }
    ensure_output_limit(&output.stdout, "Git status")?;
    parse_porcelain_status(&output.stdout)
}

fn parse_porcelain_status(bytes: &[u8]) -> Result<BTreeMap<PathBuf, GitFileState>, String> {
    let mut tokens = bytes.split(|byte| *byte == 0);
    let mut statuses = BTreeMap::new();
    while let Some(record) = tokens.next().filter(|record| !record.is_empty()) {
        if record.len() < 3 {
            return Err("Git returned an incomplete status record.".to_owned());
        }
        let index = record[0] as char;
        let worktree = record[1] as char;
        let state = porcelain_state(index, worktree);
        let path = path_from_bytes(&record[3..])?;
        statuses.insert(path, state);

        if matches!(index, 'R' | 'C') || matches!(worktree, 'R' | 'C') {
            let old_path = tokens
                .next()
                .filter(|record| !record.is_empty())
                .ok_or_else(|| "Git returned an incomplete rename status record.".to_owned())
                .and_then(path_from_bytes)?;
            statuses.insert(old_path, state);
        }
    }
    Ok(statuses)
}

fn porcelain_state(index: char, worktree: char) -> GitFileState {
    if index == '?' && worktree == '?' {
        GitFileState::Untracked
    } else if index != ' ' && worktree != ' ' {
        GitFileState::StagedAndUnstaged
    } else if index != ' ' {
        GitFileState::Staged
    } else {
        GitFileState::Unstaged
    }
}

fn change_state(statuses: &BTreeMap<PathBuf, GitFileState>, change: &GitChange) -> GitFileState {
    statuses
        .get(&change.path)
        .or_else(|| change.old_path.as_ref().and_then(|path| statuses.get(path)))
        .copied()
        .unwrap_or(GitFileState::Unstaged)
}

fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, String> {
    String::from_utf8(bytes.to_vec())
        .map(PathBuf::from)
        .map_err(|_| "Git returned a non-UTF-8 path.".to_owned())
}

fn git_command(root: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(root).env("GIT_OPTIONAL_LOCKS", "0");
    command
}

fn run_git<const N: usize>(root: &Path, arguments: [&str; N]) -> Result<Output, String> {
    git_command(root)
        .args(arguments)
        .output()
        .map_err(|error| format!("Unable to run Git: {error}"))
}

fn ensure_output_limit(output: &[u8], operation: &str) -> Result<(), String> {
    if output.len() > MAX_GIT_OUTPUT_BYTES {
        Err(format!(
            "{operation} output exceeds the 32 MiB viewer limit."
        ))
    } else {
        Ok(())
    }
}

fn command_error(output: &Output, fallback: &str) -> String {
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if detail.is_empty() {
        fallback.to_owned()
    } else {
        format!("{fallback}: {detail}")
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use tempfile::TempDir;

    use super::{GitComparison, GitFileState, diff, scan};
    use crate::diff::DiffLineKind;
    use crate::model::ChangeKind;

    #[test]
    fn scans_working_tree_and_renders_tracked_and_untracked_changes() {
        let directory = TempDir::new().expect("temp directory");
        git(directory.as_ref(), ["init", "-q"]);
        fs::write(directory.path().join("tracked.txt"), "before\n").expect("write tracked");
        fs::write(directory.path().join("staged.txt"), "before\n").expect("write staged");
        fs::write(directory.path().join("mixed.txt"), "before\n").expect("write mixed");
        git(
            directory.as_ref(),
            ["add", "tracked.txt", "staged.txt", "mixed.txt"],
        );
        git(
            directory.as_ref(),
            [
                "-c",
                "user.name=Test User",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "initial",
            ],
        );
        fs::write(directory.path().join("tracked.txt"), "before\nafter\n").expect("modify tracked");
        fs::write(directory.path().join("staged.txt"), "before\nstaged\n").expect("modify staged");
        fs::write(directory.path().join("mixed.txt"), "before\nstaged\n").expect("modify mixed");
        git(directory.as_ref(), ["add", "mixed.txt"]);
        fs::write(
            directory.path().join("mixed.txt"),
            "before\nstaged\nunstaged\n",
        )
        .expect("modify mixed again");
        git(directory.as_ref(), ["add", "staged.txt"]);
        fs::write(directory.path().join("untracked.txt"), "new\n").expect("write untracked");

        let changes = scan(directory.path(), GitComparison::WorkingTree).expect("scan git changes");
        assert_eq!(changes.len(), 4);
        let tracked = changes
            .iter()
            .find(|change| change.path == std::path::Path::new("tracked.txt"))
            .expect("tracked change");
        assert_eq!(tracked.kind, ChangeKind::Modified);
        assert!(!tracked.untracked);
        assert_eq!(tracked.state, GitFileState::Unstaged);
        let tracked_lines = diff(directory.path(), tracked).expect("tracked diff");
        assert!(
            tracked_lines
                .iter()
                .any(|line| line.kind == DiffLineKind::Addition)
        );

        let staged = changes
            .iter()
            .find(|change| change.path == std::path::Path::new("staged.txt"))
            .expect("staged change");
        assert!(!staged.untracked);
        assert_eq!(staged.state, GitFileState::Staged);
        assert!(
            diff(directory.path(), staged)
                .expect("staged diff")
                .iter()
                .any(|line| line.kind == DiffLineKind::Addition)
        );

        let mixed = changes
            .iter()
            .find(|change| change.path == std::path::Path::new("mixed.txt"))
            .expect("mixed change");
        assert_eq!(mixed.state, GitFileState::StagedAndUnstaged);

        let untracked = changes
            .iter()
            .find(|change| change.path == std::path::Path::new("untracked.txt"))
            .expect("untracked change");
        assert!(untracked.untracked);
        assert_eq!(untracked.state, GitFileState::Untracked);
        let untracked_lines = diff(directory.path(), untracked).expect("untracked diff");
        assert!(
            untracked_lines
                .iter()
                .any(|line| line.kind == DiffLineKind::Addition)
        );
    }

    #[test]
    fn scans_committed_changes_that_are_not_pushed() {
        let directory = TempDir::new().expect("repository");
        let remote = TempDir::new().expect("remote");
        git(directory.as_ref(), ["init", "-q"]);
        git(directory.as_ref(), ["branch", "-M", "main"]);
        fs::write(directory.path().join("tracked.txt"), "before\n").expect("write tracked");
        git(directory.as_ref(), ["add", "tracked.txt"]);
        git(
            directory.as_ref(),
            [
                "-c",
                "user.name=Test User",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "initial",
            ],
        );
        git(remote.as_ref(), ["init", "--bare", "-q"]);
        git(
            directory.as_ref(),
            [
                "remote",
                "add",
                "origin",
                remote.path().to_str().expect("remote path"),
            ],
        );
        git(directory.as_ref(), ["push", "-q", "-u", "origin", "main"]);
        fs::write(directory.path().join("tracked.txt"), "before\nafter\n").expect("modify tracked");
        git(directory.as_ref(), ["add", "tracked.txt"]);
        git(
            directory.as_ref(),
            [
                "-c",
                "user.name=Test User",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "local change",
            ],
        );

        let changes = scan(directory.path(), GitComparison::Unpushed).expect("scan unpushed");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, std::path::Path::new("tracked.txt"));
        assert_eq!(changes[0].comparison, GitComparison::Unpushed);
        assert!(!changes[0].untracked);
        assert_eq!(changes[0].state, GitFileState::Committed);
        let lines = diff(directory.path(), &changes[0]).expect("unpushed diff");
        assert!(lines.iter().any(|line| line.kind == DiffLineKind::Addition));
    }

    fn git<const N: usize>(directory: &std::path::Path, arguments: [&str; N]) {
        let status = Command::new("git")
            .current_dir(directory)
            .args(arguments)
            .status()
            .expect("run git");
        assert!(status.success(), "git command failed: {status}");
    }
}
