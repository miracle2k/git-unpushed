//! Removal traversal adapted from gitoxide's gix-worktree 0.59.0 (MIT).
//! See LICENSES/gitoxide-MIT.txt. Validation and statistics are local additions.
use crate::model::{DeletionResult, DeletionStats, DeletionStatus};
use crate::{git, inspect};
use anyhow::{Context, Result, bail};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// Remove a linked checkout and its registration after validating their identity
/// and checking native Git status. Never remove branches or shared objects.
pub fn remove_worktree(common_dir: &Path, worktree: &Path, collect_size: bool) -> DeletionResult {
    let git_dir = match validate(common_dir, worktree) {
        Ok(path) => path,
        Err(error) => {
            return DeletionResult {
                status: DeletionStatus::Failed,
                reason: Some(format!("{error:#}")),
                stats: None,
            };
        }
    };
    let mut stats = DeletionStats::new(collect_size);
    let checkout_error = remove_root(worktree, collect_size, false, &mut stats).err();
    // Like Git, finish administrative cleanup even if checkout removal failed.
    let git_error = remove_root(&git_dir, collect_size, false, &mut stats).err();
    if git_error.is_none()
        && let Some(parent) = git_dir.parent()
    {
        fs::remove_dir(parent).ok();
    }
    let error = checkout_error.or(git_error);
    DeletionResult {
        status: if error.is_none() {
            DeletionStatus::Deleted
        } else {
            DeletionStatus::Failed
        },
        reason: error.map(|error| format!("worktree removal failed: {error}")),
        stats: Some(stats),
    }
}

pub fn preview_worktree(
    common_dir: &Path,
    worktree: &Path,
    collect_size: bool,
) -> Result<DeletionStats> {
    let git_dir = validate(common_dir, worktree)?;
    let mut stats = DeletionStats::new(collect_size);
    remove_root(worktree, collect_size, true, &mut stats)?;
    remove_root(&git_dir, collect_size, true, &mut stats)?;
    Ok(stats)
}

