use crate::git;
use crate::model::RepositorySeed;
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Stop at checkouts just as the original script did. Registered worktrees are
/// enumerated from Git, so they need not be under any of the search roots.
pub fn discover(roots: &[PathBuf]) -> (Vec<RepositorySeed>, Vec<String>) {
    discover_with_progress(roots, |_, _, _| {})
}

pub fn discover_with_progress(
    roots: &[PathBuf],
    progress: impl Fn(usize, usize, &Path) + Sync,
) -> (Vec<RepositorySeed>, Vec<String>) {
    let mut candidates = Vec::new();
    let mut errors = Vec::new();
    let mut directories = 0;
    for root in roots {
        progress(directories, candidates.len(), root);
        if !root.is_dir() {
            errors.push(format!("{}: not an accessible directory", root.display()));
            continue;
        }
        let mut entries = WalkDir::new(root).follow_links(false).into_iter();
        while let Some(entry) = entries.next() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    errors.push(error.to_string());
                    continue;
                }
            };
            if !entry.file_type().is_dir() {
                continue;
            }
            let path = entry.path();
            directories += 1;
            if is_candidate(path) {
                candidates.push(path.to_path_buf());
                progress(directories, candidates.len(), path);
                entries.skip_current_dir();
            } else if entry.file_name() == ".git" {
                entries.skip_current_dir();
            } else if directories % 128 == 0 {
                progress(directories, candidates.len(), path);
            }
        }
    }
    candidates.sort();
    candidates.dedup();
    let resolved: Vec<_> = candidates
        .par_iter()
        .map(|path| (path, git::resolve_seed(path)))
        .collect();
    let mut repositories = BTreeMap::new();
    for (path, result) in resolved {
        match result {
            Ok(seed) => {
                repositories.entry(seed.common_dir.clone()).or_insert(seed);
            }
            Err(error) => errors.push(format!("{}: {error:#}", path.display())),
        }
    }
    if let Some(root) = roots.last() {
        progress(directories, repositories.len(), root);
    }
    (repositories.into_values().collect(), errors)
}

fn is_candidate(path: &Path) -> bool {
    path.join(".git").is_file()
        || path.join(".git").is_dir()
        || (path.join("HEAD").is_file() && path.join("objects").is_dir())
}
