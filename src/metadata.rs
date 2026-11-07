//! In-process Git reads. Callers retain native Git compatibility fallbacks.
use crate::model::WorktreeReport;
use anyhow::{Context, Result, bail};
use gix::bstr::ByteSlice;
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
};

#[derive(Debug)]
struct OpenError {
    unsupported: bool,
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("cannot open repository with Gix")
    }
}

impl std::error::Error for OpenError {}

pub(crate) fn may_use_native(error: &anyhow::Error) -> bool {
    if let Some(error) = error.downcast_ref::<OpenError>() {
        error.unsupported
    } else if let Some(error) = error.downcast_ref::<gix::Error>() {
        error.is_unsupported()
    } else {
        error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::Unsupported)
    }
}

pub(crate) fn or_native<T>(result: Result<T>, native: impl FnOnce() -> Result<T>) -> Result<T> {
    match result {
        Err(error) if may_use_native(&error) => native(),
        result => result,
    }
}

pub(crate) fn open(path: &Path) -> Result<gix::Repository> {
    let mut permissions = gix::open::Permissions::all();
    permissions.env.git_prefix = gix::sec::Permission::Deny;
    permissions.env.objects = gix::sec::Permission::Deny;
    let mut repository = gix::open_opts(
        path,
        gix::open::Options::default()
            .permissions(permissions)
            .strict_config(true)
            .lossy_config(true)
            .bail_if_untrusted(true)
            .config_overrides(["core.useReplaceRefs=false"]),
    )
    // Keep classification but do not expose configuration values or URLs in
    // public errors. Permission and data errors must not trigger another client.
    .map_err(|error| OpenError {
        unsupported: error.is_unsupported(),
    })?;
    // Gix currently opens reftable repositories with a files ref-store. Detect
    // the format before interpreting the placeholder files as real refs.
    if repository
        .config_snapshot()
        .string("extensions.refStorage")
        .is_some_and(|format| !format.eq_ignore_ascii_case(b"files"))
    {
        return Err(OpenError { unsupported: true }.into());
    }
    repository.clear_namespace();
    Ok(repository)
}

pub(crate) struct Status {
    pub modified: bool,
    pub untracked: bool,
    pub ignored: bool,
    pub conflicts: bool,
    pub head: Option<String>,
    pub branch: Option<String>,
}

pub(crate) fn status(path: &Path, refresh: bool) -> Result<Status> {
    let repo = open(path)?;
    let head = repo.head()?;
    let mut result = Status {
        modified: false,
        untracked: false,
        ignored: false,
        conflicts: false,
        head: head.id().map(|id| id.to_string()),
        branch: head
            .referent_name()
            .and_then(|name| name.as_bstr().strip_prefix(b"refs/heads/"))
            .map(|name| String::from_utf8_lossy(name).into_owned()),
    };
    let walk = repo
        .dirwalk_options()?
        .emit_ignored(Some(gix::dir::walk::EmissionMode::Matching))
        .emit_collapsed(Some(
            gix::dir::walk::CollapsedEntriesEmissionMode::OnStatusMismatch,
        ))
        .emit_empty_directories(true);
    let mut iter = repo
        .status(gix::progress::Discard)?
        .index_worktree_options_mut(|options| {
            options.dirwalk_options = Some(walk);
            options.thread_limit = Some(1);
        })
        .untracked_files(gix::status::UntrackedFiles::Collapsed)
        .index_worktree_rewrites(None)
        .tree_index_track_renames(gix::status::tree_index::TrackRenames::Disabled)
        .index_worktree_submodules(gix::status::Submodule::Given {
            ignore: gix::submodule::config::Ignore::None,
            check_dirty: false,
        })
        .into_iter(None)?;
    for item in iter.by_ref() {
        match item? {
            gix::status::Item::TreeIndex(_) => result.modified = true,
            gix::status::Item::IndexWorktree(item) => {
                if let gix::status::index_worktree::Item::DirectoryContents { entry, .. } = &item {
                    match entry.status {
                        gix::dir::entry::Status::Untracked => {
                            // Git does not report empty untracked directories.
                            if entry.property != Some(gix::dir::entry::Property::EmptyDirectory) {
                                result.untracked = true;
                            }
                        }
                        gix::dir::entry::Status::Ignored(_) => result.ignored = true,
                        _ => (),
                    }
                } else if let Some(summary) = item.summary() {
                    result.modified = true;
                    result.conflicts |= matches!(
                        summary,
                        gix::status::index_worktree::iter::Summary::Conflict
                    );
                }
            }
        }
    }
    let mut outcome = iter.into_outcome().context("incomplete Gix status")?;
    if refresh {
        // Git treats stat-cache refresh as an optional write. Keep the result
        // even if a concurrent process holds the index lock.
        if let Some(write) = outcome.write_changes() {
            write.ok();
        }
    }
    Ok(result)
}

