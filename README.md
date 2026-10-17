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

## Flag meaning

| Flag | Meaning |
| --- | --- |
| `M` | Tracked changes or conflicts |
| `?` | Untracked files |
| `S` | Shared stashes, shown once per repository |
| `P` | Commits not contained in any qualifying remote branch |
| `D` | Eligible for deletion under the selected policy |
| `U` | Clean, remotely backed up work that is not integrated into mainline |
| `I` | Ignored files; neutral and not a deletion blocker by default |
| `!` | An uncertain check, operation, lock, or error; details appear below |

`P` checks all local branches, including branches without a worktree. It does not
mean "behind upstream" and does not require an upstream to be configured. A
commit contained in a qualifying remote branch counts as backed up even if that
remote branch has a different name.

## Install and run

Requires Rust 1.91 or newer and Git on `PATH`.

```sh
make install
git-unpushed --directory ~/src
git-unpushed ~/src ~/other-projects
git-unpushed --all ~/src
git-unpushed --removable ~/src
git-unpushed --deep ~/src
git-unpushed --json ~/src
git-unpushed --stream --progress ~/src
```

`make install` builds the release binary and installs it into Cargo's bin directory
(normally `~/.cargo/bin`, which should be on `PATH`). It also replaces a previous
installation. Set `CARGO_INSTALL_ROOT` to choose a different installation root;
the executable goes into its `bin` subdirectory. `make` alone builds the release
binary without installing it.

With no roots, the current directory is scanned. Discovery stops descending once
it finds a repository; independently nested repositories and submodules are not
separate scan roots unless explicitly supplied. Git's worktree registry discovers
linked worktrees regardless of where
they are stored. Repeated and overlapping roots are deduplicated by the canonical
shared Git directory. Bare repositories can be scanned and can own linked
worktrees.

## Mainline and remote policy

Normal runs are offline and trust the locally stored remote-tracking refs.
`--fetch` explicitly refreshes eligible remotes before evaluating the repository.
The stored refs may differ from the current state on a server; offline deletion
uses that same snapshot policy.

By default, integration into **any qualifying remote's default branch** is enough.
The remote's recorded symbolic `HEAD` identifies its default branch. If that is
absent, a unique remote `main` or `master` is used; ambiguous or missing defaults
remain unknown. To restrict or override the selection:

```sh
git-unpushed --remote upstream ~/src
git-unpushed --remote origin --base origin/trunk ~/src
git-unpushed --fetch ~/src
```

Only nonlocal network remotes qualify. Filesystem paths, `file://` URLs, loopback
endpoints, and unrecognized remote helpers do not establish remote backup. URL
rewrites are resolved before classification. Unsupported or ambiguous fetch
refspec mappings are reported instead of being treated as proof. An endpoint's
URL is a policy check, not proof of physically independent storage.

### Squash merges and copied commits

Normal scans check commit ancestry first, then use a fast exact comparison for
clean, remotely backed-up linked worktrees. The comparison looks for a mainline
commit with exactly the same changed paths, file modes, and complete before/after
file contents as the branch's combined changes. A verified match can show `D`
even though squash merging replaced the original commit IDs.

The default fast check also recognizes branches whose commits were copied or
rebased individually onto main. Every branch-only commit in a linear series must
have exactly matching file changes on the same qualifying mainline. A partially
copied series, or changes scattered across different remotes' mainlines, do not
qualify. Merge histories and empty-commit evidence remain conservative. Branch
histories are read in batches, shared commits are checked once per mainline, and
the existing exact mainline index is reused; no patch-ID search is needed.

Use `--deep` to find additional squash matches when main had unrelated changes
inside the same files:

```sh
git-unpushed --deep ~/src
```

The deeper search uses patch IDs only to find candidates. Each additional match
must pass an exact check: applying the branch's full changes to the squash
parent, without ignoring or fixing whitespace, must reproduce the squash
commit's entire file tree. Verification uses temporary Git data and leaves the
checkout, real index, and refs alone. Uncertain or nonmatching worktrees stay `U`.
All ordinary cleanliness and protection checks still apply.

`--deep` is slower and opt-in. Mainline indexes are shared across worktrees in
each scan; they are not persistently cached between runs. In the audited `ux`
repository, the exact comparison found ten additional matches in about 0.5
seconds; the deeper search found three more, taking about seven seconds to index
the mainline patches. Both checks establish historical integration, just like
ancestry; a later revert does not undo that historical evidence.

## Delete worktrees

```sh
# Preview all eligible linked worktrees attached to discovered repositories.
git-unpushed --delete --dry-run ~/src

# Remove those worktrees, retaining their branches.
git-unpushed --delete ~/src

# Also allow clean, backed-up worktrees whose commits are not in mainline yet.
git-unpushed --delete --include-unmerged ~/src

# Preserve worktrees that contain ignored files.
git-unpushed --delete --keep-ignored ~/src

# Explicitly refresh the remote evidence before deletion.
git-unpushed --delete --fetch ~/src
```

