use serde_json::Value;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::TempDir;

// Every command uses disposable repositories and an isolated Git configuration.
// Fixtures push to a local bare repository, then replace its URL before scans so
// the scanner can exercise offline, nonlocal remote policy without any network.
fn isolate_git(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", "/dev/null")
        .env("GIT_CONFIG_KEY_1", "commit.gpgSign")
        .env("GIT_CONFIG_VALUE_1", "false");
}

fn git_output(path: &Path, args: &[&OsStr]) -> Output {
    let mut command = Command::new("git");
    isolate_git(&mut command);
    command.arg("-C").arg(path).args(args);
    command.output().expect("execute fixture Git")
}

fn git(path: &Path, args: &[&str]) -> String {
    let args: Vec<_> = args.iter().map(OsStr::new).collect();
    let output = git_output(path, &args);
    assert!(
        output.status.success(),
        "git {args:?} in {} failed: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("fixture Git output is UTF-8")
}

fn cli(root: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_git-unpushed"));
    isolate_git(&mut command);
    command.current_dir(root).args(args);
    command.output().expect("execute git-unpushed")
}

fn json(root: &Path, args: &[&str]) -> Value {
    let mut json_args = args.to_vec();
    json_args.push("--json");
    let output = cli(root, &json_args);
    assert!(
        output.status.success(),
        "scanner failed: {}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice(&output.stdout).expect("valid JSON report")
}

fn report_even_if_error(root: &Path, args: &[&str]) -> (Output, Value) {
    let mut json_args = args.to_vec();
    json_args.push("--json");
    let output = cli(root, &json_args);
    let report = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid report: {error}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output, report)
}

fn repositories(report: &Value) -> &[Value] {
    report["repositories"]
        .as_array()
        .expect("repositories array")
}

fn only_repository(report: &Value) -> &Value {
    assert_eq!(repositories(report).len(), 1, "{report:#}");
    &repositories(report)[0]
}

fn worktree<'a>(repository: &'a Value, path: &Path) -> &'a Value {
    let wanted = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    repository["worktrees"]
        .as_array()
        .expect("worktrees array")
        .iter()
        .find(|tree| {
            let actual = PathBuf::from(tree["path"].as_str().expect("worktree path"));
            fs::canonicalize(&actual).unwrap_or(actual) == wanted
        })
        .unwrap_or_else(|| panic!("missing {} in {repository:#}", path.display()))
}

struct Fixture {
    _temp: TempDir,
    scan: PathBuf,
    repo: PathBuf,
    remote: PathBuf,
    outside: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("temporary fixture directory");
        let fixture_root = fs::canonicalize(temp.path()).unwrap();
        let scan = fixture_root.join("scan");
        let repo = scan.join("alpha");
        let remote = fixture_root.join("remote.git");
        let outside = fixture_root.join("outside");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&outside).unwrap();
        git(
            temp.path(),
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                remote.to_str().unwrap(),
            ],
        );
        git(&repo, &["init", "--initial-branch=main"]);
        git(&repo, &["config", "user.name", "CLI test"]);
        git(&repo, &["config", "user.email", "cli-test@example.invalid"]);
        fs::write(repo.join("tracked.txt"), "initial\n").unwrap();
        fs::write(repo.join(".gitignore"), "ignored.txt\nignored/\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "Initial fixture"]);
        git(
            &repo,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&repo, &["push", "--set-upstream", "origin", "main"]);
        git(
            &repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        Self {
            _temp: temp,
            scan,
            repo,
            remote,
            outside,
        }
    }

    fn nonlocal_remote(&self) {
        git(
            &self.repo,
            &[
                "remote",
                "set-url",
                "origin",
                "https://example.invalid/repo.git",
            ],
        );
    }

    fn tree(&self, branch: &str) -> PathBuf {
        self.tree_at(branch, self.outside.join(branch))
    }

    fn tree_at(&self, branch: &str, path: PathBuf) -> PathBuf {
        git(
            &self.repo,
            &["worktree", "add", "-b", branch, path.to_str().unwrap()],
        );
        path
    }

    fn commit(&self, path: &Path, text: &str) {
        fs::write(path.join("tracked.txt"), text).unwrap();
        git(path, &["add", "tracked.txt"]);
        git(path, &["commit", "-m", text]);
    }
}

