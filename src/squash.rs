//! Recognize squashes and copied commit series using exact file changes.
//! All deep verification writes are confined to a temporary directory.
use crate::git;
use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const MARKER: &[u8] = b"\0\0COMMIT:";
const DIFF_FLAGS: &[&str] = &[
    "--no-renames",
    "--no-ext-diff",
    "--no-textconv",
    "--no-color",
    "--no-relative",
    "--ignore-submodules=none",
    "--submodule=short",
    "--diff-algorithm=myers",
    "--no-indent-heuristic",
];
const PATCH_FLAGS: &[&str] = &[
    "--patch",
    "--full-index",
    "--binary",
    "--unified=3",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

struct Delta {
    head: String,
    base: String,
    tips: Vec<usize>,
}

struct HistoryCommit {
    oid: String,
    parents: Vec<String>,
    signature: Vec<u8>,
}

// Record which mainlines contain each exact change. A series must match one
// whole mainline, not a mixture of unrelated remotes' default branches.
#[derive(Default)]
struct ExactHistory {
    signatures: HashMap<Vec<u8>, Vec<usize>>,
    commits: HashMap<String, Vec<usize>>,
}

/// Compare combined changes and individual commits using whole-file IDs. With `deep`, use
/// whitespace-insensitive patch IDs only to find candidates, then apply each
/// actual patch without whitespace relaxation and compare the complete tree.
pub(crate) fn detect(
    path: &Path,
    common_dir: &Path,
    heads: &HashSet<String>,
    mainline_tips: &HashSet<String>,
    deep: bool,
) -> Result<HashSet<String>> {
    let mut found = HashSet::new();
    if heads.is_empty() || mainline_tips.is_empty() {
        return Ok(found);
    }
    let mut tips: Vec<&str> = mainline_tips.iter().map(String::as_str).collect();
    tips.sort_unstable();
    let mut ordered_heads: Vec<&str> = heads.iter().map(String::as_str).collect();
    ordered_heads.sort_unstable();

    // A different eligible mainline can have a different merge base. Reuse the
    // same head/base delta whenever several tips share it.
    let mut deltas: Vec<Delta> = Vec::new();
    let mut seen: HashMap<(String, String), usize> = HashMap::new();
    for head in ordered_heads {
        for (tip_index, tip) in tips.iter().enumerate() {
            let Some(raw) = git::run_optional(path, &["merge-base", "--all", tip, head])? else {
                continue; // Unrelated histories cannot establish this proof.
            };
            let bases = parse_oids(&raw)?;
            if bases.len() != 1 {
                continue; // Do not pick one arbitrarily in criss-cross history.
            }
            let base = &bases[0];
            let key = (head.to_owned(), base.clone());
            if let Some(index) = seen.get(&key) {
                deltas[*index].tips.push(tip_index);
            } else {
                seen.insert(key, deltas.len());
                deltas.push(Delta {
                    head: head.to_owned(),
                    base: base.clone(),
                    tips: vec![tip_index],
                });
            }
        }
    }
    if deltas.is_empty() {
        return Ok(found);
    }

    let exact = exact_history(path, &tips)?;
    let mut unresolved = Vec::new();
    for delta in deltas {
        let mut args = vec!["diff-tree", "-r", "--raw", "-z", "--no-abbrev"];
        args.extend_from_slice(DIFF_FLAGS);
        args.extend([delta.base.as_str(), delta.head.as_str(), "--"]);
        let signature = raw_signature(&git::run(path, &args)?)?;
        if signature.is_empty() {
            // Empty net changes do not by themselves establish integration.
            continue;
        }
        if exact
            .signatures
            .get(&signature)
            .is_some_and(|membership| same_mainline(membership, &delta.tips))
        {
            found.insert(delta.head);
        } else {
            unresolved.push(delta);
        }
    }
    let remaining: HashSet<String> = unresolved
        .iter()
        .filter(|delta| !found.contains(&delta.head))
        .map(|delta| delta.head.clone())
        .collect();
    found.extend(exact_series(path, &remaining, &tips, &exact)?);
    if !deep || unresolved.iter().all(|delta| found.contains(&delta.head)) {
        return Ok(found);
    }

    let candidates = patch_history(path, &tips)?;
    let verifier = PrivateIndex::new(path, common_dir)?;
    for delta in unresolved {
        if found.contains(&delta.head) {
            continue;
        }
        let mut args = vec!["-c", "core.quotePath=true", "diff"];
        args.extend_from_slice(DIFF_FLAGS);
        args.extend_from_slice(PATCH_FLAGS);
        args.extend([delta.base.as_str(), delta.head.as_str(), "--"]);
        let patch = git::run(path, &args)?;
        let ids = git::run_with_input(path, &["patch-id", "--stable"], &patch)?;
        let mut lines = ids
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty());
        let Some(line) = lines.next() else { continue };
        if lines.next().is_some() {
            bail!("unexpected multiple branch patch IDs");
        }
        let (id, _) = parse_patch_id(line)?;
        if let Some(commits) = candidates.get(&id) {
            for commit in commits {
                if !exact
                    .commits
                    .get(commit)
                    .is_some_and(|membership| same_mainline(membership, &delta.tips))
                {
                    continue;
                }
                if verifier.matches(commit, &patch)? {
                    found.insert(delta.head.clone());
                    break;
                }
            }
        }
    }
    Ok(found)
}

