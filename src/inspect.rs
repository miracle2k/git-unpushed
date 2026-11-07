use crate::git::{self, bytes_path, trim_newline};
use crate::model::*;
use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Mapping {
    source: String,
    destination: String,
}

impl Mapping {
    fn parse(value: &str) -> Result<Self> {
        let value = value.strip_prefix('+').unwrap_or(value);
        let (source, destination) = value.split_once(':').context("unsupported fetch refspec")?;
        if !source.starts_with("refs/heads/")
            || !destination.starts_with("refs/")
            || destination.starts_with("refs/heads/")
            || destination.starts_with("refs/tags/")
            || ["refs/heads/", "refs/tags/"].iter().any(|protected| {
                let prefix = destination.split('*').next().unwrap_or("");
                protected.starts_with(prefix)
            })
            || source.matches('*').count() > 1
            || source.matches('*').count() != destination.matches('*').count()
            || source.contains(['\n', '\r', '\0'])
            || destination.contains(['\n', '\r', '\0'])
        {
            bail!("unsupported or unsafe fetch refspec");
        }
        Ok(Self {
            source: source.into(),
            destination: destination.into(),
        })
    }

    fn source_for(&self, reference: &str) -> Option<String> {
        if let Some((prefix, suffix)) = self.destination.split_once('*') {
            let middle = reference.strip_prefix(prefix)?.strip_suffix(suffix)?;
            Some(self.source.replacen('*', middle, 1))
        } else if reference == self.destination {
            Some(self.source.clone())
        } else {
            None
        }
    }

    fn destination_for(&self, source: &str) -> Option<String> {
        if let Some((prefix, suffix)) = self.source.split_once('*') {
            let middle = source.strip_prefix(prefix)?.strip_suffix(suffix)?;
            Some(self.destination.replacen('*', middle, 1))
        } else if source == self.source {
            Some(self.destination.clone())
        } else {
            None
        }
    }

    fn overlaps(&self, other: &Self) -> bool {
        match (
            self.destination.split_once('*'),
            other.destination.split_once('*'),
        ) {
            (None, None) => self.destination == other.destination,
            (Some(_), None) => self.source_for(&other.destination).is_some(),
            (None, Some(_)) => other.source_for(&self.destination).is_some(),
            (Some((left_prefix, left_suffix)), Some((right_prefix, right_suffix))) => {
                (left_prefix.starts_with(right_prefix) || right_prefix.starts_with(left_prefix))
                    && (left_suffix.ends_with(right_suffix) || right_suffix.ends_with(left_suffix))
            }
        }
    }
}

struct Remote {
    report: RemoteReport,
    mappings: Vec<Mapping>,
    refreshed: bool,
}

#[derive(Clone)]
struct Reference {
    oid: String,
    symbolic: Option<String>,
}

pub fn inspect_repository(
    seed: &RepositorySeed,
    options: &ScanOptions,
) -> Result<RepositoryReport> {
    inspect(seed, options, None, None)
}

/// Report each working-copy inspection as it completes. Callbacks can run
/// concurrently on the scan's Rayon workers, including for inaccessible trees.
pub fn inspect_repository_with_progress(
    seed: &RepositorySeed,
    options: &ScanOptions,
    completed: &(dyn Fn(&Path) + Sync),
) -> Result<RepositoryReport> {
    inspect(seed, options, None, Some(completed))
}

