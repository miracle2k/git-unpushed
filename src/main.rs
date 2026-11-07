use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use git_unpushed::model::{
    DeletionResult, DeletionStatus, RepositoryReport, RepositorySeed, ScanOptions, ScanReport,
};
use git_unpushed::render::{self, RenderOptions};
use git_unpushed::{discovery, git, inspect};
use rayon::prelude::*;
use std::io::{self, IsTerminal, Write};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Instant;

mod progress;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum When {
    Auto,
    Always,
    Never,
}

impl When {
    fn enabled(self, automatic: bool) -> bool {
        match self {
            Self::Auto => automatic,
            Self::Always => true,
            Self::Never => false,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Find unfinished work and removable Git worktrees across repositories",
    after_help = "Flags: M tracked edits, ? untracked, S stashes, P unpushed, D removable, U unintegrated, I ignored, ! uncertain.\nScans are offline unless --fetch is supplied. Deletion preserves branches."
)]
struct Cli {
    /// Search roots (defaults to the current directory)
    #[arg(value_name = "ROOT")]
    roots: Vec<PathBuf>,

    /// Search a directory; may be repeated, alongside positional roots
    #[arg(short, long = "directory", value_name = "DIR")]
    directories: Vec<PathBuf>,

    /// Include clean repositories and worktrees
    #[arg(long)]
    all: bool,

    /// Show only worktrees eligible for deletion
    #[arg(long, conflicts_with = "all")]
    removable: bool,

    /// Remove all eligible linked worktrees; preserve branches and primary checkouts
    #[arg(long)]
    delete: bool,

    /// Preview deletion without removing anything
    #[arg(long, requires = "delete")]
    dry_run: bool,

    /// Also allow clean worktrees backed up remotely but not integrated into mainline
    #[arg(long)]
    include_unmerged: bool,

    /// Exclude worktrees containing ignored files from deletion eligibility
    #[arg(long)]
    keep_ignored: bool,

    /// Refresh eligible remotes before scanning (offline by default)
    #[arg(long)]
    fetch: bool,

    /// Also detect broader squash matches with a slower, verified patch search
    #[arg(long)]
    deep: bool,

    /// Disable Git's index stat-cache refresh (useful for background scans)
    #[arg(long)]
    no_index_refresh: bool,

    /// Consider only this remote; may be repeated
    #[arg(long, value_name = "NAME")]
    remote: Vec<String>,

    /// Override mainline with a qualifying remote branch, e.g. origin/trunk
    #[arg(long, value_name = "REMOTE/BRANCH")]
    base: Option<String>,

    /// Emit complete structured scan results instead of terminal output
    #[arg(long)]
    json: bool,

    #[arg(long, value_enum, default_value = "auto")]
    color: When,

    /// Stream completed repository groups (automatic in terminals)
    #[arg(long, value_enum, default_value = "auto", default_missing_value = "always", num_args = 0..=1, require_equals = true)]
    stream: When,

    /// Show a live indicator on stderr (automatic in terminals)
    #[arg(long, value_enum, default_value = "auto", default_missing_value = "always", num_args = 0..=1, require_equals = true)]
    progress: When,

    /// Maximum parallel scan workers (default: available CPUs, capped at 8)
    #[arg(short, long, value_name = "N")]
    jobs: Option<NonZeroUsize>,