fn exact_history(path: &Path, tips: &[&str]) -> Result<ExactHistory> {
    let mut index = ExactHistory::default();
    for (tip_index, tip) in tips.iter().enumerate() {
        for commit in history(path, &[*tip], None)? {
            index.commits.entry(commit.oid).or_default().push(tip_index);
            if !commit.signature.is_empty() {
                let membership = index.signatures.entry(commit.signature).or_default();
                if membership.last() != Some(&tip_index) {
                    membership.push(tip_index);
                }
            }
        }
    }
    Ok(index)
}

fn same_mainline(membership: &[usize], selected: &[usize]) -> bool {
    selected.iter().any(|tip| membership.contains(tip))
}

fn exact_series(
    path: &Path,
    heads: &HashSet<String>,
    tips: &[&str],
    exact: &ExactHistory,
) -> Result<HashSet<String>> {
    let mut found = HashSet::new();
    for (tip_index, tip) in tips.iter().enumerate() {
        let mut remaining: Vec<&str> = heads
            .iter()
            .filter(|head| !found.contains(*head))
            .map(String::as_str)
            .collect();
        if remaining.is_empty() {
            break;
        }
        remaining.sort_unstable();
        // One batch per mainline reads each shared branch commit once. Exclude
        // only this tip: excluding every remote would hide unmatched commits.
        let graph: HashMap<String, HistoryCommit> = history(path, &remaining, Some(tip))?
            .into_iter()
            .map(|commit| (commit.oid.clone(), commit))
            .collect();
        let mut memo = HashMap::new();
        for head in remaining {
            if graph.contains_key(head) && series_matches(head, tip_index, &graph, exact, &mut memo)
            {
                found.insert(head.to_owned());
            }
        }
    }
    Ok(found)
}

fn series_matches(
    head: &str,
    tip_index: usize,
    graph: &HashMap<String, HistoryCommit>,
    exact: &ExactHistory,
    memo: &mut HashMap<String, bool>,
) -> bool {
    let mut current = head;
    let mut chain = Vec::new();
    let matched = loop {
        if let Some(matched) = memo.get(current) {
            break *matched;
        }
        let Some(commit) = graph.get(current) else {
            // Complete Git traversal excluded this ancestor because it is on
            // the selected mainline. A missing starting HEAD is rejected above.
            break true;
        };
        chain.push(current);
        if chain.len() > graph.len()
            || commit.parents.len() != 1
            || commit.signature.is_empty()
            || !exact
                .signatures
                .get(&commit.signature)
                .is_some_and(|membership| membership.contains(&tip_index))
        {
            // Merge resolutions, empty-only evidence, and partial matches do
            // not establish a fully integrated linear series.
            break false;
        }
        current = &commit.parents[0];
    };
    for oid in chain {
        memo.insert(oid.to_owned(), matched);
    }
    matched
}

fn history(path: &Path, heads: &[&str], excluded_tip: Option<&str>) -> Result<Vec<HistoryCommit>> {
    let mut args = vec![
        "log",
        "--no-notes",
        "--no-decorate",
        "--no-show-signature",
        "--diff-merges=off",
        "--format=%x00%x00COMMIT:%H %P%x00",
        "--raw",
        "-r",
        "-z",
        "--no-abbrev",
    ];
    args.extend_from_slice(DIFF_FLAGS);
    if excluded_tip.is_none() {
        args.push("--no-merges");
    }
    args.extend_from_slice(heads);
    if let Some(tip) = excluded_tip {
        args.extend(["--not", tip]);
    }
    args.push("--");
    let output = git::run(path, &args)?;
    parse_history(&output)
}

fn parse_history(output: &[u8]) -> Result<Vec<HistoryCommit>> {
    let mut commits = Vec::new();
    let mut seen = HashSet::new();
    let mut rest = output;
    while !rest.is_empty() {
        if !rest.starts_with(MARKER) {
            bail!("invalid exact-history record delimiter");
        }
        rest = &rest[MARKER.len()..];
        let oid_end = rest
            .iter()
            .position(|byte| *byte == 0)
            .context("missing exact-history commit ID delimiter")?;
        let ids = parse_oids(&rest[..oid_end])?;
        let (oid, parents) = ids.split_first().context("missing history commit ID")?;
        if !seen.insert(oid.clone()) {
            bail!("duplicate history commit ID");
        }
        rest = &rest[oid_end + 1..];
        let next = rest
            .windows(MARKER.len())
            .position(|window| window == MARKER)
            .unwrap_or(rest.len());
        let signature = raw_signature(&rest[..next])?;
        commits.push(HistoryCommit {
            oid: oid.clone(),
            parents: parents.to_vec(),
            signature,
        });
        rest = &rest[next..];
    }
    Ok(commits)
}