pub fn revalidate_worktree(
    repository: &RepositoryReport,
    original: &WorktreeReport,
    options: &ScanOptions,
) -> Result<WorktreeReport> {
    let seed = RepositorySeed {
        path: repository.common_dir.clone(),
        common_dir: repository.common_dir.clone(),
    };
    let mut offline = options.clone();
    offline.fetch = false;
    offline.refresh_index = false;
    let refreshed = inspect(&seed, &offline, Some(&original.path), None)?;
    let Some(mut result) = refreshed
        .worktrees
        .into_iter()
        .find(|entry| entry.path == original.path)
    else {
        let mut result = original.clone();
        result.removable = false;
        result.reason = Some("worktree registration changed".into());
        return Ok(result);
    };
    if result.head != original.head || result.branch != original.branch {
        result.removable = false;
        result.reason = Some("HEAD or branch changed since the scan".into());
    }
    // A failed requested refresh must never be replaced by stale evidence during
    // this offline revalidation. Successful evidence from another remote remains valid.
    if options.fetch && !repository.fetched {
        let failed: HashSet<&str> = repository
            .remotes
            .iter()
            .filter(|remote| remote.issue.as_deref() == Some("refresh failed"))
            .map(|remote| remote.name.as_str())
            .collect();
        if !failed.is_empty() {
            let mut restricted = offline;
            restricted.remotes = repository
                .remotes
                .iter()
                .filter(|remote| remote.eligible && !failed.contains(remote.name.as_str()))
                .map(|remote| remote.name.clone())
                .collect();
            if restricted.remotes.is_empty() {
                result.removable = false;
                result.reason = Some("remote refresh failed".into());
            } else {
                let checked = inspect(&seed, &restricted, Some(&original.path), None)?;
                if let Some(new) = checked
                    .worktrees
                    .into_iter()
                    .find(|entry| entry.path == original.path)
                {
                    result.removable &= new.removable;
                    if !new.removable {
                        result.reason = new.reason;
                    }
                } else {
                    result.removable = false;
                }
            }
        }
    }
    Ok(result)
}

