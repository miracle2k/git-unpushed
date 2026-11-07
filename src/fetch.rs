//! Fetch eligible remote branches without starting a Git client.
use crate::{git, inspect::Mapping, metadata};
use anyhow::{Result, bail};
use gix::{bstr::ByteSlice, refs::transaction as refs};
use std::{collections::HashSet, path::Path, sync::atomic::AtomicBool};

pub(crate) fn run(path: &Path, name: &str, mappings: &[Mapping]) -> Result<()> {
    metadata::or_native(with_gix(path, name, mappings), || {
        git::run(
            path,
            &[
                "fetch",
                "--prune",
                "--no-tags",
                "--no-prune-tags",
                "--no-recurse-submodules",
                "--",
                name,
            ],
        )?;
        Ok(())
    })
}

fn with_gix(path: &Path, name: &str, mappings: &[Mapping]) -> Result<()> {
    let mut repo = metadata::open(path)?;
    repo.config_snapshot_mut()
        .set_raw_value("gitoxide.credentials.terminalPrompt", "false")?;
    let remote = repo
        .find_remote(name)?
        .with_fetch_tags(gix::remote::fetch::Tags::None);
    let prepared = remote
        .connect(gix::remote::Direction::Fetch)?
        .prepare_fetch(gix::progress::Discard, Default::default())?;
    if prepared.ref_map().is_missing_required_mapping() {
        bail!("remote does not provide the requested branches");
    }

    // Gix exposes the server's ref advertisement and ref transactions, but
    // does not yet provide fetch --prune. Only our validated branch mappings
    // qualify; tags, local branches, and symbolic remote HEADs stay intact.
    // Prune before receiving so a branch can be replaced by a directory of
    // branches (or the reverse), just as with native Git.
    let advertised: HashSet<_> = prepared
        .ref_map()
        .remote_refs
        .iter()
        .map(|reference| reference.unpack().0)
        .collect();
    let mut edits = Vec::new();
    for reference in repo.references()?.all()? {
        let reference = reference?;
        if reference.target().try_name().is_some() {
            continue;
        }
        let name = reference.name().as_bstr().to_str()?;
        let sources: Vec<_> = mappings
            .iter()
            .filter_map(|mapping| mapping.source_for(name))
            .collect();
        if !sources.is_empty()
            && sources
                .iter()
                .all(|source| !advertised.contains(source.as_bytes().as_bstr()))
        {
            let reference = reference.detach();
            edits.push(refs::RefEdit {
                name: reference.name,
                deref: false,
                change: refs::Change::Delete {
                    expected: refs::PreviousValue::MustExistAndMatch(reference.target),
                    log: refs::RefLog::AndReference,
                },
            });
        }
    }
    if !edits.is_empty() {
        repo.edit_references_as(edits, None)?;
    }
    let outcome = prepared.receive(gix::progress::Discard, &AtomicBool::new(false))?;
    let updates = match &outcome.status {
        gix::remote::fetch::Status::NoPackReceived { update_refs, .. }
        | gix::remote::fetch::Status::Change { update_refs, .. } => update_refs,
    };
    use gix::remote::fetch::refs::update::Mode;
    if updates.updates.iter().any(|update| {
        !matches!(
            update.mode,
            Mode::NoChangeNeeded | Mode::FastForward | Mode::Forced | Mode::New
        )
    }) {
        bail!("remote tracking reference update was rejected");
    }
    Ok(())
}