#[test]
fn reports_multiple_repositories_and_external_worktrees_once() {
    let fixture = Fixture::new();
    let external = fixture.tree("external");
    let nested = fixture.tree_at("nested", fixture.repo.join("nested"));
    fixture.nonlocal_remote();
    let second = fixture.scan.join("beta");
    fs::create_dir(&second).unwrap();
    git(&second, &["init", "--initial-branch=main"]);

    let report = json(&fixture.scan, &[]);
    assert_eq!(repositories(&report).len(), 2);
    let alpha = repositories(&report)
        .iter()
        .find(|repo| repo["worktrees"].as_array().unwrap().len() == 3)
        .unwrap();
    assert_eq!(worktree(alpha, &external)["removable"], true);
    assert_eq!(worktree(alpha, &nested)["removable"], true);

    let root = fixture.repo.to_str().unwrap();
    let duplicate = external.to_str().unwrap();
    let deduplicated = json(&fixture.scan, &[root, duplicate, "--directory", root]);
    assert_eq!(repositories(&deduplicated).len(), 1);
    assert_eq!(
        only_repository(&deduplicated)["worktrees"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn clean_repositories_are_hidden_unless_all_is_requested() {
    let fixture = Fixture::new();
    fixture.nonlocal_remote();
    let hidden = cli(&fixture.scan, &["--color", "never"]);
    assert!(hidden.status.success());
    assert!(!String::from_utf8_lossy(&hidden.stdout).contains("alpha:"));
    let all = cli(&fixture.scan, &["--all", "--color", "never"]);
    assert!(all.status.success());
    assert!(String::from_utf8_lossy(&all.stdout).contains("alpha"));
    assert!(
        !all.stdout.contains(&0x1b),
        "piped plain output must contain no ANSI escapes"
    );
}

#[test]
fn disabling_streaming_keeps_sorted_plain_output_when_roots_are_reversed() {
    let fixture = Fixture::new();
    fixture.nonlocal_remote();
    let second = fixture.scan.join("zeta");
    git(
        &fixture.scan,
        &[
            "clone",
            fixture.remote.to_str().unwrap(),
            second.to_str().unwrap(),
        ],
    );
    git(
        &second,
        &[
            "remote",
            "set-url",
            "origin",
            "https://example.invalid/zeta.git",
        ],
    );
    let output = cli(
        &fixture.scan,
        &[
            second.to_str().unwrap(),
            fixture.repo.to_str().unwrap(),
            "--all",
            "--stream=never",
            "--color=never",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.find("alpha:").unwrap() < text.find("zeta:").unwrap());
    assert!(!text.contains('\x1b'));
    assert!(!output.stderr.contains(&0x1b));
    assert!(!output.stderr.contains(&b'\r'));
}

#[test]
fn explicit_streaming_and_progress_keep_json_stdout_a_single_valid_document() {
    let fixture = Fixture::new();
    fixture.tree("done");
    fixture.nonlocal_remote();
    let output = cli(
        &fixture.scan,
        &["--json", "--stream", "--progress=always", "--color=always"],
    );
    assert!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).expect("one complete JSON report");
    assert_eq!(repositories(&report).len(), 1);
    assert_eq!(
        only_repository(&report)["worktrees"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(!output.stdout.contains(&0x1b));
    assert!(!output.stdout.contains(&b'\r'));
}

#[cfg(unix)]
#[test]
fn index_refresh_avoids_repeated_clean_filters_and_preserves_staged_contents() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, SystemTime};

    let fixture = Fixture::new();
    let filter = fixture.repo.join(".git/test-clean-filter");
    let counter = fixture.repo.join(".git/filter-invocations");
    fs::write(
        &filter,
        "#!/bin/sh\nprintf 'clean\\n' >> \"${0%/*}/filter-invocations\"\ncat\n",
    )
    .unwrap();
    fs::set_permissions(&filter, fs::Permissions::from_mode(0o755)).unwrap();
    let filter_command = format!("'{}'", filter.to_str().unwrap().replace('\'', "'\\''"));
    git(
        &fixture.repo,
        &["config", "filter.test-counter.clean", &filter_command],
    );
    git(
        &fixture.repo,
        &["config", "filter.test-counter.required", "true"],
    );
    fs::write(
        fixture.repo.join(".gitattributes"),
        "tracked.txt filter=test-counter\n",
    )
    .unwrap();
    git(&fixture.repo, &["add", ".gitattributes", "tracked.txt"]);
    git(
        &fixture.repo,
        &["commit", "-m", "Install counting clean filter"],
    );
    git(&fixture.repo, &["push", "origin", "main"]);
    fixture.nonlocal_remote();

    // Preserve distinct staged and unstaged versions while allowing stat-only
    // metadata refreshes for a separate, unchanged filtered file.
    let staged = fixture.repo.join("staged.txt");
    fs::write(&staged, "staged version\n").unwrap();
    git(&fixture.repo, &["add", "staged.txt"]);
    fs::write(&staged, "working version\n").unwrap();
    let staged_tree = git(&fixture.repo, &["write-tree"]);
    let index = fixture.repo.join(".git/index");
    let tracked = fs::File::options()
        .write(true)
        .open(fixture.repo.join("tracked.txt"))
        .unwrap();
    let stale_time = SystemTime::now() - Duration::from_secs(120);
    tracked
        .set_times(fs::FileTimes::new().set_modified(stale_time))
        .unwrap();
    fs::write(&counter, "").unwrap();
    let count_filters = || fs::read_to_string(&counter).unwrap().lines().count();
    let initial_index = fs::read(&index).unwrap();

    let first = cli(&fixture.scan, &["--progress=never"]);
    assert!(first.status.success(), "{first:?}");
    let first_count = count_filters();
    assert!(
        first_count > 0,
        "stale stat information must run the clean filter"
    );
    assert_ne!(
        fs::read(&index).unwrap(),
        initial_index,
        "default scanning should persist refreshed index stat information"
    );
    let second = cli(&fixture.scan, &["--progress=never"]);
    assert!(second.status.success(), "{second:?}");
    assert_eq!(
        count_filters(),
        first_count,
        "a repeat scan should reuse refreshed stat information"
    );
    assert_eq!(git(&fixture.repo, &["write-tree"]), staged_tree);

    // A different old timestamp is deterministic and avoids Git's special
    // handling of future timestamps and racily clean index entries.
    tracked
        .set_times(fs::FileTimes::new().set_modified(stale_time - Duration::from_secs(120)))
        .unwrap();
    fs::write(&counter, "").unwrap();
    let untouched_index = fs::read(&index).unwrap();
    let first = cli(&fixture.scan, &["--no-index-refresh", "--progress=never"]);
    assert!(first.status.success(), "{first:?}");
    let first_count = count_filters();
    assert!(first_count > 0);
    let second = cli(&fixture.scan, &["--no-index-refresh", "--progress=never"]);
    assert!(second.status.success(), "{second:?}");
    assert!(
        count_filters() > first_count,
        "opting out leaves stale stat information and repeats filter work"
    );
    assert_eq!(
        fs::read(&index).unwrap(),
        untouched_index,
        "opting out must leave the original index byte-for-byte unchanged"
    );
    assert_eq!(git(&fixture.repo, &["write-tree"]), staged_tree);
    assert_eq!(
        git(&fixture.repo, &["show", ":staged.txt"]),
        "staged version\n"
    );
    assert_eq!(fs::read_to_string(&staged).unwrap(), "working version\n");
}

#[cfg(unix)]
mod live_output {
    use super::*;
    use std::io::{BufRead, BufReader, Read};
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Child, ExitStatus, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    // Always unblock fixture Git, including when an assertion panics. Only this
    // test's child receives the wrapper PATH; parallel tests use ordinary Git.
    struct GatedChild {
        child: Child,
        release: PathBuf,
    }

    impl GatedChild {
        fn release_and_wait(&mut self) -> ExitStatus {
            fs::write(&self.release, []).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    return status;
                }
                assert!(
                    Instant::now() < deadline,
                    "scanner did not exit after releasing Git"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for GatedChild {
        fn drop(&mut self) {
            let _ = fs::write(&self.release, []);
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    #[test]
    fn completed_repositories_stream_while_another_repository_is_busy() {
        let fixture = Fixture::new();
        fixture.nonlocal_remote();
        let fast = fixture.scan.join("z-fast");
        git(
            &fixture.scan,
            &[
                "clone",
                fixture.remote.to_str().unwrap(),
                fast.to_str().unwrap(),
            ],
        );
        git(
            &fast,
            &[
                "remote",
                "set-url",
                "origin",
                "https://example.invalid/fast.git",
            ],
        );
        let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|directory| directory.join("git"))
            .find(|path| path.is_file())
            .and_then(|path| fs::canonicalize(path).ok())
            .expect("real Git in PATH");
        let bin = fixture._temp.path().join("test-bin");
        fs::create_dir(&bin).unwrap();
        let wrapper = bin.join("git");
        fs::write(
            &wrapper,
            r#"#!/bin/sh
repo=
expect_path=false
is_status=false
for argument do
    if [ "$expect_path" = true ]; then
        repo=$argument
        expect_path=false
    else
        case "$argument" in
            -C) expect_path=true ;;
            status) is_status=true ;;
        esac
    fi
done
if [ "$repo" = "$TEST_SLOW_REPO" ] && [ "$is_status" = true ]; then
    : > "$TEST_GATE_REACHED"
    while [ ! -e "$TEST_RELEASE_GATE" ]; do
        kill -0 "$PPID" 2>/dev/null || exit 1
        sleep 0.01
    done
fi
exec "$TEST_REAL_GIT" "$@"
"#,
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        let release = fixture._temp.path().join("release-git");
        let reached = fixture._temp.path().join("git-is-waiting");
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        let mut command = Command::new(env!("CARGO_BIN_EXE_git-unpushed"));
        isolate_git(&mut command);
        command
            .current_dir(&fixture.scan)
            .args([
                "--all",
                "--stream",
                "--progress=always",
                "--jobs=2",
                "--color=never",
            ])
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("COLUMNS", "120")
            .env("TEST_REAL_GIT", real_git)
            .env("TEST_SLOW_REPO", &fixture.repo)
            .env("TEST_RELEASE_GATE", &release)
            .env("TEST_GATE_REACHED", &reached)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = GatedChild {
            child: command.spawn().expect("spawn scanner"),
            release,
        };
        let stdout = child.child.stdout.take().unwrap();
        let stderr = child.child.stderr.take().unwrap();
        let (stdout_tx, stdout_rx) = mpsc::channel();
        let (stderr_tx, stderr_rx) = mpsc::channel();
        let stdout_reader = thread::spawn(move || {
            let mut output = Vec::new();
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = Vec::new();
                if reader.read_until(b'\n', &mut line).unwrap() == 0 {
                    break;
                }
                output.extend_from_slice(&line);
                let _ = stdout_tx.send(line);
            }
            output
        });
        let stderr_reader = thread::spawn(move || {
            let mut output = Vec::new();
            let mut reader = stderr;
            let mut buffer = [0; 512];
            loop {
                let count = reader.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                output.extend_from_slice(&buffer[..count]);
                let _ = stderr_tx.send(buffer[..count].to_vec());
            }
            output
        });

        let first = stdout_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("fast repository must be printed before releasing slow Git");
        assert!(
            String::from_utf8_lossy(&first).contains("z-fast:"),
            "completed repository must stream first: {}",
            String::from_utf8_lossy(&first)
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !reached.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            reached.exists(),
            "slow repository must have reached gated Git status"
        );
        assert!(!child.release.exists(), "slow Git must still be blocked");

        // Discard earlier discovery/scan updates, then require live frames while
        // there are no completions or Git output to wake the rendering thread.
        while stderr_rx.try_recv().is_ok() {}
        let deadline = Instant::now() + Duration::from_millis(350);
        let mut live_progress = Vec::new();
        while Instant::now() < deadline {
            match stderr_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(chunk) => live_progress.extend(chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(error) => panic!("scanner closed stderr while Git was blocked: {error}"),
            }
        }
        assert!(
            live_progress.iter().filter(|byte| **byte == b'\r').count() >= 2,
            "progress must update repeatedly while a repository is busy: {:?}",
            String::from_utf8_lossy(&live_progress)
        );
        assert!(
            String::from_utf8_lossy(&live_progress).contains("Scanning 1/2 repos"),
            "progress must show completed repository counts while Git is blocked: {:?}",
            String::from_utf8_lossy(&live_progress)
        );

        let status = child.release_and_wait();
        let stdout = stdout_reader.join().unwrap();
        let stderr = stderr_reader.join().unwrap();
        assert!(status.success(), "{}", String::from_utf8_lossy(&stderr));
        let output = String::from_utf8(stdout).unwrap();
        assert!(output.find("z-fast:").unwrap() < output.find("alpha:").unwrap());
        assert!(
            !output.contains('\x1b'),
            "progress/color must stay off stdout"
        );
        assert!(!output.contains('\r'), "progress must stay off stdout");
    }
}

#[test]
fn integrated_worktree_with_ignored_data_is_deleted_and_branch_is_retained() {
    let fixture = Fixture::new();
    let tree = fixture.tree("done");
    fs::write(tree.join("ignored.txt"), "disposable local data").unwrap();
    fixture.nonlocal_remote();

    let preview = json(&fixture.scan, &["--delete", "--dry-run"]);
    let tree_report = worktree(only_repository(&preview), &tree);
    assert_eq!(tree_report["ignored"], true);
    assert_eq!(tree_report["removable"], true);
    assert_eq!(tree_report["deletion"]["status"], "would_delete");
    assert!(tree.exists(), "dry run must preserve worktree files");

    let removed = json(&fixture.scan, &["--delete"]);
    assert_eq!(
        worktree(only_repository(&removed), &tree)["deletion"]["status"],
        "deleted"
    );
    assert!(!tree.exists());
    assert!(fixture.repo.exists(), "primary checkout is protected");
    git(&fixture.repo, &["show-ref", "--verify", "refs/heads/done"]);
    assert!(
        !git(&fixture.repo, &["worktree", "list", "--porcelain"]).contains(tree.to_str().unwrap())
    );
}

#[test]
fn discovery_from_a_linked_checkout_can_delete_that_checkout_and_its_siblings() {
    let fixture = Fixture::new();
    let seed = fixture.tree("a-seed");
    let sibling = fixture.tree("b-sibling");
    fixture.nonlocal_remote();
    let report = json(&fixture.scan, &[seed.to_str().unwrap(), "--delete"]);
    let repository = only_repository(&report);
    for tree in [&seed, &sibling] {
        assert_eq!(worktree(repository, tree)["deletion"]["status"], "deleted");
        assert!(!tree.exists());
    }
    assert!(fixture.repo.join("tracked.txt").exists());
    git(
        &fixture.repo,
        &["show-ref", "--verify", "refs/heads/a-seed"],
    );
    git(
        &fixture.repo,
        &["show-ref", "--verify", "refs/heads/b-sibling"],
    );
}

#[test]
fn keep_ignored_prevents_deletion() {
    let fixture = Fixture::new();
    let tree = fixture.tree("ignored");
    fs::write(tree.join("ignored.txt"), "keep me").unwrap();
    fixture.nonlocal_remote();
    let report = json(&fixture.scan, &["--delete", "--keep-ignored"]);
    let tree_report = worktree(only_repository(&report), &tree);
    assert_eq!(tree_report["ignored"], true);
    assert_eq!(tree_report["removable"], false);
    assert!(tree.join("ignored.txt").exists());
}

#[test]
fn exact_squash_is_reported_as_done_by_default_but_dirty_files_still_block_removal() {
    let fixture = Fixture::new();
    let tree = fixture.tree("squashed");
    fixture.commit(&tree, "Intermediate feature\n");
    fixture.commit(&tree, "Completed feature\n");
    git(&fixture.repo, &["merge", "--squash", "squashed"]);
    git(&fixture.repo, &["commit", "-m", "Squash completed feature"]);
    git(&fixture.repo, &["push", "origin", "main", "squashed"]);
    fixture.nonlocal_remote();

    let preview = json(
        &fixture.scan,
        &["--delete", "--dry-run", "--no-index-refresh"],
    );
    let tree_report = worktree(only_repository(&preview), &tree);
    assert_eq!(tree_report["backed_up"], true);
    assert_eq!(tree_report["integrated"], true);
    assert_eq!(tree_report["removable"], true);
    assert_eq!(tree_report["deletion"]["status"], "would_delete");
    assert!(
        tree.exists(),
        "listing and dry run must retain the worktree"
    );
    let plain = cli(&fixture.scan, &["--color=never", "--no-index-refresh"]);
    assert!(plain.status.success(), "{plain:?}");
    let text = String::from_utf8(plain.stdout).unwrap();
    assert!(
        text.lines()
            .any(|line| line.split_whitespace().eq(["↳", "squashed:", "D"]))
    );

    fs::write(tree.join("tracked.txt"), "Uncommitted follow-up\n").unwrap();
    let dirty = json(&fixture.scan, &["--no-index-refresh"]);
    let tree_report = worktree(only_repository(&dirty), &tree);
    assert_eq!(tree_report["modified"], true);
    assert_eq!(tree_report["removable"], false);
    assert_eq!(
        fs::read_to_string(tree.join("tracked.txt")).unwrap(),
        "Uncommitted follow-up\n"
    );
}

#[test]
fn individually_cherry_picked_series_is_done_by_default_and_revalidated_for_deletion() {
    let fixture = Fixture::new();
    let tree = fixture.tree("copied-series");
    let mut commits = Vec::new();
    for text in [
        "First feature step\n",
        "Second feature step\n",
        "Complete feature\n",
    ] {
        fixture.commit(&tree, text);
        commits.push(git(&tree, &["rev-parse", "HEAD"]).trim().to_owned());
    }
    fs::write(fixture.repo.join("unrelated.txt"), "Mainline-only work\n").unwrap();
    git(&fixture.repo, &["add", "unrelated.txt"]);
    git(
        &fixture.repo,
        &["commit", "-m", "Advance main independently"],
    );
    for commit in &commits {
        git(&fixture.repo, &["cherry-pick", commit]);
    }
    let original_is_ancestor = git_output(
        &fixture.repo,
        &[
            OsStr::new("merge-base"),
            OsStr::new("--is-ancestor"),
            OsStr::new(&commits[2]),
            OsStr::new("main"),
        ],
    );
    assert_eq!(original_is_ancestor.status.code(), Some(1));
    git(&fixture.repo, &["push", "origin", "main", "copied-series"]);
    fixture.nonlocal_remote();

    let report = json(&fixture.scan, &["--no-index-refresh"]);
    let tree_report = worktree(only_repository(&report), &tree);
    assert_eq!(tree_report["backed_up"], true);
    assert_eq!(tree_report["integrated"], true);
    assert_eq!(tree_report["removable"], true);
    let plain = cli(&fixture.scan, &["--color=never", "--no-index-refresh"]);
    assert!(plain.status.success(), "{plain:?}");
    assert!(
        String::from_utf8(plain.stdout)
            .unwrap()
            .lines()
            .any(|line| { line.split_whitespace().eq(["↳", "copied-series:", "D"]) })
    );

    let preview = json(
        &fixture.scan,
        &["--delete", "--dry-run", "--no-index-refresh"],
    );
    assert_eq!(
        worktree(only_repository(&preview), &tree)["deletion"]["status"],
        "would_delete"
    );
    assert_eq!(
        fs::read_to_string(tree.join("tracked.txt")).unwrap(),
        "Complete feature\n"
    );
    assert!(
        tree.exists(),
        "dry run must preserve the copied series checkout"
    );
}

#[test]
fn partially_cherry_picked_series_remains_unintegrated() {
    let fixture = Fixture::new();
    let tree = fixture.tree("partial-series");
    let mut commits = Vec::new();
    for text in [
        "First feature step\n",
        "Second feature step\n",
        "Outstanding feature step\n",
    ] {
        fixture.commit(&tree, text);
        commits.push(git(&tree, &["rev-parse", "HEAD"]).trim().to_owned());
    }
    fs::write(fixture.repo.join("unrelated.txt"), "Mainline-only work\n").unwrap();
    git(&fixture.repo, &["add", "unrelated.txt"]);
    git(
        &fixture.repo,
        &["commit", "-m", "Advance main independently"],
    );
    for commit in &commits[..2] {
        git(&fixture.repo, &["cherry-pick", commit]);
    }
    git(&fixture.repo, &["push", "origin", "main", "partial-series"]);
    fixture.nonlocal_remote();

    let report = json(
        &fixture.scan,
        &["--delete", "--dry-run", "--no-index-refresh"],
    );
    let tree_report = worktree(only_repository(&report), &tree);
    assert_eq!(tree_report["backed_up"], true);
    assert_eq!(tree_report["integrated"], false);
    assert_eq!(tree_report["removable"], false);
    assert_ne!(tree_report["deletion"]["status"], "would_delete");
    assert!(tree.exists());
}

#[test]
fn copied_series_must_be_complete_on_one_eligible_mainline() {
    let fixture = Fixture::new();
    let base = git(&fixture.repo, &["rev-parse", "HEAD"]).trim().to_owned();
    let tree = fixture.tree("split-series");
    let mut commits = Vec::new();
    for name in ["first.txt", "second.txt", "third.txt"] {
        fs::write(tree.join(name), format!("Feature in {name}\n")).unwrap();
        git(&tree, &["add", name]);
        git(&tree, &["commit", "-m", name]);
        commits.push(git(&tree, &["rev-parse", "HEAD"]).trim().to_owned());
    }
    fs::write(
        fixture.repo.join("origin-only.txt"),
        "Origin mainline work\n",
    )
    .unwrap();
    git(&fixture.repo, &["add", "origin-only.txt"]);
    git(
        &fixture.repo,
        &["commit", "-m", "Advance origin independently"],
    );
    for commit in &commits[..2] {
        git(&fixture.repo, &["cherry-pick", commit]);
    }
    git(&fixture.repo, &["push", "origin", "main", "split-series"]);
    git(&fixture.repo, &["checkout", "-b", "upstream-main", &base]);
    fs::write(
        fixture.repo.join("upstream-only.txt"),
        "Upstream mainline work\n",
    )
    .unwrap();
    git(&fixture.repo, &["add", "upstream-only.txt"]);
    git(
        &fixture.repo,
        &["commit", "-m", "Advance upstream independently"],
    );
    git(&fixture.repo, &["cherry-pick", &commits[2]]);
    let second = fixture._temp.path().join("second.git");
    git(
        fixture._temp.path(),
        &[
            "init",
            "--bare",
            "--initial-branch=main",
            second.to_str().unwrap(),
        ],
    );
    git(
        &fixture.repo,
        &["remote", "add", "upstream", second.to_str().unwrap()],
    );
    git(&fixture.repo, &["push", "upstream", "upstream-main:main"]);
    git(
        &fixture.repo,
        &[
            "symbolic-ref",
            "refs/remotes/upstream/HEAD",
            "refs/remotes/upstream/main",
        ],
    );
    git(
        &fixture.repo,
        &[
            "remote",
            "set-url",
            "upstream",
            "https://example.invalid/upstream.git",
        ],
    );
    fixture.nonlocal_remote();

    let split = json(&fixture.scan, &["--no-index-refresh"]);
    let tree_report = worktree(only_repository(&split), &tree);
    assert_eq!(tree_report["backed_up"], true);
    assert_eq!(
        tree_report["integrated"], false,
        "separate remotes must not collectively prove integration"
    );
    assert_eq!(tree_report["removable"], false);

    for commit in &commits[..2] {
        git(&fixture.repo, &["cherry-pick", commit]);
    }
    git(
        &fixture.repo,
        &["remote", "set-url", "upstream", second.to_str().unwrap()],
    );
    git(&fixture.repo, &["push", "upstream", "upstream-main:main"]);
    git(
        &fixture.repo,
        &[
            "remote",
            "set-url",
            "upstream",
            "https://example.invalid/upstream.git",
        ],
    );
    let complete = json(&fixture.scan, &["--no-index-refresh"]);
    assert_eq!(
        worktree(only_repository(&complete), &tree)["integrated"],
        true
    );
    assert_eq!(
        worktree(only_repository(&complete), &tree)["removable"],
        true
    );
    let origin_only = json(&fixture.scan, &["--remote", "origin", "--no-index-refresh"]);
    assert_eq!(
        worktree(only_repository(&origin_only), &tree)["integrated"],
        false
    );
}

#[test]
fn cumulative_changes_cannot_use_a_merge_base_from_a_different_remote_mainline() {
    let fixture = Fixture::new();
    let base = git(&fixture.repo, &["rev-parse", "HEAD"]).trim().to_owned();
    let tree = fixture.tree("split-ancestry");
    let mut commits = Vec::new();
    for name in ["first.txt", "second.txt", "third.txt"] {
        fs::write(tree.join(name), format!("Feature in {name}\n")).unwrap();
        git(&tree, &["add", name]);
        git(&tree, &["commit", "-m", name]);
        commits.push(git(&tree, &["rev-parse", "HEAD"]).trim().to_owned());
    }
    // Origin contains the original first two commits, making them the branch's
    // merge base there. Upstream has only a copy of the third commit. Comparing
    // the origin-relative delta against upstream would wrongly call this done.
    git(&fixture.repo, &["merge", "--ff-only", &commits[1]]);
    git(&fixture.repo, &["push", "origin", "main", "split-ancestry"]);
    git(&fixture.repo, &["checkout", "-b", "upstream-main", &base]);
    fs::write(
        fixture.repo.join("upstream-only.txt"),
        "Unrelated upstream work\n",
    )
    .unwrap();
    git(&fixture.repo, &["add", "upstream-only.txt"]);
    git(
        &fixture.repo,
        &["commit", "-m", "Advance upstream independently"],
    );
    git(&fixture.repo, &["cherry-pick", &commits[2]]);
    let second = fixture._temp.path().join("second.git");
    git(
        fixture._temp.path(),
        &[
            "init",
            "--bare",
            "--initial-branch=main",
            second.to_str().unwrap(),
        ],
    );
    git(
        &fixture.repo,
        &["remote", "add", "upstream", second.to_str().unwrap()],
    );
    git(&fixture.repo, &["push", "upstream", "upstream-main:main"]);
    git(
        &fixture.repo,
        &[
            "symbolic-ref",
            "refs/remotes/upstream/HEAD",
            "refs/remotes/upstream/main",
        ],
    );
    git(
        &fixture.repo,
        &[
            "remote",
            "set-url",
            "upstream",
            "https://example.invalid/upstream.git",
        ],
    );
    fixture.nonlocal_remote();

    for args in [
        vec!["--no-index-refresh"],
        vec!["--deep", "--no-index-refresh"],
    ] {
        let report = json(&fixture.scan, &args);
        let tree_report = worktree(only_repository(&report), &tree);
        assert_eq!(tree_report["backed_up"], true, "args={args:?}");
        assert_eq!(tree_report["integrated"], false, "args={args:?}");
        assert_eq!(tree_report["removable"], false, "args={args:?}");
    }
}

#[test]
#[cfg(unix)]
fn exact_squash_handles_binary_paths_and_executable_modes() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let name = "binary\nfile.dat";
    fs::write(fixture.repo.join(name), b"\0original\xff").unwrap();
    git(&fixture.repo, &["add", "--", name]);
    git(&fixture.repo, &["commit", "-m", "Add binary fixture"]);
    let tree = fixture.tree("binary-squashed");
    fs::write(tree.join(name), b"\0updated\xfe").unwrap();
    fs::set_permissions(tree.join("tracked.txt"), fs::Permissions::from_mode(0o755)).unwrap();
    git(&tree, &["add", "--", name, "tracked.txt"]);
    git(
        &tree,
        &["commit", "-m", "Update binary and executable mode"],
    );
    fixture.commit(&tree, "Completed executable feature\n");
    git(&fixture.repo, &["merge", "--squash", "binary-squashed"]);
    git(&fixture.repo, &["commit", "-m", "Squash binary feature"]);
    git(
        &fixture.repo,
        &["push", "origin", "main", "binary-squashed"],
    );
    fixture.nonlocal_remote();
    let report = json(&fixture.scan, &["--no-index-refresh"]);
    let result = worktree(only_repository(&report), &tree);
    assert_eq!(result["integrated"], true);
    assert_eq!(result["removable"], true);
    assert_eq!(fs::read(tree.join(name)).unwrap(), b"\0updated\xfe");
    assert_ne!(
        fs::metadata(tree.join("tracked.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
}

#[test]
fn copied_commits_do_not_hide_unintegrated_merge_resolutions() {
    let fixture = Fixture::new();
    let feature = fixture.tree("resolved-feature");
    let side = fixture.tree("resolved-side");
    fixture.commit(&feature, "Feature version\n");
    let feature_commit = git(&feature, &["rev-parse", "HEAD"]).trim().to_owned();
    fixture.commit(&side, "Side version\n");
    let side_commit = git(&side, &["rev-parse", "HEAD"]).trim().to_owned();
    let merge = git_output(
        &feature,
        &[OsStr::new("merge"), OsStr::new("resolved-side")],
    );
    assert_eq!(
        merge.status.code(),
        Some(1),
        "expected conflicting fixture merge"
    );
    fs::write(
        feature.join("tracked.txt"),
        "Unintegrated merge resolution\n",
    )
    .unwrap();
    git(&feature, &["add", "tracked.txt"]);
    git(&feature, &["commit", "-m", "Resolve branch-only merge"]);

    fs::write(
        fixture.repo.join("unrelated.txt"),
        "Ensure copied commit IDs differ\n",
    )
    .unwrap();
    git(&fixture.repo, &["add", "unrelated.txt"]);
    git(&fixture.repo, &["commit", "-m", "Unrelated main work"]);
    git(&fixture.repo, &["cherry-pick", &feature_commit]);
    git(&fixture.repo, &["revert", "--no-edit", "HEAD"]);
    git(&fixture.repo, &["cherry-pick", &side_commit]);
    git(
        &fixture.repo,
        &[
            "push",
            "origin",
            "main",
            "resolved-feature",
            "resolved-side",
        ],
    );
    // Every nonmerge branch commit matches, but the merge added unique data.
    let cherry = git(&fixture.repo, &["cherry", "main", "resolved-feature"]);
    assert_eq!(cherry.lines().count(), 2);
    assert!(cherry.lines().all(|line| line.starts_with("- ")));
    fixture.nonlocal_remote();
    let report = json(
        &fixture.scan,
        &["--delete", "--dry-run", "--no-index-refresh"],
    );
    let result = worktree(only_repository(&report), &feature);
    assert_eq!(result["backed_up"], true);
    assert_eq!(result["integrated"], false);
    assert_eq!(result["removable"], false);
    assert_eq!(
        fs::read_to_string(feature.join("tracked.txt")).unwrap(),
        "Unintegrated merge resolution\n"
    );
}

#[test]
fn deep_squash_detection_handles_unrelated_file_edits_without_changing_checkouts() {
    let fixture = Fixture::new();
    let initial = "header = old\nline 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\nfeature = old\n";
    fixture.commit(&fixture.repo, initial);
    let tree = fixture.tree("deep-squashed");
    fixture.commit(
        &tree,
        &initial.replace("feature = old", "feature = intermediate"),
    );
    fixture.commit(
        &tree,
        &initial.replace("feature = old", "feature = finished"),
    );
    fixture.commit(
        &fixture.repo,
        &initial.replace("header = old", "header = unrelated"),
    );
    git(&fixture.repo, &["merge", "--squash", "deep-squashed"]);
    git(
        &fixture.repo,
        &["commit", "-m", "Squash with unrelated main edits"],
    );
    git(&fixture.repo, &["push", "origin", "main", "deep-squashed"]);
    fixture.nonlocal_remote();

    let index_path = |path: &Path| {
        let index = PathBuf::from(git(path, &["rev-parse", "--git-path", "index"]).trim());
        if index.is_absolute() {
            index
        } else {
            path.join(index)
        }
    };
    let indexes: Vec<_> = [&fixture.repo, &tree]
        .into_iter()
        .map(|path| {
            let index = index_path(path);
            let bytes = fs::read(&index).unwrap();
            (index, bytes)
        })
        .collect();
    let refs = git(&fixture.repo, &["show-ref"]);
    let main_contents = fs::read(fixture.repo.join("tracked.txt")).unwrap();
    let branch_contents = fs::read(tree.join("tracked.txt")).unwrap();

    let normal = json(&fixture.scan, &["--no-index-refresh"]);
    let tree_report = worktree(only_repository(&normal), &tree);
    assert_eq!(tree_report["backed_up"], true);
    assert_eq!(tree_report["integrated"], false);
    assert_eq!(tree_report["removable"], false);
    let plain = cli(&fixture.scan, &["--color=never", "--no-index-refresh"]);
    assert!(plain.status.success(), "{plain:?}");
    let text = String::from_utf8(plain.stdout).unwrap();
    assert!(
        text.lines()
            .any(|line| line.split_whitespace().eq(["↳", "deep-squashed:", "U"]))
    );
    let deep = json(&fixture.scan, &["--deep", "--no-index-refresh"]);
    let tree_report = worktree(only_repository(&deep), &tree);
    assert_eq!(tree_report["integrated"], true);
    assert_eq!(tree_report["removable"], true);
    let plain = cli(
        &fixture.scan,
        &["--deep", "--color=never", "--no-index-refresh"],
    );
    assert!(plain.status.success(), "{plain:?}");
    let text = String::from_utf8(plain.stdout).unwrap();
    assert!(
        text.lines()
            .any(|line| line.split_whitespace().eq(["↳", "deep-squashed:", "D"]))
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let hooks = fixture.repo.join(".git/verification-test-hooks");
        fs::create_dir(&hooks).unwrap();
        let hook = hooks.join("post-index-change");
        fs::write(
            &hook,
            "#!/bin/sh\nprintf fired > \"$GIT_UNPUSHED_TEST_HOOK_MARKER\"\n",
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let marker = fixture.repo.join(".git/verification-hook-ran");
        let mut command = Command::new(env!("CARGO_BIN_EXE_git-unpushed"));
        isolate_git(&mut command);
        // Override the fixture's hooksPath isolation for this scanner alone.
        // Its private index operations must supply their own empty hooks path.
        let output = command
            .current_dir(&fixture.scan)
            .args(["--deep", "--no-index-refresh", "--json"])
            .env("GIT_CONFIG_COUNT", "3")
            .env("GIT_CONFIG_KEY_2", "core.hooksPath")
            .env("GIT_CONFIG_VALUE_2", &hooks)
            .env("GIT_UNPUSHED_TEST_HOOK_MARKER", &marker)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(worktree(only_repository(&report), &tree)["removable"], true);
        assert!(
            !marker.exists(),
            "verification executed a real repository hook"
        );
    }

    for (index, bytes) in indexes {
        assert_eq!(
            fs::read(&index).unwrap(),
            bytes,
            "index {} changed",
            index.display()
        );
    }
    assert_eq!(git(&fixture.repo, &["show-ref"]), refs, "refs changed");
    assert_eq!(
        fs::read(fixture.repo.join("tracked.txt")).unwrap(),
        main_contents
    );
    assert_eq!(fs::read(tree.join("tracked.txt")).unwrap(), branch_contents);
}

#[test]
fn deep_patch_id_candidates_with_different_indentation_are_not_integrated() {
    let fixture = Fixture::new();
    let initial = "def example():\n    if enabled:\n        return old\n    return fallback\n";
    fixture.commit(&fixture.repo, initial);
    let base = git(&fixture.repo, &["rev-parse", "HEAD"]).trim().to_owned();
    let tree = fixture.tree("indentation");
    fixture.commit(&tree, &initial.replace("return old", "return new"));
    // Both patches have the same whitespace-insensitive fingerprint, but the
    // squash puts the return outside the conditional and changes its meaning.
    fixture.commit(
        &fixture.repo,
        &initial.replace("        return old", "    return new"),
    );
    git(&fixture.repo, &["push", "origin", "main", "indentation"]);
    git(
        &fixture.repo,
        &["config", "apply.ignoreWhitespace", "change"],
    );
    git(&fixture.repo, &["config", "apply.whitespace", "fix"]);
    fixture.nonlocal_remote();

    let patch_id = |revision: &str| {
        use std::io::Write;
        use std::process::Stdio;
        let patch = git(&fixture.repo, &["diff", &base, revision]);
        let mut command = Command::new("git");
        isolate_git(&mut command);
        let mut child = command
            .arg("patch-id")
            .arg("--stable")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(patch.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .split_whitespace()
            .next()
            .expect("patch ID")
            .to_owned()
    };
    assert_eq!(patch_id("indentation"), patch_id("main"));

    let report = json(&fixture.scan, &["--deep", "--no-index-refresh"]);
    let tree_report = worktree(only_repository(&report), &tree);
    assert_eq!(tree_report["backed_up"], true);
    assert_eq!(tree_report["integrated"], false);
    assert_eq!(tree_report["removable"], false);
    assert_eq!(
        fs::read_to_string(tree.join("tracked.txt")).unwrap(),
        initial.replace("return old", "return new")
    );
}

#[test]
fn squash_detection_respects_remote_and_explicit_base_selection() {
    let fixture = Fixture::new();
    let tree = fixture.tree("squashed-upstream");
    fixture.commit(&tree, "Intermediate upstream feature\n");
    fixture.commit(&tree, "Completed upstream feature\n");
    git(&fixture.repo, &["merge", "--squash", "squashed-upstream"]);
    git(&fixture.repo, &["commit", "-m", "Squash on upstream only"]);
    git(&fixture.repo, &["push", "origin", "squashed-upstream"]);
    let second = fixture._temp.path().join("second.git");
    git(
        fixture._temp.path(),
        &[
            "init",
            "--bare",
            "--initial-branch=main",
            second.to_str().unwrap(),
        ],
    );
    git(
        &fixture.repo,
        &["remote", "add", "upstream", second.to_str().unwrap()],
    );
    git(&fixture.repo, &["push", "upstream", "main"]);
    git(
        &fixture.repo,
        &[
            "symbolic-ref",
            "refs/remotes/upstream/HEAD",
            "refs/remotes/upstream/main",
        ],
    );
    git(
        &fixture.repo,
        &[
            "remote",
            "set-url",
            "upstream",
            "https://example.invalid/upstream.git",
        ],
    );
    fixture.nonlocal_remote();

    for args in [vec![], vec!["--deep"], vec!["--base", "upstream/main"]] {
        let report = json(&fixture.scan, &args);
        assert_eq!(
            worktree(only_repository(&report), &tree)["integrated"],
            true
        );
    }
    for args in [
        vec!["--remote", "origin"],
        vec!["--deep", "--remote", "origin"],
        vec!["--base", "origin/main"],
    ] {
        let report = json(&fixture.scan, &args);
        let tree_report = worktree(only_repository(&report), &tree);
        assert_eq!(tree_report["integrated"], false, "args={args:?}");
        assert_eq!(tree_report["removable"], false, "args={args:?}");
    }
}

#[test]
fn pushed_unmerged_worktree_needs_explicit_include_unmerged_for_removal() {
    let fixture = Fixture::new();
    let tree = fixture.tree("topic");
    fixture.commit(&tree, "Unmerged feature\n");
    git(&fixture.repo, &["push", "origin", "topic"]);
    fixture.nonlocal_remote();

    let default = json(&fixture.scan, &["--delete"]);
    let tree_report = worktree(only_repository(&default), &tree);
    assert_eq!(tree_report["backed_up"], true);
    assert_eq!(tree_report["integrated"], false);
    assert_eq!(tree_report["removable"], false);
    assert!(tree.exists());
    assert!(
        only_repository(&default)["unpushed_branches"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let opted_in = json(&fixture.scan, &["--delete", "--include-unmerged"]);
    assert_eq!(
        worktree(only_repository(&opted_in), &tree)["deletion"]["status"],
        "deleted"
    );
    assert!(!tree.exists());
    git(&fixture.repo, &["show-ref", "--verify", "refs/heads/topic"]);
}

#[test]
fn dirty_untracked_and_locked_worktrees_are_preserved() {
    let fixture = Fixture::new();
    let dirty = fixture.tree("dirty");
    let untracked = fixture.tree("untracked");
    let locked = fixture.tree("locked");
    fs::write(dirty.join("tracked.txt"), "work in progress\n").unwrap();
    fs::write(untracked.join("valuable.txt"), "not backed up\n").unwrap();
    git(
        &fixture.repo,
        &["worktree", "lock", locked.to_str().unwrap()],
    );
    fixture.nonlocal_remote();

    let report = json(&fixture.scan, &["--delete", "--include-unmerged"]);
    let repository = only_repository(&report);
    assert_eq!(worktree(repository, &dirty)["modified"], true);
    assert_eq!(worktree(repository, &untracked)["untracked"], true);
    assert_eq!(worktree(repository, &locked)["locked"], true);
    for path in [&dirty, &untracked, &locked, &fixture.repo] {
        assert_eq!(worktree(repository, path)["removable"], false);
        assert!(path.exists(), "must preserve {}", path.display());
    }
}

#[test]
fn index_flags_cannot_hide_tracked_edits_from_deletion_checks() {
    for flag in ["--assume-unchanged", "--skip-worktree"] {
        let fixture = Fixture::new();
        let tree = fixture.tree("hidden-edit");
        git(&tree, &["update-index", flag, "tracked.txt"]);
        fs::write(tree.join("tracked.txt"), "hidden valuable edit\n").unwrap();
        assert!(
            git(&tree, &["status", "--porcelain"]).is_empty(),
            "fixture must demonstrate an edit hidden from ordinary Git status"
        );
        fixture.nonlocal_remote();
        let (_, report) = report_even_if_error(&fixture.scan, &["--delete"]);
        let tree_report = worktree(only_repository(&report), &tree);
        assert_eq!(tree_report["removable"], false, "{flag}: {tree_report:#}");
        assert_eq!(
            fs::read_to_string(tree.join("tracked.txt")).unwrap(),
            "hidden valuable edit\n",
            "{flag} must never make hidden local changes disposable"
        );
    }
}

#[test]
fn an_in_progress_merge_blocks_otherwise_integrated_worktree() {
    let fixture = Fixture::new();
    let tree = fixture.tree("merging");
    let head = git(&tree, &["rev-parse", "HEAD"]);
    let merge_head = git(&tree, &["rev-parse", "--git-path", "MERGE_HEAD"]);
    fs::write(PathBuf::from(merge_head.trim()), head).unwrap();
    fixture.nonlocal_remote();
    let report = json(&fixture.scan, &["--delete"]);
    let tree_report = worktree(only_repository(&report), &tree);
    assert!(tree_report["operation"].is_string());
    assert_eq!(tree_report["removable"], false);
    assert!(tree.exists());
}

#[test]
fn behind_only_branch_is_backed_up_without_unpushed_flag() {
    let fixture = Fixture::new();
    fixture.commit(&fixture.repo, "Remote is newer\n");
    git(&fixture.repo, &["push", "origin", "main"]);
    git(&fixture.repo, &["reset", "--hard", "HEAD^"]);
    fixture.nonlocal_remote();
    let report = json(&fixture.scan, &[]);
    let repository = only_repository(&report);
    assert!(
        repository["unpushed_branches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(worktree(repository, &fixture.repo)["backed_up"], true);
    assert_eq!(worktree(repository, &fixture.repo)["integrated"], true);
}

#[test]
fn backup_does_not_require_an_upstream_or_matching_branch_name() {
    let fixture = Fixture::new();
    let tree = fixture.tree("feature");
    fixture.commit(&tree, "Backup under another name\n");
    git(
        &fixture.repo,
        &["push", "origin", "feature:archived-feature"],
    );
    fixture.nonlocal_remote();
    let no_upstream = git_output(
        &fixture.repo,
        &[
            OsStr::new("config"),
            OsStr::new("--get"),
            OsStr::new("branch.feature.remote"),
        ],
    );
    assert!(!no_upstream.status.success());
    let report = json(&fixture.scan, &[]);
    assert_eq!(worktree(only_repository(&report), &tree)["backed_up"], true);
    assert!(
        only_repository(&report)["unpushed_branches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn checks_unpushed_branches_without_attached_worktrees() {
    let fixture = Fixture::new();
    let tree = fixture.tree("unattached");
    fixture.commit(&tree, "Valuable unattached commit\n");
    git(
        &fixture.repo,
        &["worktree", "remove", tree.to_str().unwrap()],
    );
    fixture.nonlocal_remote();
    let report = json(&fixture.scan, &[]);
    let branches = only_repository(&report)["unpushed_branches"]
        .as_array()
        .unwrap();
    assert!(branches.iter().any(|branch| branch == "unattached"));
}

#[test]
fn local_path_and_file_remotes_do_not_establish_remote_backup() {
    for file_url in [false, true] {
        let fixture = Fixture::new();
        let tree = fixture.tree("local-only");
        if file_url {
            git(
                &fixture.repo,
                &[
                    "remote",
                    "set-url",
                    "origin",
                    &format!("file://{}", fixture.remote.display()),
                ],
            );
        }
        let report = json(&fixture.scan, &["--delete", "--include-unmerged"]);
        let repository = only_repository(&report);
        assert_eq!(repository["remotes"][0]["eligible"], false);
        assert_eq!(worktree(repository, &tree)["removable"], false);
        assert!(tree.exists());
    }
}

#[test]
fn grafts_and_replacement_refs_cannot_falsify_remote_integration() {
    for use_replace_ref in [false, true] {
        let fixture = Fixture::new();
        let tree = fixture.tree("unbacked");
        let original_main = git(&fixture.repo, &["rev-parse", "main"]).trim().to_owned();
        let tree_object = git(&fixture.repo, &["rev-parse", "HEAD^{tree}"])
            .trim()
            .to_owned();
        let independent = git(
            &fixture.repo,
            &[
                "commit-tree",
                &tree_object,
                "-m",
                "Unbacked independent root",
            ],
        )
        .trim()
        .to_owned();
        git(&tree, &["reset", "--hard", &independent]);
        if use_replace_ref {
            git(
                &fixture.repo,
                &["replace", "--graft", &original_main, &independent],
            );
        } else {
            let info = fixture.repo.join(".git/info");
            fs::create_dir_all(&info).unwrap();
            fs::write(
                info.join("grafts"),
                format!("{original_main} {independent}\n"),
            )
            .unwrap();
        }
        // The fixture really tricks ordinary Git ancestry checks. The scanner
        // must establish remote containment using the original object graph.
        git(
            &fixture.repo,
            &["merge-base", "--is-ancestor", &independent, "origin/main"],
        );
        fixture.nonlocal_remote();
        let report = json(&fixture.scan, &["--delete", "--include-unmerged"]);
        let tree_report = worktree(only_repository(&report), &tree);
        assert_eq!(
            tree_report["integrated"], false,
            "replace={use_replace_ref}"
        );
        assert_eq!(tree_report["backed_up"], false, "replace={use_replace_ref}");
        assert_eq!(tree_report["removable"], false, "replace={use_replace_ref}");
        assert!(tree.exists());
    }
}

#[test]
fn remote_transport_eligibility_honors_longest_git_url_rewrite() {
    for network_to_local in [false, true] {
        let fixture = Fixture::new();
        let tree = fixture.tree("done");
        let remote_path = fixture.remote.to_str().unwrap();
        let expected = if network_to_local {
            fixture.nonlocal_remote();
            git(
                &fixture.repo,
                &[
                    "config",
                    "url.ssh://git@example.invalid/.insteadOf",
                    "https://example.invalid/",
                ],
            );
            git(
                &fixture.repo,
                &[
                    "config",
                    &format!("url.{remote_path}.insteadOf"),
                    "https://example.invalid/repo.git",
                ],
            );
            remote_path.to_owned()
        } else {
            git(
                &fixture.repo,
                &[
                    "config",
                    "url.file:///ignored/.insteadOf",
                    fixture.remote.parent().unwrap().to_str().unwrap(),
                ],
            );
            git(
                &fixture.repo,
                &[
                    "config",
                    "url.https://example.invalid/repo.git.insteadOf",
                    remote_path,
                ],
            );
            "https://example.invalid/repo.git".to_owned()
        };
        assert_eq!(
            git(&fixture.repo, &["remote", "get-url", "origin"]).trim(),
            expected
        );
        let report = json(&fixture.scan, &[]);
        let repository = only_repository(&report);
        assert_eq!(repository["remotes"][0]["eligible"], !network_to_local);
        assert_eq!(worktree(repository, &tree)["removable"], !network_to_local);
        assert!(tree.exists(), "reporting must preserve fixtures");
    }
}

#[test]
fn any_qualifying_remote_mainline_can_integrate_work_and_remote_filter_restricts_it() {
    let fixture = Fixture::new();
    let tree = fixture.tree("feature");
    fixture.commit(&tree, "Integrated on second remote\n");
    let second = fixture._temp.path().join("second.git");
    git(
        fixture._temp.path(),
        &[
            "init",
            "--bare",
            "--initial-branch=main",
            second.to_str().unwrap(),
        ],
    );
    git(
        &fixture.repo,
        &["remote", "add", "upstream", second.to_str().unwrap()],
    );
    git(&fixture.repo, &["push", "upstream", "feature:main"]);
    git(
        &fixture.repo,
        &[
            "symbolic-ref",
            "refs/remotes/upstream/HEAD",
            "refs/remotes/upstream/main",
        ],
    );
    git(
        &fixture.repo,
        &[
            "remote",
            "set-url",
            "upstream",
            "ssh://git@example.invalid/upstream.git",
        ],
    );
    fixture.nonlocal_remote();

    let both = json(&fixture.scan, &[]);
    assert_eq!(worktree(only_repository(&both), &tree)["integrated"], true);
    assert_eq!(worktree(only_repository(&both), &tree)["removable"], true);
    let only_origin = json(&fixture.scan, &["--remote", "origin"]);
    assert_eq!(
        worktree(only_repository(&only_origin), &tree)["integrated"],
        false
    );
    assert_eq!(
        worktree(only_repository(&only_origin), &tree)["removable"],
        false
    );
}

#[test]
fn ambiguous_mainline_never_qualifies_a_worktree_for_default_removal() {
    let fixture = Fixture::new();
    let tree = fixture.tree("done");
    git(&fixture.repo, &["push", "origin", "main:master"]);
    git(
        &fixture.repo,
        &["symbolic-ref", "--delete", "refs/remotes/origin/HEAD"],
    );
    fixture.nonlocal_remote();
    let (_, report) = report_even_if_error(&fixture.scan, &["--delete"]);
    let tree_report = worktree(only_repository(&report), &tree);
    assert!(tree_report["integrated"].is_null());
    assert_eq!(tree_report["removable"], false);
    assert!(tree.exists());
}

#[test]
fn missing_registered_worktree_is_reported_without_aborting_other_repositories() {
    let fixture = Fixture::new();
    let tree = fixture.tree("missing");
    fixture.nonlocal_remote();
    fs::remove_dir_all(&tree).unwrap();
    let (_, report) = report_even_if_error(&fixture.scan, &[]);
    let tree_report = worktree(only_repository(&report), &tree);
    assert_eq!(tree_report["removable"], false);
    assert!(
        tree_report["prunable"] == true || !tree_report["errors"].as_array().unwrap().is_empty()
    );
}

#[test]
fn unusual_paths_survive_machine_output_and_do_not_inject_terminal_lines() {
    let fixture = Fixture::new();
    let tree = fixture.tree_at("feature", fixture.outside.join("line\nbreak with spaces"));
    fixture.nonlocal_remote();
    let report = json(&fixture.scan, &[]);
    assert_eq!(worktree(only_repository(&report), &tree)["removable"], true);
    let terminal = cli(&fixture.scan, &["--all", "--color", "never"]);
    assert!(terminal.status.success());
    let rendered = String::from_utf8_lossy(&terminal.stdout);
    assert!(
        !rendered.contains("line\nbreak"),
        "escape path control characters: {rendered:?}"
    );
}

#[test]
fn dry_run_requires_delete_and_unknown_flags_fail_without_mutating_worktrees() {
    let fixture = Fixture::new();
    let tree = fixture.tree("done");
    fixture.nonlocal_remote();
    assert!(!cli(&fixture.scan, &["--dry-run"]).status.success());
    assert!(
        !cli(&fixture.scan, &["--definitely-not-an-option"])
            .status
            .success()
    );
    assert!(tree.exists());
}

#[test]
fn stashes_are_repository_scoped_and_piped_default_output_has_no_color() {
    let fixture = Fixture::new();
    fixture.tree("done");
    fs::write(fixture.repo.join("tracked.txt"), "stashed work\n").unwrap();
    git(&fixture.repo, &["stash", "push", "-m", "Preserve WIP"]);
    fixture.nonlocal_remote();
    let report = json(&fixture.scan, &[]);
    assert_eq!(only_repository(&report)["stashes"], true);
    assert_eq!(
        only_repository(&report)["worktrees"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let output = cli(&fixture.scan, &[]);
    assert!(output.status.success());
    assert!(!output.stdout.contains(&0x1b));
}

#[test]
fn initialized_submodule_blocks_worktree_removal() {
    let fixture = Fixture::new();
    let module = fixture._temp.path().join("module-source");
    fs::create_dir(&module).unwrap();
    git(&module, &["init", "--initial-branch=main"]);
    git(&module, &["config", "user.name", "CLI test"]);
    git(
        &module,
        &["config", "user.email", "cli-test@example.invalid"],
    );
    fs::write(module.join("module.txt"), "submodule contents\n").unwrap();
    git(&module, &["add", "."]);
    git(&module, &["commit", "-m", "Submodule fixture"]);
    git(
        &fixture.repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            module.to_str().unwrap(),
            "module",
        ],
    );
    git(&fixture.repo, &["commit", "-am", "Add submodule"]);
    git(&fixture.repo, &["push", "origin", "main"]);
    let tree = fixture.tree("submodule");
    git(
        &tree,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
        ],
    );
    fixture.nonlocal_remote();
    let report = json(&fixture.scan, &["--delete"]);
    assert_eq!(
        worktree(only_repository(&report), &tree)["removable"],
        false
    );
    assert!(tree.join("module/module.txt").exists());
}

#[test]
fn revalidation_rejects_files_added_after_the_initial_scan() {
    const HELPER_ENV: &str = "GIT_UNPUSHED_TEST_REVALIDATION_ROOT";
    if let Some(root) = std::env::var_os(HELPER_ENV) {
        // Run library inspection in a child with the same Git isolation as CLI
        // tests, avoiding process-global environment changes in parallel tests.
        let root = PathBuf::from(root);
        let seed = git_unpushed::git::resolve_seed(&root).unwrap();
        let options = git_unpushed::model::ScanOptions {
            current_dir: root.clone(),
            ..Default::default()
        };
        let repository = git_unpushed::inspect::inspect_repository(&seed, &options).unwrap();
        let original = repository
            .worktrees
            .iter()
            .find(|tree| !tree.primary)
            .unwrap();
        assert!(original.removable);
        fs::write(original.path.join("just-created.txt"), "must not be lost\n").unwrap();
        let current =
            git_unpushed::inspect::revalidate_worktree(&repository, original, &options).unwrap();
        assert!(current.untracked);
        assert!(
            !current.removable,
            "fresh file status must invalidate the old safe result"
        );
        return;
    }

    let fixture = Fixture::new();
    let tree = fixture.tree("done");
    fixture.nonlocal_remote();
    let mut helper = Command::new(std::env::current_exe().unwrap());
    isolate_git(&mut helper);
    let output = helper
        .arg("--exact")
        .arg("revalidation_rejects_files_added_after_the_initial_scan")
        .arg("--nocapture")
        .env(HELPER_ENV, &fixture.repo)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "revalidation helper failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(tree.join("just-created.txt").exists());
}