fn inspect(
    seed: &RepositorySeed,
    options: &ScanOptions,
    only: Option<&Path>,
    completed: Option<&(dyn Fn(&Path) + Sync)>,
) -> Result<RepositoryReport> {
    let entries = git::run(&seed.path, &["worktree", "list", "--porcelain", "-z"])?;
    let mut worktrees = parse_worktrees(&entries)?;
    let primary_path = worktrees
        .first()
        .map(|entry| entry.path.clone())
        .unwrap_or_else(|| seed.path.clone());
    let bare = worktrees.first().is_some_and(|entry| entry.bare);
    let repository_path = if bare {
        seed.common_dir.clone()
    } else {
        primary_path
    };
    let mut errors = Vec::new();
    let mut remotes = load_remotes(&seed.path, options, &mut errors)?;
    if options.fetch {
        for remote in &mut remotes {
            if !remote.report.eligible {
                continue;
            }
            match git::run(
                &seed.path,
                &[
                    "fetch",
                    "--prune",
                    "--no-tags",
                    "--no-prune-tags",
                    "--no-recurse-submodules",
                    "--",
                    &remote.report.name,
                ],
            ) {
                Ok(_) => remote.refreshed = true,
                Err(_) => {
                    remote.report.issue = Some("refresh failed".into());
                    errors.push(format!("remote {}: refresh failed", remote.report.name));
                }
            }
        }
    }
    let references = load_references(&seed.path)?;
    let mut backup_tips = HashSet::new();
    let mut mainline_tips = HashSet::new();
    let explicit_base = match options.base.as_deref() {
        Some(base) => resolve_base(&seed.path, base, &references),
        None => None,
    };
    if options.base.is_some() && explicit_base.is_none() {
        errors.push("base must name an existing eligible remote branch".into());
    }
    for remote in &mut remotes {
        if !remote.report.eligible || (options.fetch && !remote.refreshed) {
            continue;
        }
        for (name, reference) in &references {
            if reference.symbolic.is_none()
                && remote
                    .mappings
                    .iter()
                    .any(|mapping| mapping.source_for(name).is_some())
            {
                backup_tips.insert(reference.oid.clone());
            }
        }
        let default = if options.base.is_some() {
            explicit_base
                .as_ref()
                .filter(|name| {
                    remote
                        .mappings
                        .iter()
                        .any(|mapping| mapping.source_for(name).is_some())
                })
                .cloned()
        } else {
            default_branch(remote, &references)
        };
        if let Some(name) = default {
            if let Some(reference) = references.get(&name) {
                mainline_tips.insert(reference.oid.clone());
                remote.report.default_branch = Some(name);
            }
        } else if options.base.is_none() {
            remote.report.issue = Some("remote default branch is unknown or ambiguous".into());
        }
    }
    if options.base.is_some() && mainline_tips.is_empty() && explicit_base.is_some() {
        errors.push("base is not mapped from an eligible remote".into());
    }
    let current = options
        .current_dir
        .canonicalize()
        .unwrap_or_else(|_| options.current_dir.clone());
    worktrees
        .par_iter_mut()
        .filter(|entry| !entry.bare && only.is_none_or(|path| entry.path == path))
        .for_each(|entry| {
            inspect_status(entry, &seed.common_dir, &current, options.refresh_index);
            if let Some(completed) = completed {
                completed(&entry.path);
            }
        });
    let mut heads: HashSet<String> = worktrees
        .iter()
        .filter_map(|entry| entry.head.clone())
        .collect();
    let branches: Vec<(String, String)> = references
        .iter()
        .filter_map(|(name, reference)| {
            name.strip_prefix("refs/heads/")
                .map(|branch| (branch.to_owned(), reference.oid.clone()))
        })
        .collect();
    heads.extend(branches.iter().map(|(_, oid)| oid.clone()));
    let shallow = match fs::metadata(seed.common_dir.join("shallow")) {
        Ok(metadata) => metadata.len() != 0,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    };
    let backup = reachable(&seed.path, &heads, &backup_tips, shallow, &mut errors);
    let mut integrated = if mainline_tips.is_empty() {
        heads.iter().map(|head| (head.clone(), None)).collect()
    } else if backup_tips == mainline_tips {
        backup.clone()
    } else {
        reachable(&seed.path, &heads, &mainline_tips, shallow, &mut errors)
    };
    // Only unfinished, clean linked worktrees need the squash fallback. Status
    // guards still apply after a match, and deletion revalidates the same proof.
    if !shallow && !mainline_tips.is_empty() {
        let candidates: HashSet<String> = worktrees
            .iter()
            .filter(|entry| {
                !entry.primary
                    && !entry.bare
                    && only.is_none_or(|path| entry.path == path)
                    && !entry.modified
                    && !entry.untracked
                    && !entry.conflicts
                    && !entry.locked
                    && !entry.prunable
                    && entry.errors.is_empty()
                    && entry.operation.is_none()
                    && entry.reason.is_none()
                    && !(options.keep_ignored && entry.ignored)
            })
            .filter_map(|entry| entry.head.as_ref())
            .filter(|head| {
                integrated.get(*head) == Some(&Some(false))
                    && backup.get(*head) == Some(&Some(true))
            })
            .cloned()
            .collect();
        if !candidates.is_empty() {
            match crate::squash::detect(
                &seed.path,
                &seed.common_dir,
                &candidates,
                &mainline_tips,
                options.deep,
            ) {
                Ok(matches) => {
                    for head in matches {
                        integrated.insert(head, Some(true));
                    }
                }
                Err(error) => {
                    errors.push(format!("could not verify squash integration: {error:#}"))
                }
            }
        }
    }
    for entry in &mut worktrees {
        if entry.bare || only.is_some_and(|path| entry.path != path) {
            continue;
        }
        entry.backed_up = entry
            .head
            .as_ref()
            .and_then(|head| backup.get(head).copied().flatten());
        entry.integrated = entry
            .head
            .as_ref()
            .and_then(|head| integrated.get(head).copied().flatten());
        assess_removal(entry, options);
    }
    let mut unpushed_branches = Vec::new();
    let mut unknown_branches = Vec::new();
    for (branch, head) in branches {
        match backup.get(&head).copied().flatten() {
            Some(false) => unpushed_branches.push(branch),
            None => unknown_branches.push(branch),
            Some(true) => (),
        }
    }
    unpushed_branches.sort();
    unknown_branches.sort();
    let fetched = options.fetch
        && remotes
            .iter()
            .filter(|remote| remote.report.eligible)
            .all(|remote| remote.refreshed)
        && remotes.iter().any(|remote| remote.report.eligible);
    Ok(RepositoryReport {
        path: repository_path,
        common_dir: seed.common_dir.clone(),
        bare,
        stashes: references.contains_key("refs/stash"),
        unpushed_branches,
        unknown_branches,
        remotes: remotes.into_iter().map(|remote| remote.report).collect(),
        fetched,
        worktrees,
        errors,
    })
}