The default deletion rule is: a linked worktree is clean and its current HEAD is
provably integrated into a qualifying remote mainline. Exact ancestry, including
equal commits, a verified squash match, or a fully matched copied commit series
establishes proof. Squash and copied-series matching also
requires the original HEAD to be backed up remotely. `--deep` enables the broader
squash search for both the listing and deletion revalidation; a patch ID alone
never establishes eligibility.
`--include-unmerged` broadens the rule to any qualifying remote branch.

Deletion affects **all eligible linked worktrees belonging to discovered
repositories**, even outside the search roots. It removes the checkout and its
Git worktree registration, **not local branches or the shared repository**.
Ignored files inside removed worktrees are deleted by default. `--keep-ignored`
skips those worktrees entirely.

Primary checkouts, locked worktrees, the current working directory, operations in
progress, initialized submodules, and uncertain checks are protected. Worktrees
with assume-unchanged or skip-worktree index entries are conservatively protected,
since ordinary Git status can hide edits to those files. Untracked
files and tracked edits block deletion regardless of Git configuration that might
normally hide them. Working-copy state and eligibility are checked again
immediately before native, non-forced Git worktree removal. A HEAD or branch
change during the scan skips that worktree. Removing a worktree can discard its
private reflog; the assessment covers its current HEAD and files, not every
historically recoverable state.

Synthetic ancestry from replacement refs or grafts is excluded from proof. In
shallow repositories, exact HEAD/remote-tip equality can establish eligibility;
other ancestry remains unknown rather than risking an incorrect positive result.

## Performance and output

Discovery uses filesystem checks; Git is invoked for repository candidates rather
than every ordinary directory. Shared refs and stashes are read once per repository,
history checks are batched, and file status is inspected with bounded parallelism.
Per-scan reuse avoids repeated history walks. No persistent cached "clean" state is
used to authorize deletion. Ordinary scans allow Git to refresh its normal index
stat cache, just as `git status` does. This avoids repeatedly rehashing unchanged
files, particularly expensive Git LFS files, without staging changes. Use
`--no-index-refresh` to suppress those optional metadata writes for background
scans. Deletion revalidation always suppresses optional index writes. Existing Git
filesystem-monitor and untracked-cache settings can further accelerate file scans.

```sh
git-unpushed --jobs 8 --timings ~/src
```

The target is roughly one second for repeated scans of a few hundred repositories
or worktrees. Actual duration depends on checkout size, untracked files, filesystem,
and Git configuration; `--timings` reports discovery and scan time separately.

On this development machine, release-build scans with eight workers took
0.75–0.77 seconds for 100 tiny repositories plus 100 linked worktrees, and
1.36–1.44 seconds for 200 repositories plus 200 linked worktrees. These synthetic
fixtures include untracked and ignored files; they do not model large checkouts.

A real collection of 200 repositories and 1,656 registered worktrees took about
124 seconds with index refresh disabled. After enabling normal Git stat-cache
refresh, a cache-warming run took 112 seconds and a repeat took 20 seconds. Its
first streamed rows appeared after 0.32 seconds. Git LFS content checks dominated
the slow run; total time at this scale is still substantially above one second.

Interactive terminal reports **stream completed repository groups immediately**.
A live indicator on stderr updates every 100 milliseconds with elapsed time,
repositories completed, worktrees checked, and a shortened path, including while
a slow Git command is still busy. The indicator is cleared around streamed rows
so it does not overwrite results. Repository groups appear in completion order;
`--stream=never` buffers the report and sorts it by repository path.

```sh
git-unpushed ~/src                         # automatic streaming and live progress
git-unpushed --stream --progress ~/src      # force both, including with redirected output
git-unpushed --progress=never ~/src         # disable the indicator
git-unpushed --stream=never ~/src           # collect and sort the entire report
```

Streaming and progress are automatic only on their respective terminal streams.
Redirected stdout stays buffered and sorted by default; ordinary nonterminal runs
emit no progress escape codes. JSON is always a single complete document even
with `--stream`; explicitly requested progress stays on stderr. Color is automatic
for terminals, disabled for pipes and `NO_COLOR`, and controllable with
`--color always|never|auto`. Paths and labels escape control characters.
`--json` returns complete structured results regardless of terminal visibility
filters. Exit status is 0 for a successful scan, including dirty work; 1 for
operational errors or failed removals; and 2 for invalid CLI arguments. One
repository's failure does not stop the rest of the report.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

Tests use disposable local Git repositories and worktrees. Remote-backed fixtures
use stored refs without contacting any network endpoint.
