import os
import click
from git import Repo, InvalidGitRepositoryError

def check_git_repo_conditions(repo_path):
    try:
        repo = Repo(repo_path)
        conditions = {
            'uncommitted_changes': bool(repo.is_dirty()),
            'untracked_files': bool(repo.untracked_files),
            'stashes': bool(repo.git.stash('list')),
            'unsynced_branches': False
        }

        # Check for local branches not synced with default remote
        for branch in repo.branches:
            try:
                tracked_branch = branch.tracking_branch()
                if tracked_branch is None or (branch.commit != tracked_branch.commit):
                    conditions['unsynced_branches'] = True
                    break
            except TypeError:
                continue

        return conditions
    except InvalidGitRepositoryError:
        return None

def recurse_directories(directory):
    results = {}
    for root, dirs, files in os.walk(directory, topdown=True):
        git_conditions = check_git_repo_conditions(root)
        if git_conditions is not None:
            # This directory is a Git repo, don't recurse further into it
            results[root] = git_conditions
            dirs[:] = []  # Don't recurse into subdirectories
        else:
            # Not a Git repo, continue recursion
            continue
    return results

def format_condition(char, status):
    """Format the condition status with appropriate color and symbol."""
    if status:
        return click.style(char, fg='red')
    else:
        return click.style(char, fg='green')

@click.command()
@click.option('--directory', default=os.getcwd(), help='Directory to check')
def main(directory):
    results = recurse_directories(directory)
    for dir_path, conditions in results.items():
        # Format the output for each condition
        modified = format_condition("M", conditions['uncommitted_changes'])
        untracked = format_condition("?", conditions['untracked_files'])
        stash = format_condition("S", conditions['stashes'])
        not_pushed = format_condition("P", conditions['unsynced_branches'])
        
        # Print the directory status on one line
        status_line = f"{dir_path}: {modified} {untracked} {stash} {not_pushed}"
        print(status_line)

if __name__ == "__main__":
    main()