fn parse_worktrees(bytes: &[u8]) -> Result<Vec<WorktreeReport>> {
    let mut result = Vec::new();
    let mut current: Option<WorktreeReport> = None;
    for field in bytes.split(|byte| *byte == 0) {
        if field.is_empty() {
            if let Some(entry) = current.take() {
                result.push(entry);
            }
        } else if let Some(path) = field.strip_prefix(b"worktree ") {
            if current.is_some() {
                bail!("malformed worktree listing");
            }
            current = Some(WorktreeReport {
                path: bytes_path(path),
                branch: None,
                head: None,
                primary: result.is_empty(),
                bare: false,
                locked: false,
                prunable: false,
                modified: false,
                untracked: false,
                ignored: false,
                conflicts: false,
                operation: None,
                integrated: None,
                backed_up: None,
                removable: false,
                reason: None,
                errors: Vec::new(),
                deletion: None,
            });
        } else if let Some(entry) = &mut current {
            if let Some(head) = field.strip_prefix(b"HEAD ") {
                let head = String::from_utf8_lossy(head).into_owned();
                if !head.bytes().all(|byte| byte == b'0') {
                    entry.head = Some(head);
                }
            } else if let Some(branch) = field.strip_prefix(b"branch refs/heads/") {
                entry.branch = Some(String::from_utf8_lossy(branch).into_owned());
            } else if field == b"bare" {
                entry.bare = true;
            } else if field.starts_with(b"locked") {
                entry.locked = true;
            } else if field.starts_with(b"prunable") {
                entry.prunable = true;
            }
        }
    }
    if let Some(entry) = current {
        result.push(entry);
    }
    if result.is_empty() {
        bail!("Git returned no worktree metadata");
    }
    Ok(result)
}

fn inspect_status(
    entry: &mut WorktreeReport,
    common_dir: &Path,
    current: &Path,
    refresh_index: bool,
) {
    if entry.prunable || !entry.path.is_dir() {
        entry
            .errors
            .push("worktree is inaccessible or prunable".into());
        return;
    }
    match git::run_status(
        &entry.path,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=normal",
            "--ignored=matching",
            "--ignore-submodules=none",
            "--no-renames",
            "--no-ahead-behind",
        ],
        refresh_index,
    ) {
        Ok(output) => {
            if let Err(error) = parse_status(&output, entry) {
                entry.errors.push(error.to_string());
            }
        }
        Err(_) => entry
            .errors
            .push("could not inspect working-copy status".into()),
    }
    let git_dir = match worktree_git_dir(&entry.path) {
        Ok(path) => path,
        Err(_) => {
            entry
                .errors
                .push("could not resolve worktree Git directory".into());
            return;
        }
    };
    for (marker, name) in [
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
        ("BISECT_LOG", "bisect"),
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase or apply"),
        ("sequencer", "sequencer"),
        ("index.lock", "index lock"),
        ("HEAD.lock", "HEAD lock"),
    ] {
        if git_dir.join(marker).exists() {
            entry.operation = Some(name.into());
            break;
        }
    }
    if ["packed-refs.lock", "shallow.lock", "config.lock"]
        .iter()
        .any(|name| common_dir.join(name).exists())
    {
        entry.operation = Some("repository lock".into());
    }
    let resolved_path = entry
        .path
        .canonicalize()
        .unwrap_or_else(|_| entry.path.clone());
    if current.starts_with(&resolved_path) {
        entry.reason = Some("current working directory".into());
    }
    if entry.path.join(".jj").exists() {
        entry.reason = Some("Jujutsu working copy".into());
    }
    if entry.path.join(".gitmodules").exists() {
        match initialized_submodules(&entry.path) {
            Ok(true) => entry.reason = Some("initialized submodules".into()),
            Err(_) => entry.errors.push("could not check submodules".into()),
            Ok(false) => (),
        }
    }
    // These index flags deliberately suppress status checks, so a clean status
    // cannot establish that deleting the tracked files preserves user data.
    if !entry.primary && !entry.modified && !entry.untracked && entry.errors.is_empty() {
        match git::has_hidden_index_entries(&entry.path) {
            Ok(hidden) => {
                if hidden {
                    entry.errors.push("index has assume-unchanged or skip-worktree flags; file cleanliness is unproven".into());
                }
            }
            Err(_) => entry
                .errors
                .push("could not check index visibility flags".into()),
        }
    }
}

fn worktree_git_dir(path: &Path) -> Result<PathBuf> {
    let marker = path.join(".git");
    if marker.is_dir() {
        return marker
            .canonicalize()
            .context("cannot resolve Git directory");
    }
    let bytes = fs::read(marker)?;
    let directory = trim_newline(&bytes)
        .strip_prefix(b"gitdir: ")
        .context("invalid Git directory file")?;
    let path = path.join(bytes_path(directory));
    path.canonicalize()
        .context("cannot resolve linked Git directory")
}

