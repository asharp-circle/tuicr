//! Live startup display of `gh` / `chd` subprocess timing.
//!
//! The blocking PR open in `App::new` runs before the TUI owns the terminal,
//! so a plain-text block on **stderr** is drawn (never stdout, which may carry
//! `--stdout` export output or subprocess data). Only sanitized command
//! labels are shown; stdin payloads, header/field values and free text are
//! redacted. Tracking covers command boundaries only, not internal HTTP.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Delay before the block first appears, so fast startups stay silent.
const SHOW_AFTER: Duration = Duration::from_millis(250);
const REFRESH: Duration = Duration::from_millis(100);
const MAX_LABEL_TOKEN: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Error,
    Timeout,
    NotFound,
    Cancelled,
}

impl Outcome {
    fn text(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Error => "error",
            Outcome::Timeout => "timeout",
            Outcome::NotFound => "not found",
            Outcome::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    id: u64,
    label: String,
    started: Duration,
    finished: Option<(Duration, Outcome)>,
}

type Clock = Arc<dyn Fn() -> Duration + Send + Sync>;

/// Records command start/finish times against an injectable clock.
pub struct Tracker {
    clock: Clock,
    origin: Duration,
    entries: Mutex<Vec<Entry>>,
}

/// Finishes its command as `Cancelled` if dropped without `finish`
/// (early return, unwinding panic).
pub struct Guard {
    tracker: Arc<Tracker>,
    id: u64,
    done: bool,
}

impl Tracker {
    pub fn new(clock: Clock) -> Arc<Self> {
        let origin = clock();
        Arc::new(Self {
            clock,
            origin,
            entries: Mutex::new(Vec::new()),
        })
    }

