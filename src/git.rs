use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::diff::{DiffLine, parse_unified_diff};
use crate::model::ChangeKind;
use crate::project::ProjectScope;

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
    let scope = ProjectScope::discover(root);
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
            "--relative",
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
    if let Some(scope) = scope {
        changes.retain(|change| {
            scope.contains_path(root, &change.path)
                || change
                    .old_path
                    .as_deref()
                    .is_some_and(|path| scope.contains_path(root, path))
        });
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

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;

    Ok(std::ffi::OsString::from_vec(bytes.to_vec()).into())
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, String> {
    String::from_utf8(bytes.to_vec())
        .map(PathBuf::from)
        .map_err(|_| "Git returned a non-UTF-8 path.".to_owned())
}

fn git_command(root: &Path) -> Command {
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut safe_directory = OsString::from("safe.directory=");
    safe_directory.push(canonical_root.as_os_str());
    let mut command = Command::new("git");
    command
        .args(["-c"])
        .arg(safe_directory)
        .arg("-C")
        .arg(canonical_root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0");
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
    fn bazelproject_limits_working_tree_changes_to_selected_directories() {
        let directory = TempDir::new().expect("repository");
        git(directory.as_ref(), ["init", "-q"]);
        fs::create_dir_all(directory.path().join(".eclipse")).expect("eclipse directory");
        fs::create_dir_all(directory.path().join("java/selected")).expect("selected directory");
        fs::create_dir_all(directory.path().join("java/other")).expect("other directory");
        fs::write(
            directory.path().join(".eclipse/.bazelproject"),
            "directories:\n  java/selected\n",
        )
        .expect("project view");
        fs::write(directory.path().join("java/selected/Main.java"), "before\n")
            .expect("selected tracked");
        fs::write(directory.path().join("java/other/Other.java"), "before\n")
            .expect("other tracked");
        git(directory.as_ref(), ["add", "."]);
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
        fs::write(
            directory.path().join("java/selected/Main.java"),
            "before\nafter\n",
        )
        .expect("selected modified");
        fs::write(
            directory.path().join("java/other/Other.java"),
            "before\nafter\n",
        )
        .expect("other modified");
        fs::write(
            directory.path().join("java/selected/Added.java"),
            "selected\n",
        )
        .expect("selected untracked");
        fs::write(directory.path().join("java/other/Added.java"), "other\n")
            .expect("other untracked");

        let changes = scan(directory.path(), GitComparison::WorkingTree).expect("scan changes");
        let paths: Vec<_> = changes.iter().map(|change| &change.path).collect();
        assert_eq!(
            paths,
            [
                std::path::Path::new("java/selected/Added.java"),
                std::path::Path::new("java/selected/Main.java")
            ]
        );
    }

    #[test]
    fn ancestor_bazelproject_limits_changes_from_a_nested_git_workspace() {
        let repository = TempDir::new().expect("repository");
        git(repository.as_ref(), ["init", "-q"]);
        fs::write(
            repository.path().join(".bazelproject"),
            "directories:\n  project/java/selected\n",
        )
        .expect("project view");
        let project = repository.path().join("project");
        fs::create_dir_all(project.join("java/selected")).expect("selected directory");
        fs::create_dir_all(project.join("java/other")).expect("other directory");
        fs::write(project.join("java/selected/Main.java"), "before\n").expect("selected tracked");
        fs::write(project.join("java/other/Other.java"), "before\n").expect("other tracked");
        git(repository.as_ref(), ["add", "."]);
        git(
            repository.as_ref(),
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
        fs::write(project.join("java/selected/Main.java"), "before\nafter\n")
            .expect("selected modified");
        fs::write(project.join("java/other/Other.java"), "before\nafter\n")
            .expect("other modified");
        fs::write(project.join("java/selected/Added.java"), "selected\n")
            .expect("selected untracked");
        fs::write(project.join("java/other/Added.java"), "other\n").expect("other untracked");

        let changes = scan(&project, GitComparison::WorkingTree).expect("scan changes");
        let paths: Vec<_> = changes.iter().map(|change| &change.path).collect();
        assert_eq!(
            paths,
            [
                std::path::Path::new("java/selected/Added.java"),
                std::path::Path::new("java/selected/Main.java")
            ]
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

    #[cfg(target_os = "linux")]
    #[test]
    fn scans_and_diffs_non_utf8_linux_paths() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let directory = TempDir::new().expect("repository");
        git(directory.as_ref(), ["init", "-q"]);
        fs::write(directory.path().join("tracked.txt"), "tracked\n").expect("write tracked");
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
        let invalid_name = OsString::from_vec(b"linux-\xff.txt".to_vec());
        fs::write(directory.path().join(&invalid_name), "new\n").expect("write non-UTF-8 path");

        let changes = scan(directory.path(), GitComparison::WorkingTree).expect("scan git changes");
        let change = changes
            .iter()
            .find(|change| change.path.as_os_str() == invalid_name)
            .expect("non-UTF-8 change");
        assert!(change.untracked);
        assert!(
            diff(directory.path(), change)
                .expect("render non-UTF-8 diff")
                .iter()
                .any(|line| line.kind == DiffLineKind::Addition)
        );
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
