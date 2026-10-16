use crate::model::{DeletionStatus, RepositoryReport, ScanReport, WorktreeReport};
use std::collections::HashMap;
use std::fmt::Write;
use std::path::Path;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    pub all: bool,
    pub removable_only: bool,
    pub color: bool,
}

pub fn repository_label_width(path: &Path) -> usize {
    UnicodeWidthStr::width(short_path(path).as_str()) + 1
}

#[derive(Clone, Copy)]
enum Tone {
    Preserve,
    Remove,
    Review,
    Neutral,
}

struct Row {
    label: String,
    flags: Vec<(&'static str, Tone)>,
    suffix: Option<String>,
    details: Vec<String>,
}

/// Render one stable, compact report. The caller decides whether color is appropriate.
pub fn render(report: &ScanReport, options: RenderOptions) -> String {
    let mut rows = Vec::new();
    for repository in &report.repositories {
        repository_rows(repository, options, &mut rows);
    }

    let mut output = render_rows(&rows, row_width(&rows), options.color);
    for error in &report.errors {
        styled(&mut output, "!", Tone::Review, options.color);
        output.push(' ');
        output.push_str(&safe_text(error));
        output.push('\n');
    }
    if rows.is_empty() && report.errors.is_empty() {
        let worktrees = report
            .repositories
            .iter()
            .flat_map(|repository| &repository.worktrees)
            .filter(|worktree| !worktree.bare)
            .count();
        let message = if options.removable_only {
            "no removable worktrees"
        } else {
            "nothing needs attention"
        };
        let _ = writeln!(
            output,
            "{} repositories, {worktrees} worktrees; {message}.",
            report.repositories.len()
        );
    }
    output
}

/// Render a completed repository without a report-wide summary or errors.
/// `width` is a minimum label width, so streamed groups can share stable columns.
pub fn render_repository(
    repository: &RepositoryReport,
    options: RenderOptions,
    width: usize,
) -> String {
    let mut rows = Vec::new();
    repository_rows(repository, options, &mut rows);
    render_rows(&rows, width.max(row_width(&rows)), options.color)
}

/// Return the widest visible label in a repository, measured in terminal columns.
pub fn repository_width(repository: &RepositoryReport, options: RenderOptions) -> usize {
    let mut rows = Vec::new();
    repository_rows(repository, options, &mut rows);
    row_width(&rows)
}

fn row_width(rows: &[Row]) -> usize {
    rows.iter()
        .map(|row| UnicodeWidthStr::width(row.label.as_str()))
        .max()
        .unwrap_or(0)
}

fn render_rows(rows: &[Row], width: usize, color: bool) -> String {
    let mut output = String::new();
    for row in rows {
        output.push_str(&row.label);
        if !row.flags.is_empty() || row.suffix.is_some() {
            let padding = width - UnicodeWidthStr::width(row.label.as_str()) + 1;
            output.extend(std::iter::repeat_n(' ', padding));
        }
        for (index, (flag, tone)) in row.flags.iter().enumerate() {
            if index > 0 {
                output.push(' ');
            }
            styled(&mut output, flag, *tone, color);
        }
        if let Some(suffix) = &row.suffix {
            if !row.flags.is_empty() {
                output.push_str(" · ");
            }
            output.push_str(suffix);
        }
        output.push('\n');
        for detail in &row.details {
            let indent = if row.label.starts_with("  ↳ ") {
                "    "
            } else {
                "  "
            };
            output.push_str(indent);
            styled(&mut output, "!", Tone::Review, color);
            output.push(' ');
            output.push_str(detail);
            output.push('\n');
        }
    }
    output
}

fn repository_rows(repository: &RepositoryReport, options: RenderOptions, rows: &mut Vec<Row>) {
    let primary = repository
        .worktrees
        .iter()
        .find(|worktree| worktree.primary);
    let children: Vec<_> = repository
        .worktrees
        .iter()
        .filter(|worktree| !worktree.primary && !worktree.bare)
        .collect();
    let mut leaf_counts = HashMap::new();
    for child in &children {
        *leaf_counts.entry(leaf_name(&child.path)).or_insert(0usize) += 1;
    }
    let visible_children: Vec<_> = children
        .into_iter()
        .filter(|worktree| {
            if options.removable_only {
                worktree.removable || worktree.deletion.is_some() || !worktree.errors.is_empty()
            } else {
                options.all || worktree.needs_attention() || worktree_unknown(worktree)
            }
        })
        .collect();

    let mut root = Row {
        label: format!("{}:", short_path(&repository.path)),
        flags: primary.map(worktree_flags).unwrap_or_default(),
        suffix: primary.and_then(worktree_suffix),
        details: primary.map(worktree_details).unwrap_or_default(),
    };
    if repository.stashes {
        insert_flag(&mut root.flags, "S", Tone::Preserve);
    }
    if !repository.unpushed_branches.is_empty() {
        insert_flag(&mut root.flags, "P", Tone::Preserve);
    }
    if !repository.unknown_branches.is_empty() || !repository.errors.is_empty() {
        insert_flag(&mut root.flags, "!", Tone::Review);
    }
    if !repository.unknown_branches.is_empty() {
        root.details.push(format!(
            "cannot check branches: {}",
            repository
                .unknown_branches
                .iter()
                .map(|branch| safe_text(branch))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    root.details
        .extend(repository.errors.iter().map(|error| safe_text(error)));
    let show_root = !visible_children.is_empty()
        || !root.details.is_empty()
        || (!options.removable_only
            && (options.all
                || repository.stashes
                || !repository.unpushed_branches.is_empty()
                || primary.is_some_and(|worktree| {
                    !worktree.bare && (worktree.needs_attention() || worktree_unknown(worktree))
                })));
    if !show_root {
        return;
    }
    if options.all && root.flags.is_empty() && root.suffix.is_none() {
        root.flags.push(("-", Tone::Neutral));
    }
    root.flags.sort_by_key(|(flag, _)| match *flag {
        "M" => 0,
        "?" => 1,
        "S" => 2,
        "P" => 3,
        "D" => 4,
        "U" => 5,
        "I" => 6,
        _ => 7,
    });
    rows.push(root);
    for worktree in visible_children {
        let leaf = leaf_name(&worktree.path);
        let mut label = if leaf_counts.get(&leaf).copied().unwrap_or(0) > 1 {
            short_path(&worktree.path)
        } else {
            safe_text(&leaf)
        };
        if let Some(branch) = &worktree.branch {
            if branch != &leaf {
                label.push_str(" [");
                label.push_str(&safe_text(branch));
                label.push(']');
            }
        } else if worktree.head.is_some() {
            label.push_str(" [detached]");
        }
        let mut flags = worktree_flags(worktree);
        let suffix = worktree_suffix(worktree);
        if options.all && flags.is_empty() && suffix.is_none() {
            flags.push(("-", Tone::Neutral));
        }
        rows.push(Row {
            label: format!("  ↳ {label}:"),
            flags,
            suffix,
            details: worktree_details(worktree),
        });
    }
}

fn worktree_flags(worktree: &WorktreeReport) -> Vec<(&'static str, Tone)> {
    if worktree.bare {
        return Vec::new();
    }
    let mut flags = Vec::new();
    if worktree.modified || worktree.conflicts {
        flags.push(("M", Tone::Preserve));
    }
    if worktree.untracked {
        flags.push(("?", Tone::Preserve));
    }
    if worktree.backed_up == Some(false) {
        flags.push(("P", Tone::Preserve));
    }
    if worktree.removable {
        flags.push(("D", Tone::Remove));
    } else if !worktree.modified
        && !worktree.conflicts
        && !worktree.untracked
        && worktree.integrated == Some(false)
        && worktree.backed_up == Some(true)
    {
        flags.push(("U", Tone::Review));
    }
    if worktree.ignored {
        flags.push(("I", Tone::Neutral));
    }
    if worktree_unknown(worktree)
        || worktree.locked
        || worktree.prunable
        || worktree.operation.is_some()
        || !worktree.errors.is_empty()
        || worktree.deletion.as_ref().is_some_and(|deletion| {
            matches!(
                deletion.status,
                DeletionStatus::Skipped | DeletionStatus::Failed
            )
        })
    {
        flags.push(("!", Tone::Review));
    }
    flags
}

fn worktree_unknown(worktree: &WorktreeReport) -> bool {
    !worktree.bare
        && (worktree.head.is_none()
            || worktree.integrated.is_none()
            || worktree.backed_up.is_none())
}

fn worktree_suffix(worktree: &WorktreeReport) -> Option<String> {
    worktree.deletion.as_ref().map(|deletion| {
        let action = match deletion.status {
            DeletionStatus::Deleted => "deleted",
            DeletionStatus::WouldDelete => "would delete",
            DeletionStatus::Skipped => "skipped",
            DeletionStatus::Failed => "failed",
        };
        match &deletion.reason {
            Some(reason) => format!("{action}: {}", safe_text(reason)),
            None => action.to_owned(),
        }
    })
}

fn worktree_details(worktree: &WorktreeReport) -> Vec<String> {
    let mut details = Vec::new();
    if worktree.locked {
        details.push("locked".to_owned());
    }
    if worktree.prunable {
        details.push("missing worktree; registration is prunable".to_owned());
    }
    if let Some(operation) = &worktree.operation {
        details.push(format!("{} in progress", safe_text(operation)));
    }
    if worktree.conflicts {
        details.push("unresolved conflicts".to_owned());
    }
    if worktree_unknown(worktree) && worktree.errors.is_empty() {
        details.push(
            if worktree.head.is_none() {
                "HEAD is unborn or unknown"
            } else {
                match (worktree.integrated.is_none(), worktree.backed_up.is_none()) {
                    (true, true) => "cannot establish remote integration or backup",
                    (true, false) => "cannot establish remote integration",
                    (false, true) => "cannot establish remote backup",
                    (false, false) => unreachable!(),
                }
            }
            .to_owned(),
        );
    }
    details.extend(worktree.errors.iter().map(|error| safe_text(error)));
    details.dedup();
    details
}

fn insert_flag(flags: &mut Vec<(&'static str, Tone)>, flag: &'static str, tone: Tone) {
    if !flags.iter().any(|(existing, _)| *existing == flag) {
        flags.push((flag, tone));
    }
}

fn styled(output: &mut String, text: &str, tone: Tone, color: bool) {
    if color {
        output.push_str(match tone {
            Tone::Preserve => "\x1b[31m",
            Tone::Remove => "\x1b[32m",
            Tone::Review => "\x1b[33m",
            Tone::Neutral => "\x1b[90m",
        });
    }
    output.push_str(text);
    if color {
        output.push_str("\x1b[0m");
    }
}

fn leaf_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn short_path(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = Path::new(&home);
        if home.is_absolute()
            && home.parent().is_some()
            && let Ok(relative) = path.strip_prefix(home)
        {
            return if relative.as_os_str().is_empty() {
                "~".to_owned()
            } else {
                format!("~/{}", safe_text(&relative.to_string_lossy()))
            };
        }
    }
    safe_text(&path.to_string_lossy())
}

fn safe_text(value: &str) -> String {
    let mut result = String::new();
    for character in value.chars() {
        if character.is_control()
            || matches!(character, '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            result.extend(character.escape_default());
        } else {
            result.push(character);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DeletionResult, RepositoryReport, ScanReport, WorktreeReport};
    use std::path::PathBuf;

    fn worktree(path: &str, primary: bool) -> WorktreeReport {
        WorktreeReport {
            path: PathBuf::from(path),
            branch: Some(leaf_name(Path::new(path))),
            head: Some("123456".to_owned()),
            primary,
            bare: false,
            locked: false,
            prunable: false,
            modified: false,
            untracked: false,
            ignored: false,
            conflicts: false,
            operation: None,
            integrated: Some(true),
            backed_up: Some(true),
            removable: !primary,
            reason: None,
            errors: Vec::new(),
            deletion: None,
        }
    }

    fn report(worktrees: Vec<WorktreeReport>) -> ScanReport {
        ScanReport {
            repositories: vec![RepositoryReport {
                path: PathBuf::from("/projects/api"),
                common_dir: PathBuf::from("/projects/api/.git"),
                bare: false,
                stashes: false,
                unpushed_branches: Vec::new(),
                unknown_branches: Vec::new(),
                remotes: Vec::new(),
                fetched: false,
                worktrees,
                errors: Vec::new(),
            }],
            errors: Vec::new(),
            elapsed_ms: 1,
        }
    }

    fn plain() -> RenderOptions {
        RenderOptions {
            all: false,
            removable_only: false,
            color: false,
        }
    }

    #[test]
    fn sparse_report_groups_worktrees_and_distinguishes_the_two_types_of_attention() {
        let primary = worktree("/projects/api", true);
        let mut dirty = worktree("/worktrees/auth", false);
        dirty.modified = true;
        dirty.untracked = true;
        dirty.backed_up = Some(false);
        dirty.integrated = Some(false);
        dirty.removable = false;
        let mut integrated = worktree("/worktrees/docs", false);
        integrated.ignored = true;
        let mut unintegrated = worktree("/worktrees/caching", false);
        unintegrated.removable = false;
        unintegrated.integrated = Some(false);
        let output = render(
            &report(vec![primary, dirty, integrated, unintegrated]),
            plain(),
        );
        assert_eq!(
            output,
            "/projects/api:\n  ↳ auth:      M ? P\n  ↳ docs:      D I\n  ↳ caching:   U\n"
        );
        assert!(!output.contains('\x1b'));
    }

    #[test]
    fn clean_roots_are_hidden_and_all_reveals_them() {
        let report = report(vec![worktree("/projects/api", true)]);
        assert_eq!(
            render(&report, plain()),
            "1 repositories, 1 worktrees; nothing needs attention.\n"
        );
        assert_eq!(
            render(
                &report,
                RenderOptions {
                    all: true,
                    ..plain()
                }
            ),
            "/projects/api: -\n"
        );
    }

    #[test]
    fn streamed_repository_hides_clean_groups_without_an_empty_summary() {
        let report = report(vec![worktree("/projects/api", true)]);
        let repository = &report.repositories[0];
        assert_eq!(render_repository(repository, plain(), 40), "");
        assert_eq!(repository_width(repository, plain()), 0);
    }

    #[test]
    fn streamed_repository_uses_shared_columns_and_preserves_worktree_details() {
        let primary = worktree("/projects/api", true);
        let mut dirty = worktree("/worktrees/auth", false);
        dirty.modified = true;
        dirty.removable = false;
        dirty.locked = true;
        let report = report(vec![primary, dirty]);
        let repository = &report.repositories[0];
        assert_eq!(repository_width(repository, plain()), 14);
        assert_eq!(
            render_repository(repository, plain(), 20),
            "/projects/api:\n  ↳ auth:            M !\n    ! locked\n"
        );
        assert_eq!(
            render_repository(repository, plain(), 0),
            render(&report, plain())
        );
    }

    #[test]
    fn streamed_repository_measures_unicode_labels_in_terminal_columns() {
        let linked = worktree("/worktrees/中文", false);
        let report = report(vec![linked]);
        let repository = &report.repositories[0];
        let output = render_repository(repository, plain(), 14);
        assert!(output.contains("  ↳ 中文:      D\n"));
        assert_eq!(repository_width(repository, plain()), 14);
    }

    #[test]
    fn removable_filter_excludes_preserved_work_and_keeps_deletion_results() {
        let mut dirty = worktree("/worktrees/auth", false);
        dirty.modified = true;
        dirty.removable = false;
        let mut removed = worktree("/worktrees/docs", false);
        removed.deletion = Some(DeletionResult {
            status: DeletionStatus::Deleted,
            reason: None,
        });
        let output = render(
            &report(vec![worktree("/projects/api", true), dirty, removed]),
            RenderOptions {
                removable_only: true,
                ..plain()
            },
        );
        assert!(!output.contains("auth"));
        assert!(output.contains("docs:"));
        assert!(output.contains("D · deleted"));
    }

    #[test]
    fn controls_are_escaped_and_duplicate_leaf_names_are_disambiguated() {
        let first = worktree("/worktrees/one/docs", false);
        let mut second = worktree("/worktrees/two/docs", false);
        second.branch = Some("feature\x1b[31m\n中文".to_owned());
        let output = render(&report(vec![first, second]), plain());
        assert!(output.contains("/worktrees/one/docs"));
        assert!(output.contains("/worktrees/two/docs"));
        assert!(output.contains("feature\\u{1b}[31m\\n中文"));
        assert!(!output.contains('\x1b'));
    }

    #[test]
    fn ignored_files_are_neutral_and_do_not_make_a_clean_primary_visible() {
        let mut primary = worktree("/projects/api", true);
        primary.ignored = true;
        let mut linked = worktree("/worktrees/docs", false);
        linked.ignored = true;
        let output = render(
            &report(vec![primary.clone(), linked]),
            RenderOptions {
                color: true,
                ..plain()
            },
        );
        assert!(output.contains("\x1b[90mI\x1b[0m"));
        assert!(!output.contains("\x1b[31mI"));
        assert!(!render(&report(vec![primary]), plain()).contains("I"));
    }

    #[test]
    fn errors_remain_visible_when_filtering_and_untrusted_text_cannot_inject_lines() {
        let mut report = report(vec![worktree("/projects/api", true)]);
        report.repositories[0]
            .errors
            .push("permission denied\n\x1b[2J".to_owned());
        report.errors.push("discovery failed\rpath".to_owned());
        let output = render(
            &report,
            RenderOptions {
                removable_only: true,
                ..plain()
            },
        );
        assert!(output.contains("! permission denied\\n\\u{1b}[2J"));
        assert!(output.contains("! discovery failed\\rpath"));
        assert!(!output.contains('\x1b'));
    }

    #[test]
    fn unknown_status_explains_missing_evidence_instead_of_removal_protection() {
        let mut primary = worktree("/projects/api", true);
        primary.integrated = None;
        primary.reason = Some("primary checkout".to_owned());
        let output = render(&report(vec![primary]), plain());
        assert!(output.contains("! cannot establish remote integration"));
        assert!(!output.contains("primary checkout"));
    }
}
