use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const REFRESH: Duration = Duration::from_millis(100);
const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const CLEAR_LINE: &str = "\r\x1b[2K";

enum Stage {
    Discovering {
        directories: usize,
        repositories: usize,
    },
    Scanning {
        done: usize,
        total: usize,
        worktrees: usize,
    },
    Deleting {
        done: usize,
        total: usize,
    },
}

struct State {
    stage: Stage,
    current: String,
    home: Option<PathBuf>,
    started: Instant,
    frame: usize,
    width: usize,
    finished: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

/// A single live stderr line. Callers must use `with_output` when streaming
/// stdout so a finished report row and an animation frame cannot interleave.
pub struct Progress {
    shared: Option<Arc<Shared>>,
    worker: Option<JoinHandle<()>>,
}

impl Progress {
    pub fn new(enabled: bool) -> Self {
        if !enabled {
            return Self {
                shared: None,
                worker: None,
            };
        }
        // Reserve the last column to avoid wrapping the progress line. COLUMNS
        // is optional (as it is in many non-interactive child environments).
        let width = std::env::var("COLUMNS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|width| *width > 1)
            .unwrap_or(80)
            .saturating_sub(1);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                stage: Stage::Discovering {
                    directories: 0,
                    repositories: 0,
                },
                current: String::new(),
                home: std::env::var_os("HOME").map(PathBuf::from),
                started: Instant::now(),
                frame: 0,
                width,
                finished: false,
            }),
            wake: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("scan-progress".to_owned())
            .spawn(move || animate(&worker_shared))
            .ok();
        Self {
            shared: worker.as_ref().map(|_| shared),
            worker,
        }
    }

    pub fn discovering(&self, directories: usize, repositories: usize, current: &Path) {
        self.update(
            Stage::Discovering {
                directories,
                repositories,
            },
            current,
        );
    }

    pub fn scanning(&self, done: usize, total: usize, worktrees: usize, current: &Path) {
        self.update(
            Stage::Scanning {
                done,
                total,
                worktrees,
            },
            current,
        );
    }

    pub fn deleting(&self, done: usize, total: usize, current: &Path) {
        self.update(Stage::Deleting { done, total }, current);
    }

    fn update(&self, stage: Stage, current: &Path) {
        let Some(shared) = &self.shared else {
            return;
        };
        let mut state = lock(&shared.state);
        if !state.finished {
            state.stage = stage;
            state.current = display_path(current, state.home.as_deref());
        }
    }

    /// Clear the indicator, write and flush stdout in `output`, then redraw.
    /// The same lock is held by the animation thread while it writes stderr.
    pub fn with_output<F, T>(&self, output: F) -> T
    where
        F: FnOnce() -> T,
    {
        let Some(shared) = &self.shared else {
            return output();
        };
        let state = lock(&shared.state);
        clear();
        let result = output();
        if !state.finished {
            draw(&state);
        }
        result
    }