fn initialized_submodules(path: &Path) -> Result<bool> {
    let output = git::run_optional(
        path,
        &[
            "config",
            "--no-includes",
            "--null",
            "--file",
            ".gitmodules",
            "--get-regexp",
            "^submodule\\..*\\.path$",
        ],
    )?;
    for field in output.unwrap_or_default().split(|byte| *byte == 0) {
        if let Some(index) = field.iter().position(|byte| *byte == b'\n')
            && path
                .join(bytes_path(&field[index + 1..]))
                .join(".git")
                .exists()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn parse_status(bytes: &[u8], entry: &mut WorktreeReport) -> Result<()> {
    let mut fields = bytes.split(|byte| *byte == 0);
    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        match field[0] {
            b'?' => entry.untracked = true,
            b'!' => entry.ignored = true,
            b'1' | b'2' | b'u' => {
                let columns: Vec<_> = field.splitn(4, |byte| *byte == b' ').collect();
                if columns.len() < 4 || columns[1].len() != 2 || columns[2].len() != 4 {
                    bail!("malformed Git status");
                }
                entry.modified |= columns[1] != b".."
                    || (columns[2][0] == b'S' && columns[2][1..].iter().any(|byte| *byte != b'.'));
                entry.conflicts |= field[0] == b'u';
                if field[0] == b'2' && fields.next().is_none() {
                    bail!("malformed Git rename status");
                }
            }
            b'#' => {
                if let Some(head) = field.strip_prefix(b"# branch.oid ") {
                    entry.head = if head == b"(initial)" {
                        None
                    } else {
                        Some(String::from_utf8_lossy(head).into_owned())
                    };
                } else if let Some(branch) = field.strip_prefix(b"# branch.head ") {
                    entry.branch = if branch == b"(detached)" {
                        None
                    } else {
                        Some(String::from_utf8_lossy(branch).into_owned())
                    };
                }
            }
            _ => bail!("unrecognized Git status record"),
        }
    }
    Ok(())
}

fn assess_removal(entry: &mut WorktreeReport, options: &ScanOptions) {
    entry.removable = false;
    let reason = if entry.primary && entry.integrated.is_none() {
        Some("primary checkout; remote integration could not be proven")
    } else if entry.primary {
        Some("primary checkout")
    } else if entry.locked {
        Some("locked worktree")
    } else if entry.prunable {
        Some("prunable worktree")
    } else if !entry.errors.is_empty() {
        Some("worktree inspection failed")
    } else if entry.operation.is_some() {
        Some("Git operation in progress")
    } else if entry.reason.is_some() {
        return;
    } else if entry.head.is_none() {
        Some("HEAD is unborn or unknown")
    } else if entry.modified || entry.untracked || entry.conflicts {
        Some("uncommitted changes")
    } else if options.keep_ignored && entry.ignored {
        Some("ignored files preserved")
    } else if entry.integrated == Some(true)
        || (options.include_unmerged && entry.backed_up == Some(true))
    {
        None
    } else if entry.integrated == Some(false) {
        Some("commits not integrated into a remote mainline")
    } else {
        Some("remote integration could not be proven")
    };
    entry.reason = reason.map(str::to_owned);
    entry.removable = reason.is_none();
}

fn load_references(path: &Path) -> Result<HashMap<String, Reference>> {
    references_from_metadata(path).or_else(|_| references_from_git(path))
}

fn references_from_metadata(path: &Path) -> Result<HashMap<String, Reference>> {
    let repository = git::open_metadata(path)?;
    let platform = repository
        .references()
        .map_err(|_| anyhow::anyhow!("cannot list references"))?;
    let mut raw = HashMap::new();
    for reference in platform
        .all()
        .map_err(|_| anyhow::anyhow!("cannot list references"))?
    {
        let reference = reference.map_err(|_| anyhow::anyhow!("cannot read reference"))?;
        let name = std::str::from_utf8(reference.name().as_bstr())?.to_owned();
        let (oid, symbolic) = match reference.target() {
            gix::refs::TargetRef::Object(oid) => (oid.to_string(), None),
            gix::refs::TargetRef::Symbolic(name) => (
                String::new(),
                Some(std::str::from_utf8(name.as_bstr())?.to_owned()),
            ),
        };
        raw.insert(name, Reference { oid, symbolic });
    }
    let mut result = HashMap::with_capacity(raw.len());
    for (name, reference) in &raw {
        let mut current_name = name;
        let mut current = reference;
        let mut visited = HashSet::new();
        while let Some(target) = &current.symbolic {
            if !visited.insert(current_name) {
                bail!("symbolic reference cycle");
            }
            current_name = target;
            current = raw.get(target).context("unresolved symbolic reference")?;
        }
        result.insert(
            name.clone(),
            Reference {
                // Do not peel annotated tags: `for-each-ref %(objectname)` returns
                // the tag object's ID. Symbolic refs report their final target.
                oid: current.oid.clone(),
                symbolic: reference.symbolic.as_ref().map(|_| current_name.clone()),
            },
        );
    }
    Ok(result)
}

fn references_from_git(path: &Path) -> Result<HashMap<String, Reference>> {
    let bytes = git::run(
        path,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(symref)",
        ],
    )?;
    let mut result = HashMap::new();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let mut fields = line.split(|byte| *byte == 0);
        let name = std::str::from_utf8(fields.next().context("invalid reference")?)?;
        let oid = std::str::from_utf8(fields.next().context("invalid reference")?)?;
        let symbolic = std::str::from_utf8(fields.next().context("invalid reference")?)?;
        result.insert(
            name.into(),
            Reference {
                oid: oid.into(),
                symbolic: (!symbolic.is_empty()).then(|| symbolic.into()),
            },
        );
    }
    Ok(result)
}