fn validate(common_dir: &Path, worktree: &Path) -> Result<PathBuf> {
    // A trailing separator would make symlink_metadata follow a root symlink.
    let normalized: PathBuf = worktree.components().collect();
    let worktree = normalized.as_path();
    if !worktree.is_absolute() || !fs::symlink_metadata(worktree)?.is_dir() {
        bail!("worktree must be an absolute directory, not a symbolic link");
    }
    let worktree = worktree.canonicalize()?;
    if std::env::current_dir()?
        .canonicalize()?
        .starts_with(&worktree)
    {
        bail!("cannot remove the current working directory");
    }
    let common_dir = common_dir.canonicalize()?;
    if common_dir.starts_with(&worktree) {
        bail!("worktree contains the shared repository");
    }
    let registrations = common_dir
        .join("worktrees")
        .canonicalize()
        .context("cannot resolve worktree registrations")?;
    let marker = worktree.join(".git");
    if !fs::symlink_metadata(&marker)?.is_file() {
        bail!("invalid linked worktree marker");
    }
    let content = fs::read(&marker)?;
    let git_dir = git::trim_newline(&content)
        .strip_prefix(b"gitdir: ")
        .context("invalid linked worktree marker")?;
    let git_dir = worktree.join(git::bytes_path(git_dir)).canonicalize()?;
    if git_dir.parent() != Some(registrations.as_path()) {
        bail!("worktree belongs to another repository");
    }
    let backlink = fs::read(git_dir.join("gitdir"))?;
    let backlink = git_dir
        .join(git::bytes_path(git::trim_newline(&backlink)))
        .canonicalize()?;
    if backlink != marker {
        bail!("worktree registration points to another checkout");
    }
    let common = fs::read(git_dir.join("commondir"))?;
    if git_dir
        .join(git::bytes_path(git::trim_newline(&common)))
        .canonicalize()?
        != common_dir
    {
        bail!("worktree common directory changed");
    }
    match fs::symlink_metadata(git_dir.join("locked")) {
        Ok(_) => bail!("worktree is locked"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    if git_dir.join("modules").exists()
        || (worktree.join(".gitmodules").exists() && inspect::initialized_submodules(&worktree)?)
    {
        bail!("worktree contains initialized submodules");
    }
    let status = git::run_status(
        &worktree,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
            "--no-renames",
        ],
        false,
    )?;
    if !status.is_empty() {
        bail!("worktree contains modified or untracked files");
    }
    if git::has_hidden_index_entries(&worktree)? {
        bail!("index visibility flags prevent safe removal");
    }
    Ok(git_dir)
}

type Leaf = (PathBuf, bool, u64);

fn threads() -> usize {
    std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(4)
}

fn remove_root(
    root: &Path,
    collect_size: bool,
    preview: bool,
    total: &mut DeletionStats,
) -> io::Result<()> {
    let root: PathBuf = root.components().collect();
    #[cfg(unix)]
    let device = root
        .parent()
        .map(fs::metadata)
        .transpose()?
        .map(|metadata| std::os::unix::fs::MetadataExt::dev(&metadata));
    let options = if collect_size {
        dua_core::Options::default()
    } else {
        dua_core::Options::default().skip_metadata()
    };
    let mut walk = dua_core::walk(
        &root,
        threads(),
        dua_core::Order::Completion,
        options,
        move |entry| {
            #[cfg(unix)]
            {
                same_device(entry.path().as_path(), device).unwrap_or(false)
            }
            #[cfg(not(unix))]
            {
                let _ = entry;
                true
            }
        },
    );
    // Bounded retries allow a writer which stops shortly after the initial scan.
    for attempt in 0..3 {
        let mut leaves = Vec::new();
        let mut directories = Vec::new();
        let mut retained: Vec<PathBuf> = Vec::new();
        let mut first_error = None;
        for entry in walk.by_ref() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    first_error.get_or_insert(error);
                    continue;
                }
            };
            let path = entry.path();
            if entry.file_type.is_dir() {
                #[cfg(unix)]
                match same_device(&path, device) {
                    Ok(true) => (),
                    Ok(false) => retained.push(path.clone()),
                    Err(error) => {
                        first_error.get_or_insert(error);
                        retained.push(path.clone());
                    }
                }
                directories.push((entry.depth, path));
            } else {
                let bytes = if collect_size {
                    match entry.metadata {
                        Some(Ok(metadata)) => metadata.len(),
                        Some(Err(error)) if error.kind() == io::ErrorKind::NotFound => continue,
                        Some(Err(error)) => {
                            first_error.get_or_insert(error);
                            continue;
                        }
                        None => {
                            first_error
                                .get_or_insert_with(|| io::Error::other("missing file size"));
                            continue;
                        }
                    }
                } else {
                    0
                };
                leaves.push((path, entry.file_type.is_symlink(), bytes));
            }
        }
        if preview {
            total.files += leaves.len() as u64;
            if let Some(bytes) = &mut total.bytes {
                *bytes += leaves.iter().map(|leaf| leaf.2).sum::<u64>();
            }
            if !retained.is_empty() {
                first_error.get_or_insert_with(|| {
                    io::Error::other("refusing to traverse a mounted filesystem")
                });
            }
            return first_error.map_or(Ok(()), Err);
        }
        let (removed, error) = remove_leaves(&leaves, collect_size);
        total.add(removed);
        if first_error.is_none() {
            first_error = error;
        }
        directories.sort_unstable_by_key(|(depth, _)| std::cmp::Reverse(*depth));
        let mut root_removed = false;
        for (_, path) in directories {
            if retained.iter().any(|mount| mount.starts_with(&path)) {
                first_error.get_or_insert_with(|| {
                    io::Error::other("refusing to remove a mounted filesystem")
                });
                continue;
            }
            match fs::remove_dir(&path) {
                Ok(()) if path == root => root_removed = true,
                Ok(()) => (),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if path == root {
                        root_removed = true;
                    }
                }
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        if root_removed {
            return Ok(());
        }
        let Some(error) = first_error else {
            return Ok(());
        };
        if error.kind() != io::ErrorKind::DirectoryNotEmpty || attempt == 2 {
            return Err(error);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        if !walk.restart() {
            return Err(error);
        }
    }
    unreachable!("the final attempt returns")
}

