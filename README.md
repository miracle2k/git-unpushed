# git-unpushed

A CLI tool for finding unfinished work (dirty files, unpushed commits) across many
repositories. It supports inspecting worktrees and identifying integrated ones that
can be deleted.

```text
~/src/api:                 S
  ↳ auth:                 M ?
  ↳ docs:                 D I
  ↳ caching:              U
~/src/website:             M P
```

Repositories appear as parent rows with their linked worktrees underneath.
Only items needing attention are shown by default.

## Install

Building requires Rust 1.91 or newer. Normal scans and remote refreshes use Gix
inside the process; run `git-unpushed` directly when Git is not installed.
Git is still needed for `--deep`, worktree deletion, and compatibility fallbacks
such as reftable repositories. Configured filters, credential helpers, and SSH
transports can also require external programs.

```sh
make install
```

Installs `git-unpushed` into Cargo's bin directory, normally `~/.cargo/bin`.
With that directory on `PATH`, run it as `git unpushed` or `git-unpushed`.

## Usage

```sh
git unpushed ~/src ~/other-projects
git unpushed --all ~/src          # Include clean repositories and worktrees
git unpushed --removable ~/src    # Show cleanup candidates
git unpushed --deep ~/src         # Find more squash-merged worktrees; slower
git unpushed --fetch ~/src        # Refresh remote refs before checking
git unpushed --json ~/src         # Full structured report
```

With no paths, scans the current directory. Linked worktrees are included even
when they live outside the search roots. Discovery stops at each repository;
independently nested repositories need to be supplied explicitly.

## Status flags

| Flag | Meaning |
| --- | --- |
| `M` | Tracked changes or conflicts |
| `?` | Untracked files |
| `S` | Shared stashes, shown once per repository |
| `P` | Commits not backed up on any qualifying remote branch |
| `D` | Linked worktree eligible for deletion |
| `U` | Clean and backed up, but integration into mainline is not confirmed |
| `I` | Ignored files; does not block deletion by default |
| `!` | Error, uncertain check, lock, or operation in progress; details follow |

`P` checks all local branches, including those without a worktree. It does not
require a configured upstream, and being behind a remote does not count as
unpushed work. Only nonlocal network remotes qualify as backup.

## Integration checks

Scans are **offline by default**, using locally stored remote refs. `--fetch`
refreshes those refs first.

Integration is checked against qualifying remotes' default branches, falling back
to an unambiguous `main` or `master`. Override the remote or mainline with:

```sh
git unpushed --remote upstream ~/src
git unpushed --remote origin --base origin/trunk ~/src
```

Default checks recognize commit ancestry, exact squash matches, and individually
copied or rebased commit series. `--deep` adds slower, verified squash matching.
Unconfirmed work stays `U`; that does not necessarily mean it was never merged.

## Delete worktrees

```sh
git unpushed --delete --dry-run ~/src  # Preview
git unpushed --delete ~/src           # Remove eligible worktrees
git unpushed --delete --no-delete-size ~/src  # Count files without collecting sizes
```

Deletion removes checkout directories and their worktree registrations, retaining
local branches and the shared repository. It runs **without prompting** and
applies to **all eligible linked worktrees attached to discovered repositories**,
including those outside the supplied paths.

By default, worktrees must be clean and provably integrated into a qualifying
remote mainline. Dirty, locked, uncertain, or otherwise protected worktrees are
skipped, as are primary checkouts and the worktree containing the current working
directory. Eligibility is checked again immediately before removal.

Deletion ends with a total such as `Deleted 3 worktrees · 42817 files · 1.8 GiB`.
Totals include checkout files, ignored files, symbolic links, and the removed
worktree registrations. Branches and shared repository objects are retained.
File counts come from successful removals; partial failures contribute only the
files already removed. `--json` includes per-worktree `deletion.stats` and a
report-wide `deletion_summary`.

Byte totals are logical file lengths measured before removal, including link
lengths, and can differ from disk space recovered with sparse files, hard links,
compression, or snapshots. `--no-delete-size` skips file-size collection and
reports only file and worktree counts (`bytes: null` in JSON). Dry runs show
estimated totals without deleting files. Measurement and removal do not follow
symbolic links or cross mounted filesystems.

| Option | Effect |
| --- | --- |
| `--keep-ignored` | Skip entire worktrees containing ignored files; otherwise those files are deleted too |
| `--include-unmerged` | Also allow clean worktrees backed up remotely but not integrated into mainline |
| `--deep` | Use the broader integration checks when assessing eligibility |
| `--fetch` | Refresh remote refs before assessing eligibility |

## Output and options

Terminal output streams completed repositories with a live progress indicator.
Use `--stream=never` for a buffered report sorted by path, or `--progress=never`
to hide the indicator. Piped output is buffered and sorted by default.

`--json` always emits a complete report. Exit codes are `0` for a successful scan
(including dirty work), `1` for operational errors, and `2` for invalid arguments.

See `git-unpushed --help` for all options, including parallelism (`--jobs`),
timings (`--timings`), and color controls. Timings include the number of native
Git commands attempted, so compatibility fallbacks are visible.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

To compare two release builds on stable repositories without refreshing their
indexes or fetching remotes:

```sh
cargo run --release --example benchmark_scan -- /path/to/baseline /path/to/candidate /path/to/repos
```

The helper alternates the binaries, checks that their JSON reports and exit codes
match (excluding elapsed time), and reports median scan times and binary sizes.
Use `--runs 5` for more samples. Close other tools that modify the scanned
repositories during the comparison.

To compare native Git removal, removal with file counts, and removal with file
and byte counts on disposable worktrees:

```sh
cargo run --release --example benchmark_removal -- --files 10000 --runs 5
```

This creates its own temporary repository, rotates the three modes, and excludes
fixture creation from the timings. It does not remove any existing worktrees.