    pub fn begin(self: &Arc<Self>, label: String) -> Guard {
        let mut entries = self.lock();
        let id = entries.len() as u64 + 1;
        entries.push(Entry {
            id,
            label,
            started: (self.clock)(),
            finished: None,
        });
        Guard {
            tracker: Arc::clone(self),
            id,
            done: false,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn finish(&self, id: u64, outcome: Outcome) {
        let now = (self.clock)();
        if let Some(entry) = self.lock().iter_mut().find(|e| e.id == id)
            && entry.finished.is_none()
        {
            entry.finished = Some((now, outcome));
        }
    }

    /// Total startup elapsed since the tracker was created.
    fn elapsed(&self) -> Duration {
        (self.clock)().saturating_sub(self.origin)
    }

    /// Render the block: header with total elapsed, then the newest rows that
    /// fit `max_rows`. Each command shows its own elapsed time; concurrent
    /// durations are never summed.
    pub fn render(&self, max_rows: usize, width: usize) -> Vec<String> {
        let now = (self.clock)();
        let entries = self.lock().clone();
        let mut lines = vec![fit(
            format!("tuicr starting... {}", fmt_duration(self.elapsed())),
            width,
        )];
        // Too narrow for a row without wrapping (which breaks cleanup).
        let keep = if width < 30 {
            0
        } else {
            max_rows.saturating_sub(1)
        };
        let skip = entries.len().saturating_sub(keep);
        for e in entries.iter().skip(skip) {
            let state = match e.finished {
                None => format!("running {}", fmt_duration(now.saturating_sub(e.started))),
                Some((end, outcome)) => format!(
                    "{} {}",
                    outcome.text(),
                    fmt_duration(end.saturating_sub(e.started))
                ),
            };
            // Truncate only the label so state and duration stay visible.
            let prefix = format!("  #{} ", e.id);
            let suffix = format!("  [{state}]");
            let room = width
                .saturating_sub(prefix.chars().count() + suffix.chars().count() + 1)
                .max(4);
            lines.push(format!("{prefix}{}{suffix}", fit(e.label.clone(), room)));
        }
        lines
    }
}

impl Guard {
    pub fn finish(mut self, outcome: Outcome) {
        self.done = true;
        self.tracker.finish(self.id, outcome);
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.done {
            self.tracker.finish(self.id, Outcome::Cancelled);
        }
    }
}

fn fmt_duration(d: Duration) -> String {
    if d < Duration::from_secs(60) {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        format!("{}m{:02}s", d.as_secs() / 60, d.as_secs() % 60)
    }
}

fn fit(line: String, width: usize) -> String {
    if width < 4 || line.chars().count() < width {
        return line;
    }
    let mut out: String = line.chars().take(width.saturating_sub(2)).collect();
    out.push('…');
    out
}

/// Value-taking flags whose argument is never displayed.
const REDACT_NEXT: &[&str] = &[
    "-H",
    "--header",
    "-f",
    "--raw-field",
    "-F",
    "--field",
    "--input",
    "-q",
    "--jq",
    "-t",
    "--template",
    "-b",
    "--body",
    "--token",
];

/// Build a readable label with secrets, payloads and free text removed.
pub fn sanitize_command(program: &str, args: &[String]) -> String {
    let mut parts = vec![program.to_string()];
    let mut redact_next = false;
    for arg in args {
        if redact_next {
            redact_next = false;
            parts.push("<redacted>".into());
            continue;
        }
        if REDACT_NEXT.contains(&arg.as_str()) {
            parts.push(arg.clone());
            redact_next = true;
            continue;
        }
        parts.push(sanitize_token(arg));
    }
    parts.join(" ")
}

fn sanitize_token(arg: &str) -> String {
    if let Some((key, _)) = arg.split_once('=')
        && arg.starts_with('-')
    {
        let flag = key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        let key = if flag {
            key.to_string()
        } else {
            sanitize_token(key)
        };
        return format!("{key}=<redacted>");
    }
    let lowered = arg.to_ascii_lowercase();
    let suspicious = [
        "token",
        "bearer",
        "secret",
        "password",
        "ghp_",
        "gho_",
        "github_pat",
    ]
    .iter()
    .any(|needle| lowered.contains(needle));
    let plain = arg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:@,#".contains(c));
    if suspicious || !plain || arg.is_empty() || arg.chars().count() > MAX_LABEL_TOKEN {
        "<redacted>".into()
    } else {
        arg.to_string()
    }
}

// ---- process-wide tracker used by the subprocess boundaries ----

static ACTIVE: Mutex<Option<Arc<Tracker>>> = Mutex::new(None);

fn active() -> Option<Arc<Tracker>> {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn is_traced_program(program: &str) -> bool {
    matches!(program, "gh" | "chd")
}

/// Begin tracking a `gh` or `chd` command; `None` when startup display is off
/// or the program is not one we show.
pub fn begin_command(program: &str, args: &[String]) -> Option<Guard> {
    if !is_traced_program(program) {
        return None;
    }
    let tracker = active()?;
    Some(tracker.begin(sanitize_command(program, args)))
}

#[cfg(test)]
pub(crate) fn install_for_test(tracker: Option<Arc<Tracker>>) {
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = tracker;
}

/// Lines currently drawn; shared so the panic hook can erase them.
static DRAWN: Mutex<usize> = Mutex::new(0);
static STOP: AtomicBool = AtomicBool::new(false);

fn erase_and_draw<W: Write>(out: &mut W, lines: &[String]) {
    draw_block(out, lines, false);
}

/// Draw under the `DRAWN` lock; when `respect_stop`, skip if a stop (panic
/// hook or `stop()`) already erased the block.
fn draw_block<W: Write>(out: &mut W, lines: &[String], respect_stop: bool) {
    let mut drawn = DRAWN.lock().unwrap_or_else(|e| e.into_inner());
    if respect_stop && STOP.load(Ordering::SeqCst) {
        return;
    }
    let mut buf = String::new();
    if *drawn > 0 {
        buf.push_str(&format!("\x1b[{}A", *drawn));
    }
    buf.push_str("\r\x1b[J");
    for line in lines {
        buf.push_str(line);
        buf.push('\n');
    }
    *drawn = lines.len();
    let _ = out.write_all(buf.as_bytes());
    let _ = out.flush();
}

/// Erase the block if a display is active; safe from a panic hook.
pub fn emergency_stop() {
    if active().is_none() {
        return;
    }
    STOP.store(true, Ordering::SeqCst);
    let mut err = std::io::stderr();
    erase_and_draw(&mut err, &[]);
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Running startup display; stops (erasing itself) on `stop` or drop.
pub struct StartupDisplay {
    handle: Option<JoinHandle<()>>,
}

impl StartupDisplay {
    /// Start only when stderr and stdin are terminals and `TERM` is capable.
    pub fn start_if_interactive() -> Option<Self> {
        let capable = std::env::var("TERM").map(|t| t != "dumb").unwrap_or(false);
        if !(std::io::stderr().is_terminal() && std::io::stdin().is_terminal() && capable) {
            return None;
        }
        Some(Self::start())
    }

    fn start() -> Self {
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        let origin = *ORIGIN.get_or_init(Instant::now);
        let tracker = Tracker::new(Arc::new(move || origin.elapsed()));
        STOP.store(false, Ordering::SeqCst);
        *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&tracker));
        let handle = std::thread::spawn(move || {
            let mut err = std::io::stderr();
            while !STOP.load(Ordering::SeqCst) {
                std::thread::sleep(REFRESH);
                if STOP.load(Ordering::SeqCst) {
                    break;
                }
                if tracker.elapsed() < SHOW_AFTER {
                    continue;
                }
                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                let lines = tracker.render((rows as usize).saturating_sub(3).max(2), cols as usize);
                draw_block(&mut err, &lines, true);
            }
        });
        Self {
            handle: Some(handle),
        }
    }

    pub fn stop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        STOP.store(true, Ordering::SeqCst);
        let _ = handle.join();
        *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let mut err = std::io::stderr();
        erase_and_draw(&mut err, &[]);
    }
}

impl Drop for StartupDisplay {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static LOCK: Mutex<()> = Mutex::new(());