#[cfg(unix)]
fn same_device(path: &Path, device: Option<u64>) -> io::Result<bool> {
    if path.parent().is_none() {
        return Ok(false);
    }
    fs::symlink_metadata(path).map(|metadata| {
        device.is_none_or(|device| std::os::unix::fs::MetadataExt::dev(&metadata) == device)
    })
}

fn remove_leaves(leaves: &[Leaf], collect_size: bool) -> (DeletionStats, Option<io::Error>) {
    let remove_chunk = |chunk: &[Leaf]| {
        let mut stats = DeletionStats::new(collect_size);
        let mut first_error = None;
        for (path, symlink, bytes) in chunk {
            let result = if *symlink {
                gix::fs::symlink::remove(path)
            } else {
                fs::remove_file(path)
            };
            #[cfg(windows)]
            let result = if !*symlink
                && result
                    .as_ref()
                    .is_err_and(|error| error.kind() == io::ErrorKind::PermissionDenied)
            {
                gix::fs::clear_readonly_attribute(path).and_then(|_| fs::remove_file(path))
            } else {
                result
            };
            match result {
                Ok(()) => {
                    stats.files += 1;
                    if let Some(total) = &mut stats.bytes {
                        *total += bytes;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        (stats, first_error)
    };
    if leaves.len() < 2 {
        return remove_chunk(leaves);
    }
    std::thread::scope(|scope| {
        let handles: Vec<_> = leaves
            .chunks(leaves.len().div_ceil(threads()))
            .map(|chunk| scope.spawn(|| remove_chunk(chunk)))
            .collect();
        let mut stats = DeletionStats::new(collect_size);
        let mut first_error = None;
        for handle in handles {
            let (removed, error) = handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            stats.add(removed);
            if first_error.is_none() {
                first_error = error;
            }
        }
        (stats, first_error)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_failures_count_only_successful_file_removals() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file");
        let directory = temp.path().join("directory");
        fs::write(&file, b"data").unwrap();
        fs::create_dir(&directory).unwrap();
        let leaves = vec![
            (file.clone(), false, 4),
            (directory.clone(), false, 999),
            (temp.path().join("missing"), false, 999),
        ];
        let (stats, error) = remove_leaves(&leaves, true);
        assert!(error.is_some());
        assert_eq!(stats.files, 1);
        assert_eq!(stats.bytes, Some(4));
        assert!(!file.exists() && directory.exists());
    }

    #[test]
    fn locks_and_mismatched_backlinks_reject_removal() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let worktree = root.join("checkout");
        let common = root.join("common");
        let private = common.join("worktrees/topic");
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&private).unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", private.display()),
        )
        .unwrap();
        fs::write(private.join("gitdir"), format!("{}\n", private.display())).unwrap();
        fs::write(private.join("commondir"), "../..\n").unwrap();
        assert!(
            validate(&common, &worktree)
                .unwrap_err()
                .to_string()
                .contains("another checkout")
        );
        fs::write(
            private.join("gitdir"),
            format!("{}\n", worktree.join(".git").display()),
        )
        .unwrap();
        fs::write(private.join("locked"), "keep").unwrap();
        assert!(
            validate(&common, &worktree)
                .unwrap_err()
                .to_string()
                .contains("locked")
        );
        assert!(worktree.join(".git").exists() && private.join("locked").exists());
    }

    #[cfg(unix)]
    #[test]
    fn mount_boundaries_are_not_descended() {
        let temp = tempfile::tempdir().unwrap();
        let device = std::os::unix::fs::MetadataExt::dev(&fs::metadata(temp.path()).unwrap());
        assert!(same_device(temp.path(), Some(device)).unwrap());
        assert!(!same_device(temp.path(), Some(device.wrapping_add(1))).unwrap());
        assert!(!same_device(Path::new("/"), Some(device)).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn trailing_separators_cannot_hide_root_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target");
        let link = temp.path().join("link");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep"), b"keep").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let trailing = PathBuf::from(format!("{}/", link.display()));
        assert!(
            validate(&temp.path().join("common"), &trailing)
                .unwrap_err()
                .to_string()
                .contains("not a symbolic link")
        );
        assert!(target.join("keep").exists());
    }
}
