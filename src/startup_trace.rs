//! Live startup display of `gh` and `chd` subprocess timing during `App::new`.
//!
//! Drawn to stderr so stdout remains available for `--stdout` export or pipes.
//! Command labels redact payloads and free text; tracking covers process
//! boundaries, not internal HTTP.

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
    /// Still running when the summary was printed.
    Unfinished,
}

impl Outcome {
    fn text(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Error => "error",
            Outcome::Timeout => "timeout",
            Outcome::NotFound => "not found",
            Outcome::Cancelled => "cancelled",
            Outcome::Unfinished => "unfinished",
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

/// Finishes the command as `Cancelled` if dropped before `finish`.
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

    /// Renders the header and newest rows up to `max_rows`. Each command
    /// shows its own elapsed time without summing concurrent durations.
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

impl Tracker {
    /// Formats the startup summary with full labels for scrollback; commands
    /// still running are marked `unfinished`.
    pub fn summary(&self) -> Vec<String> {
        let now = (self.clock)();
        let mut entries = self.lock();
        for e in entries.iter_mut().filter(|e| e.finished.is_none()) {
            e.finished = Some((now, Outcome::Unfinished));
        }
        let mut lines = vec![format!("tuicr startup: {}", fmt_duration(self.elapsed()))];
        for e in entries.iter() {
            let (end, outcome) = e.finished.expect("all entries finished above");
            lines.push(format!(
                "  #{} {}  [{} {}]",
                e.id,
                e.label,
                outcome.text(),
                fmt_duration(end.saturating_sub(e.started))
            ));
        }
        lines
    }

    fn has_commands(&self) -> bool {
        !self.lock().is_empty()
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
    if width == 0 {
        return String::new();
    }
    if line.chars().count() < width {
        return line;
    }
    if width < 4 {
        return line.chars().take(width.saturating_sub(1)).collect();
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

// Process-wide tracker used at subprocess boundaries.

static ACTIVE: Mutex<Option<Arc<Tracker>>> = Mutex::new(None);

fn active() -> Option<Arc<Tracker>> {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn is_traced_program(program: &str) -> bool {
    matches!(program, "gh" | "chd")
}

/// Tracks a `gh` or `chd` command; `None` for other programs or display off.
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

/// Draws under lock; with `respect_stop`, skips if a stop already ran.
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

/// Finalizes the display on panic or signal. See `finalize_to`.
pub fn emergency_stop() {
    finalize_to(&mut std::io::stderr());
}

/// Replaces the live block with the scrollback summary once. Silent if no
/// command ran or already finalized.
fn finalize_to<W: Write>(out: &mut W) {
    let mut drawn = DRAWN.lock().unwrap_or_else(|e| e.into_inner());
    STOP.store(true, Ordering::SeqCst);
    let Some(tracker) = ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    let mut buf = String::new();
    if *drawn > 0 {
        buf.push_str(&format!("\x1b[{}A\r\x1b[J", *drawn));
    }
    *drawn = 0;
    if tracker.has_commands() {
        for line in tracker.summary() {
            buf.push_str(&line);
            buf.push('\n');
        }
    }
    let _ = out.write_all(buf.as_bytes());
    let _ = out.flush();
}

/// Prints the summary on SIGINT/SIGTERM/SIGHUP and triggers default action.
/// Installed once for process lifetime on a thread, not in the signal handler.
#[cfg(unix)]
fn install_signal_eraser() {
    use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        // Leave signals ignored at launch (`nohup`) ignored.
        let handled: Vec<i32> = [SIGINT, SIGTERM, SIGHUP]
            .into_iter()
            .filter(|&sig| !is_ignored(sig))
            .collect();
        let Ok(mut signals) = signal_hook::iterator::Signals::new(handled) else {
            return;
        };
        std::thread::spawn(move || {
            if let Some(signal) = signals.forever().next() {
                emergency_stop();
                let _ = signal_hook::low_level::emulate_default_handler(signal);
                std::process::exit(128 + signal);
            }
        });
    });
}

#[cfg(unix)]
fn is_ignored(signal: i32) -> bool {
    // SAFETY: a null `act` only queries the current disposition.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        libc::sigaction(signal, std::ptr::null(), &mut old) == 0
            && old.sa_sigaction == libc::SIG_IGN
    }
}

/// Startup timing display; `stop` or drop prints the durable summary.
pub struct StartupDisplay {
    handle: Option<JoinHandle<()>>,
    stopped: bool,
}

impl StartupDisplay {
    /// Tracks startup commands, redrawing live only on capable interactive
    /// terminals; other stderr gets just the final summary.
    pub fn start_for_startup() -> Self {
        let capable = std::env::var("TERM").map(|t| t != "dumb").unwrap_or(false);
        let live = std::io::stderr().is_terminal() && std::io::stdin().is_terminal() && capable;
        Self::begin(live)
    }

    fn begin(live: bool) -> Self {
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        let origin = *ORIGIN.get_or_init(Instant::now);
        let tracker = Tracker::new(Arc::new(move || origin.elapsed()));
        STOP.store(false, Ordering::SeqCst);
        *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&tracker));
        #[cfg(unix)]
        install_signal_eraser();
        if !live {
            return Self {
                handle: None,
                stopped: false,
            };
        }
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
            stopped: false,
        }
    }