fn load_remotes(
    path: &Path,
    options: &ScanOptions,
    errors: &mut Vec<String>,
) -> Result<Vec<Remote>> {
    let config = git::run_optional(
        path,
        &[
            "config",
            "--null",
            "--get-regexp",
            "^(remote\\..*\\.(url|fetch|vcs)|url\\..*\\.insteadof)$",
        ],
    )?;
    #[derive(Default)]
    struct Configuration {
        specifications: Vec<String>,
        url: Option<String>,
        foreign_vcs: bool,
    }
    let mut configurations: HashMap<String, Configuration> = HashMap::new();
    let mut rewrites = Vec::new();
    for field in config.unwrap_or_default().split(|byte| *byte == 0) {
        let Some(index) = field.iter().position(|byte| *byte == b'\n') else {
            continue;
        };
        let key = std::str::from_utf8(&field[..index])?;
        let value = String::from_utf8_lossy(&field[index + 1..]).into_owned();
        if let Some(base) = key
            .strip_prefix("url.")
            .and_then(|key| key.strip_suffix(".insteadof"))
        {
            rewrites.push((base.to_owned(), value));
            continue;
        }
        let Some(key) = key.strip_prefix("remote.") else {
            continue;
        };
        if let Some(name) = key.strip_suffix(".fetch") {
            configurations
                .entry(name.into())
                .or_default()
                .specifications
                .push(value);
        } else if let Some(name) = key.strip_suffix(".url") {
            configurations
                .entry(name.into())
                .or_default()
                .url
                .get_or_insert(value);
        } else if let Some(name) = key.strip_suffix(".vcs") {
            configurations.entry(name.into()).or_default().foreign_vcs = true;
        }
    }
    let mut result = Vec::new();
    for (name, configuration) in configurations {
        let selected = options.remotes.is_empty() || options.remotes.contains(&name);
        let safe_name = !name.starts_with('-') && !name.contains(['\n', '\r', '\0']);
        let external = safe_name
            && !configuration.foreign_vcs
            && configuration
                .url
                .as_ref()
                .is_some_and(|url| nonlocal_url(&rewrite_url(url, &rewrites)));
        let mut issue = if !selected {
            Some("excluded by remote filter".into())
        } else if !external {
            Some("local or unsupported remote transport".into())
        } else {
            None
        };
        let mut mappings = Vec::new();
        for specification in configuration.specifications {
            match Mapping::parse(&specification) {
                Ok(mapping) => {
                    if !mappings.contains(&mapping) {
                        mappings.push(mapping);
                    }
                }
                Err(_) => {
                    if selected && external {
                        issue = Some("unsupported or unsafe fetch refspec".into());
                    }
                }
            }
        }
        if mappings.is_empty() && selected && external {
            issue = Some("no supported fetch refspec".into());
        }
        if mappings.iter().enumerate().any(|(index, left)| {
            mappings[index + 1..]
                .iter()
                .any(|right| left.overlaps(right))
        }) && selected
            && external
        {
            issue = Some("ambiguous overlapping fetch refspecs".into());
        }
        let eligible = selected && external && issue.is_none();
        result.push(Remote {
            report: RemoteReport {
                name,
                eligible,
                default_branch: None,
                issue,
            },
            mappings,
            refreshed: false,
        });
    }
    result.sort_by(|left, right| left.report.name.cmp(&right.report.name));
    // An overlapping mapping cannot prove which remote supplied a local ref.
    let mut overlapping = HashSet::new();
    for (index, remote) in result.iter().enumerate() {
        for (other_index, other) in result.iter().enumerate().skip(index + 1) {
            if remote
                .mappings
                .iter()
                .any(|left| other.mappings.iter().any(|right| left.overlaps(right)))
            {
                overlapping.insert(index);
                overlapping.insert(other_index);
            }
        }
    }
    for index in overlapping {
        let remote = &mut result[index];
        if remote.report.eligible {
            remote.report.eligible = false;
            remote.report.issue = Some("fetch refspec overlaps another remote".into());
        }
    }
    for name in &options.remotes {
        if !result.iter().any(|remote| remote.report.name == *name) {
            errors.push(format!("remote {name}: not configured"));
        }
    }
    for remote in &result {
        if remote
            .report
            .issue
            .as_deref()
            .is_some_and(|issue| issue.contains("refspec"))
        {
            errors.push(format!(
                "remote {}: {}",
                remote.report.name,
                remote
                    .report
                    .issue
                    .as_deref()
                    .unwrap_or("unsupported mapping")
            ));
        }
    }
    Ok(result)
}

