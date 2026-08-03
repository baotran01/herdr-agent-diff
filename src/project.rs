use std::fs;
use std::path::{Component, Path, PathBuf};

const PROJECT_VIEW_PATHS: &[&[&str]] = &[&[".eclipse", ".bazelproject"], &[".bazelproject"]];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectScope {
    base: PathBuf,
    view_path: PathBuf,
    included: Vec<PathBuf>,
    excluded: Vec<PathBuf>,
}

impl ProjectScope {
    pub(crate) fn discover(root: &Path) -> Option<Self> {
        let mut directory = root.canonicalize().ok()?;
        loop {
            for path_parts in PROJECT_VIEW_PATHS {
                let path = path_parts
                    .iter()
                    .fold(directory.clone(), |path, part| path.join(part));
                if !path.is_file() {
                    continue;
                }
                let contents = fs::read_to_string(&path).ok()?;
                let mut scope = Self::parse(&contents)?;
                scope.base = directory;
                scope.view_path = path;
                return Some(scope);
            }

            if directory.join(".git").exists() {
                return None;
            }
            let parent = directory.parent()?.to_path_buf();
            if parent == directory {
                return None;
            }
            directory = parent;
        }
    }

    pub(crate) fn allows_entry(&self, path: &Path, is_dir: bool) -> bool {
        self.allows_absolute(path, is_dir)
    }

    pub(crate) fn contains_path(&self, root: &Path, path: &Path) -> bool {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        self.allows_absolute(&absolute, false)
    }

    pub(crate) fn view_path(&self) -> &Path {
        &self.view_path
    }

    fn parse(contents: &str) -> Option<Self> {
        let mut in_directories = false;
        let mut included = Vec::new();
        let mut excluded = Vec::new();

        for raw_line in contents.lines() {
            let line = raw_line.split('#').next()?.trim();
            if line.is_empty() {
                continue;
            }
            let indented = raw_line.chars().next().is_some_and(char::is_whitespace);
            if !indented {
                in_directories = line == "directories:";
                continue;
            }
            if !in_directories {
                continue;
            }

            let (excluded_entry, raw_path) = match line.strip_prefix('-') {
                Some(path) => (true, path.trim()),
                None => (false, line),
            };
            let path = normalize_workspace_path(raw_path)?;
            let destination = if excluded_entry {
                &mut excluded
            } else {
                &mut included
            };
            if !destination.contains(&path) {
                destination.push(path);
            }
        }

        if included.is_empty() {
            return None;
        }
        Some(Self {
            base: PathBuf::new(),
            view_path: PathBuf::new(),
            included,
            excluded,
        })
    }

    fn allows_absolute(&self, path: &Path, is_dir: bool) -> bool {
        if let Ok(relative) = path.strip_prefix(&self.base) {
            return self.allows_relative(relative, is_dir);
        }
        // When the pane starts above the project-view directory, keep the
        // ancestor path open so the walker can reach the scoped project.
        self.base.starts_with(path)
    }

    fn allows_relative(&self, path: &Path, is_dir: bool) -> bool {
        if path.as_os_str().is_empty() {
            return true;
        }
        if self
            .excluded
            .iter()
            .any(|excluded| path_is_equal_or_below(path, excluded))
        {
            return false;
        }
        if is_dir {
            self.included.iter().any(|included| {
                path_is_root(included)
                    || path_is_equal_or_below(path, included)
                    || pattern_is_below_path(included, path)
            })
        } else {
            self.contains_relative_path(path)
        }
    }

    fn contains_relative_path(&self, path: &Path) -> bool {
        self.included
            .iter()
            .any(|included| path_is_root(included) || path_is_equal_or_below(path, included))
            && !self
                .excluded
                .iter()
                .any(|excluded| path_is_equal_or_below(path, excluded))
    }
}

fn normalize_workspace_path(raw_path: &str) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in Path::new(raw_path).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(component) => normalized.push(component),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(normalized)
}

fn path_is_root(path: &Path) -> bool {
    path.as_os_str().is_empty()
}

fn path_is_equal_or_below(path: &Path, base: &Path) -> bool {
    let path_parts = path_components(path);
    let base_parts = path_components(base);
    path_parts.len() >= base_parts.len()
        && base_parts
            .iter()
            .zip(path_parts)
            .all(|(base, path)| component_matches(base, path))
}

fn path_components(path: &Path) -> Vec<&std::ffi::OsStr> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(component) => Some(component),
            _ => None,
        })
        .collect()
}

fn component_matches(pattern: &std::ffi::OsStr, component: &std::ffi::OsStr) -> bool {
    pattern == std::ffi::OsStr::new("*") || pattern == component
}

fn pattern_is_below_path(pattern: &Path, path: &Path) -> bool {
    let pattern_components = path_components(pattern);
    let path_components = path_components(path);
    pattern_components.len() >= path_components.len()
        && path_components
            .iter()
            .zip(pattern_components)
            .all(|(path, pattern)| component_matches(pattern, path))
}

#[cfg(test)]
mod tests {
    use super::{ProjectScope, normalize_workspace_path};

    #[test]
    fn parses_directories_and_exclusions() {
        let scope = ProjectScope::parse(
            "directories:\n  java/foo\n  java/bar # source\n  -java/foo/generated\ntargets:\n  //...\n",
        )
        .expect("scope");

        assert!(scope.contains_relative_path(std::path::Path::new("java/foo/Main.java")));
        assert!(scope.contains_relative_path(std::path::Path::new("java/bar/Main.java")));
        assert!(!scope.contains_relative_path(std::path::Path::new("java/foo/generated/x.java")));
        assert!(!scope.contains_relative_path(std::path::Path::new("other/Main.java")));
    }

    #[test]
    fn matches_bazelproject_wildcard_exclusions() {
        let scope = ProjectScope::parse("directories:\n  .\n  -examples/*\n").expect("scope");

        assert!(scope.contains_relative_path(std::path::Path::new("src/Main.kt")));
        assert!(!scope.contains_relative_path(std::path::Path::new("examples/README.md")));
        assert!(!scope.contains_relative_path(std::path::Path::new("examples/android/BUILD")));
        assert!(scope.allows_relative(std::path::Path::new("examples"), true));
        assert!(!scope.allows_relative(std::path::Path::new("examples/android"), true));
    }

    #[test]
    fn rejects_paths_that_escape_the_workspace() {
        assert!(normalize_workspace_path("../outside").is_none());
        assert!(normalize_workspace_path("/absolute").is_none());
        assert_eq!(
            normalize_workspace_path("./java/foo").expect("normalized"),
            std::path::Path::new("java/foo")
        );
    }
}