    pub fn stop(&mut self) {
        if std::mem::replace(&mut self.stopped, true) {
            return;
        }
        STOP.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        finalize_to(&mut std::io::stderr());
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
        let lines = t.render(3, 30);
        assert_eq!(lines.len(), 3);
        assert!(lines[2].contains("#6"));
        assert!(lines.iter().all(|l| l.chars().count() <= 30));
    }

    #[test]
    fn tiny_widths_never_exceed_width() {
        for width in 0..4 {
            assert!(fit("tuicr starting... 1.0s".into(), width).chars().count() <= width);
        }
        let (_ms, t) = fake();
        for width in 0..4 {
            assert!(t.render(5, width)[0].chars().count() <= width);
        }
    }

    /// Kills and reaps the child on drop so failed assertions cannot leak it.
    #[cfg(unix)]
    struct ChildGuard(std::process::Child);

    #[cfg(unix)]
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(unix)]
    fn spawn_signal_child(ignore_hup: bool) -> ChildGuard {
        use std::io::Read;
        let exe = std::env::current_exe().unwrap();
        let test = "startup_trace::tests::signal_child_entry";
        let mut command = if ignore_hup {
            let mut c = std::process::Command::new("sh");
            c.arg("-c")
                .arg(r#"trap '' HUP; exec "$0" --exact "$1" --nocapture"#)
                .arg(exe)
                .arg(test);
            c
        } else {
            let mut c = std::process::Command::new(exe);
            c.args(["--exact", test, "--nocapture"]);
            c
        };
        let mut child = ChildGuard(
            command
                .env("TUICR_SIGNAL_CHILD", "1")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );
        let mut stdout = child.0.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            let mut byte = [0u8; 1];
            while !String::from_utf8_lossy(&seen).contains("READY") {
                if !matches!(stdout.read(&mut byte), Ok(1)) {
                    break;
                }
                seen.push(byte[0]);
            }
            let _ = tx.send(String::from_utf8_lossy(&seen).contains("READY"));
        });
        let ready = rx.recv_timeout(Duration::from_secs(10));
        assert_eq!(ready, Ok(true), "child never reported READY");
        child
    }

    #[cfg(unix)]
    fn kill_signal(child: &ChildGuard, name: &str) {
        let status = std::process::Command::new("kill")
            .args([format!("-{name}"), child.0.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// Entry point re-executed by the signal tests; a no-op in a normal run.
    #[cfg(unix)]
    #[test]
    fn signal_child_entry() {
        if std::env::var("TUICR_SIGNAL_CHILD").is_err() {
            return;
        }
        let mut display = StartupDisplay::begin(false);
        if std::env::var("TUICR_SIGNAL_CHILD").as_deref() == Ok("summary") {
            let _guard = begin_command("gh", &args(&["pr", "view", "7"]));
            println!("READY");
            let _ = std::io::stdout().flush();
            std::thread::sleep(Duration::from_secs(10));
            return;
        }
        display.stop();
        println!("READY");
        let _ = std::io::stdout().flush();
        std::thread::sleep(Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn signals_keep_default_action_after_display_stops() {
        use std::os::unix::process::ExitStatusExt;
        let mut child = spawn_signal_child(false);
        kill_signal(&child, "TERM");
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "SIGTERM was ignored after stop()"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(status.signal(), Some(15));
    }

    #[cfg(unix)]
    #[test]
    fn sigterm_during_startup_persists_summary_on_stderr() {
        use std::io::Read;
        let exe = std::env::current_exe().unwrap();
        let mut child = ChildGuard(
            std::process::Command::new(exe)
                .args([
                    "--exact",
                    "startup_trace::tests::signal_child_entry",
                    "--nocapture",
                ])
                .env("TUICR_SIGNAL_CHILD", "summary")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut stdout = child.0.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut seen = String::new();
            let mut byte = [0u8; 1];
            while !seen.contains("READY") && matches!(stdout.read(&mut byte), Ok(1)) {
                seen.push(byte[0] as char);
            }
            let _ = tx.send(seen.contains("READY"));
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)), Ok(true));
        kill_signal(&child, "TERM");
        let mut err = String::new();
        child
            .0
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut err)
            .unwrap();
        assert!(err.contains("tuicr startup:"), "{err:?}");
        assert!(err.contains("#1 gh pr view 7  [unfinished"), "{err:?}");
        assert!(!err.contains("\x1b"), "no escapes on non-tty: {err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn inherited_ignored_sighup_stays_ignored() {
        let child = spawn_signal_child(true);
        kill_signal(&child, "HUP");
        std::thread::sleep(Duration::from_millis(500));
        let mut child = child;
        let survived = child.0.try_wait().unwrap().is_none();
        assert!(survived, "ignored SIGHUP terminated the process");
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
    fn finalize_replaces_live_block_with_durable_summary_once() {
        let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (ms, t) = fake();
        install_for_test(Some(Arc::clone(&t)));
        let done = t.begin("gh pr view 1 --json <redacted>".into());
        ms.store(1500, Ordering::SeqCst);
        done.finish(Outcome::Ok);
        let _running = t.begin("chd pr-cache open".into());
        ms.store(2000, Ordering::SeqCst);
        let mut out = Vec::new();
        draw_block(&mut out, &t.render(10, 100), false);
        let mut out2 = Vec::new();
        finalize_to(&mut out2);
        let s = String::from_utf8(out2).unwrap();
        let body = s
            .strip_prefix("\x1b[3A\r\x1b[J")
            .unwrap_or_else(|| panic!("erases live block: {s:?}"));
        assert!(s.contains("tuicr startup: 2.0s\n"));
        assert!(s.contains("#1 gh pr view 1 --json <redacted>  [ok 1.5s]\n"));
        assert!(s.contains("#2 chd pr-cache open  [unfinished 0.5s]\n"));
        assert!(s.ends_with('\n'), "no trailing erase");
        assert!(!body.contains('\x1b'), "summary has no escapes");
        let mut again = Vec::new();
        finalize_to(&mut again);
        assert!(again.is_empty(), "no duplicate summary");
        assert_eq!(*DRAWN.lock().unwrap(), 0);
    }

    #[test]
    fn non_live_summary_has_no_escape_sequences() {
        let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_ms, t) = fake();
        install_for_test(Some(Arc::clone(&t)));
        t.begin("gh api user".into()).finish(Outcome::NotFound);
        let mut out = Vec::new();
        finalize_to(&mut out);
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains('\x1b'));
        assert!(s.contains("gh api user  [not found 0.0s]"));
    }

    #[test]
    fn no_commands_means_no_output() {
        let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_ms, t) = fake();
        install_for_test(Some(t));
        let mut out = Vec::new();
        finalize_to(&mut out);
        assert!(out.is_empty());
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

    #[test]
    fn emergency_stop_deactivates_tracking_and_is_idempotent() {
        let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_ms, t) = fake();
        install_for_test(Some(t));
        assert!(begin_command("gh", &[]).is_some());
        let mut sink = Vec::new();
        finalize_to(&mut sink);
        assert!(begin_command("gh", &[]).is_none());
        finalize_to(&mut sink);
        assert_eq!(*DRAWN.lock().unwrap(), 0);
    }
}