// Fetch uses the first configured URL, with exactly one longest-prefix insteadOf
// rewrite. Config output preserves precedence order, so equal-length matches keep
// the first configured rule, just as Git does. pushInsteadOf is deliberately absent.
fn rewrite_url(url: &str, rewrites: &[(String, String)]) -> String {
    let mut best: Option<(&str, &str)> = None;
    for (replacement, prefix) in rewrites {
        if url.starts_with(prefix) && best.is_none_or(|(_, previous)| prefix.len() > previous.len())
        {
            best = Some((replacement, prefix));
        }
    }
    if let Some((replacement, prefix)) = best {
        format!("{replacement}{}", &url[prefix.len()..])
    } else {
        url.to_owned()
    }
}

fn nonlocal_url(url: &str) -> bool {
    if url.is_empty() || url.contains(['\n', '\r', '\0']) {
        return false;
    }
    let host = if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "https" | "http" | "ssh" | "git"
        ) {
            return false;
        }
        rest.split('/')
            .next()
            .unwrap_or("")
            .rsplit('@')
            .next()
            .unwrap_or("")
    } else {
        if url.contains("::") || url.starts_with(['/', '.', '~', '\\']) {
            return false;
        }
        let Some((host, path)) = url.split_once(':') else {
            return false;
        };
        if path.is_empty() || host.contains(['/', '\\']) || host.len() == 1 {
            return false;
        }
        host.rsplit('@').next().unwrap_or("")
    };
    let host = if let Some(bracketed) = host.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("")
    } else {
        host.split(':').next().unwrap_or("")
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    !host.is_empty()
        && host != "localhost"
        && !host.ends_with(".localhost")
        && !host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback() || address.is_unspecified())
}

fn default_branch(remote: &Remote, references: &HashMap<String, Reference>) -> Option<String> {
    let mut symbolic = HashSet::new();
    for mapping in &remote.mappings {
        if let Some(head) = mapping.destination_for("refs/heads/HEAD")
            && let Some(target) = references
                .get(&head)
                .and_then(|reference| reference.symbolic.as_ref())
            && remote
                .mappings
                .iter()
                .any(|mapping| mapping.source_for(target).is_some())
            && references.contains_key(target)
        {
            symbolic.insert(target.clone());
        }
    }
    if symbolic.len() == 1 {
        return symbolic.into_iter().next();
    }
    if symbolic.len() > 1 {
        return None;
    }
    let mut fallback = HashSet::new();
    for mapping in &remote.mappings {
        for branch in ["refs/heads/main", "refs/heads/master"] {
            if let Some(name) = mapping.destination_for(branch)
                && references
                    .get(&name)
                    .is_some_and(|reference| reference.symbolic.is_none())
            {
                fallback.insert(name);
            }
        }
    }
    if fallback.len() == 1 {
        fallback.into_iter().next()
    } else {
        None
    }
}

