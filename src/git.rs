use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use crate::diff::{DiffLine, parse_unified_diff};
use crate::model::ChangeKind;

const MAX_GIT_OUTPUT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitChange {
    pub kind: ChangeKind,
    pub path: PathBuf,
    pub old_path: Option<PathBuf>,
    pub untracked: bool,
}

pub fn scan(root: &Path) -> Result<Vec<GitChange>, String> {
    let head = run_git(root, ["rev-parse", "--verify", "HEAD"])?;
    if !head.status.success() {
        return Err(
            if String::from_utf8_lossy(&head.stderr).contains("not a git repository") {
                "This workspace is not inside a Git repository.".into()
            } else {
                "This Git repository has no HEAD commit yet.".into()
            },
        );
    }

    let tracked = run_git(
        root,
        [
            "diff",
            "--name-status",
            "-z",
            "--find-renames",
            "--no-ext-diff",
            "--no-textconv",
            "HEAD",
            "--",
        ],
    )?;
    if !tracked.status.success() {
        return Err(command_error(&tracked, "Git status failed"));
    }
    ensure_output_limit(&tracked.stdout, "Git status")?;

    let untracked = run_git(
        root,
        ["ls-files", "--others", "--exclude-standard", "-z", "--"],
    )?;
    if !untracked.status.success() {
        return Err(command_error(&untracked, "Git untracked-file scan failed"));
    }
    ensure_output_limit(&untracked.stdout, "Git untracked-file scan")?;

    let mut changes = parse_name_status(&tracked.stdout)?;
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
            });
        }
    }
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(changes)
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
        let mut command = git_command(root);
        command.args([
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--find-renames",
            "--unified=3",
            "HEAD",
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

fn parse_name_status(bytes: &[u8]) -> Result<Vec<GitChange>, String> {
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
        });
    }
    Ok(changes)
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

    use super::{diff, scan};
    use crate::diff::DiffLineKind;
    use crate::model::ChangeKind;

    #[test]
    fn scans_tracked_and_untracked_changes_and_renders_both() {
        let directory = TempDir::new().expect("temp directory");
        git(directory.as_ref(), ["init", "-q"]);
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
        fs::write(directory.path().join("tracked.txt"), "before\nafter\n").expect("modify tracked");
        fs::write(directory.path().join("untracked.txt"), "new\n").expect("write untracked");

        let changes = scan(directory.path()).expect("scan git changes");
        assert_eq!(changes.len(), 2);
        let tracked = changes
            .iter()
            .find(|change| change.path == std::path::Path::new("tracked.txt"))
            .expect("tracked change");
        assert_eq!(tracked.kind, ChangeKind::Modified);
        assert!(!tracked.untracked);
        let tracked_lines = diff(directory.path(), tracked).expect("tracked diff");
        assert!(
            tracked_lines
                .iter()
                .any(|line| line.kind == DiffLineKind::Addition)
        );

        let untracked = changes
            .iter()
            .find(|change| change.path == std::path::Path::new("untracked.txt"))
            .expect("untracked change");
        assert!(untracked.untracked);
        let untracked_lines = diff(directory.path(), untracked).expect("untracked diff");
        assert!(
            untracked_lines
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