fn worktree(
    path: std::path::PathBuf,
    primary: bool,
    bare: bool,
    locked: bool,
    prunable: bool,
    head: (Option<String>, Option<String>),
) -> WorktreeReport {
    WorktreeReport {
        path,
        head: head.0,
        branch: head.1,
        primary,
        bare,
        locked,
        prunable,
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
    }
}

fn head_info(repo: &gix::Repository, name: &str) -> Result<(Option<String>, Option<String>)> {
    let mut reference = repo.find_reference(name)?;
    let mut branch = None;
    let mut visited = HashSet::new();
    while let gix::refs::TargetRef::Symbolic(name) = reference.target() {
        if !visited.insert(name.to_owned()) {
            bail!("symbolic HEAD cycle");
        }
        branch = name
            .as_bstr()
            .strip_prefix(b"refs/heads/")
            .map(|name| String::from_utf8_lossy(name).into_owned());
        let Some(next) = repo.try_find_reference(name)? else {
            return Ok((None, branch)); // Unborn branch.
        };
        reference = next;
    }
    Ok((reference.target().try_id().map(|id| id.to_string()), branch))
}

pub(crate) fn worktrees(path: &Path) -> Result<Vec<WorktreeReport>> {
    let repo = open(path)?;
    let main = repo.main_repo()?;
    let main_path = if main.is_bare() {
        main.common_dir()
    } else {
        main.workdir().context("missing main checkout")?
    };
    let head = if main.is_bare() {
        (None, None)
    } else {
        head_info(&main, "HEAD")?
    };
    let mut result = vec![worktree(
        main_path.to_path_buf(),
        true,
        main.is_bare(),
        false,
        false,
        head,
    )];
    let mut linked = Vec::new();
    for proxy in repo.worktrees()? {
        let path = proxy.base()?;
        let locked = proxy.is_locked();
        let prunable = proxy.is_prunable();
        // Listing only needs the private HEAD ref. Reading it through the
        // trusted main repository avoids loading each checkout's configuration
        // and matches Git's metadata-only worktree listing.
        let name = format!("worktrees/{}/HEAD", proxy.id()?.to_str()?);
        linked.push(worktree(
            path,
            false,
            false,
            locked,
            prunable,
            head_info(&main, &name)?,
        ));
    }
    let ignore_case = repo
        .config_snapshot()
        .boolean("core.ignoreCase")
        .unwrap_or(false);
    linked.sort_by(|a, b| {
        let a = a.path.as_os_str();
        let b = b.path.as_os_str();
        if ignore_case {
            a.as_encoded_bytes()
                .iter()
                .map(u8::to_ascii_lowercase)
                .cmp(b.as_encoded_bytes().iter().map(u8::to_ascii_lowercase))
        } else {
            a.cmp(b)
        }
    });
    result.extend(linked);
    Ok(result)
}

fn selected_config(config: &gix::config::File, submodules: bool) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    for section in config.sections() {
        let header = section.header();
        let name = header.name().to_str()?.to_ascii_lowercase();
        if !(if submodules {
            name == "submodule"
        } else {
            name == "remote" || name == "url"
        }) {
            continue;
        }
        let Some(subsection) = header.subsection_name() else {
            continue;
        };
        let subsection = subsection.to_str()?;
        let mut seen = HashSet::new();
        for key in section.value_names() {
            let key = key.to_ascii_lowercase();
            if !seen.insert(key.clone()) {
                continue;
            }
            let selected = match name.as_str() {
                "remote" => matches!(key.as_str(), "url" | "fetch" | "vcs"),
                "url" => key == "insteadof",
                "submodule" => key == "path",
                _ => false,
            };
            if !selected {
                continue;
            }
            for value in section.values(&key) {
                output.extend_from_slice(format!("{name}.{subsection}.{key}\n").as_bytes());
                output.extend_from_slice(value.as_ref());
                output.push(0);
            }
        }
    }
    Ok(output)
}

pub(crate) fn remote_config(path: &Path) -> Result<Vec<u8>> {
    let repo = open(path)?;
    selected_config(repo.config_snapshot().plumbing(), false)
}

