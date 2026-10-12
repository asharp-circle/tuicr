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
const MAX_FILLS_IN_FLIGHT: usize = 4;

pub(crate) fn is_enabled() -> bool {
    enabled()
}

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

/// Head and base-tip SHAs of the PR, ETag-validated by chd against GitHub
/// (`If-None-Match`; a 304 proves the cached body current). `None` when chd is
/// absent or validation fails, so a cached body is never presented as fresh.
pub fn validated_pr_identity(repo: &ForgeRepository, number: u64) -> Option<(String, String)> {
    if !enabled() {
        return None;
    }
    let args: Vec<String> = [
        "pr-cache",
        "revalidate",
        &repo.host,
        &repo.owner,
        &repo.name,
        &number.to_string(),
        "pr",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let out = crate::profile::time("pr.chd_revalidate_pr", || run_chd(&args, READ_TIMEOUT))?;
    parse_pr_identity(&out)
}

fn parse_pr_identity(json: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let sha = |side: &str| {
        v.get(side)?
            .get("sha")?
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some((sha("head")?, sha("base")?))
}

/// Cached PR collection rows (`commits`, as a JSON array) for
/// exactly this head and base tip. The rows are a pure function of those two
/// SHAs, so an exact key match needs no further validation.
pub fn get_list(pr: &PullRequestDetails, name: &str) -> Option<String> {
    if !enabled() || pr.head_sha.is_empty() || pr.base_sha.is_empty() {
        return None;
    }
    run_chd(&list_args("get-list", pr, name), READ_TIMEOUT).filter(|rows| !rows.is_empty())
}

/// Ask chd to page through and store this collection. chd stores it only if
/// the PR identity is unchanged before and after its own fetch.
pub fn request_list_fill(pr: &PullRequestDetails, name: &str) {
    if enabled() && !pr.head_sha.is_empty() && !pr.base_sha.is_empty() {
        spawn_fill(list_args("fill-list", pr, name));
    }
}

fn list_args(op: &str, pr: &PullRequestDetails, name: &str) -> Vec<String> {
    let mut args = key_args(op, pr);
    // chd order: HOST OWNER REPO N NAME HEAD BASE_TIP
    let (head, base) = (args.remove(6), args.remove(6));
    args.extend([name.to_string(), head, base]);
    args
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
    let cached = crate::profile::time_with(
        "pr.diff_cache_lookup",
        || lookup(pr),
        |hit| if hit.is_some() { "hit" } else { "miss" }.to_string(),
    );
    if let Some(cached) = cached {
        let paired = crate::profile::time_with(
            "pr.diff_cache_pair",
            || pair(&cached),
            |r| if r.is_ok() { "accepted" } else { "rejected" }.to_string(),
        );
        if let Ok(parsed) = paired {
            return Ok(parsed);
        }
    }
    let live = crate::profile::time("pr.diff_live_fetch", fetch)?;
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
    fn list_args_follow_chd_order() {
        assert_eq!(
            list_args("get-list", &pr(), "commits"),
            [
                "pr-cache",
                "get-list",
                "github.com",
                "o",
                "r",
                "1",
                "commits",
                "a",
                "b"
            ]
        );
    }

    #[test]
    fn parses_pr_identity() {
        let j = r#"{"head":{"sha":"h1"},"base":{"sha":"b1"}}"#;
        assert_eq!(parse_pr_identity(j), Some(("h1".into(), "b1".into())));
        assert_eq!(
            parse_pr_identity(r#"{"head":{"sha":""},"base":{"sha":"b"}}"#),
            None
        );
        assert_eq!(parse_pr_identity("nope"), None);
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
        for k in ["a", "b", "c", "d"] {
            set.insert(k.into());
        }
        drop(set);
        spawn_fill(vec!["e".into()]);
        assert_eq!(in_flight().lock().unwrap().len(), MAX_FILLS_IN_FLIGHT);
        in_flight().lock().unwrap().clear();
    }
}