fn resolve_base(
    path: &Path,
    base: &str,
    references: &HashMap<String, Reference>,
) -> Option<String> {
    let output = git::run(
        path,
        &[
            "rev-parse",
            "--verify",
            "--symbolic-full-name",
            "--end-of-options",
            base,
        ],
    )
    .ok()?;
    let name = std::str::from_utf8(trim_newline(&output)).ok()?;
    references
        .get(name)
        .filter(|reference| reference.symbolic.is_none())
        .map(|_| name.into())
}

fn reachable(
    path: &Path,
    heads: &HashSet<String>,
    tips: &HashSet<String>,
    shallow: bool,
    errors: &mut Vec<String>,
) -> HashMap<String, Option<bool>> {
    if tips.is_empty() {
        return heads
            .iter()
            .map(|head| (head.clone(), Some(false)))
            .collect();
    }
    if heads.iter().all(|head| tips.contains(head)) {
        return heads
            .iter()
            .map(|head| (head.clone(), Some(true)))
            .collect();
    }
    if shallow {
        return heads
            .iter()
            .map(|head| (head.clone(), tips.contains(head).then_some(true)))
            .collect();
    }
    let mut input = Vec::new();
    for head in heads {
        input.extend_from_slice(head.as_bytes());
        input.push(b'\n');
    }
    for tip in tips {
        input.push(b'^');
        input.extend_from_slice(tip.as_bytes());
        input.push(b'\n');
    }
    match git::run_with_input(path, &["rev-list", "--stdin"], &input) {
        Ok(output) => {
            let unique: HashSet<&[u8]> = output.split(|byte| *byte == b'\n').collect();
            heads
                .iter()
                .map(|head| (head.clone(), Some(!unique.contains(head.as_bytes()))))
                .collect()
        }
        Err(_) => {
            if !errors
                .iter()
                .any(|error| error == "could not prove commit reachability")
            {
                errors.push("could not prove commit reachability".into());
            }
            heads.iter().map(|head| (head.clone(), None)).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_external_known_transports_qualify() {
        for url in [
            "git@example.com:team/repo.git",
            "https://example.com/repo",
            "ssh://git@example.com:2222/repo",
            "git://example.com/repo",
        ] {
            assert!(nonlocal_url(url), "{url}");
        }
        for url in [
            "../repo",
            "/tmp/repo",
            "file:///tmp/repo",
            "https://localhost/repo",
            "ssh://127.0.0.1/repo",
            "ssh://[::1]/repo",
            "ext::anything",
            "helper::anything",
            "C:\\repo",
            "repo.bundle",
        ] {
            assert!(!nonlocal_url(url), "{url}");
        }
    }

    #[test]
    fn nul_status_preserves_rename_path_boundaries() {
        let mut entry =
            parse_worktrees(b"worktree /tmp/repo\0HEAD abc\0branch refs/heads/main\0\0")
                .unwrap()
                .remove(0);
        parse_status(b"# branch.oid abc\0# branch.head main\0? newline\npath\0! ignored\0\x32 R. N... 100644 100644 100644 abc def R100 new\0? old rename path\0", &mut entry).unwrap();
        assert!(entry.modified && entry.untracked && entry.ignored);
        assert!(!entry.conflicts);
    }

    #[test]
    fn mappings_do_not_accept_local_branch_or_tag_namespaces() {
        assert!(Mapping::parse("+refs/heads/*:refs/heads/*").is_err());
        assert!(Mapping::parse("+refs/heads/*:refs/tags/*").is_err());
        let mapping = Mapping::parse("+refs/heads/*:refs/remotes/origin/*").unwrap();
        assert_eq!(
            mapping.source_for("refs/remotes/origin/main").as_deref(),
            Some("refs/heads/main")
        );
        assert_eq!(
            mapping.destination_for("refs/heads/feature").as_deref(),
            Some("refs/remotes/origin/feature")
        );
    }
}