    /// Stop immediately, join the animation thread, and leave no progress line.
    pub fn finish(&mut self) {
        if let Some(shared) = self.shared.take() {
            let mut state = lock(&shared.state);
            state.finished = true;
            shared.wake.notify_one();
            clear();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.finish();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn animate(shared: &Shared) {
    let mut state = lock(&shared.state);
    loop {
        if state.finished {
            break;
        }
        draw(&state);
        state.frame = (state.frame + 1) % FRAMES.len();
        let (next, _) = shared
            .wake
            .wait_timeout(state, REFRESH)
            .unwrap_or_else(|error| error.into_inner());
        state = next;
    }
}

fn draw(state: &State) {
    let line = format_line(
        &state.stage,
        &state.current,
        state.started.elapsed(),
        state.frame,
        state.width,
    );
    let mut stderr = io::stderr().lock();
    let _ = write!(stderr, "{CLEAR_LINE}{line}");
    let _ = stderr.flush();
}

fn clear() {
    let mut stderr = io::stderr().lock();
    let _ = write!(stderr, "{CLEAR_LINE}");
    let _ = stderr.flush();
}

fn format_line(
    stage: &Stage,
    current: &str,
    elapsed: Duration,
    frame: usize,
    width: usize,
) -> String {
    let counts = match stage {
        Stage::Discovering {
            directories,
            repositories,
        } => format!("Discovering · {repositories} repos · {directories} dirs"),
        Stage::Scanning {
            done,
            total,
            worktrees,
        } => format!("Scanning {done}/{total} repos · {worktrees} worktrees"),
        Stage::Deleting { done, total } => format!("Deleting {done}/{total} worktrees"),
    };
    let mut line = format!(
        "{} {counts} · {:.1}s",
        FRAMES[frame % FRAMES.len()],
        elapsed.as_secs_f64()
    );
    if !current.is_empty() {
        let prefix_width = UnicodeWidthStr::width(line.as_str()) + 3;
        let available = width.saturating_sub(prefix_width).min(60);
        if available > 1 {
            line.push_str(" · ");
            line.push_str(&tail(current, available));
        }
    }
    // Very narrow terminals still get a spinner and as much context as fits.
    prefix(&line, width)
}

fn display_path(path: &Path, home: Option<&Path>) -> String {
    let text = if let Some(home) = home.filter(|home| home.is_absolute() && home.parent().is_some())
        && let Ok(relative) = path.strip_prefix(home)
    {
        if relative.as_os_str().is_empty() {
            "~".to_owned()
        } else {
            format!("~/{}", relative.to_string_lossy())
        }
    } else {
        path.to_string_lossy().into_owned()
    };
    let mut safe = String::new();
    for character in text.chars() {
        if character.is_control()
            || matches!(
                character,
                '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
            )
        {
            safe.extend(character.escape_default());
        } else {
            safe.push(character);
        }
    }
    safe
}

fn prefix(text: &str, width: usize) -> String {
    let mut used = 0;
    text.chars()
        .take_while(|character| {
            used += UnicodeWidthChar::width(*character).unwrap_or(0);
            used <= width
        })
        .collect()
}

fn tail(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut used = 1; // The ellipsis.
    let mut tail: Vec<char> = text
        .chars()
        .rev()
        .take_while(|character| {
            used += UnicodeWidthChar::width(*character).unwrap_or(0);
            used <= width
        })
        .collect();
    tail.reverse();
    std::iter::once('…').chain(tail).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_elapsed_are_visible_during_each_stage() {
        let elapsed = Duration::from_millis(1250);
        let discovery = format_line(
            &Stage::Discovering {
                directories: 180,
                repositories: 42,
            },
            "~/src",
            elapsed,
            0,
            100,
        );
        assert_eq!(
            discovery,
            "⠋ Discovering · 42 repos · 180 dirs · 1.2s · ~/src"
        );
        let scanning = format_line(
            &Stage::Scanning {
                done: 37,
                total: 142,
                worktrees: 98,
            },
            "~/repo",
            elapsed,
            1,
            100,
        );
        assert_eq!(
            scanning,
            "⠙ Scanning 37/142 repos · 98 worktrees · 1.2s · ~/repo"
        );
        assert_eq!(
            format_line(
                &Stage::Deleting { done: 2, total: 5 },
                "~/wt/old",
                elapsed,
                2,
                100
            ),
            "⠹ Deleting 2/5 worktrees · 1.2s · ~/wt/old"
        );
    }

    #[test]
    fn paths_are_shortened_and_terminal_controls_are_escaped() {
        assert_eq!(
            display_path(Path::new("/home/me/src"), Some(Path::new("/home/me"))),
            "~/src"
        );
        assert_eq!(
            display_path(Path::new("/home/other/src"), Some(Path::new("/home/me"))),
            "/home/other/src"
        );
        assert_eq!(
            display_path(Path::new("/repo\n\u{1b}[2J\u{202e}"), None),
            "/repo\\n\\u{1b}[2J\\u{202e}"
        );
    }

    #[test]
    fn unicode_paths_fit_the_available_terminal_columns() {
        assert_eq!(tail("/long/path/你好", 8), "…th/你好");
        assert_eq!(tail("/long/path/你好", 1), "…");
        assert_eq!(tail("/long/path/你好", 0), "");
        for width in [0, 1, 10, 50, 79, 100] {
            let line = format_line(
                &Stage::Scanning {
                    done: 37,
                    total: 142,
                    worktrees: 98,
                },
                "/some/long/directory/with/a/长名字/worktree/你好",
                Duration::from_secs(1),
                0,
                width,
            );
            assert!(UnicodeWidthStr::width(line.as_str()) <= width, "{line}");
            assert!(!line.contains('\n'));
        }
    }

    #[test]
    fn disabled_progress_passes_output_through_and_finishes_repeatedly() {
        let mut progress = Progress::new(false);
        progress.discovering(1, 1, Path::new("/repo"));
        progress.scanning(1, 1, 2, Path::new("/repo"));
        progress.deleting(1, 1, Path::new("/wt"));
        assert_eq!(progress.with_output(|| 42), 42);
        progress.finish();
        progress.finish();
    }
}