    /// Print scan timing to stderr
    #[arg(long)]
    timings: bool,
}

fn main() -> ExitCode {
    match execute(Cli::parse()) {
        Ok(has_errors) => {
            if has_errors {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(error) => {
            // Broken pipes are normal when the output is consumed by head, etc.
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
            {
                return ExitCode::SUCCESS;
            }
            eprintln!("git-unpushed: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn execute(cli: Cli) -> Result<bool> {
    let started = Instant::now();
    let terminal = io::stdout().is_terminal();
    let stream = !cli.json && cli.stream.enabled(terminal);
    let render_options = RenderOptions {
        all: cli.all,
        removable_only: cli.removable,
        color: cli.color.enabled(
            terminal
                && std::env::var_os("NO_COLOR").is_none()
                && std::env::var("TERM").ok().is_none_or(|term| term != "dumb"),
        ),
    };
    let mut progress = progress::Progress::new(cli.progress.enabled(
        !cli.json
            && io::stderr().is_terminal()
            && std::env::var("TERM").ok().is_none_or(|term| term != "dumb"),
    ));
    let current_dir = std::env::current_dir().context("cannot determine the current directory")?;
    let current_dir = current_dir.canonicalize().unwrap_or(current_dir);
    let mut roots = cli.roots;
    roots.extend(cli.directories);
    if roots.is_empty() {
        roots.push(current_dir.clone());
    }
    let jobs = cli.jobs.map(NonZeroUsize::get).unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(8)
    });
    let pool = rayon::ThreadPoolBuilder::new().num_threads(jobs).build()?;
    let options = ScanOptions {
        remotes: cli.remote,
        base: cli.base,
        include_unmerged: cli.include_unmerged,
        keep_ignored: cli.keep_ignored,
        fetch: cli.fetch,
        deep: cli.deep,
        refresh_index: !cli.no_index_refresh,
        current_dir,
    };
    let discovery_started = Instant::now();
    let (seeds, errors) = pool.install(|| {
        discovery::discover_with_progress(&roots, |directories, repositories, path| {
            progress.discovering(directories, repositories, path);
        })
    });
    let discovery_elapsed = discovery_started.elapsed();
    let scan_started = Instant::now();
    let total = seeds.len();
    let completed = AtomicUsize::new(0);
    let worktrees = AtomicUsize::new(0);
    let cancelled = AtomicBool::new(false);
    let minimum_width = seeds
        .iter()
        .map(|seed| render::repository_label_width(&seed.path))
        .max()
        .unwrap_or(0);
    let mut streamed_any = false;
    let (sender, receiver) = mpsc::channel();
    let mut repositories = std::thread::scope(|scope| -> Result<Vec<RepositoryReport>> {
        let progress = &progress;
        let options = &options;
        let pool = &pool;
        let seeds = &seeds;
        let completed = &completed;
        let worktrees = &worktrees;
        let cancelled = &cancelled;
        let delete = cli.delete;
        let dry_run = cli.dry_run;
        let worker = scope.spawn(move || {
            pool.install(|| {
                seeds.par_iter().for_each_with(sender, |sender, seed| {
                    if cancelled.load(Ordering::Relaxed) {
                        return;
                    }
                    progress.scanning(
                        completed.load(Ordering::Relaxed),
                        total,
                        worktrees.load(Ordering::Relaxed),
                        &seed.path,
                    );
                    let mut repository =
                        inspect::inspect_repository_with_progress(seed, options, &|path| {
                            let checked = worktrees.fetch_add(1, Ordering::Relaxed) + 1;
                            progress.scanning(
                                completed.load(Ordering::Relaxed),
                                total,
                                checked,
                                path,
                            );
                        })
                        .unwrap_or_else(|error| failed_repository(seed, error));
                    if delete && !cancelled.load(Ordering::Relaxed) {
                        delete_eligible(&mut repository, options, dry_run, cancelled, progress);
                    }
                    completed.fetch_add(1, Ordering::Relaxed);
                    let _ = sender.send(repository);
                });
            });
        });
        let mut repositories = Vec::with_capacity(total);
        let mut output_error = None;
        for repository in receiver {
            progress.scanning(
                completed.load(Ordering::Relaxed),
                total,
                worktrees.load(Ordering::Relaxed),
                &repository.path,
            );
            if stream && output_error.is_none() {
                let output = render::render_repository(&repository, render_options, minimum_width);
                if !output.is_empty() {
                    let result = progress.with_output(|| {
                        let mut stdout = io::stdout().lock();
                        stdout.write_all(output.as_bytes())?;
                        stdout.flush()
                    });
                    if let Err(error) = result {
                        cancelled.store(true, Ordering::Relaxed);
                        output_error = Some(error);
                    } else {
                        streamed_any = true;
                    }
                }
            }
            repositories.push(repository);
        }
        worker
            .join()
            .map_err(|_| anyhow::anyhow!("scan worker panicked"))?;
        if let Some(error) = output_error {
            return Err(error.into());
        }
        Ok(repositories)
    })?;
    repositories.sort_by(|left, right| left.path.cmp(&right.path));
    let scan_elapsed = scan_started.elapsed();
    progress.finish();
    let report = ScanReport {
        repositories,
        errors,
        elapsed_ms: started.elapsed().as_millis(),
    };
    let has_errors =
        !report.errors.is_empty() || report.repositories.iter().any(RepositoryReport::has_errors);
    let output = if cli.json {
        serde_json::to_string_pretty(&report)? + "\n"
    } else if !streamed_any {
        render::render(&report, render_options)
    } else if report.errors.is_empty() {
        String::new()
    } else {
        render::render(
            &ScanReport {
                repositories: Vec::new(),
                errors: report.errors.clone(),
                elapsed_ms: report.elapsed_ms,
            },
            render_options,
        )
    };
    io::stdout().lock().write_all(output.as_bytes())?;
    if cli.timings {
        eprintln!(
            "discovery {:.3}s · scan {:.3}s · total {:.3}s · {} repositories · {} workers · {} Git commands",
            discovery_elapsed.as_secs_f64(),
            scan_elapsed.as_secs_f64(),
            started.elapsed().as_secs_f64(),
            report.repositories.len(),
            jobs,
            git::invocation_count()
        );
    }
    Ok(has_errors)
}

fn failed_repository(seed: &RepositorySeed, error: anyhow::Error) -> RepositoryReport {
    RepositoryReport {
        path: seed.path.clone(),
        common_dir: seed.common_dir.clone(),
        bare: false,
        stashes: false,
        unpushed_branches: Vec::new(),
        unknown_branches: Vec::new(),
        remotes: Vec::new(),
        fetched: false,
        worktrees: Vec::new(),
        errors: vec![format!("{error:#}")],
    }
}

fn delete_eligible(
    repository: &mut RepositoryReport,
    options: &ScanOptions,
    dry_run: bool,
    cancelled: &AtomicBool,
    progress: &progress::Progress,
) {
    // Worktrees in one repository are removed serially; separate repositories
    // remain parallel. The consumer can stop further removals on a broken pipe.
    let total = repository
        .worktrees
        .iter()
        .filter(|worktree| worktree.removable)
        .count();
    let mut completed = 0;
    for index in 0..repository.worktrees.len() {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        if !repository.worktrees[index].removable {
            continue;
        }
        let original = repository.worktrees[index].clone();
        progress.deleting(completed, total, &original.path);
        match inspect::revalidate_worktree(repository, &original, options) {
            Ok(mut current) if current.removable => {
                let result = if dry_run {
                    DeletionResult {
                        status: DeletionStatus::WouldDelete,
                        reason: None,
                    }
                } else {
                    match git::remove_worktree(&repository.common_dir, &current.path) {
                        Ok(()) => DeletionResult {
                            status: DeletionStatus::Deleted,
                            reason: None,
                        },
                        Err(error) => DeletionResult {
                            status: DeletionStatus::Failed,
                            reason: Some(format!("{error:#}")),
                        },
                    }
                };
                current.deletion = Some(result);
                repository.worktrees[index] = current;
            }
            Ok(mut current) => {
                current.deletion = Some(DeletionResult {
                    status: DeletionStatus::Skipped,
                    reason: current
                        .reason
                        .clone()
                        .or_else(|| Some("eligibility changed during scan".into())),
                });
                repository.worktrees[index] = current;
            }
            Err(error) => {
                repository.worktrees[index].removable = false;
                repository.worktrees[index].deletion = Some(DeletionResult {
                    status: DeletionStatus::Failed,
                    reason: Some(format!("revalidation failed: {error:#}")),
                });
            }
        }
        completed += 1;
        progress.deleting(completed, total, &original.path);
    }
}