// -z and --no-renames yield pairs of raw headers and exact path bytes. Keep
// modes, old/new full object IDs, status and path; only record delimiters vary.
fn raw_signature(raw: &[u8]) -> Result<Vec<u8>> {
    let start = raw
        .iter()
        .position(|byte| !matches!(byte, b'\0' | b'\n'))
        .unwrap_or(raw.len());
    let raw = &raw[start..];
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let mut fields: Vec<&[u8]> = raw.split(|byte| *byte == 0).collect();
    if fields.pop() != Some(b"".as_slice()) || !fields.len().is_multiple_of(2) {
        bail!("invalid raw Git diff record");
    }
    let mut records = Vec::new();
    for pair in fields.chunks_exact(2) {
        let header = pair[0];
        let path = pair[1];
        let parts: Vec<&[u8]> = header.split(|byte| *byte == b' ').collect();
        if parts.len() != 5
            || !parts[0].starts_with(b":")
            || path.is_empty()
            || parts[0].len() != 7
            || parts[1].len() != 6
            || !parts[0][1..]
                .iter()
                .chain(parts[1])
                .all(|byte| matches!(byte, b'0'..=b'7'))
            || parts[4].len() != 1
            || !b"AMDTUXB".contains(&parts[4][0])
        {
            bail!("invalid raw Git diff header");
        }
        validate_oid(parts[2])?;
        validate_oid(parts[3])?;
        records.push((path, header));
    }
    records.sort_unstable();
    let mut signature = Vec::with_capacity(raw.len());
    for (path, header) in records {
        signature.extend_from_slice(header);
        signature.push(0);
        signature.extend_from_slice(path);
        signature.push(0);
    }
    Ok(signature)
}

fn patch_history(path: &Path, tips: &[&str]) -> Result<HashMap<String, Vec<String>>> {
    let mut args = vec![
        "-c",
        "core.quotePath=true",
        "log",
        "--no-merges",
        "--no-notes",
        "--no-decorate",
        "--no-show-signature",
        "--format=commit %H",
    ];
    args.extend_from_slice(DIFF_FLAGS);
    args.extend_from_slice(PATCH_FLAGS);
    args.extend_from_slice(tips);
    args.push("--");
    // Stream large history patches directly between Git processes. Retain only
    // the compact patch-ID/commit pairs, never all historical patch contents.
    let mut history = git::command(path, &args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("could not start mainline patch history")?;
    let pipe = history
        .stdout
        .take()
        .context("patch history stdout unavailable")?;
    let output = match git::command(path, &["patch-id", "--stable"])
        .stdin(Stdio::from(pipe))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            let _ = history.kill();
            let _ = history.wait();
            return Err(error).context("could not compute mainline patch IDs");
        }
    };
    let status = history.wait().context("could not wait for patch history")?;
    if !status.success() || !output.status.success() {
        bail!("could not compute mainline patch IDs");
    }
    let mut index: HashMap<String, Vec<String>> = HashMap::new();
    for line in output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let (id, commit) = parse_patch_id(line)?;
        index.entry(id).or_default().push(commit);
    }
    Ok(index)
}

fn parse_patch_id(line: &[u8]) -> Result<(String, String)> {
    let words: Vec<&[u8]> = line
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|word| !word.is_empty())
        .collect();
    if words.len() != 2 {
        bail!("invalid Git patch ID record");
    }
    validate_oid(words[0])?;
    validate_oid(words[1])?;
    Ok((
        String::from_utf8(words[0].to_vec())?,
        String::from_utf8(words[1].to_vec())?,
    ))
}

fn parse_oids(output: &[u8]) -> Result<Vec<String>> {
    output
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|word| !word.is_empty())
        .map(|word| {
            validate_oid(word)?;
            Ok(String::from_utf8(word.to_vec())?)
        })
        .collect()
}

fn validate_oid(oid: &[u8]) -> Result<()> {
    if !matches!(oid.len(), 40 | 64) || !oid.iter().all(u8::is_ascii_hexdigit) {
        bail!("invalid Git object ID");
    }
    Ok(())
}

struct PrivateIndex<'a> {
    path: &'a Path,
    _directory: tempfile::TempDir,
    index: PathBuf,
    objects: PathBuf,
    hooks: PathBuf,
    alternate: String,
}