    fn fake() -> (Arc<AtomicU64>, Arc<Tracker>) {
        let ms = Arc::new(AtomicU64::new(0));
        let m = Arc::clone(&ms);
        let t = Tracker::new(Arc::new(move || {
            Duration::from_millis(m.load(Ordering::SeqCst))
        }));
        (ms, t)
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn concurrent_commands_keep_individual_durations() {
        let (ms, t) = fake();
        let a = t.begin("gh a".into());
        ms.store(100, Ordering::SeqCst);
        let b = t.begin("chd b".into());
        ms.store(1100, Ordering::SeqCst);
        b.finish(Outcome::Ok);
        ms.store(2000, Ordering::SeqCst);
        a.finish(Outcome::Timeout);
        let lines = t.render(10, 100);
        assert!(lines[0].contains("2.0s"));
        assert!(lines[1].contains("#1 gh a") && lines[1].contains("timeout 2.0s"));
        assert!(lines[2].contains("#2 chd b") && lines[2].contains("ok 1.0s"));
    }

    #[test]
    fn running_command_shows_live_elapsed() {
        let (ms, t) = fake();
        let _g = t.begin("gh x".into());
        ms.store(1500, Ordering::SeqCst);
        assert!(t.render(5, 80)[1].contains("running 1.5s"));
        ms.store(2500, Ordering::SeqCst);
        assert!(t.render(5, 80)[1].contains("running 2.5s"));
    }

    #[test]
    fn dropped_guard_is_cancelled() {
        let (ms, t) = fake();
        drop(t.begin("gh x".into()));
        ms.store(10, Ordering::SeqCst);
        assert!(t.render(5, 80)[1].contains("cancelled"));
    }

    #[test]
    fn render_respects_row_and_width_limits() {
        let (_ms, t) = fake();
        for i in 0..6 {
            t.begin(format!("gh cmd{i} with a long label"))
                .finish(Outcome::Ok);
        }
        let lines = t.render(3, 20);
        assert_eq!(lines.len(), 3);
        assert!(lines[2].contains("#6"));
        assert!(lines.iter().all(|l| l.chars().count() <= 20));
    }

    #[test]
    fn very_narrow_width_shows_header_only() {
        let (_ms, t) = fake();
        let _g = t.begin("gh x".into());
        let lines = t.render(5, 20);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].chars().count() <= 20);
    }

    #[test]
    fn narrow_width_truncates_label_not_state() {
        let (_ms, t) = fake();
        t.begin("chd get-diff ".to_string() + &"a".repeat(80))
            .finish(Outcome::Ok);
        let line = &t.render(5, 60)[1];
        assert!(line.ends_with("[ok 0.0s]") && line.contains('…'));
        assert!(line.chars().count() <= 60);
    }

    #[test]
    fn labels_redact_payloads_and_secrets() {
        let label = sanitize_command(
            "gh",
            &args(&[
                "api",
                "graphql",
                "-f",
                "query=query { secret }",
                "-H",
                "Authorization: Bearer ghp_x",
                "--input",
                "-",
                "repos/o/r/pulls/185",
                "--body",
                "hello world",
                "--token=abc",
                "a b\nc\x1b[31m",
            ]),
        );
        assert_eq!(
            label,
            "gh api graphql -f <redacted> -H <redacted> --input <redacted> repos/o/r/pulls/185 --body <redacted> --token=<redacted> <redacted>"
        );
        assert!(!label.contains("ghp_") && !label.contains('\x1b'));
    }

    #[test]
    fn only_gh_and_chd_are_tracked_and_inactive_is_noop() {
        let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        install_for_test(None);
        assert!(begin_command("git", &[]).is_none());
        assert!(begin_command("gh", &[]).is_none());
    }

    #[test]
    fn draw_and_erase_emit_cursor_sequences() {
        let mut out = Vec::new();
        erase_and_draw(&mut out, &["a".into(), "b".into()]);
        erase_and_draw(&mut out, &[]);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("\x1b[2A") && s.ends_with("\x1b[J"));
        assert_eq!(*DRAWN.lock().unwrap(), 0);
    }

    #[test]
    fn process_boundary_records_gh_command_outcome() {
        let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_ms, t) = fake();
        install_for_test(Some(Arc::clone(&t)));
        let _ = crate::process::run_command_output("gh", None, ["--tuicr-no-such-flag", "x y"]);
        let _ = crate::process::run_command_output("git", None, ["--version"]);
        install_for_test(None);
        let lines = t.render(50, 200);
        assert!(lines.iter().all(|l| !l.contains(" git ")));
        let ours = lines
            .iter()
            .find(|l| l.contains("--tuicr-no-such-flag"))
            .expect("tracked gh command");
        assert!(ours.contains("gh --tuicr-no-such-flag <redacted>"));
        assert!(!ours.contains("running"));
    }
}