pub(crate) fn submodule_config(path: &Path) -> Result<Vec<u8>> {
    let config = gix::config::File::from_bytes_no_includes(
        &fs::read(path.join(".gitmodules"))?,
        gix::config::file::Metadata::from(gix::config::Source::Local),
        Default::default(),
    )?;
    selected_config(&config, true)
}

pub(crate) fn reachable(
    path: &Path,
    heads: &HashSet<String>,
    tips: &HashSet<String>,
) -> Result<HashMap<String, bool>> {
    let repo = open(path)?;
    let cache = repo.commit_graph_if_enabled()?;
    let mut graph = repo.revision_graph(cache.as_ref());
    let tips: Vec<_> = tips
        .iter()
        .map(|tip| gix::hash::ObjectId::from_hex(tip.as_bytes()))
        .collect::<std::result::Result<_, _>>()?;
    if tips.is_empty() {
        bail!("missing reachability tips");
    }
    let mut result = HashMap::new();
    for head in heads {
        let id = gix::hash::ObjectId::from_hex(head.as_bytes())?;
        let bases = repo.merge_bases_many_with_graph(id, &tips, &mut graph)?;
        result.insert(
            head.clone(),
            bases.iter().any(|base| base.as_ref() == id.as_ref()),
        );
    }
    Ok(result)
}

pub(crate) fn raw_diff(path: &Path, old: &str, new: &str) -> Result<Vec<u8>> {
    let repo = open(path)?;
    let old = repo
        .find_commit(gix::hash::ObjectId::from_hex(old.as_bytes())?)?
        .tree()?;
    let new = repo
        .find_commit(gix::hash::ObjectId::from_hex(new.as_bytes())?)?
        .tree()?;
    tree_diff(&repo, &old, &new)
}

pub(crate) fn tree_diff(
    repo: &gix::Repository,
    old: &gix::Tree<'_>,
    new: &gix::Tree<'_>,
) -> Result<Vec<u8>> {
    use gix::diff::tree::recorder::Change;
    let mut recorder = gix::diff::tree::Recorder::default();
    gix::diff::tree(
        gix::objs::TreeRefIter::from_bytes(&old.data, repo.object_hash()),
        gix::objs::TreeRefIter::from_bytes(&new.data, repo.object_hash()),
        &mut gix::diff::tree::State::default(),
        &repo.objects,
        &mut recorder,
    )?;
    let zero = gix::hash::ObjectId::null(repo.object_hash()).to_string();
    let mut output = Vec::new();
    for record in recorder.records {
        let (old_mode, old_id, new_mode, new_id, status, path) = match record {
            Change::Addition {
                entry_mode,
                oid,
                path,
                ..
            } if !entry_mode.is_tree() => (
                "000000".into(),
                zero.clone(),
                format!("{entry_mode:o}"),
                oid.to_string(),
                'A',
                path,
            ),
            Change::Deletion {
                entry_mode,
                oid,
                path,
                ..
            } if !entry_mode.is_tree() => (
                format!("{entry_mode:o}"),
                oid.to_string(),
                "000000".into(),
                zero.clone(),
                'D',
                path,
            ),
            Change::Modification {
                previous_entry_mode,
                previous_oid,
                entry_mode,
                oid,
                path,
            } => {
                if previous_entry_mode.is_tree() && entry_mode.is_tree() {
                    continue;
                }
                if previous_entry_mode.is_tree() {
                    (
                        "000000".into(),
                        zero.clone(),
                        format!("{entry_mode:o}"),
                        oid.to_string(),
                        'A',
                        path,
                    )
                } else if entry_mode.is_tree() {
                    (
                        format!("{previous_entry_mode:o}"),
                        previous_oid.to_string(),
                        "000000".into(),
                        zero.clone(),
                        'D',
                        path,
                    )
                } else {
                    let status = if previous_entry_mode.kind() as u16 & 0o170000
                        == entry_mode.kind() as u16 & 0o170000
                    {
                        'M'
                    } else {
                        'T'
                    };
                    (
                        format!("{previous_entry_mode:o}"),
                        previous_oid.to_string(),
                        format!("{entry_mode:o}"),
                        oid.to_string(),
                        status,
                        path,
                    )
                }
            }
            _ => continue,
        };
        output.extend_from_slice(
            format!(":{old_mode} {new_mode} {old_id} {new_id} {status}\0").as_bytes(),
        );
        output.extend_from_slice(path.as_ref());
        output.push(0);
    }
    Ok(output)
}
