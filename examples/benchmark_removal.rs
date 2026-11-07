//! Benchmark only disposable worktrees created by this process.
use anyhow::{Context, Result, bail};
use clap::Parser;
use git_unpushed::{model::DeletionStatus, removal};
use std::{fs, path::Path, time::Instant};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "10000")]
    files: usize,
    #[arg(long, default_value = "5", value_parser = clap::value_parser!(u32).range(1..))]
    runs: u32,
}

fn git(path: &Path, args: &[&str]) -> Result<()> {
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let hooks = format!("core.hooksPath={null}");
    let mut command_args = vec![
        "-c",
        hooks.as_str(),
        "-c",
        "commit.gpgSign=false",
        "-c",
        "user.name=Benchmark",
        "-c",
        "user.email=benchmark@example.invalid",
    ];
    command_args.extend_from_slice(args);
    git_unpushed::git::run(path, &command_args).map(|_| ())
}

fn main() -> Result<()> {
    let args = Args::parse();
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let repo = root.join("repository");
    fs::create_dir(&repo)?;
    git(&repo, &["init", "--quiet", "--initial-branch=main"])?;
    fs::write(repo.join(".gitignore"), "ignored/\n")?;
    git(&repo, &["add", "."])?;
    git(&repo, &["commit", "--quiet", "-m", "Benchmark fixture"])?;
    let common = repo.join(".git");
    let modes = ["native Git", "files only", "files and bytes"];
    let mut timings = [Vec::new(), Vec::new(), Vec::new()];
    let data = vec![b'x'; 1024];
    for round in 0..args.runs {
        // Rotate the order so the same mode is not always first.
        for offset in 0..3 {
            let mode = (round as usize + offset) % 3;
            let tree = root.join(format!("tree-{round}-{mode}"));
            git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "--detach",
                    tree.to_str().context("temporary path is not UTF-8")?,
                    "main",
                ],
            )?;
            for index in 0..args.files {
                let directory = tree.join("ignored").join((index / 100).to_string());
                if index.is_multiple_of(100) {
                    fs::create_dir_all(&directory)?;
                }
                fs::write(directory.join(index.to_string()), &data)?;
            }
            let started = Instant::now();
            if mode == 0 {
                git(
                    &common,
                    &[
                        "-c",
                        "status.showUntrackedFiles=all",
                        "-c",
                        "diff.ignoreSubmodules=none",
                        "worktree",
                        "remove",
                        "--",
                        tree.to_str().context("temporary path is not UTF-8")?,
                    ],
                )?;
            } else {
                let result = removal::remove_worktree(&common, &tree, mode == 2);
                if !matches!(result.status, DeletionStatus::Deleted) {
                    bail!("removal failed: {:?}", result.reason);
                }
                let stats = result.stats.context("missing removal statistics")?;
                if stats.files < args.files as u64 {
                    bail!("removal undercounted files");
                }
            }
            let seconds = started.elapsed().as_secs_f64();
            timings[mode].push(seconds);
            println!("run {} {}: {seconds:.3}s", round + 1, modes[mode]);
        }
    }
    println!(
        "{} ignored files of 1 KiB per worktree; fixture creation excluded",
        args.files
    );
    for (mode, samples) in modes.into_iter().zip(&mut timings) {
        samples.sort_by(f64::total_cmp);
        let middle = samples.len() / 2;
        let median = if samples.len().is_multiple_of(2) {
            (samples[middle - 1] + samples[middle]) / 2.0
        } else {
            samples[middle]
        };
        println!("median {mode}: {median:.3}s");
    }
    Ok(())
}