impl<'a> PrivateIndex<'a> {
    fn new(path: &'a Path, common_dir: &Path) -> Result<Self> {
        let directory =
            tempfile::tempdir().context("could not create squash verification directory")?;
        let objects = directory.path().join("objects");
        std::fs::create_dir(&objects).context("could not create private Git objects directory")?;
        let hooks = directory.path().join("hooks");
        std::fs::create_dir(&hooks).context("could not create private Git hooks directory")?;
        let index = directory.path().join("index");
        let alternate = quote_alternate(&common_dir.join("objects"));
        Ok(Self {
            path,
            _directory: directory,
            index,
            objects,
            hooks,
            alternate,
        })
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = git::command(
            self.path,
            &[
                "-c",
                "core.splitIndex=false",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.sparseCheckout=false",
                "-c",
                "index.sparse=false",
                "-c",
                "apply.ignoreWhitespace=false",
                "-c",
                "apply.whitespace=nowarn",
            ],
        );
        let mut hooks_config = std::ffi::OsString::from("core.hooksPath=");
        hooks_config.push(&self.hooks);
        command
            .arg("-c")
            .arg(hooks_config)
            .args(args)
            .env("GIT_INDEX_FILE", &self.index)
            .env("GIT_OBJECT_DIRECTORY", &self.objects)
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &self.alternate);
        command
    }

    fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        let output = self
            .command(args)
            .stderr(Stdio::null())
            .output()
            .context("could not start private Git verification")?;
        private_output(output)
    }

    fn matches(&self, commit: &str, patch: &[u8]) -> Result<bool> {
        let parents = parse_oids(&git::run(
            self.path,
            &["rev-list", "--parents", "-n", "1", commit],
        )?)?;
        if parents.len() != 2 {
            return Ok(false); // Root or merge commits have no single parent delta.
        }
        self.run(&["read-tree", "--reset", &parents[1]])?;
        let mut child = self
            .command(&["apply", "--cached", "--binary", "--whitespace=nowarn", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("could not start private patch verification")?;
        let mut stdin = child
            .stdin
            .take()
            .context("private Git stdin unavailable")?;
        let write_result = stdin.write_all(patch);
        drop(stdin);
        let status = child
            .wait()
            .context("could not wait for private patch verification")?;
        if !status.success() {
            return Ok(false); // A patch-ID collision or non-applicable edit proves nothing.
        }
        write_result.context("could not write private verification patch")?;
        let actual = self.run(&["write-tree"])?;
        let target = git::run(
            self.path,
            &["rev-parse", "--verify", &format!("{commit}^{{tree}}")],
        )?;
        Ok(git::trim_newline(&actual) == git::trim_newline(&target))
    }
}

fn private_output(output: Output) -> Result<Vec<u8>> {
    if !output.status.success() {
        bail!("private Git verification failed");
    }
    Ok(output.stdout)
}

// Git accepts C-quoted entries in GIT_ALTERNATE_OBJECT_DIRECTORIES. Quoting
// every unusual byte also protects paths containing the list separator.
fn quote_alternate(path: &Path) -> String {
    let mut quoted = String::from("\"");
    for byte in path.as_os_str().as_encoded_bytes() {
        match byte {
            b'"' => quoted.push_str("\\\""),
            b'\\' => quoted.push_str("\\\\"),
            b' '..=b'~' => quoted.push(char::from(*byte)),
            _ => quoted.push_str(&format!("\\{byte:03o}")),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::raw_signature;

    #[test]
    fn log_record_delimiters_do_not_change_exact_diff_signature() {
        let header = format!(":100644 100644 {} {} M", "1".repeat(40), "2".repeat(40));
        let mut diff = header.into_bytes();
        // Preserve path bytes including leading newlines and non-UTF-8 bytes.
        diff.extend_from_slice(b"\0\nodd\xffpath\0");
        let mut log_body = b"\0\n".to_vec();
        log_body.extend_from_slice(&diff);
        assert_eq!(raw_signature(&log_body).unwrap(), diff);
        assert_eq!(raw_signature(&diff).unwrap(), diff);
        assert!(raw_signature(b"\0\n\0").unwrap().is_empty());
    }

    #[test]
    fn exact_signatures_sort_paths_and_reject_truncated_records() {
        let header = format!(":100644 100644 {} {} M", "1".repeat(40), "2".repeat(40));
        let first = format!("{header}\0a\0").into_bytes();
        let second = format!("{header}\0z\0").into_bytes();
        let mut reversed = second.clone();
        reversed.extend_from_slice(&first);
        let mut ordered = first;
        ordered.extend_from_slice(&second);
        assert_eq!(raw_signature(&reversed).unwrap(), ordered);
        assert!(raw_signature(&second[..second.len() - 1]).is_err());
    }
}
