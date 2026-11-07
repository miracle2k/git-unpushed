//! Compare release binaries on the same stable, offline scan roots.
use anyhow::{Context, Result, bail};
use clap::Parser;
use serde_json::Value;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(Parser)]
struct Args {
    baseline: PathBuf,
    candidate: PathBuf,
    #[arg(long, default_value = "3")]
    runs: NonZeroUsize,
    #[arg(required = true)]
    roots: Vec<PathBuf>,
}

fn scan(binary: &Path, roots: &[PathBuf]) -> Result<(Duration, Value, Option<i32>)> {
    let started = Instant::now();
    let output = Command::new(binary)
        .args([
            "--json",
            "--no-index-refresh",
            "--progress=never",
            "--color=never",
            "--jobs=8",
        ])
        .args(roots)
        .output()
        .context("could not run scan binary")?;
    let elapsed = started.elapsed();
    let mut report: Value =
        serde_json::from_slice(&output.stdout).context("scan did not produce a JSON report")?;
    report
        .as_object_mut()
        .context("expected a report object")?
        .remove("elapsed_ms");
    Ok((elapsed, report, output.status.code()))
}

fn median(samples: &mut [Duration]) -> f64 {
    samples.sort();
    let middle = samples.len() / 2;
    if samples.len().is_multiple_of(2) {
        (samples[middle - 1].as_secs_f64() + samples[middle].as_secs_f64()) / 2.0
    } else {
        samples[middle].as_secs_f64()
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut baseline = Vec::new();
    let mut candidate = Vec::new();
    let mut expected = None;
    for round in 0..args.runs.get() {
        let order = if round.is_multiple_of(2) {
            [false, true]
        } else {
            [true, false]
        };
        for is_candidate in order {
            let binary = if is_candidate {
                &args.candidate
            } else {
                &args.baseline
            };
            let (elapsed, report, status) = scan(binary, &args.roots)?;
            if let Some((previous, previous_status)) = &expected {
                if &report != previous || &status != previous_status {
                    bail!(
                        "scan results or exit codes differ; compare reports and rerun on stable roots"
                    );
                }
            } else {
                let repositories = report["repositories"]
                    .as_array()
                    .context("expected repositories")?;
                let worktrees: usize = repositories
                    .iter()
                    .filter_map(|repo| repo["worktrees"].as_array())
                    .map(Vec::len)
                    .sum();
                println!(
                    "{} repositories, {worktrees} worktrees; exit code {status:?}",
                    repositories.len()
                );
                expected = Some((report, status));
            }
            let name = if is_candidate {
                "candidate"
            } else {
                "baseline"
            };
            println!("run {} {name}: {:.3}s", round + 1, elapsed.as_secs_f64());
            if is_candidate {
                candidate.push(elapsed);
            } else {
                baseline.push(elapsed);
            }
        }
    }
    let baseline = median(&mut baseline);
    let candidate = median(&mut candidate);
    println!(
        "median: baseline {baseline:.3}s, candidate {candidate:.3}s; {:.1}% less time",
        (1.0 - candidate / baseline) * 100.0
    );
    for (name, binary) in [("baseline", args.baseline), ("candidate", args.candidate)] {
        println!("{name} binary: {} bytes", std::fs::metadata(binary)?.len());
    }
    Ok(())
}
