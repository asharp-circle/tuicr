//! Client for the chd-owned PR cache (`chd pr-cache ...`).
//!
//! chd is the only writer of cache files; tuicr reads and requests fills through
//! the CLI. A diff is reused only on an exact (head, resolved merge base)
//! match, never on age. Any chd failure (absent, miss, error, timeout) falls
//! back to the live fetch.

use std::collections::HashSet;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::error::Result;
use crate::forge::traits::{ForgeRepository, PullRequestDetails};

const DISABLE_ENV: &str = "TUICR_CHD_CACHE";
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const FILL_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_FILLS_IN_FLIGHT: usize = 2;

fn enabled() -> bool {
    // Unit tests must never reach the developer's real chd cache.
    if cfg!(test) {
        return false;
    }
    std::env::var(DISABLE_ENV).map(|v| v != "0").unwrap_or(true)
}

/// Run `chd` with a hard timeout and return `None` on absence, non-zero exit, or timeout.
fn run_chd(args: &[String], timeout: Duration) -> Option<String> {
    run_program("chd", args, timeout)
}

fn run_program(program: &str, args: &[String], timeout: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
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
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return None;
            }
        }
    };
    let bytes = reader.join().ok()?;
    status
        .success()
        .then(|| String::from_utf8_lossy(&bytes).into_owned())
}

fn in_flight() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(Default::default)
}

/// Fire-and-forget fill request, deduplicated by key and capped in number.
fn spawn_fill(args: Vec<String>) {
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

fn key_args(op: &str, pr: &PullRequestDetails) -> Vec<String> {
    vec![
        "pr-cache".into(),
        op.into(),
        pr.repository.host.clone(),
        pr.repository.owner.clone(),
        pr.repository.name.clone(),
        pr.number.to_string(),
        pr.head_sha.clone(),
        pr.base_sha.clone(),
    ]
}

/// Cached patch for exactly this head + merge base, if chd has one.
pub fn get_diff(pr: &PullRequestDetails) -> Option<String> {
    if !enabled() || pr.head_sha.is_empty() || pr.base_sha.is_empty() {
        return None;
    }
    run_chd(&key_args("get-diff", pr), READ_TIMEOUT).filter(|patch| !patch.is_empty())
}

/// Ask chd to fill its cache for this exact (head, merge base) in the
/// background. chd fetches the SHA-pinned diff itself; tuicr never supplies
/// cache bytes, so a diff fetched while the PR moved cannot be stored under
/// the wrong identity.
pub fn request_diff_fill(pr: &PullRequestDetails) {
    if enabled() && !pr.head_sha.is_empty() && !pr.base_sha.is_empty() {
        spawn_fill(key_args("fill-diff", pr));
    }
}

/// Immutable file content at a revision SHA, if chd has it.
pub fn get_blob(repo: &ForgeRepository, sha: &str, path: &str) -> Option<String> {
    if !enabled() || sha.is_empty() {
        return None;
    }
    run_chd(&blob_args("get-blob", repo, sha, path), READ_TIMEOUT)
}

/// Ask chd to fetch and store this revision file (raw bytes) in the background.
pub fn request_blob_fill(repo: &ForgeRepository, sha: &str, path: &str) {
    if enabled() && !sha.is_empty() {
        spawn_fill(blob_args("fill-blob", repo, sha, path));
    }
}

fn blob_args(op: &str, repo: &ForgeRepository, sha: &str, path: &str) -> Vec<String> {
    vec![
        "pr-cache".into(),
        op.into(),
        repo.host.clone(),
        repo.owner.clone(),
        repo.name.clone(),
        sha.into(),
        path.into(),
    ]
}

/// Use the cached patch only if `pair` accepts it against live metadata.
/// Otherwise, fetch live and ask chd to fill the entry.
pub fn diff_with_cache<T>(
    pr: &PullRequestDetails,
    lookup: impl FnOnce(&PullRequestDetails) -> Option<String>,
    store: impl FnOnce(&PullRequestDetails),
    fetch: impl FnOnce() -> Result<String>,
    pair: impl Fn(&str) -> Result<T>,
) -> Result<T> {
    if let Some(cached) = lookup(pr)
        && let Ok(parsed) = pair(&cached)
    {
        return Ok(parsed);
    }
    let live = fetch()?;
    let parsed = pair(&live)?;
    store(pr);
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::traits::ForgeRepository;
    use std::cell::Cell;

    fn pr() -> PullRequestDetails {
        PullRequestDetails {
            repository: ForgeRepository::github("github.com", "o", "r"),
            number: 1,
            title: String::new(),
            url: String::new(),
            state: String::new(),
            is_draft: false,
            author: None,
            head_ref_name: String::new(),
            base_ref_name: String::new(),
            head_sha: "a".into(),
            base_sha: "b".into(),
            body: String::new(),
            updated_at: None,
            closed: false,
            merged_at: None,
            diff_start_sha: None,
        }
    }

    #[test]
    fn hit_skips_live_fetch() {
        let fetched = Cell::new(false);
        let out = diff_with_cache(
            &pr(),
            |_| Some("cached".into()),
            |_| panic!("no store on hit"),
            || {
                fetched.set(true);
                Ok("live".into())
            },
            |s| Ok(s.to_string()),
        )
        .unwrap();
        assert_eq!(out, "cached");
        assert!(!fetched.get());
    }

    #[test]
    fn unpairable_cache_falls_back_and_contributes_live() {
        let stored = Cell::new(false);
        let out = diff_with_cache(
            &pr(),
            |_| Some("bad".into()),
            |_| {
                stored.set(true);
            },
            || Ok("live".into()),
            |s| {
                if s == "bad" {
                    Err(crate::error::TuicrError::Io(std::io::Error::other("x")))
                } else {
                    Ok(s.to_string())
                }
            },
        )
        .unwrap();
        assert_eq!(out, "live");
        assert!(stored.get());
    }

    #[test]
    fn miss_fetches_live() {
        let out = diff_with_cache(
            &pr(),
            |_| None,
            |_| {},
            || Ok("live".into()),
            |s| Ok(s.to_string()),
        )
        .unwrap();
        assert_eq!(out, "live");
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
        set.insert("a".into());
        set.insert("b".into());
        drop(set);
        spawn_fill(vec!["c".into()]);
        assert_eq!(in_flight().lock().unwrap().len(), 2);
        in_flight().lock().unwrap().clear();
    }
}
