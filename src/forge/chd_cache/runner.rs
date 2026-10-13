//! Process runner and availability gate for the `chd` CLI.

use std::collections::HashSet;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub(super) const DISABLE_ENV: &str = "TUICR_CHD_CACHE";
/// READ_TIMEOUT: Bounds foreground cache reads tightly before live fallback.
pub(super) const READ_TIMEOUT: Duration = Duration::from_secs(3);
/// OPEN_TIMEOUT: Bounds bundle identity validation and paging before live fallback.
pub(super) const OPEN_TIMEOUT: Duration = Duration::from_secs(8);
pub(super) const FILL_TIMEOUT: Duration = Duration::from_secs(120);
pub(super) const MAX_FILLS_IN_FLIGHT: usize = 4;

pub(crate) fn is_enabled() -> bool {
    enabled()
}

/// Mock runner that answers `chd` arguments in tests without a real cache.
#[cfg(test)]
pub(crate) type FakeChd = std::rc::Rc<dyn Fn(&[String]) -> Option<String>>;

#[cfg(test)]
thread_local! {
    static FAKE_CHD: std::cell::RefCell<Option<FakeChd>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn install_fake_chd(fake: Option<FakeChd>) {
    super::bundle::clear_for_test();
    FAKE_CHD.with(|f| *f.borrow_mut() = fake);
}

#[cfg(test)]
pub(super) fn fake_chd() -> Option<FakeChd> {
    FAKE_CHD.with(|f| f.borrow().clone())
}

/// DEGRADE_COOLDOWN: Cache bypass duration after a read timeout to prevent repeated stalls.
const DEGRADE_COOLDOWN: Duration = Duration::from_secs(30);

static DEGRADED_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

fn mark_degraded(now: Instant) {
    *DEGRADED_UNTIL.lock().unwrap_or_else(|e| e.into_inner()) = Some(now + DEGRADE_COOLDOWN);
}

/// True if `chd` is enabled and outside a timeout cooldown.
fn chd_available(env_value: Option<&str>, degraded_until: Option<Instant>, now: Instant) -> bool {
    env_value.is_none_or(|v| v != "0") && degraded_until.is_none_or(|until| now >= until)
}

#[cfg(test)]
pub(super) fn enabled() -> bool {
    // Unit tests require `install_fake_chd` to avoid accessing the real cache.
    fake_chd().is_some()
}

#[cfg(not(test))]
pub(super) fn enabled() -> bool {
    let until = *DEGRADED_UNTIL.lock().unwrap_or_else(|e| e.into_inner());
    chd_available(
        std::env::var(DISABLE_ENV).ok().as_deref(),
        until,
        Instant::now(),
    )
}

/// Run `chd` with a hard timeout and return `None` on absence, non-zero exit, or timeout.
pub(super) fn run_chd(
    args: &[String],
    #[cfg_attr(test, allow(unused_variables))] timeout: Duration,
) -> Option<String> {
    #[cfg(test)]
    return fake_chd().and_then(|fake| fake(args));
    #[cfg(not(test))]
    run_program("chd", args, timeout)
}

fn run_program(program: &str, args: &[String], timeout: Duration) -> Option<String> {
    let guard = crate::startup_trace::begin_command(program, args);
    let finish = |outcome| {
        if let Some(guard) = guard {
            guard.finish(outcome);
        }
    };
    use crate::startup_trace::Outcome;
    let spawned = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(err) => {
            finish(if err.kind() == std::io::ErrorKind::NotFound {
                Outcome::NotFound
            } else {
                Outcome::Error
            });
            return None;
        }
    };
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            other => {
                if other.is_ok() && timeout < FILL_TIMEOUT {
                    mark_degraded(Instant::now());
                }
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                finish(if other.is_ok() {
                    Outcome::Timeout
                } else {
                    Outcome::Error
                });
                return None;
            }
        }
    };
    let bytes = reader.join().ok()?;
    finish(if status.success() {
        Outcome::Ok
    } else {
        Outcome::Error
    });
    status
        .success()
        .then(|| String::from_utf8_lossy(&bytes).into_owned())
}

fn in_flight() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(Default::default)
}

/// Fire-and-forget fill request, deduplicated by key and capped in number.
pub(super) fn spawn_fill(args: Vec<String>) {
    #[cfg(test)]
    if fake_chd().is_some() {
        let _ = run_chd(&args, FILL_TIMEOUT);
        return;
    }
    let key = args.join("\0");
    {
        let mut set = in_flight().lock().unwrap_or_else(|e| e.into_inner());
        if set.len() >= MAX_FILLS_IN_FLIGHT || !set.insert(key.clone()) {
            return;
        }
    }
    std::thread::spawn(move || {
        let _ = run_chd(&args, FILL_TIMEOUT);
        in_flight()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_timeout_starts_a_cooldown() {
        *DEGRADED_UNTIL.lock().unwrap() = None;
        let out = run_program("sleep", &["5".to_string()], Duration::from_millis(50));
        let until = DEGRADED_UNTIL.lock().unwrap().take();
        assert!(out.is_none());
        let until = until.expect("timeout starts a cooldown");
        assert!(until > Instant::now());
        assert!(until <= Instant::now() + DEGRADE_COOLDOWN);
    }

    #[test]
    fn missing_chd_binary_yields_none() {
        assert!(
            run_program(
                "tuicr-definitely-missing-binary",
                &[],
                Duration::from_millis(200)
            )
            .is_none()
        );
    }

    #[test]
    fn fill_requests_are_deduplicated_and_capped() {
        let mut set = in_flight().lock().unwrap();
        set.clear();
        for k in ["a", "b", "c", "d"] {
            set.insert(k.into());
        }
        drop(set);
        spawn_fill(vec!["e".into()]);
        assert_eq!(in_flight().lock().unwrap().len(), MAX_FILLS_IN_FLIGHT);
        in_flight().lock().unwrap().clear();
    }
    #[test]
    fn availability_honours_env_and_expires_cooldown() {
        let now = Instant::now();
        let later = now + DEGRADE_COOLDOWN;
        assert!(chd_available(None, None, now));
        assert!(chd_available(Some("1"), None, now));
        assert!(!chd_available(Some("0"), None, now));
        assert!(!chd_available(None, Some(later), now));
        assert!(chd_available(None, Some(later), later));
        assert!(!chd_available(Some("0"), Some(now), later));
    }
}
