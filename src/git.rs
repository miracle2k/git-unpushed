//! Native Git execution. Never include Git's stderr (which may contain a remote URL)
//! in public errors.
use crate::model::RepositorySeed;
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

static NATIVE_COMMANDS: AtomicU64 = AtomicU64::new(0);

pub fn invocation_count() -> u64 {
    NATIVE_COMMANDS.load(Ordering::Relaxed)
}

pub(crate) fn command(path: &Path, args: &[&str]) -> Command {
    command_with_optional_locks(path, args, false)
}

fn command_with_optional_locks(path: &Path, args: &[&str], optional_locks: bool) -> Command {
    NATIVE_COMMANDS.fetch_add(1, Ordering::Relaxed);
    let mut command = Command::new("git");
    if !optional_locks {
        command.arg("--no-optional-locks");
    }
    command
        .arg("-C")
        .arg(path)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", if optional_locks { "1" } else { "0" })
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env(
            "GIT_GRAFT_FILE",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C");
    for variable in [
        "GIT_DIR",
        "GIT_COMMON_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_QUARANTINE_PATH",
        "GIT_NAMESPACE",
        "GIT_SHALLOW_FILE",
    ] {
        command.env_remove(variable);
    }
    command
}

fn checked(output: Output, operation: &str) -> Result<Vec<u8>> {
    if !output.status.success() {
        bail!(
            "git {operation} failed (exit {})",
            output
                .status
                .code()
                .map_or_else(|| "signal".into(), |code| code.to_string())
        );
    }
    Ok(output.stdout)
}

pub fn run(path: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = command(path, args)
        .output()
        .context("could not start Git")?;
    checked(output, args.first().copied().unwrap_or("command"))
}

/// Native status can refresh the index's file stat cache without staging files.
/// Disabling this write makes Git repeat costly content and clean-filter checks
/// whenever a worktree's cached stat information is out of date.
pub(crate) fn run_status(path: &Path, args: &[&str], refresh_index: bool) -> Result<Vec<u8>> {
    let output = command_with_optional_locks(path, args, refresh_index)
        .output()
        .context("could not start Git")?;
    checked(output, "status")
}

/// Read repository metadata without inheriting repository selectors, replacement
/// objects, namespaces, or alternate object stores from the process environment.
/// Native Git remains the fallback for unsupported formats and index extensions.
pub(crate) fn open_metadata(path: &Path) -> Result<gix::Repository> {
    crate::metadata::open(path)
}

pub(crate) fn has_hidden_index_entries(path: &Path) -> Result<bool> {
    let cached = (|| -> Result<bool> {
        let repository = open_metadata(path)?;
        let index = repository.index()?;
        // Gix resolves split indexes when reading the file. Sparse directory
        // entries carry SKIP_WORKTREE, which is enough to reject deletion
        // without expanding the directory into individual files.
        let hidden =
            gix::index::entry::Flags::ASSUME_VALID | gix::index::entry::Flags::SKIP_WORKTREE;
        Ok(index
            .entries()
            .iter()
            .any(|entry| entry.flags.intersects(hidden)))
    })();
    crate::metadata::or_native(cached, || {
        let output = run(path, &["ls-files", "-v", "-z"])?;
        Ok(output.split(|byte| *byte == 0).any(|field| {
            field
                .first()
                .is_some_and(|tag| tag.is_ascii_lowercase() || *tag == b'S')
        }))
    })
}

/// The status inspection already resolved the private Git directory and read
/// HEAD. Reuse those facts instead of reopening the repository.
pub(crate) fn has_hidden_index_entries_at(
    path: &Path,
    git_dir: &Path,
    head: Option<&str>,
) -> Result<bool> {
    let kind = match head.map(str::len) {
        Some(40) => gix::hash::Kind::Sha1,
        Some(64) => gix::hash::Kind::Sha256,
        _ => return has_hidden_index_entries(path),
    };
    let cached = (|| -> Result<bool> {
        let index = gix::index::File::at(git_dir.join("index"), kind, false, Default::default())?;
        let hidden =
            gix::index::entry::Flags::ASSUME_VALID | gix::index::entry::Flags::SKIP_WORKTREE;
        Ok(index
            .entries()
            .iter()
            .any(|entry| entry.flags.intersects(hidden)))
    })();
    crate::metadata::or_native(cached, || {
        let output = run(path, &["ls-files", "-v", "-z"])?;
        Ok(output.split(|byte| *byte == 0).any(|field| {
            field
                .first()
                .is_some_and(|tag| tag.is_ascii_lowercase() || *tag == b'S')
        }))
    })
}

pub(crate) fn run_optional(path: &Path, args: &[&str]) -> Result<Option<Vec<u8>>> {
    let output = command(path, args)
        .output()
        .context("could not start Git")?;
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    checked(output, args.first().copied().unwrap_or("command")).map(Some)
}

pub(crate) fn run_with_input(path: &Path, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
    let mut child = command(path, args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("could not start Git")?;
    // Input is a small list of object IDs; write on a separate thread so an
    // unusually large repository cannot deadlock its input and output pipes.
    let mut stdin = child.stdin.take().context("Git stdin unavailable")?;
    let input = input.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output().context("could not wait for Git")?;
    let write_result = writer
        .join()
        .map_err(|_| anyhow::anyhow!("Git input writer failed"))?;
    let result = checked(output, args.first().copied().unwrap_or("command"));
    write_result.context("could not write Git input")?;
    result
}

pub fn resolve_seed(path: &Path) -> Result<RepositorySeed> {
    if let Some(seed) = seed_from_metadata(path) {
        return Ok(seed);
    }
    match crate::metadata::open(path) {
        Ok(repo) => {
            return Ok(RepositorySeed {
                path: repo
                    .workdir()
                    .unwrap_or_else(|| repo.common_dir())
                    .to_path_buf(),
                common_dir: repo
                    .common_dir()
                    .canonicalize()
                    .context("cannot resolve common Git directory")?,
            });
        }
        Err(error) if crate::metadata::may_use_native(&error) => (),
        Err(_) => bail!("cannot resolve repository metadata"),
    }
    let common = run(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common_dir = bytes_path(trim_newline(&common))
        .canonicalize()
        .context("cannot resolve common Git directory")?;
    let bare = run(path, &["rev-parse", "--is-bare-repository"])?;
    let repository_path = if trim_newline(&bare) == b"true" {
        path.canonicalize()
            .context("cannot resolve repository path")?
    } else {
        let top = run(path, &["rev-parse", "--show-toplevel"])?;
        bytes_path(trim_newline(&top))
            .canonicalize()
            .context("cannot resolve repository root")?
    };
    Ok(RepositorySeed {
        path: repository_path,
        common_dir,
    })
}

// Discovery already hands us checkout roots. Standard metadata is sufficient to
// group them; the subsequent native Git inspection validates the repository.
fn seed_from_metadata(path: &Path) -> Option<RepositorySeed> {
    let root = path.canonicalize().ok()?;
    let marker = root.join(".git");
    let git_dir = if marker.is_dir() {
        marker.canonicalize().ok()?
    } else if marker.is_file() {
        let bytes = std::fs::read(&marker).ok()?;
        let relative = trim_newline(&bytes).strip_prefix(b"gitdir: ")?;
        root.join(bytes_path(relative)).canonicalize().ok()?
    } else if root.join("HEAD").is_file() && root.join("objects").is_dir() {
        root.clone()
    } else {
        return None;
    };
    if !git_dir.join("HEAD").is_file() {
        return None;
    }
    let common_dir = match std::fs::read(git_dir.join("commondir")) {
        Ok(bytes) => git_dir
            .join(bytes_path(trim_newline(&bytes)))
            .canonicalize()
            .ok()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => git_dir,
        Err(_) => return None,
    };
    if !common_dir.join("objects").is_dir() {
        return None;
    }
    Some(RepositorySeed {
        path: root,
        common_dir,
    })
}

pub fn remove_worktree(repo_path: &Path, worktree_path: &Path) -> Result<()> {
    let result = crate::removal::remove_worktree(repo_path, worktree_path, false);
    if matches!(result.status, crate::model::DeletionStatus::Deleted) {
        Ok(())
    } else {
        bail!(
            "{}",
            result
                .reason
                .unwrap_or_else(|| "worktree removal failed".into())
        )
    }
}

pub(crate) fn trim_newline(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\n").unwrap_or(bytes)
}

pub(crate) fn bytes_path(bytes: &[u8]) -> std::path::PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(bytes.to_vec()).into()
    }
    #[cfg(not(unix))]
    {
        String::from_utf8_lossy(bytes).into_owned().into()
    }
}
